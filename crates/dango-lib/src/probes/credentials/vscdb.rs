use std::collections::HashMap;
use std::path::Path;

use rusqlite::{Connection, OpenFlags};

/// 把 SQLite 的 value 列转成字符串。
///
/// VS Code 系的 `state.vscdb` 里 value 通常是 TEXT，但也可能是 BLOB 或数字，
/// 所以统一按 `rusqlite::types::Value` 取出再转换，
/// 避免单个键的类型不匹配让整条查询（乃至整个数据源）废掉。
fn sqlite_value_to_string(value: rusqlite::types::Value) -> Option<String> {
    use rusqlite::types::Value as SqlValue;
    match value {
        SqlValue::Null => None,
        SqlValue::Integer(i) => Some(i.to_string()),
        SqlValue::Real(f) => Some(f.to_string()),
        SqlValue::Text(s) => Some(s),
        SqlValue::Blob(b) => String::from_utf8(b).ok(),
    }
}

/// 打开 `state.vscdb`，优先**只读**。
///
/// 只读是首选：这些库是 Cursor / Devin Desktop 正在使用的活文件，只读连接不会抢写锁。
/// 实测这台机器上 Cursor 的 WAL 库（包括 `-wal` 非空、`-shm` 缺失的崩溃现场）
/// 纯 `SQLITE_OPEN_READ_ONLY` 也能打开，所以正常情况下走的都是只读路径。
///
/// 但只读并非在所有环境下都成立：只读连接无法创建 `-shm`。以下情况只读打开会失败：
/// 需要 WAL 恢复而库所在目录 / 文件系统不可写、Windows 上文件被独占或权限受限，
/// 以及**Cursor 应用自己正以读写方式持有这个库**——它是常驻的写入方，我们的连接只是
/// 众多读者之一。
/// 因此保留一条降级路径：
/// - 不带 `SQLITE_OPEN_CREATE`：库文件不存在时照样报错，绝不新建文件；
/// - 降级后立即 `PRAGMA query_only`，这条连接发不出任何写语句
///   （实测写入返回 "attempt to write a readonly database"），
///   只会按需重建 `-wal` / `-shm`，与 Cursor / VS Code 自己打开这个库的行为一致。
pub(crate) fn open_vscdb_readonly(db_path: &Path) -> Result<Connection, String> {
    let ro_flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = match Connection::open_with_flags(db_path, ro_flags) {
        Ok(conn) => conn,
        Err(ro_err) => {
            let rw_flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
            let conn = Connection::open_with_flags(db_path, rw_flags).map_err(|rw_err| {
                format!(
                    "cred.sqlite_open: path={} ro={} rw={}",
                    db_path.display(),
                    ro_err,
                    rw_err
                )
            })?;
            conn.pragma_update(None, "query_only", true).map_err(|e| {
                format!(
                    "cred.sqlite_readonly_mode: path={} err={}",
                    db_path.display(),
                    e
                )
            })?;
            conn
        }
    };

    // 活文件可能被对方短暂独占，给一点点等待时间，而不是一撞锁就失败
    let _ = conn.busy_timeout(std::time::Duration::from_millis(500));

    Ok(conn)
}

/// 从 VS Code 系的 `state.vscdb`（SQLite，表结构 `ItemTable(key, value)`）里读取指定键。
///
/// 用 bundled rusqlite 直接读，不再 spawn `sqlite3` / `python3` / PowerShell：
/// Windows 10/11 不自带 sqlite3.exe，普通 Windows 也没装 System.Data.SQLite 程序集，
/// 那些外部命令会「命令找不到 → if let Ok(...) 静默失败」，是排查困难的根源。
///
/// 返回的 map 只包含实际存在的键；打不开或查询失败时返回带路径的可读错误，不静默吞掉。
pub(crate) fn read_vscdb_items(
    db_path: &Path,
    keys: &[&str],
) -> Result<HashMap<String, String>, String> {
    let conn = open_vscdb_readonly(db_path)?;

    let mut stmt = conn
        .prepare("SELECT value FROM ItemTable WHERE key = ?1")
        .map_err(|e| format!("cred.sqlite_query: path={} err={}", db_path.display(), e))?;

    let mut out = HashMap::new();
    for key in keys {
        let row = stmt.query_row(rusqlite::params![key], |row| {
            row.get::<_, rusqlite::types::Value>(0)
        });
        match row {
            Ok(value) => {
                if let Some(s) = sqlite_value_to_string(value) {
                    out.insert((*key).to_string(), s);
                }
            }
            // 键不存在属于正常情况（没登录 / 版本不同），跳过即可
            Err(rusqlite::Error::QueryReturnedNoRows) => {}
            Err(e) => {
                return Err(format!(
                    "cred.key_read: path={} key={} err={}",
                    db_path.display(),
                    key,
                    e
                ));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SQLite 的 value 列在不同 VS Code 版本里类型会变，
    /// 必须逐类型都能转成字符串，而不是只认 TEXT。
    #[test]
    fn sqlite_values_convert_by_type() {
        use rusqlite::types::Value as SqlValue;

        assert_eq!(
            sqlite_value_to_string(SqlValue::Text("token".into())),
            Some("token".to_string())
        );
        assert_eq!(
            sqlite_value_to_string(SqlValue::Integer(42)),
            Some("42".to_string())
        );
        assert_eq!(
            sqlite_value_to_string(SqlValue::Real(1.5)),
            Some("1.5".to_string())
        );
        assert_eq!(
            sqlite_value_to_string(SqlValue::Blob(b"blob".to_vec())),
            Some("blob".to_string())
        );
        // NULL 表示键存在但无值，以及非 UTF-8 的 BLOB，都应视为「读不到」
        assert_eq!(sqlite_value_to_string(SqlValue::Null), None);
        assert_eq!(
            sqlite_value_to_string(SqlValue::Blob(vec![0xff, 0xfe])),
            None
        );
    }
}
