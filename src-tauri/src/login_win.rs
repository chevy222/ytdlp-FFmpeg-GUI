//! Windows 专用：经 WebView2 CookieManager（COM）抓取含 HttpOnly 的 Cookie（DL-05）。
//! 仅 Windows target 编译；任何失败返回 None，调用方回退到注入 URL 携带的 document.cookie。

#![cfg(windows)]

use std::sync::mpsc;

use tauri::webview::PlatformWebview;
use webview2_com::GetCookiesCompletedHandler;
use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2Cookie, ICoreWebView2CookieList, ICoreWebView2_2,
    COREWEBVIEW2_COOKIE_SAME_SITE_KIND,
};
use windows::core::{Interface, PCWSTR, PWSTR};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
};

use ytdlp_core::cookies::CookieEntry;

/// 等 CookieManager 回调的上限。
/// 必须小于调用方（`handle_login_done`）的 10s 兜底，否则主线程先卡住、调用方的超时形同虚设。
const COOKIE_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

/// 带截止时间的消息泵等待，替代 `webview2_com::wait_with_pump`。
///
/// WebView2 的异步回调要靠 STA 消息泵投递，所以不能裸阻塞 `recv()`；但
/// `wait_with_pump` **没有超时**——回调不来（webview 正在销毁、COM 出问题）就永远泵下去，
/// 而这段代码跑在**主线程**（`with_webview` 的回调里），一旦卡住就是"窗口空白 + 点 × 没反应"
/// 的整机冻结。这里自己泵并设上限，超时返回 None，调用方回退到 URL 携带的 cookie。
fn wait_with_pump_timeout<T>(rx: &mpsc::Receiver<T>, timeout: std::time::Duration) -> Option<T> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(v) = rx.try_recv() {
            return Some(v);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        unsafe {
            let mut msg = MSG::default();
            if PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            } else {
                // 没有消息时空转会烧 CPU，睡 2ms 再探
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
    }
}

/// 同步抓取某 host 域下全部 Cookie（含 HttpOnly）。
///
/// 走 WebView2 CookieManager.GetCookies（COM，非 JS document.cookie，因此含 HttpOnly）。
/// GetCookies 异步完成，回调在 STA 消息泵执行；这里用 `wait_with_pump_timeout` 泵消息等待，
/// 不能裸阻塞 `recv()`（否则回调永远不执行，死锁），也不能无限泵（见该函数注释）。
pub fn fetch_cookies_com(webview: &PlatformWebview, host: &str) -> Option<Vec<CookieEntry>> {
    let controller = webview.controller();
    let core = unsafe { controller.CoreWebView2().ok()? };
    // CookieManager 在 ICoreWebView2_2 上（基接口无此方法）
    let core2 = core.cast::<ICoreWebView2_2>().ok()?;
    let manager = unsafe { core2.CookieManager().ok()? };

    let uri = format!("https://{}/", host);
    let uri_wide: Vec<u16> = uri.encode_utf16().chain(std::iter::once(0)).collect();

    let (tx, rx) = mpsc::channel::<Option<Vec<CookieEntry>>>();
    let completed: webview2_com::CompletedClosure<
        windows::core::HRESULT,
        Option<ICoreWebView2CookieList>,
    > = Box::new(
        move |err: windows::core::Result<()>, cookies: Option<ICoreWebView2CookieList>| {
            let result = (|| -> windows::core::Result<Option<Vec<CookieEntry>>> {
                err?;
                let list = match cookies {
                    Some(l) => l,
                    None => return Ok(None),
                };
                let mut count = 0u32;
                unsafe { list.Count(&mut count) }?;
                let mut out = Vec::with_capacity(count as usize);
                for i in 0..count {
                    let cookie = unsafe { list.GetValueAtIndex(i) }?;
                    out.push(cookie_to_entry(&cookie));
                }
                Ok(Some(out))
            })();
            let _ = tx.send(result.ok().flatten());
            Ok(())
        },
    );

    let handler = GetCookiesCompletedHandler::create(completed);
    unsafe {
        manager
            .GetCookies(PCWSTR(uri_wide.as_ptr()), &handler)
            .ok()?;
    }
    wait_with_pump_timeout(&rx, COOKIE_WAIT_TIMEOUT).flatten()
}

/// 读取 Cookie 的宽字符串属性（Name/Value/Domain/Path）。
unsafe fn get_wide(c: &ICoreWebView2Cookie, idx: u8) -> String {
    let mut p = PWSTR::null();
    let ok = match idx {
        0 => c.Name(&mut p),
        1 => c.Value(&mut p),
        2 => c.Domain(&mut p),
        _ => c.Path(&mut p),
    };
    if ok.is_ok() && !p.is_null() {
        let s = p.to_string().unwrap_or_default();
        // COM 返回的宽字符串由 CoTaskMemAlloc 分配，调用方必须用 CoTaskMemFree 释放
        // （PWSTR 没有 Drop）。否则每次抓取每个 cookie 的 Name/Value/Domain/Path
        // 都泄漏一块原生内存，长时间反复登录会持续累积。
        unsafe {
            windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const core::ffi::c_void));
        }
        s
    } else {
        String::new()
    }
}

fn bool_from(b: windows::core::BOOL) -> bool {
    b.0 != 0
}

/// WebView2 Cookie → 内部 CookieEntry。
fn cookie_to_entry(c: &ICoreWebView2Cookie) -> CookieEntry {
    let mut is_http = windows::core::BOOL(0);
    let mut is_secure = windows::core::BOOL(0);
    let mut same_site = COREWEBVIEW2_COOKIE_SAME_SITE_KIND::default();
    let mut expires = 0.0f64;
    unsafe {
        let _ = c.IsHttpOnly(&mut is_http);
        let _ = c.IsSecure(&mut is_secure);
        let _ = c.SameSite(&mut same_site);
        let _ = c.Expires(&mut expires);
    }
    CookieEntry {
        name: unsafe { get_wide(c, 0) },
        value: unsafe { get_wide(c, 1) },
        domain: unsafe { get_wide(c, 2) },
        path: unsafe { get_wide(c, 3) },
        expires: if expires > 0.0 { Some(expires) } else { None },
        http_only: bool_from(is_http),
        secure: bool_from(is_secure),
        same_site: match same_site.0 {
            1 => "lax".into(),
            2 => "strict".into(),
            _ => "none".into(),
        },
    }
}
