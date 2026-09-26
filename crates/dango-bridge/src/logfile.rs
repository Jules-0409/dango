//! 请求日志文件的落盘与落盘前的滚动。
//!
//! `requests.jsonl` 以前是「只 append、从不收拾」：跑久了能涨到几百 MB，出事想捞最近
//! 几条还得先想办法把这坨切开。这里补上滚动 —— 写入前发现超标就把老内容转走。
//!
//! 规则（连同「为什么」一起写在这，改之前先读这段）：
//!   - 单文件上限 8MB。自用服务的日志是拿来事后翻的，8MB 够装几万条请求；再大用编辑器
//!     打开都卡，而且真正在查的往往是最近那几份。
//!   - 最多留 5 份历史。再多基本没人会看，白占磁盘还拖慢备份。
//!   - 后缀用纯数字：最新一份是 `requests.jsonl.1`，最旧的是 `.5`。不用时间戳，是因为
//!     时间戳要么依赖系统时钟（回调时会重复）、要么得多存一份状态；数字后缀一次 rename
//!     链就能推进，简单且不会撞名。
//!   - **当前文件**始终叫 `requests.jsonl`、始终在同一目录下 —— 面板的
//!     `GET /logs/recent` 读的就是它，滚动不能让这个路径语义变掉。
//!
//! 热路径上的开销：正常情况下每条日志只是「在内存里加一个长度 + 一次 append」，不碰任何
//! stat。只有累加值快撞上限、确实需要滚动时，才去动一整套文件系统调用。

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use tokio::io::AsyncWriteExt;

/// 当前日志文件名（面板 `/logs/recent` 认这个名字，别改）。
pub const CURRENT_FILE_NAME: &str = "requests.jsonl";

/// 单文件上限（放宽至约 32MB），见模块头。文件不会涨过它（单行本身就超标的极端情况除外）。
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// 历史文件份数上限，见模块头。
pub const KEEP_HISTORY: usize = 5;

/// 把一行日志追加到 `dir/requests.jsonl`，需要时先滚动。
///
/// 只在「文件确实要超过上限」时才动文件系统。滚动失败不会吞掉这一行 —— 照旧追加，
/// 再把错误交回调用方（调用方会打成「写日志失败：…」），既不静默丢日志、也不让请求失败。
pub async fn append_line(dir: &Path, line: &str) -> io::Result<()> {
    append_with(MAX_FILE_BYTES, KEEP_HISTORY, dir, line).await
}

/// 记「本进程见过这个文件已经写到多大了」，键是当前文件的完整路径。
///
/// 为什么不每条日志前都 stat 一下：热路径上多一次系统调用就是白花的钱，而这个大小本来
/// 就能自己累加出来。代价是别的进程也往同一文件写时我们会偏小 —— 桥是单进程在跑，接受
/// 这个偏差；真撞上限要滚动时会再 stat 一次，按真实值决定转不转。
fn sizes() -> &'static Mutex<HashMap<PathBuf, u64>> {
    static SIZES: OnceLock<Mutex<HashMap<PathBuf, u64>>> = OnceLock::new();
    SIZES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn tracked(path: &Path) -> Option<u64> {
    let map = sizes().lock().unwrap_or_else(|err| err.into_inner());
    map.get(path).copied()
}

fn store(path: &Path, size: u64) {
    let mut map = sizes().lock().unwrap_or_else(|err| err.into_inner());
    map.insert(path.to_path_buf(), size);
}

/// 滚动要连着 rename 一串文件，两份并发滚动会把后缀顺序搅乱，得串起来。
/// 但这把锁只在「快到上限」时才抢，正常写入不经过它，热路径不受影响。
fn rotate_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// 内部实现：把上限/份数显式传进来，好让单测用很小的阈值就能跑到滚动路径。
async fn append_with(max_bytes: u64, keep: usize, dir: &Path, line: &str) -> io::Result<()> {
    // 第一次写日志时目录可能还不存在，和原来的行为一致：写之前补上。
    tokio::fs::create_dir_all(dir).await?;
    let current = dir.join(CURRENT_FILE_NAME);

    let mut written = match tracked(&current) {
        Some(size) => size,
        None => {
            // 进程刚起来、第一次见这个文件：只在这里 stat 一次，把已有大小接上。
            // 否则重启后从 0 开始累加，会把一个已经很大的文件又白白撑满一个上限。
            let size = tokio::fs::metadata(&current)
                .await
                .map(|meta| meta.len())
                .unwrap_or(0);
            store(&current, size);
            size
        }
    };

    let mut rotation_error = None;
    if written + line.len() as u64 > max_bytes {
        let _guard = rotate_lock().lock().await;
        // 拿到锁后再看一次真实大小：前面排队的请求可能已经替我们转过了。
        let actual = tokio::fs::metadata(&current)
            .await
            .map(|meta| meta.len())
            .unwrap_or(0);
        if actual + line.len() as u64 > max_bytes {
            match rotate(&current, keep).await {
                Ok(()) => written = 0,
                Err(err) => {
                    // 转不动（权限、磁盘满、并发……）就照着老文件继续追加 ——
                    // 宁可这一段超出上限，也不能因为转档失败把这条日志吃掉。
                    rotation_error = Some(io::Error::new(
                        err.kind(),
                        format!("滚动失败，本行仍已追加：{err}"),
                    ));
                    written = actual;
                }
            }
        } else {
            written = actual;
        }
    }

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&current)
        .await?;
    file.write_all(line.as_bytes()).await?;
    file.flush().await?;
    store(&current, written + line.len() as u64);

    match rotation_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// 把 `current` 转成 `.1`，原来的 `.1` 后退成 `.2`……最旧的 `.keep` 丢掉。
async fn rotate(current: &Path, keep: usize) -> io::Result<()> {
    if keep == 0 {
        // 不保留历史就等于直接清空当前文件 —— 用 rename 覆盖成空更省事，
        // 但这里选择删掉，语义上「不保留」不该留下一个空文件占位。
        return remove_if_exists(current).await;
    }
    // 先删最旧的一份，再从后往前挪：反着来能保证每一步的目的地都是空的
    // （`.keep` 删了，`.keep-1` 才能安全地挪成 `.keep`，依次类推）。
    remove_if_exists(&history_path(current, keep)).await?;
    for index in (1..keep).rev() {
        rename_if_exists(
            &history_path(current, index),
            &history_path(current, index + 1),
        )
        .await?;
    }
    tokio::fs::rename(current, history_path(current, 1)).await?;
    Ok(())
}

/// `requests.jsonl` + `2` → `requests.jsonl.2`。
fn history_path(current: &Path, index: usize) -> PathBuf {
    let mut name = current.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{index}"));
    current.with_file_name(name)
}

/// 删文件，但「本来就没有」不算错 —— 滚动过程里缺某一份是正常的。
async fn remove_if_exists(path: &Path) -> io::Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// 挪文件，但「源本来就没有」不算错（历史还没排满时就会这样）。
async fn rename_if_exists(from: &Path, to: &Path) -> io::Result<()> {
    match tokio::fs::rename(from, to).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// 每个测试用独立目录：大小是全局按路径记的，共用目录会互相干扰。
    fn temp_dir(tag: &str) -> PathBuf {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "antigravity-logfile-{tag}-{}-{nanos}-{seq}",
            std::process::id()
        ))
    }

    /// 测完就把目录删掉，别在系统临时目录里留垃圾。
    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        out.sort();
        out
    }

    fn read(dir: &Path, name: &str) -> String {
        std::fs::read_to_string(dir.join(name)).unwrap_or_default()
    }

    /// 10 字节一行，配合 40 字节的上限正好 4 行一滚。
    const LINE: &str = "012345678\n";

    #[tokio::test]
    async fn rotates_once_the_current_file_outgrows_the_limit() {
        let dir = TempDir(temp_dir("rotate"));
        for _ in 0..5 {
            append_with(40, 5, &dir.0, LINE).await.unwrap();
        }
        // 第 5 行把前 4 行顶进了 .1，当前文件重新从这一行开始
        assert!(dir.0.join(CURRENT_FILE_NAME).exists());
        assert_eq!(read(&dir.0, "requests.jsonl.1"), LINE.repeat(4));
        assert_eq!(read(&dir.0, CURRENT_FILE_NAME), LINE);
    }

    #[tokio::test]
    async fn history_count_never_exceeds_the_keep_limit() {
        let dir = TempDir(temp_dir("cap"));
        // 21 行、每 5 行滚一次 => 滚 5 次；keep=2 时目录里最多 1 份当前 + 2 份历史
        for _ in 0..21 {
            append_with(40, 2, &dir.0, LINE).await.unwrap();
        }
        let names = names(&dir.0);
        assert!(names.len() <= 3, "历史份数没被截住：{names:?}");
        assert!(names.contains(&CURRENT_FILE_NAME.to_string()));
        assert!(names.contains(&"requests.jsonl.1".to_string()));
        assert!(names.contains(&"requests.jsonl.2".to_string()));
    }

    #[tokio::test]
    async fn current_path_is_stable_and_old_lines_stay_readable() {
        let dir = TempDir(temp_dir("semantics"));
        for _ in 0..5 {
            append_with(40, 5, &dir.0, LINE).await.unwrap();
        }
        // 当前文件还是老名字、老位置 —— 面板 /logs/recent 就认它
        let current = dir.0.join(CURRENT_FILE_NAME);
        assert!(current.is_file());
        // 最新那行在当前文件里，旧的 4 行在历史文件里能读到
        assert!(read(&dir.0, CURRENT_FILE_NAME).contains("012345678"));
        assert_eq!(read(&dir.0, "requests.jsonl.1").lines().count(), 4);
    }

    #[tokio::test]
    async fn a_small_file_is_not_rotated() {
        let dir = TempDir(temp_dir("small"));
        for _ in 0..2 {
            append_with(40, 5, &dir.0, LINE).await.unwrap();
        }
        // 没到上限就只该有当前文件一个，不该凭空多出历史
        assert_eq!(names(&dir.0), vec![CURRENT_FILE_NAME.to_string()]);
    }

    #[tokio::test]
    async fn a_failed_rotation_still_appends_and_reports() {
        let dir = TempDir(temp_dir("fail"));
        // 让滚动的第一步就失败：历史位置被一个非空目录占着，删不掉也挪不动。
        std::fs::create_dir_all(dir.0.join("requests.jsonl.1").join("busy")).unwrap();
        let mut last = Ok(());
        for _ in 0..5 {
            // 前 4 行正常写，第 5 行会触发滚动并失败
            last = append_with(40, 1, &dir.0, LINE).await;
        }
        assert!(last.is_err(), "滚动失败应该如实报出来，而不是装没事");
        // 关键：报错归报错，这一行没被吃掉
        assert_eq!(read(&dir.0, CURRENT_FILE_NAME).lines().count(), 5);
    }
}
