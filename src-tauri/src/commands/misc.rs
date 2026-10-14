//! 杂项命令：队列读数、打开目录、清理临时文件、检查更新、打开外部链接。

use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager, State};
use ytdlp_core::log;
use super::{CmdResult, ensure_main_window, err_string};
use crate::state::AppState;

/// 队列读数（UI 展示"运行中 x/y、排队 n"）。
///
/// 条目的状态不区分"运行中"与"排队中"（都是 Downloading/Transcoding/Merging），
/// 因此这个数字只能由队列本身给出，否则界面会出现"运行中 5/3"这种自相矛盾的读数。
#[tauri::command(async)]
pub fn queue_status(state: State<'_, AppState>) -> CmdResult<serde_json::Value> {
    let q = state.queue.lock();
    Ok(serde_json::json!({
        "running": q.running_count(),
        "waiting": q.waiting_count(),
        "concurrency": q.concurrency(),
    }))
}

#[tauri::command(async)]
pub fn open_item_dir(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    let hist = state.history.lock();
    let item = hist.get(&id).ok_or("条目不存在")?;
    let target = item
        .file
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| item.path.clone().map(PathBuf::from))
        .ok_or("该条目没有本地文件")?;
    if target.is_dir() {
        return open_in_explorer(&target, None);
    }
    // `unwrap_or_else(|| target.clone())`：target 后面还要作为 /select 的目标用，
    // 不能在这里被 move 走
    let dir = target
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| target.clone());
    // 目标是文件时让资源管理器直接选中它：对含空格/逗号的路径，
    // 「只开目录」会让用户自己再找一遍，而 /select 能精确定位
    open_in_explorer(&dir, Some(&target))
}

#[cfg(windows)]
fn open_in_explorer(dir: &Path, select: Option<&Path>) -> CmdResult<()> {
    let mut cmd = std::process::Command::new("explorer");
    match select {
        // `/select,<path>` 是 explorer 的保留参数：路径必须紧跟逗号且作为**同一个**
        // 参数传入（Rust 不经过 shell，这里正好可控），带引号反而会被当成字面量
        Some(file) if file.is_file() => {
            cmd.arg(format!("/select,{}", file.display()));
        }
        _ => {
            cmd.arg(dir);
        }
    }
    cmd.spawn().map_err(err_string)?;
    Ok(())
}

#[cfg(not(windows))]
fn open_in_explorer(_dir: &Path, _select: Option<&Path>) -> CmdResult<()> {
    Err("仅 Windows 支持打开目录".into())
}

/// 清理 temp/ 下的残留（任务私有目录、合并中间目录、工具下载半成品）。
///
/// 跳过运行中任务的私有目录与在跑的工具下载；只清历史残留，避免把进行中的任务搞坏。
/// 豁免清单必须覆盖 temp 下**所有**非任务 id 命名的活跃产物：
/// - `merge_<uuid>/`：进行中的合并（AppState.merge_tmp 登记制，精确到目录名；
///   归一化中间产物随 R4 也落在该目录内，temp/ 根部的 norm_* 一律视为残留可清）
/// - `tool_dl/`：进行中的工具下载
/// - `ytdlp-*.txt`、`thumb-*/`：进行中的下载产物定位文件与缩略图 yt-dlp 兜底目录
///
/// 返回实际清理掉的条目数。启动时（此时豁免表为空）也调它清上次崩溃留下的垃圾。
pub fn clear_temp_inner(state: &AppState) -> std::io::Result<usize> {
    let dir = state.paths.temp_dir();
    if !dir.is_dir() {
        return Ok(0);
    }
    let active: Vec<String> = state.cancels.lock().keys().cloned().collect();
    let busy = !active.is_empty();
    let tool_dl_busy = active.iter().any(|k| k.starts_with("tool-dl-"));
    let merge_tmp: Vec<String> = state.merge_tmp.lock().iter().cloned().collect();
    let mut removed = 0usize;
    for e in std::fs::read_dir(&dir)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        let path = e.path();
        let skip = active.contains(&name)
            || (tool_dl_busy && name == "tool_dl")
            // 合并私有目录按登记精确豁免（run_merge_task 登记 / finish 前移除）
            || merge_tmp.contains(&name)
            // thumb-*/ytdlp-* 是单次调用内创建、返回前删除的瞬态产物，只有崩溃
            // 会留下：有任务在跑时保守跳过（体积小，下次启动必清）
            || (busy && (name.starts_with("thumb-") || name.starts_with("ytdlp-")));
        if skip {
            continue;
        }
        // 目录与普通文件都要清：旧实现一律用 remove_dir_all，对散落的
        // 临时文件必然失败且被 `let _` 吞掉 —— "清理临时文件"其实一直清不掉它们
        let result = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match result {
            Ok(()) => removed += 1,
            Err(err) => log::warn(format!("清理临时文件失败 {}：{err}", path.display())),
        }
    }
    Ok(removed)
}

#[tauri::command]
pub async fn clear_temp(app: AppHandle, win: tauri::WebviewWindow) -> CmdResult<()> {
    ensure_main_window(&win)?;
    let _state = app.state::<AppState>();
    // 大临时目录 remove_dir_all 是秒级阻塞，放 blocking 线程（A6）
    tauri::async_runtime::spawn_blocking(move || {
        let st = app.state::<AppState>();
        clear_temp_inner(&st).map_err(err_string)
    })
    .await
    .map_err(|e| format!("清理临时文件任务异常：{e}"))??;
    Ok(())
}

/// 检查更新结果：有新版时返回，无新版返回 None。
#[derive(serde::Serialize)]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: String,
    pub url: String,
}

/// 查询 GitHub 最新 release：返回 (tag, html_url)。
///
/// 用系统 curl 发请求（与 tool_download 一致，避免 TLS 交叉编译问题）；
/// 调用方负责隐藏控制台窗口（hide_console）与放到阻塞线程（A6）。
/// `proxy`：应用代理（GitHub 常被墙，与工具链下载同口径）。
fn fetch_latest_release(proxy: Option<&str>) -> CmdResult<(String, String)> {
    let api_url = "https://api.github.com/repos/chevy222/ytdlp-FFmpeg-GUI/releases/latest";
    let mut cmd = std::process::Command::new("curl");
    cmd.args([
        "-sS",
        "-L",
        "--max-time",
        "10",
        "-H",
        "Accept: application/vnd.github+json",
        "-H",
        "User-Agent: ytdlp-FFmpeg-GUI",
    ]);
    if let Some(p) = proxy.filter(|p| !p.trim().is_empty()) {
        cmd.args(["--proxy", p]);
    }
    cmd.arg(api_url);
    ytdlp_core::exec::hide_console(&mut cmd);
    let output = cmd
        .output()
        .map_err(|e| format!("无法调用 curl：{e}"))?;
    if !output.status.success() {
        return Err(format!(
            "GitHub API 请求失败（{}）：{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let body = String::from_utf8_lossy(&output.stdout);
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("解析 GitHub API 响应失败：{e}"))?;
    let tag = v["tag_name"]
        .as_str()
        .unwrap_or("")
        .trim_start_matches('v')
        .to_string();
    if tag.is_empty() {
        return Err("GitHub API 响应中无 tag_name（可能尚未发布过 release）".into());
    }
    let release_url = v["html_url"]
        .as_str()
        .unwrap_or("https://github.com/chevy222/ytdlp-FFmpeg-GUI/releases")
        .to_string();
    Ok((tag, release_url))
}

/// 检查更新：请求 GitHub API 获取最新 release tag，与当前版本比较。
///
/// - 配置 `general.check_update` 为 false 时直接返回 None；
/// - 有新版时返回 Some(UpdateInfo)，无新版返回 None；
/// - 失败时返回错误，前端静默忽略（检查更新是可选增强，不应阻塞启动）。
#[tauri::command]
pub async fn check_update(app: AppHandle) -> CmdResult<Option<UpdateInfo>> {
    let (enabled, proxy) = {
        let state = app.state::<AppState>();
        let c = state.config.lock();
        (c.general.check_update, c.network.effective_proxy())
    };
    if !enabled {
        return Ok(None);
    }
    let current = env!("CARGO_PKG_VERSION").to_string();
    // curl 最长 10s：必须离开 async worker（A6），并带上应用代理
    let (tag, release_url) =
        tauri::async_runtime::spawn_blocking(move || fetch_latest_release(Some(proxy.as_str())))
            .await
            .map_err(|e| format!("检查更新任务异常：{e}"))??;
    if version_greater(&tag, &current) {
        Ok(Some(UpdateInfo {
            current_version: current,
            latest_version: tag,
            url: release_url,
        }))
    } else {
        Ok(None)
    }
}

/// 设置页用：总是返回当前版本与最新 release 版本（不受 check_update 配置控制）。
///
/// 前端在"启动时检查更新"旁边展示"当前 vX.Y.Z / 最新 vA.B.C"，让用户在
/// 关闭自动检查时也能手动看到版本状态。查询失败时 latest_version 为空串。
#[tauri::command]
pub async fn get_version_info(app: AppHandle) -> CmdResult<UpdateInfo> {
    let proxy = {
        let state = app.state::<AppState>();
        let c = state.config.lock();
        c.network.effective_proxy()
    };
    let current = env!("CARGO_PKG_VERSION").to_string();
    // curl 最长 10s：离开 async worker（A6），并带应用代理
    let fetched = tauri::async_runtime::spawn_blocking(move || {
        fetch_latest_release(Some(proxy.as_str()))
    })
    .await
    .unwrap_or_else(|_| Err("查询任务异常".into()));
    match fetched {
        Ok((tag, url)) => Ok(UpdateInfo {
            current_version: current,
            latest_version: tag,
            url,
        }),
        Err(_) => Ok(UpdateInfo {
            current_version: current,
            latest_version: String::new(),
            url: "https://github.com/chevy222/ytdlp-FFmpeg-GUI/releases".into(),
        }),
    }
}

/// 在系统默认浏览器中打开外部 URL。
///
/// Tauri webview 内的 `<a target="_blank">` 不会自动打开系统浏览器（默认被
/// 拦截或在新 webview 打开），设置页的"最新版本"链接需要走这个命令。
/// 仅允许 https/http scheme，防止 `file://` / `javascript:` 等注入。
#[tauri::command(async)]
pub fn open_url(win: tauri::WebviewWindow, url: String) -> CmdResult<()> {
    ensure_main_window(&win)?;
    let parsed = url::Url::parse(&url).map_err(|_| "URL 格式不正确".to_string())?;
    if !matches!(parsed.scheme(), "https" | "http") {
        return Err("仅支持 http/https 链接".into());
    }
    open::that(&url).map_err(|e| format!("打开浏览器失败：{e}"))
}

/// 简单语义化版本比较：`a > b` 返回 true。缺失段按 0 处理。
fn version_greater(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.split('.')
            .filter_map(|x| x.parse::<u64>().ok())
            .collect()
    };
    let va = parse(a);
    let vb = parse(b);
    for i in 0..va.len().max(vb.len()) {
        let na = va.get(i).copied().unwrap_or(0);
        let nb = vb.get(i).copied().unwrap_or(0);
        if na != nb {
            return na > nb;
        }
    }
    false
}
