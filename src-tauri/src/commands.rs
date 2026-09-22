//! Tauri 命令层：统一列表 CRUD + 解析/下载/取消/删除 + 配置 + Cookie + 依赖自检。
//!
//! 后台任务用 std::thread + 事件 `item:update`（payload = MediaItem）回推前端；
//! 关键状态变更才持久化 history.json（进度高频更新只 emit 不落盘）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tauri::{AppHandle, Emitter, Manager, State};
use ytdlp_core::config::AppConfig;
use ytdlp_core::config::NetworkConfig;
use ytdlp_core::cookies::CookieStore;
use ytdlp_core::download::{self, post_process, probe_output, run_download, DownloadParams};
use ytdlp_core::exec::{ToolResolver, ToolSource};
use ytdlp_core::merge::{self, MergeParams};
use ytdlp_core::model::{ItemKind, MediaItem, Status};
use ytdlp_core::probe::{self, ProbeErrorKind};
use ytdlp_core::tool_download::{installed_matches, InstalledIndex, ToolDownloader, ToolKind};
use ytdlp_core::transcode::{self, TranscodeParams};
use ytdlp_core::worker::SubmitOutcome;
use ytdlp_core::{transition, CoreError};

use crate::login;
use crate::state::AppState;

/// 硬件编码器探测（TC-16）：QSV/NVENC/AMF 可用性，供设置页标注。
#[tauri::command]
pub fn probe_hw_encoders(app: AppHandle) -> CmdResult<serde_json::Value> {
    let state = app.state::<AppState>();
    let resolver = state.resolver();
    let hw =
        transcode::detect_hw_encoders(&resolver).map_err(|e| format!("探测编码器失败：{}", e))?;
    Ok(serde_json::json!({ "qsv": hw.qsv, "nvenc": hw.nvenc, "amf": hw.amf }))
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
    tool: String,
    update: bool,
) -> CmdResult<ToolInstallResult> {
    let kind = ToolKind::from_config_key(&tool).ok_or_else(|| format!("未知工具键：{tool}"))?;
    // 注册取消标志（前端点"取消"置 true）。同一工具不允许并发下载：
    // 重复注册会覆盖旧标志，导致第一次下载的"取消"指向失效
    let cancel_key = format!("tool-dl-{tool}");
    if state.cancel_flag(&cancel_key).is_some() {
        return Err("该工具正在下载中".into());
    }
    let flag = state.register_cancel(&cancel_key);
    let tools_dir = state.paths.tools_dir();
    let dl = ToolDownloader::new(tools_dir.clone(), state.paths.temp_dir().join("tool_dl"));
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
    state.cancels.lock().unwrap().remove(&cancel_key);
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
    let dest = match plan_tool_install(ctx, kind, update)? {
        InstallPlan::Done(done) => return Ok(done),
        InstallPlan::Download(dest) => dest,
    };
    let app2 = ctx.app.clone();
    let dl2 = ctx.dl.clone().with_cancel(Some(cancel.clone()));
    let key2 = ctx.key.to_string();
    let dest2 = dest.clone();
    let joined = tauri::async_runtime::spawn_blocking(move || {
        let mut prog = |phase: String, pct: f32| {
            let _ = app2.emit(
                "tool:progress",
                serde_json::json!({ "tool": key2, "phase": phase, "percent": pct }),
            );
        };
        let done = dl2.download(kind, &dest2, &mut prog)?;
        let version = ytdlp_core::exec::tool_version_at(kind.tool(), &done.path);
        Ok::<_, String>((done, version))
    })
    .await
    .map_err(|e| format!("下载任务异常：{e}"))?;
    let (done, version) = joined?;

    // 记下这次安装的远端产物指纹：下次「更新」靠它判断有没有新版本
    if let Some(sha) = &done.remote_sha256 {
        let mut index = InstalledIndex::load(ctx.tools_dir);
        index.record(ctx.key, &done.path, sha);
        if let Err(e) = index.save(ctx.tools_dir) {
            eprintln!("{e}");
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
fn plan_tool_install(
    ctx: &ToolInstallCtx<'_>,
    kind: ToolKind,
    update: bool,
) -> CmdResult<InstallPlan> {
    let name = kind.tool().name();
    if !update {
        // 下载：固定落 tools\，已有托管副本就不重复下
        let dest = ctx.dl.target_path(kind);
        if ctx.dl.is_installed(kind) {
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
    let resolver = ctx.state.resolver();
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
    let unchanged = match (&local, ctx.dl.latest_version(kind)) {
        (Some(l), Some(remote)) => ytdlp_core::exec::versions_equal(l, &remote),
        _ => {
            let remote = ctx.dl.remote_sha(kind);
            let index = InstalledIndex::load(ctx.tools_dir);
            installed_matches(index.get(ctx.key), &target, remote.as_deref())
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

type CmdResult<T> = Result<T, String>;

fn err_string(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// 更新条目并返回克隆（变更即原子写仅对状态迁移生效由调用方决定）。
fn update_item(app: &AppHandle, id: &str, f: impl FnOnce(&mut MediaItem)) -> Option<MediaItem> {
    let state = app.state::<AppState>();
    let mut hist = state.history.lock().unwrap();
    let item = hist.get(id)?.clone();
    let mut item = item;
    f(&mut item);
    hist.upsert(item.clone());
    drop(hist);
    let _ = app.emit("item:update", &item);
    Some(item)
}

/// 状态迁移收口（P1-1）：update_item 闭包内一律走 transition_in——
/// 白名单之外的迁移被拒绝并保留原状态，保证需求 §9 的状态机白名单真正有约束力。
fn transition_in(it: &mut MediaItem, to: Status) {
    if transition(it.status, to).is_ok() {
        it.status = to;
    }
}

/// 记录失败日志：多行错误信息拆成逐条（单条塞多行在 UI 上易被截断观感），
/// 首行带前缀，其余行原样追加。
fn log_error_lines(app: &AppHandle, id: &str, prefix: &str, e: &CoreError) {
    let error_text = e.to_string();
    let mut lines = error_text.lines();
    if let Some(first) = lines.next() {
        log_item(app, id, format!("{prefix}{first}"));
    }
    for l in lines {
        if !l.trim().is_empty() {
            log_item(app, id, l.to_string());
        }
    }
}

/// 记录日志行并 emit。
fn log_item(app: &AppHandle, id: &str, line: impl Into<String>) {
    update_item(app, id, |it| {
        it.push_log(line);
    });
}

/// 持久化（状态迁移后调用）。
fn persist(app: &AppHandle) {
    app.state::<AppState>().persist();
}

// ---------- 添加与解析 ----------

#[tauri::command]
pub fn add_url(app: AppHandle, urls: Vec<String>) -> CmdResult<()> {
    let state = app.state::<AppState>();
    let mut hist = state.history.lock().unwrap();
    for raw in urls {
        let url = clean_url(&raw);
        if url.is_empty() {
            continue;
        }
        // DL-01：只接受 http(s):// —— UI 与 CLI 两条入口在此共用同一校验，
        // 否则 CLI 裸参数会把本地文件名当 URL 入列
        if !url.starts_with("http://") && !url.starts_with("https://") {
            continue;
        }
        let item = MediaItem::from_url(url);
        let id = item.id.clone();
        hist.upsert(item);
        // 解析线程不占并发 slot
        let app2 = app.clone();
        std::thread::spawn(move || {
            run_probe(app2, id);
        });
    }
    drop(hist);
    persist(&app);
    let _ = app.emit("list:changed", ());
    Ok(())
}

#[tauri::command]
pub fn add_local(app: AppHandle, paths: Vec<String>, recursive: bool) -> CmdResult<()> {
    let mut files: Vec<PathBuf> = Vec::new();
    for p in paths {
        let pb = PathBuf::from(&p);
        if pb.is_dir() {
            scan_dir(&pb, recursive, &mut files);
        } else if pb.is_file() {
            files.push(pb);
        }
    }
    if files.is_empty() {
        return Err("没有找到可添加的文件".into());
    }
    let state = app.state::<AppState>();
    let mut hist = state.history.lock().unwrap();
    for f in files {
        let item = MediaItem::from_path(f.to_string_lossy().into_owned());
        let id = item.id.clone();
        hist.upsert(item);
        let app2 = app.clone();
        std::thread::spawn(move || {
            run_probe(app2, id);
        });
    }
    drop(hist);
    persist(&app);
    let _ = app.emit("list:changed", ());
    Ok(())
}

fn scan_dir(dir: &Path, recursive: bool, out: &mut Vec<PathBuf>) {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if recursive {
                scan_dir(&p, true, out);
            }
        } else if download::is_media_file(&p) {
            out.push(p);
        }
    }
}

/// URL 清洗（DL-01）：去引号/空白，抖音 modal_id 归一化由 yt-dlp 处理。
fn clean_url(raw: &str) -> String {
    raw.trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim()
        .to_string()
}

/// 解析任务（URL 或本地；阻塞运行在线程中）。
fn run_probe(app: AppHandle, id: String) {
    let state = app.state::<AppState>();
    let item = {
        let hist = state.history.lock().unwrap();
        hist.get(&id).cloned()
    };
    let Some(item) = item else {
        return;
    };
    log_item(&app, &id, "开始解析元数据…");

    let resolver = state.resolver();
    let network = state.config.lock().unwrap().network.clone();
    let netscape = resolve_cookies(&state, &item);

    let playlist_on = state.config.lock().unwrap().download.playlist;
    let result = if item.url.is_some() {
        probe::probe_url(
            &resolver,
            item.url.as_deref().unwrap_or_default(),
            netscape.as_deref(),
            &network,
            playlist_on,
            &state.paths.temp_dir(),
            |l| log_item(&app, &id, l),
        )
    } else {
        let path = item.path.clone().unwrap_or_default();
        probe::probe_local(&resolver, Path::new(&path), |l| log_item(&app, &id, l)).map(|p| {
            let mut mp = p;
            let title = item.title.clone();
            mp.meta.title = Some(title);
            ytdlp_core::probe::UrlProbe {
                meta: mp.meta,
                site: Some("本地文件".into()),
                host: None,
                is_playlist: false,
                thumbnail_url: None,
            }
        })
    };

    match result {
        Ok(p) => {
            let url_src = item.url.is_some();
            let cover_idx = p.meta.cover_stream_index;
            update_item(&app, &id, |it| {
                it.meta = p.meta;
                it.site = p.site.clone();
                if p.host.is_some() {
                    it.host = p.host.clone();
                }
                transition_in(it, Status::Ready);
                it.percent = 0.0;
                it.error = None;
                it.push_log("解析完成，已就绪".to_string());
                if url_src {
                    let heights: Vec<String> = it.meta.download_formats.iter()
                        .map(|f| format!("{}", f.height.unwrap_or(0)))
                        .collect();
                    it.push_log(format!("可用格式：{} 项（高度：{}）", it.meta.download_formats.len(), heights.join(", ")));
                }
            });
            state.persist();
            // 封面缩略图（异步生成，不阻塞就绪）
            {
                let app2 = app.clone();
                let id2 = id.clone();
                let cache_dir = state.paths.cache_dir();
                let thumb_url = p.thumbnail_url.clone();
                let local_path = item.path.clone();
                // 本地文件优先取内嵌封面（元数据），与桌面缩略图同源
                let resolver2 = resolver.clone();
                let proxy2 = network.proxy_url.clone();
                tauri::async_runtime::spawn(async move {
                    let dest = ytdlp_core::thumbs::thumb_path(&cache_dir, &id2);
                    let r = if let Some(u) = thumb_url {
                        if u.is_empty() {
                            Ok(())
                        } else {
                            // 直连失败自动带设置里的代理重试一次（YouTube 等站点
                            // 缩略图服务器直连拉不动，主下载却走代理）
                            ytdlp_core::thumbs::save_remote_thumb(&u, &dest, Some(proxy2.as_str()))
                        }
                    } else if let Some(p) = local_path {
                        ytdlp_core::thumbs::ensure_thumb(
                            &resolver2,
                            std::path::Path::new(&p),
                            &dest,
                            cover_idx.map(|i| i as usize),
                            &mut |l| log_item(&app2, &id2, l),
                        )
                    } else {
                        Ok(())
                    };
                    if let Ok(()) = r {
                        update_item(&app2, &id2, |it| {
                            it.thumb = Some(dest.to_string_lossy().into_owned());
                        });
                        app2.state::<AppState>().persist();
                    }
                });
            }
            // URL 已就绪：触发 5 秒倒计时自动下载（前端计时，后端只发可下载信号）
            if url_src {
                let _ = app.emit("item:ready", serde_json::json!({ "id": id }));
            }
            // 播放列表（DL-09）：开启时把合集展开为逐集条目平铺进列表
            if url_src && playlist_on && p.is_playlist {
                expand_playlist(&app, &id, &item, &resolver, netscape.as_deref(), &network);
            }
        }
        Err(f) => {
            let status = if f.kind == ProbeErrorKind::NeedLogin {
                Status::NeedLogin
            } else {
                Status::Failed
            };
            update_item(&app, &id, |it| {
                transition_in(it, status);
                it.error = Some(f.to_string());
                // 多行错误逐行落日志：单条塞多行在日志弹窗里会被错误截断
                let s = f.to_string();
                let mut lines = s.lines();
                if let Some(first) = lines.next() {
                    it.push_log(format!("解析失败：{first}"));
                }
                for l in lines {
                    if !l.trim().is_empty() {
                        it.push_log(l.to_string());
                    }
                }
            });
            state.persist();
        }
    }
    // 解析阶段临时目录清理（cookie 文件现在持久存在 config/cookies/ 下，不删）
    let _ = std::fs::remove_dir_all(state.paths.task_temp_dir(&id));
    persist(&app);
}

// ---------- 列表 ----------

#[tauri::command]
pub fn list_items(state: State<'_, AppState>) -> CmdResult<Vec<MediaItem>> {
    let hist = state.history.lock().unwrap();
    Ok(hist.items.clone())
}

// ---------- 动作 ----------

#[tauri::command]
pub fn start_download(
    app: AppHandle,
    id: String,
    format_id: Option<String>,
    audio_only: bool,
) -> CmdResult<()> {
    let state = app.state::<AppState>();
    {
        let mut hist = state.history.lock().unwrap();
        let mut item = hist.get(&id).cloned().ok_or("条目不存在")?;
        if item.status != Status::Ready {
            return Err(format!("当前状态不可下载：{}", item.status.label()));
        }
        if item.url.as_deref().map(str::is_empty).unwrap_or(true) {
            return Err("该条目没有可下载的 URL（本地文件请使用转码）".into());
        }
        let new_status = transition(item.status, Status::Downloading).map_err(err_string)?;
        item.status = new_status;
        // 排队场景下 launch_next 从条目读回这两个参数：必须在提交前落进条目，
        // 否则排队任务会丢失用户选择的格式/仅音频选项
        item.format_id = format_id.clone();
        item.audio_only = audio_only;
        hist.upsert(item);
    }
    // 提交并发队列
    let outcome = {
        let mut q = state.queue.lock().unwrap();
        q.submit(&id)
    };
    if outcome == SubmitOutcome::Queued {
        log_item(&app, &id, "已排队，等待并发 slot…");
        persist(&app);
        return Ok(());
    }
    log_item(&app, &id, "开始下载…");
    persist(&app);
    let app2 = app.clone();
    std::thread::spawn(move || {
        run_download_task(app2, id, format_id, audio_only);
    });
    Ok(())
}

fn run_download_task(app: AppHandle, id: String, format_id: Option<String>, audio_only: bool) {
    let state = app.state::<AppState>();
    let cancel = state.register_cancel(&id);

    let (url, cfg, general, resolver, out_dir, template, proxy, netscape, sections, js_runtime) = {
        // 只在锁内 clone 条目：后续 Cookie 导出（export_netscape）是文件 IO，
        // 持 history 锁做会阻塞所有 update_item 调用（前端刷新卡顿）
        let item = {
            let hist = state.history.lock().unwrap();
            hist.get(&id).cloned()
        };
        let Some(item) = item else {
            // 条目在排队期间被删除：释放并发 slot 并清掉取消标志，
            // 否则这个额度会被永久占用，等待中的任务永远不启动
            state.cancels.lock().unwrap().remove(&id);
            release_slot(&app, &id);
            return;
        };
        let url = item.url.clone().unwrap_or_default();
        let cfg = state.config.lock().unwrap().download.clone();
        let general = state.config.lock().unwrap().general.clone();
        let resolver = state.resolver();
        let out_dir = default_output_dir(&state);
        let template = cfg.filename_template.clone();
        let proxy = state.config.lock().unwrap().network.resolve_proxy(&url);
        let netscape = resolve_cookies(&state, &item);
        let sections = item.sections.clone();
        // JS 运行时（§3.6 依赖）：托管/配置的 deno 必须显式传给 yt-dlp，
        // 否则 YouTube 组件会因"没有 JS 运行时"失败（依赖自检却是通过的）
        let js_runtime = resolver
            .resolve(ytdlp_core::exec::Tool::Deno)
            .ok()
            .map(|p| format!("deno:{}", p.to_string_lossy()));
        (
            url, cfg, general, resolver, out_dir, template, proxy, netscape, sections, js_runtime,
        )
    };

    let params = DownloadParams {
        format_id: format_id.clone(),
        audio_only,
        out_dir: out_dir.clone(),
        filename_template: template,
        embed_cover: cfg.embed_cover,
        proxy,
        cookies_file: netscape,
        sections,
        js_runtime,
        // ffmpeg 位置（§3.6 依赖）：合并容器、嵌入封面、时间范围裁剪
        // （--download-sections 帮助里写明 "Needs ffmpeg"）都要用 ffmpeg，
        // 而 yt-dlp 只在 PATH 与自身目录找 —— 托管模式（tools/）必须显式传入。
        ffmpeg_path: resolver
            .resolve(ytdlp_core::exec::Tool::Ffmpeg)
            .ok()
            .map(|p| p.to_string_lossy().into_owned()),
    };

    let app2 = app.clone();
    let id2 = id.clone();
    let app3 = app.clone();
    let id3 = id.clone();
    let app4 = app.clone();
    let id4 = id.clone();
    // 里程碑诊断：后端确实解析到中间进度的直接证据（进日志弹窗可查，
    // 用于区分"后端没解析到"与"前端没渲染"两类问题）
    let band = std::cell::Cell::new(0u8);
    let prog_result = run_download(
        &resolver,
        &url,
        &params,
        &cfg,
        &state.paths.temp_dir(),
        &cancel,
        move |p| {
            update_item(&app2, &id2, |it| {
                // Destination 行（切换到下一条流，percent 恒为 0）只更新目标文件名，
                // 不把进度打回 0：DASH 双流下载视频 100% → 音频 Destination 会把
                // percent 重置，快速下载看起来就像"一直 0%"
                if p.file.is_some() {
                    it.file = p.file.clone();
                }
                if p.file.is_none() || p.percent > 0.0 {
                    it.percent = p.percent;
                    if let Some(s) = p.speed {
                        it.speed = Some(s);
                    }
                    if let Some(e) = p.eta {
                        it.eta = Some(e);
                    }
                }
            });
            if p.file.is_none() && p.percent < 100.0 {
                let b = (p.percent / 25.0).floor() as u8;
                if b > band.get() && b >= 1 {
                    band.set(b);
                    log_item(
                        &app3,
                        &id3,
                        format!("下载进度 {:.0}%（后端已解析到中间进度）", p.percent),
                    );
                }
            }
        },
        move |l| log_item(&app4, &id4, l),
    );

    let mut outcome = match prog_result {
        Ok(o) => o,
        Err(e) => {
            finish_download(&app, &id, Err(e), &params);
            return;
        }
    };

    // 后处理（DL-04）
    let first = outcome.output_paths.first().cloned();
    if outcome.preexisting {
        // 重复下载同一 URL：yt-dlp 未覆盖既有文件（--no-overwrites），
        // 此时不做后处理，避免原地重编码覆盖用户既有文件
        log_item(&app, &id, "产物已存在（未覆盖），跳过后处理");
    } else if let Some(path) = &first {
        log_item(&app, &id, "下载完成，开始后处理…");
        update_item(&app, &id, |it| {
            transition_in(it, Status::PostProcessing);
        });
        let app2 = app.clone();
        let cfg2 = cfg.clone();
        let general2 = general.clone();
        let cancel2 = cancel.clone();
        let pp = post_process(&resolver, path, &cfg2, &general2, &cancel2, |line| {
            log_item(&app2, &id, line);
        });
        match pp {
            Ok((final_path, meta)) => {
                outcome.output_paths = vec![final_path];
                update_item(&app, &id, |it| {
                    it.meta = meta;
                });
                state.persist();
            }
            Err(e) => {
                finish_download(&app, &id, Err(e), &params);
                return;
            }
        }
    }
    finish_download(&app, &id, Ok(outcome), &params);
}

fn finish_download(
    app: &AppHandle,
    id: &str,
    result: Result<download::DownloadOutcome, CoreError>,
    _params: &DownloadParams,
) {
    let state = app.state::<AppState>();
    // 本次任务的最终产物（回填 path + 抽帧封面共用）
    let final_path = result
        .as_ref()
        .ok()
        .and_then(|o| o.output_paths.first().cloned());
    let final_path_str = final_path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    let final_status = match &result {
        Ok(o) => {
            log_item(
                app,
                id,
                format!("下载完成：{} 个文件", o.output_paths.len()),
            );
            Status::Done
        }
        Err(CoreError::Cancelled) => {
            log_item(app, id, "已取消，清理残留");
            Status::Canceled
        }
        Err(e) => {
            log_error_lines(app, id, "下载失败：", e);
            Status::Failed
        }
    };
    update_item(app, id, |it| {
        transition_in(it, final_status);
        it.percent = if final_status == Status::Done {
            100.0
        } else {
            it.percent
        };
        if let Err(e) = &result {
            it.error = Some(e.to_string());
        }
        if final_status == Status::Done {
            it.speed = None;
            it.eta = None;
            // 产物路径回填（MD-06 / TC-11）：下载产物必须能像本地文件一样
            // 直接进入转码/合并，否则列表里连"转码"按钮都不会出现。
            if let Some(p) = &final_path_str {
                it.path = Some(p.clone());
                it.file = Some(p.clone());
            }
        }
    });
    // 下载完成后：条目还没有封面时（解析期远程缩略图失败的兜底），
    // 用最终产物抽帧补一张；已有封面（远程图）则保留，不做无谓抽帧
    if final_status == Status::Done {
        if let Some(out_path) = final_path {
            let (need_thumb, cover_idx) = {
                let hist = state.history.lock().unwrap();
                match hist.get(id) {
                    // 产物已有缩略图则不动；否则优先内嵌封面（元数据）
                    Some(it) => (it.thumb.clone().is_none(), it.meta.cover_stream_index),
                    None => (false, None),
                }
            };
            if need_thumb {
                let app2 = app.clone();
                let id2 = id.to_string();
                let cache_dir = state.paths.cache_dir();
                let resolver2 = state.resolver();
                tauri::async_runtime::spawn(async move {
                    let dest = ytdlp_core::thumbs::thumb_path(&cache_dir, &id2);
                    if ytdlp_core::thumbs::ensure_thumb(
                        &resolver2,
                        std::path::Path::new(&out_path),
                        &dest,
                        cover_idx.map(|i| i as usize),
                        &mut |l| log_item(&app2, &id2, l),
                    )
                    .is_ok()
                    {
                        update_item(&app2, &id2, |it| {
                            it.thumb = Some(dest.to_string_lossy().into_owned());
                        });
                        app2.state::<AppState>().persist();
                    }
                });
            }
        }
    }
    // 任务结束：只清理本任务私有临时目录（cookie 文件持久存在 config/cookies/ 下，不删）
    let _ = std::fs::remove_dir_all(state.paths.task_temp_dir(id));
    // 清理取消标志注册表条目（否则随任务数无限累积；也让后续 cancel_item
    // 的 cancel_flag 查询能正确区分"运行中"与"已结束"）
    state.cancels.lock().unwrap().remove(id);
    // 释放并发 slot，启动下一个等待任务（下载/转码/合并共用）
    release_slot(app, id);
    persist(app);
}

fn default_output_dir(state: &AppState) -> PathBuf {
    if let Some(dir) = &state.cli.lock().unwrap().dir {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Some(dir) = &state.config.lock().unwrap().general.default_output_dir {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    // 默认桌面（§3.6 通用）
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .map(|p| p.join("Desktop"))
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|p| p.join("Desktop"))
        })
        .unwrap_or_else(|| state.paths.root().join("output"))
}

/// Cookie 文件解析：CLI `--cookies` 优先；否则从 Cookie 库按站点导出 Netscape 临时文件。
fn resolve_cookies(state: &AppState, item: &MediaItem) -> Option<PathBuf> {
    if let Some(p) = &state.cli.lock().unwrap().cookies {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    let host = item.host.clone().or_else(|| {
        item.url
            .as_deref()
            .and_then(ytdlp_core::cookies::host_from_url)
    })?;
    let store = CookieStore::new(state.paths.cookies_dir());
    store.cookies_file(&host).ok().flatten()
}

/// 释放并发 slot 并启动下一个等待任务（下载/转码/合并共用）。
fn release_slot(app: &AppHandle, id: &str) {
    let state = app.state::<AppState>();
    let next = {
        let mut q = state.queue.lock().unwrap();
        q.finish(id)
    };
    if let Some(next_id) = next {
        launch_next(app, next_id);
    }
}

/// 合并面板参数（MG-01/04：低频操作，仅面板内配置，不落 config.json）。
#[derive(Debug, Clone)]
pub struct MergeJob {
    pub ids: Vec<String>,
    pub filename: String,
    pub container: String,
    pub encoder_mode: String,
    pub normalize_audio: bool,
}

/// 批量合并（MG-01..04：多选按序拼接；参数在合并面板配置）。
#[tauri::command]
pub fn start_merge(
    app: AppHandle,
    ids: Vec<String>,
    filename: Option<String>,
    container: Option<String>,
    encoder: Option<String>,
    normalize: Option<bool>,
) -> CmdResult<()> {
    let state = app.state::<AppState>();
    if ids.len() < 2 {
        return Err("合并至少需要 2 个条目".into());
    }
    let mut jobs = Vec::new();
    // 跳过原因出锁后再写日志：log_item → update_item 会再次 lock history，
    // std::sync::Mutex 不可重入，锁内调用会当场死锁（UI 永久冻结）
    let mut skipped: Vec<(String, String)> = Vec::new();
    {
        let mut hist = state.history.lock().unwrap();
        for id in &ids {
            let Some(item) = hist.get(id).cloned() else {
                return Err(format!("条目不存在：{id}"));
            };
            let has_file = item
                .path
                .as_deref()
                .map(|p| std::path::Path::new(p).is_file())
                .unwrap_or(false);
            if !has_file {
                skipped.push((id.clone(), "合并被跳过：无本地输入文件".into()));
                continue;
            }
            match transition(item.status, Status::Merging) {
                Ok(to) => {
                    // 进入新任务前进度归零：否则上一轮下载/转码的 100% 会一直挂在进度列
                    hist.upsert(MediaItem {
                        status: to,
                        percent: 0.0,
                        ..item
                    });
                    jobs.push(id.clone());
                }
                Err(e) => skipped.push((id.clone(), format!("合并被跳过：{e}"))),
            }
        }
    }
    for (id, msg) in skipped {
        log_item(&app, &id, msg);
    }
    if jobs.len() < 2 {
        return Err("可合并条目不足 2 个".into());
    }
    let norm = normalize.unwrap_or_else(|| state.config.lock().unwrap().general.normalize_audio);
    let job = MergeJob {
        ids: jobs,
        filename: filename.unwrap_or_else(default_merge_name),
        container: container.unwrap_or_else(|| "mp4".into()),
        encoder_mode: encoder.unwrap_or_else(|| "auto".into()),
        normalize_audio: norm,
    };
    // 一次合并 = 一个作业：只把**锚点条目**提交给并发队列，其余参与条目仅标记状态。
    // 旧实现对每个 id 各提交一次，导致同一合并被并发执行 N 次（同路径互写、产物重复）。
    let anchor = job.ids[0].clone();
    for id in &job.ids {
        state
            .merge_jobs
            .lock()
            .unwrap()
            .insert(id.clone(), job.clone());
    }
    let outcome = {
        let mut q = state.queue.lock().unwrap();
        q.submit(&anchor)
    };
    for id in &job.ids {
        if id != &anchor {
            log_item(&app, id, "随本批合并作业一起执行…");
        }
    }
    if outcome == SubmitOutcome::Queued {
        log_item(&app, &anchor, "已排队，等待并发 slot…");
    } else {
        log_item(&app, &anchor, "开始合并…");
        let app2 = app.clone();
        let anchor2 = anchor.clone();
        std::thread::spawn(move || run_merge_task(app2, anchor2));
    }
    persist(&app);
    Ok(())
}

/// 默认合并输出名：合并_<时间戳>（MG-06；本地时区日期，见 core timefmt）。
fn default_merge_name() -> String {
    format!("合并_{}", today_stamp())
}

fn today_stamp() -> String {
    ytdlp_core::timefmt::date_stamp(
        ytdlp_core::timefmt::now_secs(),
        ytdlp_core::timefmt::local_offset_secs(),
    )
}

/// 合并任务线程（提交队列后执行；进度经 `item:update` 回推）。
fn run_merge_task(app: AppHandle, id: String) {
    let state = app.state::<AppState>();
    let cancel = state.register_cancel(&id);
    let job = state.merge_jobs.lock().unwrap().get(&id).cloned();
    let Some(job) = job else {
        release_slot(&app, &id);
        return;
    };
    // 取消标志共享给全部参与条目：从任意被勾选条目点"取消"都能取消这次合并
    {
        let mut cancels = state.cancels.lock().unwrap();
        for jid in &job.ids {
            cancels.insert(jid.clone(), cancel.clone());
        }
    }
    let (inputs, params) = {
        let mut inputs = Vec::new();
        let mut anchor = None;
        {
            let hist = state.history.lock().unwrap();
            for jid in &job.ids {
                if let Some(item) = hist.get(jid) {
                    if anchor.is_none() {
                        anchor = Some(item.clone());
                    }
                    if let Some(p) = &item.path {
                        inputs.push(std::path::PathBuf::from(p));
                    }
                }
            }
        }
        let cfg = state.config.lock().unwrap().clone();
        let p = MergeParams {
            inputs,
            out_dir: default_output_dir(&state),
            // 中间产物一律落在 <exe 同级>\temp\（§3.7），不写进用户输出目录
            temp_dir: state.paths.temp_dir(),
            filename: job.filename.clone(),
            container: job.container.clone(),
            encoder_mode: job.encoder_mode.clone(),
            collision_policy: cfg.general.collision_policy.clone(),
            normalize_audio: job.normalize_audio,
            max_gain_db: cfg.general.max_gain_db,
        };
        (p.inputs.clone(), p)
    };
    if inputs.len() < 2 {
        log_item(&app, &id, "合并输入不足，已取消");
        {
            let mut jobs = state.merge_jobs.lock().unwrap();
            for jid in &job.ids {
                jobs.remove(jid);
            }
        }
        for jid in &job.ids {
            update_item(&app, jid, |it| {
                transition_in(it, Status::Failed);
                it.error = Some("合并输入不足（需要至少 2 个本地文件）".into());
            });
        }
        // 必须释放 slot，否则并发额度会被这条作业永久占用
        release_slot(&app, &id);
        persist(&app);
        return;
    }
    let app2 = app.clone();
    let id2 = id.clone();
    let result = merge::run_merge(
        &state.resolver(),
        &params,
        &cancel,
        move |pct| {
            update_item(&app2, &id2, |it| {
                it.percent = pct;
            });
        },
        |line| log_item(&app, &id, line),
    );
    finish_merge(&app, &id, &job.ids, result);
}

/// 合并收尾：全部参与条目恢复状态、产物作为新条目回列表、释放队列 slot。
fn finish_merge(
    app: &AppHandle,
    id: &str,
    ids: &[String],
    result: Result<std::path::PathBuf, CoreError>,
) {
    let state = app.state::<AppState>();
    let final_status = match &result {
        Ok(out) => {
            log_item(app, id, format!("合并完成，产物回列表：{}", out.display()));
            Status::Done
        }
        Err(CoreError::Cancelled) => {
            log_item(app, id, "合并已取消，清理残留");
            Status::Canceled
        }
        Err(e) => {
            log_error_lines(app, id, "合并失败：", e);
            Status::Failed
        }
    };
    let err_text = result.as_ref().err().map(|e| e.to_string());
    // 全部参与条目一起恢复状态（锚点条目另带进度/错误）
    for jid in ids {
        let orig = restore_status(app, jid);
        let is_anchor = jid == id;
        let err = err_text.clone();
        let pct = final_status == Status::Done;
        update_item(app, jid, |it| {
            transition_in(it, orig);
            if is_anchor && pct {
                it.percent = 100.0;
            }
            if let Some(e) = &err {
                it.error = Some(e.clone());
            }
        });
    }
    if let Ok(out) = &result {
        let meta = download::probe_output(&state.resolver(), out, |_| {}).unwrap_or_default();
        let title = out
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "合并产物".into());
        let mut prod = MediaItem::new(ItemKind::MergeOut, title);
        prod.path = Some(out.to_string_lossy().into_owned());
        prod.status = Status::Done;
        prod.percent = 100.0;
        prod.meta = meta;
        prod.updated_at = now_str();
        state.history.lock().unwrap().upsert(prod.clone());
        // 产物缩略图：从最终产物抽帧（与下载完成兜底同链路）
        {
            let app2 = app.clone();
            let pid = prod.id.clone();
            let cache_dir = state.paths.cache_dir();
            let resolver2 = state.resolver();
            let out2 = out.clone();
            let cover_idx = prod.meta.cover_stream_index;
            tauri::async_runtime::spawn(async move {
                let dest = ytdlp_core::thumbs::thumb_path(&cache_dir, &pid);
                if ytdlp_core::thumbs::ensure_thumb(
                    &resolver2,
                    &out2,
                    &dest,
                    cover_idx.map(|i| i as usize),
                    &mut |_| {},
                )
                .is_ok()
                {
                    update_item(&app2, &pid, |it| {
                        it.thumb = Some(dest.to_string_lossy().into_owned());
                    });
                }
                app2.state::<AppState>().persist();
            });
        }
        let _ = app.emit("item:ready", serde_json::json!({ "id": prod.id }));
        let _ = app.emit("list:changed", ());
    }
    {
        let mut jobs = state.merge_jobs.lock().unwrap();
        for jid in ids {
            jobs.remove(jid);
        }
    }
    // 合并的取消标志注册在每个参与条目上（run_merge_task），一并清理
    {
        let mut cancels = state.cancels.lock().unwrap();
        for jid in ids {
            cancels.remove(jid);
        }
    }
    release_slot(app, id);
    persist(app);
}

/// 设置时间范围下载（DL-12）：起止 "HH:MM:SS"；空串清除。
#[tauri::command]
pub fn set_sections(app: AppHandle, id: String, start: String, end: String) -> CmdResult<()> {
    let valid = |t: &str| {
        if t.is_empty() {
            return true;
        }
        let parts: Vec<&str> = t.split(':').collect();
        if parts.len() != 3 {
            return false;
        }
        parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) && p.len() <= 2)
    };
    if !valid(&start) || !valid(&end) {
        return Err("时间格式应为 HH:MM:SS（如 00:01:00）".into());
    }
    let sections = if start.is_empty() && end.is_empty() {
        None
    } else {
        Some((start, end))
    };
    let state = app.state::<AppState>();
    update_item(&app, &id, |it| {
        it.sections = sections.clone();
        match &sections {
            Some((s, e)) => {
                it.push_log(format!("已设置时间范围下载：{} - {}", s, e));
            }
            None => {
                it.push_log("已清除时间范围".to_string());
            }
        }
    });
    state.persist();
    Ok(())
}

/// 批量转码（TC-05：按 设置-转码/通用 默认参数执行，不弹确认窗）。
#[tauri::command]
pub fn start_transcode(app: AppHandle, ids: Vec<String>) -> CmdResult<()> {
    let state = app.state::<AppState>();
    if ids.is_empty() {
        return Err("未选择条目".into());
    }
    let mut to_run = Vec::new();
    // 同 start_merge：跳过日志出锁后再写，避免 history 锁重入死锁
    let mut skipped: Vec<(String, String)> = Vec::new();
    {
        let mut hist = state.history.lock().unwrap();
        for id in &ids {
            let Some(item) = hist.get(id).cloned() else {
                return Err(format!("条目不存在：{id}"));
            };
            let has_file = item
                .path
                .as_deref()
                .map(|p| std::path::Path::new(p).is_file())
                .unwrap_or(false);
            if !has_file {
                skipped.push((
                    id.clone(),
                    "转码被跳过：无本地输入文件（先下载或添加本地文件）".into(),
                ));
                continue;
            }
            match transition(item.status, Status::Transcoding) {
                Ok(to) => {
                    // 进入新任务前进度归零：否则上一轮下载/转码的 100% 会一直挂在进度列
                    let updated = MediaItem {
                        status: to,
                        percent: 0.0,
                        ..item
                    };
                    hist.upsert(updated.clone());
                    drop(hist);
                    let _ = app.emit("item:update", &updated);
                    hist = state.history.lock().unwrap();
                    to_run.push(id.clone());
                }
                Err(e) => skipped.push((id.clone(), format!("转码被跳过：{e}"))),
            }
        }
    }
    for (id, msg) in skipped {
        log_item(&app, &id, msg);
    }
    if to_run.is_empty() {
        return Err("没有可转码的条目（需要已解析的本地文件）".into());
    }
    for id in to_run {
        let outcome = {
            let mut q = state.queue.lock().unwrap();
            q.submit(&id)
        };
        if outcome == SubmitOutcome::Queued {
            log_item(&app, &id, "已排队，等待并发 slot…");
        } else {
            log_item(&app, &id, "开始转码…");
            let app2 = app.clone();
            std::thread::spawn(move || run_transcode_task(app2, id));
        }
    }
    persist(&app);
    Ok(())
}

/// 播放列表展开（DL-09）：flat-playlist 拿每集 URL，逐条作为独立条目解析平铺。
///
/// 逐集在单个后台线程里**顺序**解析：大合集（数百集）若每集各开线程，
/// 会瞬间并发起等量 yt-dlp 子进程，打满系统资源并容易触发站点风控；
/// 顺序解析每集就绪即回显（item:update），体验可接受。
fn expand_playlist(
    app: &AppHandle,
    id: &str,
    item: &MediaItem,
    resolver: &ToolResolver,
    cookies: Option<&Path>,
    network: &NetworkConfig,
) {
    let url = item.url.clone().unwrap_or_default();
    if url.is_empty() {
        return;
    }
    match probe::list_playlist_entries(resolver, &url, cookies, network, |l| log_item(app, id, l)) {
        Ok(entries) => {
            let n = entries.len();
            log_item(app, id, format!("播放列表展开：{n} 集"));
            let mut ids = Vec::with_capacity(entries.len());
            {
                let st = app.state::<AppState>();
                let mut hist = st.history.lock().unwrap();
                for e in entries {
                    let entry = MediaItem::from_url(e.url);
                    ids.push(entry.id.clone());
                    hist.upsert(entry);
                }
            }
            // 新条目整批入列表：发一次 list:changed 让前端立即拿到完整清单
            //（后续每集解析进度经 item:update 逐条回推）
            let _ = app.emit("list:changed", ());
            let app2 = app.clone();
            std::thread::spawn(move || {
                for eid in ids {
                    run_probe(app2.clone(), eid);
                }
            });
        }
        Err(f) => {
            log_item(app, id, format!("播放列表展开失败：{}", f.message));
        }
    }
}

/// 转码任务线程（提交队列后执行；进度经 `item:update` 回推）。
fn run_transcode_task(app: AppHandle, id: String) {
    let state = app.state::<AppState>();
    let cancel = state.register_cancel(&id);
    // 锁纪律（§11.15）：history 锁内只 clone 条目，config/cli/default_output_dir
    // 一律出锁后再取（P1-3：持 history 锁期间嵌套取 config/cli 锁会让 update_item 卡顿）
    let (item, path) = {
        let hist = state.history.lock().unwrap();
        let item = match hist.get(&id) {
            Some(i) => i.clone(),
            None => {
                // 条目在排队期间被删除：释放 slot + 取消标志，避免额度被永久占用
                state.cancels.lock().unwrap().remove(&id);
                release_slot(&app, &id);
                return;
            }
        };
        let path = match &item.path {
            Some(p) => std::path::PathBuf::from(p),
            None => {
                // 无本地文件（start_transcode 已校验过，此处为兜底）：
                // 出锁后判失败并释放，不能把条目永远留在 Transcoding 状态
                update_item(&app, &id, |it| {
                    transition_in(it, Status::Failed);
                    it.error = Some("无本地输入文件".into());
                });
                state.cancels.lock().unwrap().remove(&id);
                release_slot(&app, &id);
                persist(&app);
                return;
            }
        };
        (item, path)
    };
    let cfg = state.config.lock().unwrap().clone();
    let out_dir = default_output_dir(&state);
    // 产物命名：条目标题是 URL（探测没拿到真标题）或为空时，退回输入文件名——
    // 否则标题经 sanitize 后 "https___www.youtube.com_watch_v=xxx.mp4" 就是输出名
    let title = {
        let t = item.title.trim();
        let is_url = t.starts_with("http://") || t.starts_with("https://") || t.contains("://");
        if t.is_empty() || is_url {
            std::path::Path::new(&path)
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
                .unwrap_or_else(|| t.to_string())
        } else {
            t.to_string()
        }
    };
    let meta = item.meta.clone();
    let params = TranscodeParams {
        input: path.clone(),
        out_dir,
        title,
        filename_template: cfg.download.filename_template.clone(),
        container: "mp4".into(),
        encoder_mode: cfg.transcode.force_encoder_mode.clone(),
        low_power: cfg.transcode.low_power,
        max_w: cfg.transcode.max_w,
        max_h: cfg.transcode.max_h,
        brcap_kbps: cfg.transcode.brcap_kbps,
        br_default_kbps: cfg.transcode.br_default_kbps,
        normalize_audio: cfg.general.normalize_audio,
        max_gain_db: cfg.general.max_gain_db,
        rot_angle: item.rot_angle,
        keep_cover: cfg.transcode.keep_cover,
        collision_policy: cfg.general.collision_policy.clone(),
    };
    let app2 = app.clone();
    let id2 = id.clone();
    let app4 = app.clone();
    let id4 = id.clone();
    // 里程碑诊断：与下载同口径，区分"后端没解析到"与"前端没渲染"
    let band = std::cell::Cell::new(0u8);
    let result = transcode::run_transcode(
        &state.resolver(),
        &params,
        &meta,
        &cancel,
        move |pct| {
            update_item(&app2, &id2, |it| {
                it.percent = pct;
            });
            if pct < 100.0 {
                let b = (pct / 25.0).floor() as u8;
                if b > band.get() && b >= 1 {
                    band.set(b);
                    log_item(
                        &app4,
                        &id4,
                        format!("转码进度 {:.0}%（后端已解析到中间进度）", pct),
                    );
                }
            }
        },
        |line| log_item(&app, &id, line),
    );
    finish_transcode(&app, &id, result);
}

/// 转码收尾：原条目恢复、产物作为新条目回列表、释放队列 slot。
fn finish_transcode(app: &AppHandle, id: &str, result: Result<std::path::PathBuf, CoreError>) {
    let state = app.state::<AppState>();
    let (orig_status, final_status) = match &result {
        Ok(out) => {
            log_item(app, id, format!("转码完成，产物回列表：{}", out.display()));
            (restore_status(app, id), Status::Done)
        }
        Err(CoreError::Cancelled) => {
            log_item(app, id, "已取消，清理残留");
            (restore_status(app, id), Status::Canceled)
        }
        Err(e) => {
            log_error_lines(app, id, "转码失败：", e);
            (restore_status(app, id), Status::Failed)
        }
    };
    update_item(app, id, |it| {
        transition_in(it, orig_status);
        it.percent = if final_status == Status::Done {
            100.0
        } else {
            it.percent
        };
        if let Err(e) = &result {
            it.error = Some(e.to_string());
        }
    });
    // 成功：产物作为新条目回到列表（TC-11）
    if let Ok(out) = &result {
        let meta = probe_output(&state.resolver(), out, |_| {}).unwrap_or_default();
        let title = out
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "转码产物".into());
        let mut prod = MediaItem::new(ItemKind::TranscodeOut, title);
        prod.path = Some(out.to_string_lossy().into_owned());
        prod.status = Status::Done;
        prod.percent = 100.0;
        prod.meta = meta;
        prod.updated_at = now_str();
        state.history.lock().unwrap().upsert(prod.clone());
        // 产物缩略图：从最终产物抽帧（与下载完成兜底同链路）
        {
            let app2 = app.clone();
            let pid = prod.id.clone();
            let cache_dir = state.paths.cache_dir();
            let resolver2 = state.resolver();
            let out2 = out.clone();
            let cover_idx = prod.meta.cover_stream_index;
            tauri::async_runtime::spawn(async move {
                let dest = ytdlp_core::thumbs::thumb_path(&cache_dir, &pid);
                if ytdlp_core::thumbs::ensure_thumb(
                    &resolver2,
                    &out2,
                    &dest,
                    cover_idx.map(|i| i as usize),
                    &mut |_| {},
                )
                .is_ok()
                {
                    update_item(&app2, &pid, |it| {
                        it.thumb = Some(dest.to_string_lossy().into_owned());
                    });
                }
                app2.state::<AppState>().persist();
            });
        }
        let _ = app.emit("item:ready", serde_json::json!({ "id": prod.id }));
        let _ = app.emit("list:changed", ());
    }
    // 清理取消标志 + 释放并发 slot，启动下一个等待任务
    state.cancels.lock().unwrap().remove(id);
    release_slot(app, id);
    persist(app);
}

/// 转码结束后原条目恢复状态：本地文件 → 已就绪；下载产物/转码产物 → 已完成（可再转码）。
fn restore_status(app: &AppHandle, id: &str) -> Status {
    let state = app.state::<AppState>();
    let hist = state.history.lock().unwrap();
    let Some(item) = hist.get(id) else {
        return Status::Ready;
    };
    match item.kind {
        ItemKind::LocalFile | ItemKind::TranscodeOut => Status::Ready,
        _ => Status::Done,
    }
}

/// 队列 slot 释放后启动下一个等待任务（下载/转码按条目状态分流；M3 合并接入）。
fn launch_next(app: &AppHandle, next_id: String) {
    let app2 = app.clone();
    std::thread::spawn(move || {
        let st = app2.state::<AppState>();
        let item = {
            let h = st.history.lock().unwrap();
            h.get(&next_id).cloned()
        };
        let Some(item) = item else {
            // 等待期间条目被删除：finish 已把这个 slot 记到它名下，
            // 必须再释放一次，后面的等待任务才能继续
            release_slot(&app2, &next_id);
            return;
        };
        log_item(&app2, &next_id, "开始执行…");
        match item.status {
            Status::Transcoding => run_transcode_task(app2, next_id),
            Status::Merging => run_merge_task(app2, next_id),
            _ => {
                log_item(&app2, &next_id, "开始下载…");
                let (fid, aonly) = (item.format_id.clone(), item.audio_only);
                run_download_task(app2, next_id, fid, aonly);
            }
        }
    });
}

fn now_str() -> String {
    ytdlp_core::timefmt::datetime_str(
        ytdlp_core::timefmt::now_secs(),
        ytdlp_core::timefmt::local_offset_secs(),
    )
}

#[tauri::command]
pub fn cancel_item(app: AppHandle, id: String) -> CmdResult<()> {
    let state = app.state::<AppState>();
    // 等待中：直接移除
    let removed = {
        let mut q = state.queue.lock().unwrap();
        q.cancel_waiting(&id)
    };
    if removed {
        update_item(&app, &id, |it| {
            transition_in(it, Status::Canceled);
            it.push_log("已取消（队列中移除）".to_string());
        });
        persist(&app);
        return Ok(());
    }
    // 运行中：置取消标志，进程树由任务线程终止
    if let Some(flag) = state.cancel_flag(&id) {
        flag.store(true, Ordering::Relaxed);
        update_item(&app, &id, |it| {
            transition_in(it, Status::Canceled);
            it.push_log("正在取消…".to_string());
        });
        persist(&app);
        Ok(())
    } else {
        Err("该任务未在运行中".into())
    }
}

#[tauri::command]
pub fn remove_item(app: AppHandle, id: String) -> CmdResult<()> {
    let state = app.state::<AppState>();
    let mut hist = state.history.lock().unwrap();
    if hist.remove(&id) {
        drop(hist);
        persist(&app);
        let _ = app.emit("item:removed", id);
        Ok(())
    } else {
        Err("条目不存在".into())
    }
}

#[tauri::command]
pub fn clear_done(app: AppHandle) -> CmdResult<()> {
    let state = app.state::<AppState>();
    state.history.lock().unwrap().clear_terminal();
    persist(&app);
    let _ = app.emit("list:changed", ());
    Ok(())
}

#[tauri::command]
pub fn retry_item(app: AppHandle, id: String) -> CmdResult<()> {
    let state = app.state::<AppState>();
    {
        let mut hist = state.history.lock().unwrap();
        let mut item = hist.get(&id).cloned().ok_or("条目不存在")?;
        if !item.status.is_terminal() && item.status != Status::NeedLogin {
            return Err("仅失败/已取消/需要登录可重试".into());
        }
        let ns = transition(item.status, Status::Probing).map_err(err_string)?;
        item.status = ns;
        item.error = None;
        hist.upsert(item);
    }
    let app2 = app.clone();
    std::thread::spawn(move || run_probe(app2, id));
    persist(&app);
    Ok(())
}

/// 建窗必须在主线程之外完成。
///
/// `WebviewWindowBuilder::new` 在 Windows 上会把建窗请求投递给主线程事件循环并等待结果；
/// 而同步命令本身跑在主线程，于是"主线程等自己"——窗口只建出 HWND、从没绘制过
/// （空白页），连 WM_CLOSE 都没人处理（点 × 关不掉）。官方 API 文档对此有明确警告。
/// 所以两个登录入口都定义成 async 命令，并把建窗放进 `spawn_blocking`。
async fn open_login_off_main_thread(
    app: AppHandle,
    host: String,
    unsupported_msg: String,
) -> CmdResult<()> {
    tauri::async_runtime::spawn_blocking(move || {
        let login_url = login::login_url_for_host(&host)
            .ok_or_else(|| unsupported_msg.replace("{host}", &host))?;
        login::open_login(&app, &host, &login_url).map_err(err_string)?;
        Ok(())
    })
    .await
    .map_err(|e| format!("登录窗口启动失败：{e}"))?
}

#[tauri::command]
pub async fn relogin_item(app: AppHandle, id: String) -> CmdResult<()> {
    let host = {
        let state = app.state::<AppState>();
        let hist = state.history.lock().unwrap();
        let item = hist.get(&id).ok_or("条目不存在")?;
        item.host.clone().or_else(|| {
            item.url
                .as_deref()
                .and_then(ytdlp_core::cookies::host_from_url)
        })
    };
    let host = host.ok_or("无法确定登录站点")?;
    open_login_off_main_thread(
        app,
        host,
        "站点 {host} 不支持内置登录：请在设置-Cookie 中为该站点添加 Cookie 后重试".into(),
    )
    .await
}

/// 从设置页直接打开某站点的内置登录窗（不依赖列表条目状态）。
///
/// 列表里的"去登录"原先只在 `NeedLogin` 时出现，但不少站点**未登录也能解析**
/// 出受限清晰度（B 站未登录只给低码率，条目状态是 Ready 而不是 NeedLogin），
/// 于是用户没有任何入口去登录换取高清晰度。这里提供一个与条目无关的入口。
///
/// 传站点级域名（如 `bilibili.com`）即可：`cookie_candidates` 会按
/// "精确 host → 父域 → www 子域" 回退，条目侧的 `www.bilibili.com` 一样命中。
#[tauri::command]
pub async fn open_login_site(app: AppHandle, host: String) -> CmdResult<()> {
    open_login_off_main_thread(
        app,
        host,
        "站点 {host} 不支持内置登录：请在下方的 Cookie 列表中直接导入".into(),
    )
    .await
}

// ---------- 配置 ----------

#[tauri::command]
pub fn get_config(state: State<'_, AppState>) -> CmdResult<AppConfig> {
    Ok(state.config.lock().unwrap().clone())
}

#[tauri::command]
pub fn save_config(app: AppHandle, config: AppConfig) -> CmdResult<()> {
    let state = app.state::<AppState>();
    {
        let mut cur = state.config.lock().unwrap();
        *cur = config.clone();
    }
    let path = state.paths.config_file();
    let _ = std::fs::create_dir_all(state.paths.config_dir());
    config.save(&path).map_err(err_string)?;
    // 并发上限即时生效
    state
        .queue
        .lock()
        .unwrap()
        .set_concurrency(config.general.concurrency as usize);
    Ok(())
}

// ---------- Cookie ----------

#[tauri::command]
pub fn list_cookies(state: State<'_, AppState>) -> CmdResult<Vec<serde_json::Value>> {
    let store = CookieStore::new(state.paths.cookies_dir());
    let mut out = Vec::new();
    for host in store.list_hosts().map_err(err_string)? {
        let cookies = store.load_host(&host).map_err(err_string)?;
        out.push(serde_json::json!({
            "host": host,
            "count": cookies.len(),
            "expires_at": cookies.iter().filter_map(|c| c.expires).fold(0.0, f64::max),
        }));
    }
    Ok(out)
}

#[tauri::command]
pub fn save_cookies(
    app: AppHandle,
    host: String,
    cookies: Vec<ytdlp_core::cookies::CookieEntry>,
) -> CmdResult<()> {
    let state = app.state::<AppState>();
    let normalized = normalize_cookie_host(&host);
    let store = CookieStore::new(state.paths.cookies_dir());
    store.save_host(&normalized, cookies).map_err(err_string)?;
    let _ = app.emit("cookies:changed", normalized);
    Ok(())
}

/// 规范化 cookie 存储 host：YouTube 相关域名统一存 www.youtube.com.txt
fn normalize_cookie_host(host: &str) -> String {
    match host.to_lowercase().as_str() {
        "youtube.com" | "youtu.be" | "m.youtube.com" | "www.youtube.com" => "www.youtube.com".into(),
        "bilibili.com" | "www.bilibili.com" | "m.bilibili.com" => "www.bilibili.com".into(),
        "x.com" | "www.x.com" | "twitter.com" | "www.twitter.com" => "www.x.com".into(),
        "douyin.com" | "www.douyin.com" | "m.douyin.com" => "www.douyin.com".into(),
        h => h.to_string(),
    }
}

#[tauri::command]
pub fn delete_cookie(app: AppHandle, host: String) -> CmdResult<()> {
    let state = app.state::<AppState>();
    let store = CookieStore::new(state.paths.cookies_dir());
    store.delete_host(&host).map_err(err_string)?;
    let _ = app.emit("cookies:changed", host);
    Ok(())
}

// ---------- 依赖自检 ----------

#[tauri::command]
pub fn probe_dependencies(state: State<'_, AppState>) -> CmdResult<Vec<ToolStatus>> {
    let resolver = state.resolver();
    let mut out = Vec::new();
    for tool in [
        ytdlp_core::exec::Tool::YtDlp,
        ytdlp_core::exec::Tool::Ffmpeg,
        ytdlp_core::exec::Tool::Ffprobe,
        ytdlp_core::exec::Tool::Deno,
    ] {
        let resolved = resolver.resolve(tool);
        let (path, version, ok) = match &resolved {
            Ok(p) => {
                let v = ytdlp_core::exec::tool_version(&resolver, tool);
                (Some(p.to_string_lossy().into_owned()), v, true)
            }
            Err(_) => (None, None, false),
        };
        out.push(ToolStatus {
            tool: tool.name().to_string(),
            path,
            version,
            ok,
        });
    }
    Ok(out)
}

// ---------- 其他 ----------

#[tauri::command]
pub fn open_item_dir(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    let hist = state.history.lock().unwrap();
    let item = hist.get(&id).ok_or("条目不存在")?;
    let target = item
        .file
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| item.path.clone().map(PathBuf::from))
        .ok_or("该条目没有本地文件")?;
    let dir = if target.is_dir() {
        target
    } else {
        target.parent().map(Path::to_path_buf).unwrap_or(target)
    };
    open_in_explorer(&dir)
}

#[cfg(windows)]
fn open_in_explorer(dir: &Path) -> CmdResult<()> {
    std::process::Command::new("explorer")
        .arg(dir)
        .spawn()
        .map_err(err_string)?;
    Ok(())
}

#[cfg(not(windows))]
fn open_in_explorer(_dir: &Path) -> CmdResult<()> {
    Err("仅 Windows 支持打开目录".into())
}

#[tauri::command]
pub fn clear_temp(app: AppHandle) -> CmdResult<()> {
    let state = app.state::<AppState>();
    let dir = state.paths.temp_dir();
    if dir.is_dir() {
        // 跳过运行中任务的私有目录（temp/<任务id>/ 里是正在使用的 Cookie 导出等）
        // 与工具下载的 temp/tool_dl；只清历史残留，避免把进行中的任务搞坏
        let active: Vec<String> = state.cancels.lock().unwrap().keys().cloned().collect();
        let tool_dl_busy = active.iter().any(|k| k.starts_with("tool-dl-"));
        for e in std::fs::read_dir(&dir).map_err(err_string)? {
            let e = e.map_err(err_string)?;
            let name = e.file_name().to_string_lossy().into_owned();
            if active.contains(&name) || (tool_dl_busy && name == "tool_dl") {
                continue;
            }
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
    Ok(())
}

/// 设置条目旋转角度（UL-12：随条目保存，转码时生效；M2 使用）。
#[tauri::command]
pub fn rot_item(app: AppHandle, id: String, degrees: u16) -> CmdResult<()> {
    let angle = ytdlp_core::model::RotAngle::from_degrees(degrees);
    update_item(&app, &id, |it| {
        it.rot_angle = angle;
    });
    // "随条目保存"必须落盘，否则重启后丢失
    persist(&app);
    Ok(())
}
