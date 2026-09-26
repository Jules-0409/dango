//! 自用面板的一整页 HTML。
//!
//! 页面本体在仓库根目录的 `ui/panel.html`（**JS 版和 Rust 版共用同一份**：
//! JS 那边是运行时读文件，这边是编译期 `include_str!` 嵌进来）—— 改一处两边都改到，
//! 观感不会漂移。数据全部现拉：`/healthz`、`/logs/recent`、`/quota?all=1`。
//!
//! 页面里的 `__VERSION__` / `__RUNTIME__` 两个占位符只做简单替换（没有别的插值，
//! 所以不需要模板引擎，也就没有注入面）。

/// 面板 HTML 模板（占位符：`__VERSION__`、`__RUNTIME__`）
const PANEL_HTML: &str = include_str!("../ui/panel.html");

/// 渲染整页。`version` 是桥的版本号，运行时标注成 Rust（面板上能一眼看出是哪个实现）。
pub fn render_panel(version: &str) -> String {
    PANEL_HTML
        .replace("__VERSION__", version)
        .replace("__RUNTIME__", "Rust")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_version_and_runtime_into_the_page() {
        let html = render_panel("1.2.3");
        assert!(html.contains("<title>antigravity-bridge 1.2.3</title>"));
        assert!(html.contains("v1.2.3 · Rust"));
        assert!(!html.contains("__VERSION__"), "占位符没换干净");
        assert!(!html.contains("__RUNTIME__"), "占位符没换干净");
    }

    #[test]
    fn page_pulls_the_same_endpoints_as_the_js_panel() {
        let html = render_panel("0.0.0");
        for needle in [
            "/healthz",
            "/logs/recent?n=25",
            "/logs/entry?id=",
            "/quota?all=1",
            "/control/restart",
        ] {
            assert!(html.contains(needle), "少了 {needle}");
        }
        // 深色模式那套显式配色不能丢（丢了就是白底白字）
        assert!(html.contains("--fg: #1d1d1f"));
        assert!(html.contains("prefers-color-scheme: dark"));
        // 窗口走原生标题栏（拖动交给系统）：页面里不该再有自绘拖动区，
        // 也不用给红绿灯留左边距 —— 留了就是白边，还是拖动发飘的老路子
        assert!(!html.contains("-webkit-app-region"));
        assert!(!html.contains("body.tauri header"));
    }
}
