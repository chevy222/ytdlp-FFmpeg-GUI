//! 站点 Cookie 管理命令（设置-Cookie 页）；登录窗经 `save_cookies_inner` 直接调用（不走 IPC）。

use super::{ensure_main_window, err_string, CmdResult};
use crate::state::AppState;
use tauri::{AppHandle, Emitter, Manager};
use ytdlp_core::cookies::CookieStore;

#[tauri::command(async)]
pub fn list_cookies(
    app: AppHandle,
    win: tauri::WebviewWindow,
) -> CmdResult<Vec<serde_json::Value>> {
    ensure_main_window(&win)?;
    let state = app.state::<AppState>();
    let store = CookieStore::new(state.paths.cookies_dir());
    let mut out = Vec::new();
    for host in store.list_hosts().map_err(err_string)? {
        let count = store.load_host(&host).map_err(err_string)?.len();
        out.push(serde_json::json!({ "host": host, "count": count }));
    }
    Ok(out)
}

/// 保存 cookie 的核心逻辑（无窗口守卫，供 Rust 内部直接调用）。
///
/// `handle_login_done` 从登录窗上下文调用此函数——登录窗的 webview label
/// 不是 `main`，不能走命令层的 `ensure_main_window` 守卫；因此该能力不暴露
/// 为 IPC 命令，避免登录窗内的第三方页面覆写任意 host 的 cookie。
pub(crate) fn save_cookies_inner(
    app: &AppHandle,
    host: &str,
    cookies: Vec<ytdlp_core::cookies::CookieEntry>,
) -> CmdResult<()> {
    let state = app.state::<AppState>();
    let normalized = normalize_cookie_host(host);
    let store = CookieStore::new(state.paths.cookies_dir());
    store.save_host(&normalized, cookies).map_err(err_string)?;
    let _ = app.emit("cookies:changed", normalized);
    Ok(())
}

/// 规范化 cookie 存储 host：YouTube 相关域名统一存 www.youtube.com.txt
fn normalize_cookie_host(host: &str) -> String {
    match host.to_lowercase().as_str() {
        "youtube.com" | "youtu.be" | "m.youtube.com" | "www.youtube.com" => {
            "www.youtube.com".into()
        }
        "bilibili.com" | "www.bilibili.com" | "m.bilibili.com" => "www.bilibili.com".into(),
        "x.com" | "www.x.com" | "twitter.com" | "www.twitter.com" => "www.x.com".into(),
        "douyin.com" | "www.douyin.com" | "m.douyin.com" => "www.douyin.com".into(),
        h => h.to_string(),
    }
}

#[tauri::command(async)]
pub fn delete_cookie(app: AppHandle, win: tauri::WebviewWindow, host: String) -> CmdResult<()> {
    ensure_main_window(&win)?;
    let state = app.state::<AppState>();
    // 与 save_cookies_inner 同口径：库文件存的是规范化后的 host（www.youtube.com.txt），
    // 传站点级域名（youtube.com）来删也必须命中 —— 否则 delete_host 见文件不存在
    // 直接返回 Ok，用户以为删掉了、实际还在，下次下载仍带着旧 Cookie。
    let normalized = normalize_cookie_host(&host);
    let store = CookieStore::new(state.paths.cookies_dir());
    store.delete_host(&normalized).map_err(err_string)?;
    let _ = app.emit("cookies:changed", normalized);
    Ok(())
}
