//! 构建身份：把 git sha / 编译时间 / 目标三元组塞成编译期环境变量，让 `/version`
//! 报得出「现在跑的到底是哪一次编译」——这几天编了七八次，光看 version=0.1.0
//! 认不出来，得靠 sha + 编译时间才能对上号。
//!
//! 全程「取不到也绝不红构建」：没装 git、源码包里没有 .git、TARGET 没设，一律退化成
//! "unknown"，而不是让整个 workspace 编不动。

use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    println!("cargo:rustc-env=BRIDGE_GIT_SHA={}", git_sha());
    println!("cargo:rustc-env=BRIDGE_BUILT_AT={}", built_at());
    println!(
        "cargo:rustc-env=BRIDGE_TARGET={}",
        std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string())
    );

    // 只在「本包源码 / build.rs / HEAD 动过」时重跑：HEAD 一动就是新 commit，面板上的
    // sha 必须跟着变；src 动过说明是新一轮编译，编译时间也得刷新。
    //
    // 但别只盯 .git/HEAD：在分支上提交时 HEAD 文件的内容（`ref: refs/heads/main`）不变，
    // 变的是 refs/heads/main —— 只看 HEAD 的话，「提交但没改代码」那一笔会让 /version 里
    // 的 sha 差一代（实测踩到过）。reflog（.git/logs/HEAD）每次提交 / 切换 / 重置都会追加，
    // 盯它才追得上；clone 出来还没写过 reflog 的仓库里它不存在，cargo 会把「一直没有」
    // 当没变，不会白重跑。
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/logs/HEAD");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
}

/// 仓库根的 short sha（`git rev-parse --short HEAD`）。
///
/// build.rs 的 cwd 是包目录（core/），仓库根在上一级，所以用 `git -C` 显式指过去，
/// 不依赖调用方的 cwd。任何一步不成立（没装 git、不在仓库里、沙箱里没有 .git）都回
/// "unknown"：这是给人看的诊断信息，不值得把构建搞挂。
fn git_sha() -> String {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let Some(root) = Path::new(&manifest).parent() else {
        return "unknown".to_string();
    };
    match Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
    {
        Ok(out) if out.status.success() => {
            let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if sha.is_empty() {
                "unknown".to_string()
            } else {
                sha
            }
        }
        _ => "unknown".to_string(),
    }
}

/// 编译时间（UTC ISO8601，秒精度，形如 2026-09-17T21:00:00Z）。
///
/// 优先认 SOURCE_DATE_EPOCH（可复现构建那一套，CI 里想钉死时间就设它），没有就用当前
/// 系统时间。时间换算是手写的：build.rs 里引不了 core 自己的 iso_from_millis（那要等
/// crate 编出来），引 chrono 又违反「离线不加依赖」，跑 `date -u` 则多一个外部命令依赖
/// ——手写这十几行反而最稳。
fn built_at() -> String {
    let secs = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        });
    iso8601_utc(secs)
}

/// epoch 秒 → `YYYY-MM-DDTHH:MM:SSZ`。日期部分用 Howard Hinnant 的 civil_from_days，
/// 和 core/src/types.rs 的 `iso_from_millis` 是同一套算法（那边是毫秒，这里只需要秒）。
fn iso8601_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // days_from_civil 的逆运算：把「1970-01-01 起的天数」换回 (年, 月, 日)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year0 = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year0 + 1 } else { year0 };

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}
