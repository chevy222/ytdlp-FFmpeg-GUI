//! 工具链托管下载/更新与依赖自检（设置-依赖页）：下载计划、进度上报、版本/指纹比对。

use super::{ensure_main_window, CmdResult};
use crate::state::AppState;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};
use ytdlp_core::exec::{ToolResolver, ToolSource};
use ytdlp_core::log;
use ytdlp_core::tool_download::{installed_matches, InstalledIndex, ToolDownloader, ToolKind};
use ytdlp_core::transcode;

/// 硬件编码器探测（TC-16）：QSV/NVENC/AMF 可用性，供设置页标注。
/// 探测要起 ffmpeg 子进程（秒级），用 `spawn_blocking` 避免占住 async worker（A6）。
#[tauri::command]
pub async fn probe_hw_encoders(app: AppHandle) -> CmdResult<serde_json::Value> {
    let state = app.state::<AppState>();
    let resolver = state.resolver();
    tauri::async_runtime::spawn_blocking(move || {
        transcode::detect_hw_encoders(&resolver)
            .map_err(|e| format!("探测编码器失败：{}", e))
            .map(|hw| serde_json::json!({ "qsv": hw.qsv, "nvenc": hw.nvenc, "amf": hw.amf }))
    })
    .await
    .map_err(|e| format!("探测编码器任务异常：{e}"))?
}

/// 依赖页「下载 / 更新」结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolInstallResult {
    /// 本次是否真的下载并安装了文件（false = 已有托管副本 / 已是最新）
    pub updated: bool,
    /// 该工具当前生效的路径（「下载」成功后前端写回设置输入框）
    pub path: String,
    /// 安装后（或当前）的版本号；工具不报版本时为 None
    pub version: Option<String>,
    /// 给用户看的一句话结论
    pub message: String,
}

/// 「下载 / 更新」的执行计划。
enum InstallPlan {
    /// 不需要下载，直接把结论返回前端
    Done(ToolInstallResult),
    /// 需要下载并安装到该路径
    Download(PathBuf),
}

/// 依赖页"链接"弹窗展示的地址（觉得下载慢时用户可手动下载）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolUrlInfo {
    /// dependencies.* 配置键（与前端 data-tool 同名）
    pub tool: String,
    /// 显示名（yt-dlp / ffmpeg / …）
    pub name: String,
    /// 程序「下载/更新」实际使用的包地址
    pub download_url: String,
    /// 构建/发布页（人工查版本、手动下载的入口）
    pub check_url: String,
}

/// 四个工具的下载/版本检查地址（纯常量，无网络请求）。
#[tauri::command]
pub fn tool_urls() -> CmdResult<Vec<ToolUrlInfo>> {
    let keys = ["yt_dlp_path", "ffmpeg_path", "ffprobe_path", "deno_path"];
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        let kind =
            ToolKind::from_config_key(key).ok_or_else(|| format!("内部错误：未知工具键 {key}"))?;
        out.push(ToolUrlInfo {
            tool: key.to_string(),
            name: kind.tool().name().to_string(),
            download_url: kind.url().to_string(),
            check_url: kind.check_page_url().to_string(),
        });
    }
    Ok(out)
}

/// 依赖页「下载 / 更新」（进度经 `tool:progress` 上报，下载中前端按钮变"取消"）。
///
/// - `update=false`（下载）：固定装到 `<exe 同级>\tools\`；已有托管副本就直接返回
///   （不重复下载）。成功后前端把路径写回设置，之后优先用这份。
/// - `update=true`（更新）：只更新**当前生效的那一份**——设置里填的路径或 `tools\`
///   托管副本；当前用的是系统 PATH 里的（不归本程序管）则提示用户自己更新。
///   更新前先判断有没有新版本：yt-dlp / deno 比 release tag，ffmpeg / ffprobe 比
///   gyan.dev 的 release-version 与本地版本；版本取不到再回退指纹比对。已是最新就不下载。
#[tauri::command]
pub async fn download_tool(
    app: AppHandle,
    state: State<'_, AppState>,
    win: tauri::WebviewWindow,
    tool: String,
    update: bool,
) -> CmdResult<ToolInstallResult> {
    ensure_main_window(&win)?;
    let kind = ToolKind::from_config_key(&tool).ok_or_else(|| format!("未知工具键：{tool}"))?;
    // 注册取消标志（前端点"取消"置 true）。同一工具不允许并发下载：
    // 用 try_register_cancel **原子**"判定并占用"——已存在即返回 None，
    // 避免 is_some()+register 之间两次并发调用都拿到同一 flag 各下载一份。
    let cancel_key = format!("tool-dl-{tool}");
    let flag = state
        .try_register_cancel(&cancel_key)
        .ok_or_else(|| "该工具正在下载中".to_string())?;
    let tools_dir = state.paths.tools_dir();
    // 工具维护流量（github.com / gyan.dev）走应用代理：这两个域通常不在用户的
    // "下载站点分流"白名单里，但被墙时依赖下载/更新必须仍可用。
    // 代理地址留空则回退默认代理（见 NetworkConfig::effective_proxy）。
    let proxy = state.config.lock().network.effective_proxy();
    let dl = ToolDownloader::new(tools_dir.clone(), state.paths.temp_dir().join("tool_dl"))
        .with_proxy(Some(proxy));
    let ctx = ToolInstallCtx {
        app: &app,
        state: state.inner(),
        dl: &dl,
        tools_dir: tools_dir.as_path(),
        key: tool.as_str(),
    };

    let outcome = run_tool_install(&ctx, kind, update, &flag).await;
    // 无论成功/失败/取消（含"未找到""已是最新"这类早退）都清理取消标志，
    // 否则注册表条目泄漏，之后每次点按钮都报"该工具正在下载中"
    state.cancels.lock().remove(&cancel_key);
    outcome
}

/// 「下载 / 更新」共用的调用上下文（免得把同一批参数串成一长排）。
struct ToolInstallCtx<'a> {
    app: &'a AppHandle,
    state: &'a AppState,
    dl: &'a ToolDownloader,
    tools_dir: &'a Path,
    /// 配置键名（`yt_dlp_path` 等）：进度事件与安装指纹都用它作标识
    key: &'a str,
}

async fn run_tool_install(
    ctx: &ToolInstallCtx<'_>,
    kind: ToolKind,
    update: bool,
    cancel: &Arc<AtomicBool>,
) -> CmdResult<ToolInstallResult> {
    let app = ctx.app.clone();
    let dl = ctx.dl.clone().with_cancel(Some(cancel.clone()));
    let resolver = ctx.state.resolver();
    let tools_dir = ctx.tools_dir.to_path_buf();
    let key = ctx.key.to_string();
    // 「下载 / 更新」全流程都在阻塞线程里：查最新版本、查远端指纹是 curl 网络往返
    // （秒级），下载与解压是分钟级 IO。放在 async 执行器上会占住 worker
    // （执行器线程数 ≈ CPU 核数），把其它异步命令一起拖慢。
    tauri::async_runtime::spawn_blocking(move || {
        install_blocking(&app, &dl, &resolver, kind, update, &tools_dir, &key)
    })
    .await
    .map_err(|e| format!("下载任务异常：{e}"))?
}

/// 「下载 / 更新」的阻塞执行体（在阻塞线程里调用）。
fn install_blocking(
    app: &AppHandle,
    dl: &ToolDownloader,
    resolver: &ToolResolver,
    kind: ToolKind,
    update: bool,
    tools_dir: &Path,
    key: &str,
) -> CmdResult<ToolInstallResult> {
    let dest = match plan_install(dl, resolver, kind, update, tools_dir, key)? {
        InstallPlan::Done(done) => return Ok(done),
        InstallPlan::Download(dest) => dest,
    };
    let tool_key = key.to_string();
    let mut prog = |phase: String, pct: f32| {
        let _ = app.emit(
            "tool:progress",
            serde_json::json!({ "tool": tool_key, "phase": phase, "percent": pct }),
        );
    };
    let done = dl.download(kind, &dest, &mut prog)?;
    let version = ytdlp_core::exec::tool_version_at(kind.tool(), &done.path);

    // 记下这次安装的远端产物指纹：下次「更新」靠它判断有没有新版本。
    // load + record + save 走原子方法：两个工具并发"下载/更新"时，
    // 后写入者不会用陈旧索引把先写入者的指纹抹掉。
    if let Some(sha) = &done.remote_sha256 {
        if let Err(e) = InstalledIndex::record_install(tools_dir, key, &done.path, sha) {
            log::warn(format!("安装指纹写入失败（只影响下次「更新」的判断）：{e}"));
        }
    }

    let name = kind.tool().name();
    let vs = version
        .as_deref()
        .map(|v| format!("（{v}）"))
        .unwrap_or_default();
    let verb = if update {
        "已更新到"
    } else {
        "已安装到"
    };
    Ok(ToolInstallResult {
        updated: true,
        path: done.path.to_string_lossy().into_owned(),
        version,
        message: format!("{name}{vs}{verb} {}", done.path.display()),
    })
}

/// 下载 / 更新的前置判断（联网的只有"查版本号"或"查产物指纹"一步）。
fn plan_install(
    dl: &ToolDownloader,
    resolver: &ToolResolver,
    kind: ToolKind,
    update: bool,
    tools_dir: &Path,
    key: &str,
) -> CmdResult<InstallPlan> {
    let name = kind.tool().name();
    if !update {
        // 下载：固定落 tools\，已有托管副本就不重复下
        let dest = dl.target_path(kind);
        if dl.is_installed(kind) {
            let version = ytdlp_core::exec::tool_version_at(kind.tool(), &dest);
            let vs = version
                .as_deref()
                .map(|v| format!("（{v}）"))
                .unwrap_or_default();
            return Ok(InstallPlan::Done(ToolInstallResult {
                updated: false,
                path: dest.to_string_lossy().into_owned(),
                version,
                message: format!("{name}{vs}已在 tools\\ 中；如需检查新版本请点\"更新\""),
            }));
        }
        return Ok(InstallPlan::Download(dest));
    }

    // 更新：只动"当前生效的那一份"
    let (target, source) = resolver
        .resolve_with_source(kind.tool())
        .map_err(|e| format!("{e}。请先点\"下载\"装一份托管副本，或在设置里填写路径"))?;
    if source == ToolSource::Path {
        return Err(format!(
            "{name} 当前用的是系统 PATH 里的 {}：请在系统里手动更新，或点\"下载\"在本程序 tools\\ 目录装一份托管副本（之后就能在这里更新）",
            target.display()
        ));
    }

    let local = ytdlp_core::exec::tool_version_at(kind.tool(), &target);
    // 有版本号的（yt-dlp / deno）比版本号；没有版本号的（ffmpeg / ffprobe 滚动构建）比产物指纹
    let unchanged = match (&local, dl.latest_version(kind)) {
        (Some(l), Some(remote)) => ytdlp_core::exec::versions_equal(l, &remote),
        _ => {
            let remote = dl.remote_sha(kind);
            let index = InstalledIndex::load(tools_dir);
            installed_matches(index.get(key), &target, remote.as_deref())
        }
    };
    if unchanged {
        let vs = local
            .as_deref()
            .map(|v| format!("（{v}）"))
            .unwrap_or_default();
        return Ok(InstallPlan::Done(ToolInstallResult {
            updated: false,
            path: target.to_string_lossy().into_owned(),
            version: local,
            message: format!("{name}{vs}已是最新，无需更新"),
        }));
    }
    Ok(InstallPlan::Download(target))
}

/// 取消进行中的工具下载（前端点"取消"按钮）。
#[tauri::command]
pub fn cancel_tool_download(state: State<'_, AppState>, tool: String) -> CmdResult<()> {
    let key = format!("tool-dl-{tool}");
    if let Some(flag) = state.cancel_flag(&key) {
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(())
}

/// 依赖自检项（前端显示）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolStatus {
    pub tool: String,
    pub path: Option<String>,
    pub version: Option<String>,
    pub ok: bool,
}

#[tauri::command]
pub async fn probe_dependencies(app: AppHandle) -> CmdResult<Vec<ToolStatus>> {
    let state = app.state::<AppState>();
    let resolver = state.resolver();
    // 每个工具要起子进程取版本（yt-dlp --version / ffmpeg -version …，各自带
    // 截止时间），放 blocking 线程，避免占住 async worker（A6）。
    tauri::async_runtime::spawn_blocking(move || {
        // 四个工具并行探测：串行时病态环境下（杀软扫描/网络盘）依赖页最坏要
        // 等 4×截止时间；并行后最坏约等于单次的截止时间
        let handles: Vec<_> = [
            ytdlp_core::exec::Tool::YtDlp,
            ytdlp_core::exec::Tool::Ffmpeg,
            ytdlp_core::exec::Tool::Ffprobe,
            ytdlp_core::exec::Tool::Deno,
        ]
        .into_iter()
        .map(|tool| {
            let resolver = resolver.clone();
            std::thread::spawn(move || {
                let resolved = resolver.resolve(tool);
                let (path, version, ok) = match &resolved {
                    Ok(p) => {
                        let v = ytdlp_core::exec::tool_version(&resolver, tool);
                        (Some(p.to_string_lossy().into_owned()), v, true)
                    }
                    Err(_) => (None, None, false),
                };
                ToolStatus {
                    tool: tool.name().to_string(),
                    path,
                    version,
                    ok,
                }
            })
        })
        .collect();
        // 按原工具顺序汇总；线程 panic（理论上不可达）时给该工位一个占位
        Ok(handles
            .into_iter()
            .map(|h| {
                h.join().unwrap_or_else(|_| ToolStatus {
                    tool: "探测线程异常".into(),
                    path: None,
                    version: None,
                    ok: false,
                })
            })
            .collect())
    })
    .await
    .map_err(|e| format!("依赖自检任务异常：{e}"))?
}
