//! 内置登录 WebView2 登录窗（DL-05）：
//! - 登录窗顶部中心固定"登录完成"按钮 + 提示条
//! - SPA 站点（YouTube 等）注入脚本 setInterval 每 800ms 自检重建（#ytdlp-login-bar / #ytdlp-login-hint）
//! - 点击"登录完成"→ 跳转本地 done URL（携带 document.cookie）→ Rust 抓取保存
//! - Windows 上优先经 WebView2 CookieManager（COM）抓取含 HttpOnly 的 Cookie；失败回退 URL 携带的 cookie
//! - 保存后关闭登录窗并自动重解析 NeedLogin 条目
//! - 两种关闭方式：顶部提示条内"关闭"、Esc 键（都跳本地 close URL，由 Rust 关窗；
//!   窗口系统标题栏的 × 也可用。曾有的右上角悬浮 × 已删——提示条里已有"关闭"，重复）
//!
//! **两条硬约束（违反就复现"新窗口空白 + 整机卡死、关都关不掉"）**：
//! 1. 创建窗口不能发生在主线程/同步命令里。Tauri 官方文档（`WebviewWindowBuilder::new`）：
//!    *On Windows, this function deadlocks when used in a synchronous command and event handlers…
//!    You should use `async` commands and separate threads when creating windows.*
//!    所以 `open_login_site` / `relogin_item` 必须带 `#[tauri::command(async)]`。
//!    一旦死锁，新窗口只创建了 HWND、从没绘制过（= 空白），WM_CLOSE 也无人处理（= 点 × 没用）。
//! 2. 关闭/保存的嗅探必须走 `on_navigation`（导航**开始**即触发，返回 false 取消导航），
//!    不能用 `on_page_load` —— 后者要等页面加载成功，而 `http://127.0.0.1/...` 上没有任何
//!    服务在监听，加载必然失败，于是"关闭"按钮在空白页面前完全失效。
//!
//! 另外 `on_page_load` 与 `on_navigation` 回调都在主线程执行，任何"就地等主线程"的调用
//! 都会自锁（`with_webview` + 泵消息尤其危险），所以这里的保存流程一律丢到子线程。

use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::commands::save_cookies;

/// 登录窗使用的 UA：桌面 Chrome。
///
/// WebView2 的默认 UA 带 `Edg/` 等标识，部分站点会据此返回空白页或进入重定向
/// 循环；换成标准桌面 Chrome UA 以提高兼容性。
///
/// 维护提示：站点风控会按 UA 的版本号判定"浏览器是否过旧"，这里的 Chrome 版本
/// 需要隔一段时间跟着上调（改的是这个常量，不是别处）。
pub const LOGIN_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// 开窗进行中标志：开窗已被移到 async 命令的线程上，这里再挡一道重复点击
/// （同 label 建两次会报错，用户看不出为什么）。
static LOGIN_OPENING: AtomicBool = AtomicBool::new(false);

/// 已知支持内置登录的站点 → 登录 URL。
pub fn login_url_for_host(host: &str) -> Option<String> {
    let h = host.to_lowercase();
    if h.contains("youtube") || h.contains("youtu.be") {
        Some("https://www.youtube.com/".into())
    } else if h.contains("bilibili") {
        Some("https://www.bilibili.com/".into())
    } else if h.contains("x.com") || h.contains("twitter") {
        Some("https://x.com/".into())
    } else if h.contains("douyin") {
        Some("https://www.douyin.com/".into())
    } else {
        None
    }
}

/// 打开登录窗。
///
/// **调用方必须保证不在主线程**（用 `#[tauri::command(async)]` 的 async 命令，
/// 或自己 `std::thread::spawn`），否则 Windows 上会死锁 —— 见文件头注释。
pub fn open_login(app: &AppHandle, host: &str, url: &str) -> Result<(), String> {
    // 已存在则聚焦
    if let Some(win) = app.get_webview_window("ytdlp-login") {
        let _ = win.set_focus();
        return Ok(());
    }
    if LOGIN_OPENING.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let result = build_login_window(app, host, url);
    LOGIN_OPENING.store(false, Ordering::SeqCst);
    result
}

fn build_login_window(app: &AppHandle, host: &str, url: &str) -> Result<(), String> {
    let script = login_inject_script();
    let app_for_nav = app.clone();
    let host_for_nav = host.to_string();
    WebviewWindowBuilder::new(
        app,
        "ytdlp-login",
        WebviewUrl::External(url.parse().map_err(|e| format!("无效登录 URL：{}", e))?),
    )
    .title(format!("登录 - 影栈（{}）", host))
    .inner_size(1000.0, 760.0)
    .min_inner_size(720.0, 560.0)
    .center()
    .user_agent(LOGIN_USER_AGENT)
    .initialization_script(&script)
    // close / done 两个"魔法 URL"的嗅探走导航开始事件：即使 127.0.0.1 上没人监听
    // （导航必然失败）也能触发，返回 false 顺手取消这次无意义的导航。
    //
    // 判定必须用**结构化字段全等**（scheme+host+port+path），不能用
    // `url.contains("ytdlp-login-done")`：任何被登录窗加载的页面（广告 iframe、
    // 被劫持的跳转）只要导航到含该子串的地址，就能带着自己构造的 `cookies=…`
    // 走进"保存 Cookie"分支，把攻击者给的凭据写进该站点的 cookie 库。
    .on_navigation(move |url| {
        match classify_magic_url(url) {
            Some(MagicNav::Close) => {
                let app = app_for_nav.clone();
                std::thread::spawn(move || close_login_window(&app));
                false
            }
            Some(MagicNav::Done) => {
                let app = app_for_nav.clone();
                let host = host_for_nav.clone();
                let done = url.to_string();
                // 回调在主线程：保存流程（with_webview + 泵消息、写 Cookie、关窗）一律下放子线程
                std::thread::spawn(move || {
                    if let Some(win) = app.get_webview_window("ytdlp-login") {
                        handle_login_done(&win, &host, &done);
                    }
                });
                false
            }
            None => true,
        }
    })
    .build()
    .map_err(|e| format!("打开登录窗口失败：{}", e))?;
    Ok(())
}

fn close_login_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("ytdlp-login") {
        let _ = win.close();
    }
}

/// 注入脚本中两个"魔法 URL"的测试基准值（关窗 / 登录完成）。
///
/// 注入脚本是含大量花括号的 `r#"…"#` 原始字符串（不宜用 `format!` 插值），URL 在
/// 脚本里以字面量书写；这两个常量仅在 `#[cfg(test)]` 下编译，供测试断言注入脚本确实
/// 包含正确 URL，防止脚本字面量与判定逻辑两处不同步。
#[cfg(test)]
const CLOSE_URL: &str = "http://127.0.0.1/ytdlp-login-close";
#[cfg(test)]
const DONE_URL: &str = "http://127.0.0.1/ytdlp-login-done";

/// 魔法 URL 的判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MagicNav {
    Close,
    Done,
}

/// 严格识别魔法 URL：必须是 `http://127.0.0.1[:80]` 上的固定路径，查询串不参与
/// 判定（`cookies=…` 由保存流程自己解析）。返回 None 表示这是普通导航，放行。
fn classify_magic_url(url: &url::Url) -> Option<MagicNav> {
    if url.scheme() != "http" || url.host_str() != Some("127.0.0.1") || url.port().is_some() {
        return None;
    }
    match url.path() {
        "/ytdlp-login-close" => Some(MagicNav::Close),
        "/ytdlp-login-done" => Some(MagicNav::Done),
        _ => None,
    }
}

fn handle_login_done(win: &tauri::WebviewWindow, host: &str, _url: &str) {
    let app = win.app_handle().clone();
    // 仅保留 COM 抓取通路：**不接受 URL 携带的 `cookies=` 载荷**。
    // 该回退分支无法区分"是我注入的完成按钮点的"还是"页面任何脚本发起的
    // 顶层跳转"（`window.top.location='http://127.0.0.1/ytdlp-login-done?cookies=…'`
    // 被风控的广告脚本就够）——攻击者可直接写入任意 cookie 并关窗、伪造
    // "已保存"提示。COM CookieManager 抓取的是当前页面真实会话（含 HttpOnly），
    // 且 on_navigation 已取消到 127.0.0.1 的导航、webview 仍停留在原站点，
    // 因此只从 COM 取、失败就明确报错，绝不写页面提供的载荷。
    #[cfg(windows)]
    {
        let (tx, rx) = std::sync::mpsc::channel::<Option<Vec<ytdlp_core::cookies::CookieEntry>>>();
        let host_owned = host.to_string();
        let _ = win.with_webview(move |webview| {
            let _ = tx.send(crate::login_win::fetch_cookies_com(&webview, &host_owned));
        });
        // 兜底超时：CookieManager 回调不返回时不要永久挂起
        let got = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .ok()
            .flatten();
        if let Some(cookies) = got {
            if cookies.is_empty() {
                // 站点确实没给会话 cookie（可能未登录成功）——不要静默当"完成"
                let _ = app.emit(
                    "login:failed",
                    serde_json::json!({ "host": host, "reason": "未捕获到登录 cookie，请确认已在页面内完成登录" }),
                );
                return;
            }
            if let Err(e) = save_cookies(app.clone(), host.to_string(), cookies) {
                let _ = app.emit(
                    "login:failed",
                    serde_json::json!({ "host": host, "reason": format!("保存 cookie 失败：{e}") }),
                );
                return;
            }
            finish_login(&app, host);
        } else {
            let _ = app.emit(
                "login:failed",
                serde_json::json!({ "host": host, "reason": "读取浏览器 Cookie 失败，请重试登录" }),
            );
        }
    }
    // 非 Windows 平台没有 COM CookieManager，也没有安全通路，明确报错而非写假载荷
    #[cfg(not(windows))]
    {
        let _ = app.emit(
            "login:failed",
            serde_json::json!({ "host": host, "reason": "当前平台不支持自动读取登录 Cookie" }),
        );
    }
}
fn finish_login(app: &AppHandle, host: &str) {
    // 关闭登录窗
    if let Some(win) = app.get_webview_window("ytdlp-login") {
        let _ = win.close();
    }
    // 通知前端（触发 NeedLogin 条目自动重解析）
    let _ = app.emit("login:done", serde_json::json!({ "host": host }));
}

/// 注入脚本：顶部中心"登录完成"按钮 + 提示条 + SPA 800ms 保活重建（DL-05）。
///
/// 要点：
/// - 关闭手段两种（提示条内"关闭"、Esc），页面异常时也能退出；
/// - `ensure` 每 800ms 重建被站点框架清掉的 UI，自身元素还在时立即返回，不会重复插入；
/// - `failHint` 的判据是"body 里除本脚本插入的元素外没有其它元素"。
///   早期实现写的是 `body.childElementCount === 0`，而脚本自己就会往 body 插
///   元素，该条件恒为假 —— 提示条从未生效过。
fn login_inject_script() -> String {
    r#"
(function () {
  'use strict';
  var CLOSE_URL = 'http://127.0.0.1/ytdlp-login-close';
  function closeWin() { window.location.href = CLOSE_URL; }
  function isOwn(el) { return (el.id || '').indexOf('ytdlp-') === 0; }
  function ensure() {
    if (document.getElementById('ytdlp-login-bar')) { return; }
    var root = document.body || document.documentElement;
    if (!root) { return; }
    var bar = document.createElement('div');
    bar.id = 'ytdlp-login-bar';
    bar.setAttribute('style',
      'position:fixed;top:0;left:50%;transform:translateX(-50%);z-index:2147483647;' +
      'display:flex;flex-direction:column;align-items:center;gap:6px;' +
      'padding:6px 16px 8px;background:rgba(15,118,110,0.96);border-radius:0 0 10px 10px;' +
      'box-shadow:0 2px 10px rgba(0,0,0,0.35);font-family:system-ui,sans-serif;');
    var btn = document.createElement('button');
    btn.id = 'ytdlp-login-btn';
    btn.textContent = '登录完成';
    btn.setAttribute('style',
      'border:none;cursor:pointer;font-size:14px;font-weight:600;color:#ffffff;' +
      'background:#0f766e;padding:8px 22px;border-radius:6px;box-shadow:0 0 0 1px rgba(255,255,255,0.5);');
    btn.onclick = function () {
      var cookies = encodeURIComponent(document.cookie);
      var host = encodeURIComponent(location.hostname);
      window.location.href = 'http://127.0.0.1/ytdlp-login-done?host=' + host + '&cookies=' + cookies;
    };
    var hint = document.createElement('div');
    hint.id = 'ytdlp-login-hint';
    hint.textContent = '登录完成后点"登录完成"保存 Cookie；Esc 可关闭本窗';
    hint.setAttribute('style', 'font-size:12px;color:rgba(255,255,255,0.9);');
    // 提示条内的关闭按钮：页面异常/空白时也能退出
    var barClose = document.createElement('button');
    barClose.id = 'ytdlp-login-bar-close';
    barClose.textContent = '关闭';
    barClose.setAttribute('style',
      'border:none;cursor:pointer;font-size:12px;color:#fff;' +
      'background:rgba(255,255,255,0.20);padding:4px 14px;border-radius:5px;');
    barClose.onclick = closeWin;
    bar.appendChild(btn);
    bar.appendChild(hint);
    bar.appendChild(barClose);
    // 挂载到页面：缺失此句时整个顶部条（含"登录完成"）都不会显示
    root.appendChild(bar);
  }
  ensure();
  setInterval(ensure, 800);
  // Esc 关闭（每个文档只注册一次；新文档会重新执行本脚本）
  document.addEventListener('keydown', function (e) {
    if (e.key === 'Escape' || e.keyCode === 27) { closeWin(); }
  }, true);
  function failHint() {
    if (document.getElementById('ytdlp-fail-hint')) { return; }
    if (document.readyState !== 'complete' || !document.body) { return; }
    var kids = document.body.children;
    var foreign = 0;
    for (var i = 0; i < kids.length; i++) {
      if (!isOwn(kids[i])) { foreign++; }
    }
    if (foreign > 0) { return; }
    var el = document.createElement('div');
    el.id = 'ytdlp-fail-hint';
    el.textContent = '页面似乎没有加载出来：登录窗跟随系统代理，请确认代理软件已开启"系统代理"后重试；也可按 Esc 或点上方提示条内的"关闭"退出本窗。';
    el.setAttribute('style',
      'position:fixed;left:0;right:0;bottom:0;z-index:2147483646;' +
      'background:#7f1d1d;color:#fff;font-family:system-ui,sans-serif;font-size:13px;' +
      'padding:10px 16px;text-align:center;');
    (document.body || document.documentElement).appendChild(el);
  }
  setInterval(failHint, 800);
})();
"#
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_url_mapping() {
        assert!(login_url_for_host("www.youtube.com")
            .unwrap()
            .contains("youtube"));
        assert!(login_url_for_host("www.bilibili.com")
            .unwrap()
            .contains("bilibili"));
        assert!(login_url_for_host("x.com").unwrap().contains("x.com"));
        assert!(login_url_for_host("www.douyin.com")
            .unwrap()
            .contains("douyin"));
        assert!(login_url_for_host("example.com").is_none());
    }

    #[test]
    fn inject_script_has_800ms_keepalive() {
        let s = login_inject_script();
        assert!(s.contains("setInterval(ensure, 800)"));
        assert!(s.contains("ytdlp-login-bar"));
        assert!(s.contains("ytdlp-login-hint"));
        assert!(s.contains("登录完成"));
        // 关闭手段两种：条内"关闭"、Esc（"ytdlp-login-close" 只剩 close URL 本身）
        assert!(s.contains("ytdlp-login-close"));
        assert!(s.contains("ytdlp-login-bar-close"));
        assert!(s.contains("Escape"));
        // 右上角悬浮 × 已删：提示条里已有"关闭"，不再注入第二个关闭按钮
        assert!(!s.contains("closeBtn"));
        // 顶部条必须真正挂载到页面（删 × 时误删挂载语句的历史回归）
        assert!(s.contains("root.appendChild(bar)"));
        // 空白检测判据不能再依赖 body.childElementCount ——
        // 脚本自己会往 body 插元素，该条件恒为假（旧实现的 bug）
        assert!(!s.contains("childElementCount === 0"));
        assert!(s.contains("isOwn"));
    }

    #[test]
    fn login_user_agent_is_plain_desktop_chrome() {
        // 覆盖 WebView2 默认 UA：不应带 Edg/ 或 WebView2 标识
        assert!(LOGIN_USER_AGENT.contains("Windows NT"));
        assert!(LOGIN_USER_AGENT.contains("Chrome/"));
        assert!(!LOGIN_USER_AGENT.contains("Edg/"));
        assert!(!LOGIN_USER_AGENT.contains("WebView2"));
    }

    #[test]
    fn injected_script_uses_the_magic_urls() {
        // 注入脚本里的 URL 必须与 classify_magic_url 认得的一模一样
        let s = login_inject_script();
        assert!(s.contains(CLOSE_URL), "脚本缺少关闭 URL");
        assert!(s.contains(DONE_URL), "脚本缺少登录完成 URL");
    }

    #[test]
    fn magic_url_matching_is_strict() {
        for raw in [
            "http://127.0.0.1/ytdlp-login-close",
            "http://127.0.0.1/ytdlp-login-done?host=x&cookies=y",
        ] {
            let u = url::Url::parse(raw).unwrap();
            assert!(classify_magic_url(&u).is_some(), "{raw} 应被识别");
        }
        // 关键回归：任意页面只要"碰巧"带了魔法词，不得触发关窗/保存 Cookie
        for raw in [
            "https://evil.example/ytdlp-login-done?cookies=SID%3Dx",
            "http://evil.example/ytdlp-login-close",
            "http://127.0.0.1:8080/ytdlp-login-done",
            "http://127.0.0.1/other/ytdlp-login-done",
            "http://127.0.0.1.evil.example/ytdlp-login-done",
        ] {
            let u = url::Url::parse(raw).unwrap();
            assert!(classify_magic_url(&u).is_none(), "{raw} 不应被识别");
        }
    }
}
