//! The fixed local port table. Every service binds exactly its port and
//! exits if it is taken — none of them ever hunts for a free one — so a port
//! number alone identifies the service, which is what the token ledger's
//! "who paid" routing relies on.
//!
//! The bridges are separate binaries without a dango-lib dependency; their
//! own defaults (`dango-bridge` `DEFAULT_PORT`, `cursor-bridge` config) are
//! pinned to these numbers by tests in both crates and by
//! `every_local_url_in_the_workspace_uses_a_listed_port` below.

/// The widget's own control API / settings page.
pub const CONTROL_API: u16 = 8049;
/// Gemini (Antigravity account pool) + Qoder bridge.
pub const GEMINI_BRIDGE: u16 = 8050;
/// Cursor Agent wrapper (runs each request through the Cursor CLI agent —
/// not a pass-through proxy, so the token ledger keeps nothing from it).
pub const CURSOR_BRIDGE: u16 = 8052;

pub const ALL: [u16; 3] = [CONTROL_API, GEMINI_BRIDGE, CURSOR_BRIDGE];

#[cfg(test)]
mod tests {
    use super::*;

    fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name != "target") {
                    rust_files(&path, out);
                }
            } else if path
                .extension()
                .is_some_and(|ext| ext == "rs" || ext == "js")
            {
                out.push(path);
            }
        }
    }

    /// Test-only values: 8051 = "someone overrode the port", 8060 = a fake
    /// agent endpoint, 8099 = a retired proxy.
    const FIXTURE_PORTS: [u16; 3] = [8051, 8060, 8099];

    #[test]
    fn every_local_url_in_the_workspace_uses_a_listed_port() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut files = Vec::new();
        rust_files(&root.join("crates"), &mut files);
        rust_files(&root.join("ui"), &mut files);
        let mut strays = Vec::new();
        for file in files {
            if file.ends_with("ports.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&file).unwrap_or_default();
            for (at, _) in text.match_indices("127.0.0.1:80") {
                let digits: String = text[at + 10..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                let Ok(port) = digits.parse::<u16>() else {
                    continue;
                };
                if !ALL.contains(&port) && !FIXTURE_PORTS.contains(&port) {
                    strays.push(format!("{}: {port}", file.display()));
                }
            }
        }
        assert!(strays.is_empty(), "端口表之外的本机端口：{strays:?}");
    }
}
