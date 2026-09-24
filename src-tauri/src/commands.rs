//! Tauri 命令层：统一列表 CRUD + 解析/下载/取消/删除 + 配置 + Cookie + 依赖自检。
//!
//! 后台任务用 std::thread + 事件 `item:update`（payload = MediaItem）回推前端；
//! 关键状态变更才持久化 history.json（进度高频更新只 emit 不落盘）。
//!
//! **命令的线程语义**：Tauri 的同步 `#[tauri::command]` 在**主线程**执行
//! （`login.rs` 里建窗死锁的教训同源），因此凡是会做 IO / 起子进程 / 序列化大对象
//! 的命令都必须离开主线程，否则窗口会卡在"点按钮没反应"。两种范式二选一、不可混用：
//!
//! 1. 同步阻塞命令：`#[tauri::command(async)] pub fn`（Tauri 把整个函数丢到阻塞线程池）。
//!    函数体仍是同步的，**内部不能出现 `.await`**；要跑后台任务就 `std::thread::spawn`
//!    或 `spawn_blocking(...)` 后不 await（fire-and-forget）。
//! 2. 需要 `.await` 拿结果的命令：`#[tauri::command] pub async fn`（注意宏**不带** `(async)`），
//!    阻塞操作包在 `spawn_blocking(...).await` 里，期间不占用 async worker。
//!
//! **硬性约定（A6）**：`#[tauri::command(async)] pub fn` 的函数体内**不得出现** fs / 子进程 /
//! persist / remove_dir_all 等分钟级阻塞调用——`(async)` 只把命令移出主线程，仍可能占住
//! tokio 的 async worker（worker 数 ≈ CPU 核数），分钟级阻塞会饿死同文件里真正异步的其它
//! 命令。凡含 IO / 子进程 / 大序列化 / persist 的命令，统一用「范式 2」：
//! `pub async fn` + `spawn_blocking(...).await`。已收敛的命令见 `probe_dependencies`、
//! `probe_hw_encoders`、`clear_temp`、`download_tool`、`add_local`、三处缩略图。
//!
//! 常见错误：给同步 `pub fn` 加了 `#[tauri::command(async)]` 却又在函数体里 `.await`
//! （`(async)` 不会把函数变成 async），会直接 E0728 编译失败。

use std::collections::VecDeque;
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
use ytdlp_core::{log, transition, CoreError};

use crate::login;
use crate::state::AppState;

/// 解析（探测）并发上限：粘贴 100 条链接时不能瞬间起 100 个 yt-dlp 子进程
/// ——打满本机资源，也必然触发站点风控。播放列表展开（DL-09）本就是顺序解析，
/// 这里把"多条 URL 同时入列"收进同一条约束。
const PROBE_CONCURRENCY: u64 = 3;

/// A1：敏感命令的调用方守卫。自定义命令不受 capability 的 `windows` 白名单约束，
/// 任何窗口/webview（含登录窗里承载的第三方页面）都能 `invoke`。当前 capability 未声明
/// `remote.urls`（登录窗加载的是远程站点，本就不能用 IPC），但为防未来误加 remote / 误建
/// 可执行窗口，给真正敏感的命令加一层"仅主窗口可调用"的硬校验。
///
/// 只对需要从主窗口触发的命令用；`save_cookies` 等由 login.rs 内部 Rust 直接调用
/// （不走 IPC）的命令**不加**，避免误伤登录流程。
fn ensure_main_window(win: &tauri::WebviewWindow) -> Result<(), String> {
    if win.label() == "main" {
        Ok(())
    } else {
        Err("该操作仅允许从主窗口发起".into())
    }
}

/// 解析闸门：计数信号量（条件变量实现）。
///
/// 旧实现是「原子自旋 + 50ms sleep」抢闸位：粘贴 100 条链接会起 100 条线程，
/// 其中 97 条在自旋等 3 个闸位，等待时间和线程数成正比、还白烧 CPU。
/// 换条件变量后等待者在内核里休眠，归还时唤醒一个。
struct ProbeGate {
    in_use: parking_lot::Mutex<u64>,
    free: parking_lot::Condvar,
}

static PROBE_GATE: std::sync::LazyLock<ProbeGate> = std::sync::LazyLock::new(|| ProbeGate {
    in_use: parking_lot::Mutex::new(0),
    free: parking_lot::Condvar::new(),
});

/// 解析闸位守卫：`Drop` 归还并唤醒等待者，任何早退/panic 路径都不会漏。
struct ProbeSlot;

impl ProbeSlot {
    fn acquire() -> Self {
        let mut n = PROBE_GATE.in_use.lock();
        while *n >= PROBE_CONCURRENCY {
            PROBE_GATE.free.wait(&mut n);
        }
        *n += 1;
        Self
    }
}

impl Drop for ProbeSlot {
    fn drop(&mut self) {
        let mut n = PROBE_GATE.in_use.lock();
        let left = n.saturating_sub(1);
        *n = left;
        drop(n);
        PROBE_GATE.free.notify_one();
    }
}

/// 并发额度守卫：任务线程持有它，`Drop` 时释放并发 slot。
///
/// 必须有这一层：`release_slot` 原先只在正常路径手写调用，任务线程一旦 panic
/// （例如 `f32::clamp` 的断言、某个 `unwrap`），slot 就永久少一格 ——
/// 并发上限 3 的机器上"死"三次之后，所有任务只会排队、永远不开始。
struct SlotGuard {
    app: AppHandle,
    id: String,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        release_slot(&self.app, &self.id);
    }
}

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

type CmdResult<T> = Result<T, String>;

fn err_string(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// 更新条目并返回克隆（变更即原子写仅对状态迁移生效由调用方决定）。
/// 仅用于低频的状态迁移/元数据更新；高频进度与日志走 update_progress / log_item
/// 的轻量事件，避免每秒数十次全量 clone + 序列化整个 MediaItem（含 300 行日志）。
fn update_item(app: &AppHandle, id: &str, f: impl FnOnce(&mut MediaItem)) -> Option<MediaItem> {
    let state = app.state::<AppState>();
    let mut hist = state.history.lock();
    let mut item = hist.get(id)?.clone();
    f(&mut item);
    hist.upsert(item.clone());
    drop(hist);
    let _ = app.emit("item:update", &item);
    Some(item)
}

/// 高频进度事件 payload：只带变化的字段（Option::None 表示本次不更新该字段），
/// 不携带日志/元数据，IPC 体积从整条目（可达数十 KB）降到几十字节。
#[derive(serde::Serialize, Clone)]
struct ProgressPayload {
    id: String,
    percent: Option<f32>,
    speed: Option<String>,
    eta: Option<String>,
    file: Option<String>,
}

/// 高频进度更新：锁内原地改数值（不 clone 整条目、不 upsert），出锁发轻量事件。
fn update_progress(app: &AppHandle, p: ProgressPayload) {
    {
        let state = app.state::<AppState>();
        let mut hist = state.history.lock();
        if let Some(it) = hist.get_mut(&p.id) {
            if let Some(pct) = p.percent {
                it.percent = pct;
            }
            if p.speed.is_some() {
                it.speed = p.speed.clone();
            }
            if p.eta.is_some() {
                it.eta = p.eta.clone();
            }
            if p.file.is_some() {
                it.file = p.file.clone();
            }
        }
    }
    let _ = app.emit("item:progress", &p);
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

/// 记录日志行：锁内原地 push（不 clone 整条目），发轻量 `item:log` 事件
/// （只含 id + 单行文本），前端增量 append；不再触发全量 item:update。
fn log_item(app: &AppHandle, id: &str, line: impl Into<String>) {
    let line = line.into();
    {
        let state = app.state::<AppState>();
        let mut hist = state.history.lock();
        if let Some(it) = hist.get_mut(id) {
            it.push_log(line.clone());
        }
    }
    let _ = app.emit("item:log", serde_json::json!({ "id": id, "line": line }));
}

/// 持久化（状态迁移后调用）。
fn persist(app: &AppHandle) {
    app.state::<AppState>().persist();
}

// ---------- 添加与解析 ----------

#[tauri::command(async)]
pub fn add_url(app: AppHandle, urls: Vec<String>) -> CmdResult<()> {
    let state = app.state::<AppState>();
    // 新条目继承"仅音频默认"（设置-下载）：出 history 锁之前先取，避免嵌套锁
    let default_audio_only = state.config.lock().download.audio_only;
    let mut hist = state.history.lock();
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
        let mut item = MediaItem::from_url(url);
        item.audio_only = default_audio_only;
        let id = item.id.clone();
        hist.upsert(item);
        // 解析线程不占并发 slot（下载/转码/合并才有并发上限），但受解析闸门约束：
        // 一次粘贴 100 条链接不能瞬间起 100 个 yt-dlp
        let app2 = app.clone();
        std::thread::spawn(move || {
            let _slot = ProbeSlot::acquire();
            run_probe(app2, id);
        });
    }
    drop(hist);
    persist(&app);
    let _ = app.emit("list:changed", ());
    Ok(())
}

#[tauri::command]
pub async fn add_local(app: AppHandle, paths: Vec<String>, recursive: bool) -> CmdResult<()> {
    // 目录扫描放阻塞线程：递归遍历上万个文件足以让 async worker 肉眼可见地卡住
    let files = tauri::async_runtime::spawn_blocking(move || {
        let mut files: Vec<PathBuf> = Vec::new();
        for p in paths {
            let pb = PathBuf::from(&p);
            if pb.is_dir() {
                scan_dir(&pb, recursive, &mut files);
            } else if pb.is_file() {
                files.push(pb);
            }
        }
        files
    })
    .await
    .map_err(|e| format!("扫描目录失败：{e}"))?;
    if files.is_empty() {
        return Err("没有找到可添加的文件".into());
    }
    let state = app.state::<AppState>();
    let mut hist = state.history.lock();
    for f in files {
        let item = MediaItem::from_path(f.to_string_lossy().into_owned());
        let id = item.id.clone();
        hist.upsert(item);
        let app2 = app.clone();
        std::thread::spawn(move || {
            let _slot = ProbeSlot::acquire();
            run_probe(app2, id);
        });
    }
    drop(hist);
    persist(&app);
    let _ = app.emit("list:changed", ());
    Ok(())
}

/// 目录扫描的最大递归深度。
///
/// Windows 的 junction / 目录符号链接让"目录树"可以成环（用户目录下就有系统
/// 预置的兼容链接），无界递归会栈溢出或把整个盘扫进来。32 层对素材目录足够。
const MAX_SCAN_DEPTH: usize = 32;

fn scan_dir(dir: &Path, recursive: bool, out: &mut Vec<PathBuf>) {
    scan_dir_inner(dir, recursive, 0, out);
}

fn scan_dir_inner(dir: &Path, recursive: bool, depth: usize, out: &mut Vec<PathBuf>) {
    if depth >= MAX_SCAN_DEPTH {
        return;
    }
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for e in rd.flatten() {
        // DirEntry::file_type() 不跟随符号链接：junction / 目录符号链接一律跳过
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        let p = e.path();
        // 写成"单层 if + else if"，避免嵌套 if（collapsible_if 是 CI 的
        // -D warnings 门禁之一）
        if ft.is_dir() && recursive {
            scan_dir_inner(&p, true, depth + 1, out);
        } else if !ft.is_dir() && download::is_media_file(&p) {
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
///
/// 注册取消标志：**解析中的条目也要能取消**（yt-dlp 卡在 DNS/握手时，
/// `--socket-timeout` 管不到建连阶段，只有取消能救），取消由 `probe::probe_url`
/// 内部的看门狗落实为进程树终止。
fn run_probe(app: AppHandle, id: String) {
    let state = app.state::<AppState>();
    let item = {
        let hist = state.history.lock();
        hist.get(&id).cloned()
    };
    let Some(item) = item else {
        return;
    };
    let cancel = state.register_cancel(&id);
    log_item(&app, &id, "开始解析元数据…");

    let resolver = state.resolver();
    let network = state.config.lock().network.clone();
    // 任务私有临时目录：cookie 导出落这里，任务结束整体清理
    let task_tmp = state.paths.task_temp_dir(&id);
    let _ = std::fs::create_dir_all(&task_tmp);
    let (netscape, cookie_warn) = resolve_cookies(&state, &item, &task_tmp);
    if let Some(w) = cookie_warn {
        log_item(&app, &id, w);
    }

    let playlist_on = state.config.lock().download.playlist;
    let result = if item.url.is_some() {
        probe::probe_url(
            &resolver,
            item.url.as_deref().unwrap_or_default(),
            netscape.as_deref(),
            &network,
            playlist_on,
            Some(&cancel),
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
                it.thumbnail_url = p.thumbnail_url.clone();
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
                // 封面缩略图（异步生成，不阻塞就绪）。
                // spawn_blocking：里面是 curl 网络下载 + ffmpeg 子进程，全是阻塞调用，
                // 放在 async 执行器的 worker 上会占住线程（执行器线程数 ≈ CPU 核数）
                {
                    let app2 = app.clone();
                    let id2 = id.clone();
                    let cache_dir = state.paths.cache_dir();
                    let thumb_url = p.thumbnail_url.clone();
                    let local_path = item.path.clone();
                    // 本地文件优先取内嵌封面（元数据），与桌面缩略图同源
                    let resolver2 = resolver.clone();
                    let proxy2 = network.proxy_url.clone();
                    tauri::async_runtime::spawn_blocking(move || {
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
                expand_playlist(
                    &app,
                    &id,
                    &item,
                    &resolver,
                    netscape.as_deref(),
                    &network,
                    &cancel,
                );
            }
        }
        Err(f) => {
            // 取消 → 已取消；需要登录 → 需登录；其余 → 失败
            let status = match f.kind {
                ProbeErrorKind::NeedLogin => Status::NeedLogin,
                ProbeErrorKind::Cancelled => Status::Canceled,
                _ => Status::Failed,
            };
            update_item(&app, &id, |it| {
                transition_in(it, status);
                if f.kind != ProbeErrorKind::Cancelled {
                    it.error = Some(f.to_string());
                }
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
        }
    }
    // 解析阶段临时目录清理（cookie 导出在本任务私有目录下，库文件本身不删）
    let _ = std::fs::remove_dir_all(&task_tmp);
    // 清掉取消标志注册表条目（否则随条目数累积，也让后续 cancel_item 的
    // "在不在运行中"判断失真）
    state.cancels.lock().remove(&id);
    // 落盘只做一次：上面两个分支里的 persist 都与这一次重复（每次都是
    // clone + 序列化 + fsync 整份 history，批量粘贴时按条数线性放大）
    persist(&app);
}

// ---------- 列表 ----------

/// 列表全量返回（含每条最多 300 行日志）。
/// `(async)`：克隆 + 序列化整份历史是"MB 级"操作，前端在活动任务期间每 1.5s
/// 轮询一次，留在主线程会周期性卡住窗口。
/// 轮询请用 [`list_items_lite`]：这一份只用于首次加载（日志弹窗需要历史）。
#[tauri::command(async)]
pub fn list_items(state: State<'_, AppState>) -> CmdResult<Vec<MediaItem>> {
    let hist = state.history.lock();
    Ok(hist.items.clone())
}

/// 列表轻量快照：**不含**日志。
///
/// 列表渲染只需要状态/进度/元数据；每条最多 300 行的日志（100 条时可达 MB 级）
/// 让每次轮询的 IPC 与 JSON 解析成本凭空翻几倍。日志有两条正式通路：
/// `item:log` 增量事件 + 打开弹窗时的 [`get_item_log`]。
#[tauri::command(async)]
pub fn list_items_lite(state: State<'_, AppState>) -> CmdResult<Vec<MediaItem>> {
    let hist = state.history.lock();
    Ok(hist
        .items
        .iter()
        .map(|i| {
            let mut c = i.clone();
            c.log = VecDeque::new();
            c
        })
        .collect())
}

/// 取某条目的完整日志（打开日志弹窗时按需拉取的权威副本）。
#[tauri::command(async)]
pub fn get_item_log(state: State<'_, AppState>, id: String) -> CmdResult<Vec<String>> {
    let hist = state.history.lock();
    Ok(hist
        .get(&id)
        .map(|i| i.log.iter().cloned().collect())
        .unwrap_or_default())
}

// ---------- 动作 ----------

#[tauri::command(async)]
pub fn start_download(
    app: AppHandle,
    id: String,
    format_id: Option<String>,
    audio_only: bool,
) -> CmdResult<()> {
    let state = app.state::<AppState>();
    // 状态切换必须出锁后 emit item:update：前端 1.5s 轮询有"有 busy 条目才
    // 刷新列表"的优化，若不主动通知，前端永远认为状态是 Ready、永不拉列表，
    // 进度条也不显示（progressCell 仅在 Downloading 等状态渲染）——yt-dlp
    // 实际在跑但 UI 完全无感知。
    let updated = {
        let mut hist = state.history.lock();
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
        hist.upsert(item.clone());
        item
    };
    let _ = app.emit("item:update", &updated);
    // 提交并发队列
    let outcome = {
        let mut q = state.queue.lock();
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
    // 并发额度守卫：panic / 任何提前返回都会释放 slot（手写 release_slot 漏一次
    // 就等于永久少一格并发额度）
    let _slot = SlotGuard {
        app: app.clone(),
        id: id.clone(),
    };

    let (
        url,
        cfg,
        general,
        resolver,
        out_dir,
        template,
        proxy,
        netscape,
        sections,
        js_runtime,
        cookie_warn,
    ) = {
        // 只在锁内 clone 条目：后续 Cookie 导出（export_netscape）是文件 IO，
        // 持 history 锁做会阻塞所有 update_item 调用（前端刷新卡顿）
        let item = {
            let hist = state.history.lock();
            hist.get(&id).cloned()
        };
        let Some(item) = item else {
            // 条目在排队期间被删除：清掉取消标志即可（并发 slot 由 _slot 守卫释放）
            state.cancels.lock().remove(&id);
            return;
        };
        let url = item.url.clone().unwrap_or_default();
        let cfg = state.config.lock().download.clone();
        let general = state.config.lock().general.clone();
        let resolver = state.resolver();
        let out_dir = default_output_dir(&state);
        let template = cfg.filename_template.clone();
        let proxy = state.config.lock().network.resolve_proxy(&url);
        let task_tmp = state.paths.task_temp_dir(&id);
        let (netscape, cookie_warn) = resolve_cookies(&state, &item, &task_tmp);
        let sections = item.sections.clone();
        // JS 运行时（§3.6 依赖）：托管/配置的 deno 必须显式传给 yt-dlp，
        // 否则 YouTube 组件会因"没有 JS 运行时"失败（依赖自检却是通过的）
        let js_runtime = resolver
            .resolve(ytdlp_core::exec::Tool::Deno)
            .ok()
            .map(|p| format!("deno:{}", p.to_string_lossy()));
        (
            url,
            cfg,
            general,
            resolver,
            out_dir,
            template,
            proxy,
            netscape,
            sections,
            js_runtime,
            cookie_warn,
        )
    };
    if let Some(w) = cookie_warn {
        log_item(&app, &id, w);
    }

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
            // Destination 行（切换到下一条流，percent 恒为 0）只更新目标文件名，
            // 不把进度打回 0：DASH 双流下载视频 100% → 音频 Destination 会把
            // percent 重置，快速下载看起来就像"一直 0%"
            let progress = if p.file.is_some() {
                ProgressPayload {
                    id: id2.clone(),
                    percent: None,
                    speed: None,
                    eta: None,
                    file: p.file.clone(),
                }
            } else {
                ProgressPayload {
                    id: id2.clone(),
                    percent: Some(p.percent),
                    speed: p.speed,
                    eta: p.eta,
                    file: None,
                }
            };
            update_progress(&app2, progress);
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
            finish_download(&app, &id, Err(e));
            return;
        }
    };

    // 后处理（DL-04）
    let first = outcome.output_paths.first().cloned();
    if outcome.matched_by_scan {
        // 产物是"目录里新出现的最新媒体文件"（弱证据）：并发下载写同一个输出目录
        // 时可能是别人的产物，因此只提示、不后处理、不写入条目 path
        log_item(
            &app,
            &id,
            format!(
                "疑似产物（按目录新增文件推断，未做后处理）：{}",
                first
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            ),
        );
    } else if outcome.preexisting {
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
        let pp = post_process(
            &resolver,
            path,
            &cfg2,
            &general2,
            &cancel2,
            |pct| {
                // 后处理是整片重编码，没有进度就是"卡在 100% 不动"的观感
                update_progress(
                    &app,
                    ProgressPayload {
                        id: id.clone(),
                        percent: Some(pct),
                        speed: None,
                        eta: None,
                        file: None,
                    },
                );
            },
            |line| log_item(&app2, &id, line),
        );
        match pp {
            Ok((final_path, meta)) => {
                outcome.output_paths = vec![final_path];
                update_item(&app, &id, |it| {
                    it.meta = meta;
                });
                // 不在这里落盘：finish_download 收尾时会写一次（同样的快照），
                // 每次 persist 都是 clone + 序列化 + fsync 整份 history
            }
            Err(e) => {
                finish_download(&app, &id, Err(e));
                return;
            }
        }
    }
    finish_download(&app, &id, Ok(outcome));
}

fn finish_download(app: &AppHandle, id: &str, result: Result<download::DownloadOutcome, CoreError>) {
    let state = app.state::<AppState>();
    // 本次任务的最终产物（回填 path + 抽帧封面共用）。
    // 目录扫描来的"弱证据"产物不写进条目：它可能根本不是本次任务的产物。
    let final_path = result.as_ref().ok().and_then(|o| {
        if o.matched_by_scan {
            None
        } else {
            o.output_paths.first().cloned()
        }
    });
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
                let hist = state.history.lock();
                match hist.get(id) {
                    // 产物已有缩略图则不动；否则优先取 yt-dlp --write-thumbnail
                    // 写在输出目录的封面文件，ffmpeg 抽帧仅作兜底
                    Some(it) => (
                        it.thumb.clone().is_none(),
                        it.meta.cover_stream_index,
                    ),
                    None => (false, None),
                }
            };
            if need_thumb {
                let app2 = app.clone();
                let id2 = id.to_string();
                let cache_dir = state.paths.cache_dir();
                let resolver2 = state.resolver();
                // spawn_blocking：内部是 std::fs::copy 与 ffmpeg 子进程（阻塞）
                tauri::async_runtime::spawn_blocking(move || {
                    let dest = ytdlp_core::thumbs::thumb_path(&cache_dir, &id2);
                    let out = std::path::Path::new(&out_path);
                    // 优先取 yt-dlp --write-thumbnail 写在输出目录的封面：
                    // yt-dlp 下载时已经拉过 webp 封面，直接复制到 thumbs，
                    // 不需要重新从网络下载，也不需要 ffmpeg 抽帧。
                    let written = ytdlp_core::thumbs::collect_written_thumbnail(out);
                    let ok = if let Some(thumb_file) = written {
                        match std::fs::copy(&thumb_file, &dest) {
                            Ok(_) => {
                                let _ = std::fs::remove_file(&thumb_file);
                                log_item(&app2, &id2, format!("缩略图：取自 yt-dlp 封面 {}", thumb_file.display()));
                                true
                            }
                            Err(e) => {
                                log_item(&app2, &id2, format!("缩略图：复制封面失败（{}），回退 ffmpeg 抽帧", e));
                                false
                            }
                        }
                    } else {
                        log_item(&app2, &id2, "缩略图：输出目录无封面文件，ffmpeg 抽帧".to_string());
                        false
                    };
                    // 兜底：ffmpeg 从产物抽内嵌封面流，再不行抽首帧
                    let ok = ok
                        || ytdlp_core::thumbs::ensure_thumb(
                            &resolver2,
                            out,
                            &dest,
                            cover_idx.map(|i| i as usize),
                            &mut |l| log_item(&app2, &id2, l),
                        )
                        .is_ok();
                    if ok {
                        update_item(&app2, &id2, |it| {
                            it.thumb = Some(dest.to_string_lossy().into_owned());
                        });
                        app2.state::<AppState>().persist();
                    }
                });
            }
        }
    }
    // 任务结束：清理本任务私有临时目录（cookie 导出文件也在这个目录下，一并清理；
    // config/cookies/ 里的站点库文件不受影响）
    let _ = std::fs::remove_dir_all(state.paths.task_temp_dir(id));
    // 清理取消标志注册表条目（否则随任务数无限累积；也让后续 cancel_item
    // 的 cancel_flag 查询能正确区分"运行中"与"已结束"）
    state.cancels.lock().remove(id);
    // 并发 slot 由调用方（run_download_task）的 SlotGuard 释放：
    // 手写释放 + 守卫释放会重复归还，可能把下一个等待任务"超发"启动
    persist(app);
}

fn default_output_dir(state: &AppState) -> PathBuf {
    if let Some(dir) = &state.cli.lock().dir {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Some(dir) = &state.config.lock().general.default_output_dir {
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

/// Cookie 文件准备：CLI `--cookies` 优先；否则把 Cookie 库里匹配站点的条目
/// **合并导出到任务私有文件**（`<task_temp>/cookies-<host>.txt`）。
///
/// 导出而不是直接给库文件：库文件在任务期间可能被"重新登录/删除"改写，而
/// yt-dlp 正在读它（旧实现还会把合并结果写回库文件本身，见
/// `cookies::export_for_task` 的注释）。返回值第二项是给用户看的告警。
fn resolve_cookies(
    state: &AppState,
    item: &MediaItem,
    task_tmp: &Path,
) -> (Option<PathBuf>, Option<String>) {
    if let Some(p) = &state.cli.lock().cookies {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return (Some(pb), None);
        }
        return (None, Some(format!("CLI 指定的 cookie 文件不存在：{p}")));
    }
    let host = match item.host.clone().or_else(|| {
        item.url
            .as_deref()
            .and_then(ytdlp_core::cookies::host_from_url)
    }) {
        Some(h) => h,
        None => return (None, None),
    };
    let store = CookieStore::new(state.paths.cookies_dir());
    let dest = task_tmp.join(format!(
        "cookies-{}.txt",
        ytdlp_core::cookies::sanitize_host(&host)
    ));
    match store.export_for_task(&host, &dest) {
        Ok(p) => (p, None),
        // 读失败绝不当成"该站点没有 cookie"：否则现象是"明明登录了却说需要登录"，
        // 日志里没有任何线索
        Err(e) => (None, Some(format!("读取站点 {host} 的 Cookie 失败：{e}"))),
    }
}

/// 释放并发 slot 并启动下一个等待任务（下载/转码/合并共用）。
fn release_slot(app: &AppHandle, id: &str) {
    let state = app.state::<AppState>();
    let next = {
        let mut q = state.queue.lock();
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
#[tauri::command(async)]
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
        let mut hist = state.history.lock();
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
                    let updated = MediaItem {
                        status: to,
                        percent: 0.0,
                        ..item
                    };
                    hist.upsert(updated.clone());
                    // 出锁再 emit：避免在主线程外持着 history 锁做事件派发
                    // （与 start_transcode 同一套写法）
                    drop(hist);
                    let _ = app.emit("item:update", &updated);
                    hist = state.history.lock();
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
        // 上面已经把通过的条目改成 Merging 了：这里必须还原，否则它们永远卡在
        // "合并中"——既不在队列里（没有执行体），也不在终态（retry 只接受
        // 失败/已取消/需要登录），用户除了删条目没有别的出路。
        for jid in &jobs {
            let orig = restore_status(&app, jid);
            update_item(&app, jid, |it| {
                transition_in(it, orig);
                it.error = Some("可合并条目不足 2 个，本次合并未执行".into());
            });
        }
        persist(&app);
        return Err("可合并条目不足 2 个".into());
    }
    let norm = normalize.unwrap_or_else(|| state.config.lock().general.normalize_audio);
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
            .insert(id.clone(), job.clone());
    }
    let outcome = {
        let mut q = state.queue.lock();
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
    // 并发额度守卫（见 SlotGuard 注释）
    let _slot = SlotGuard {
        app: app.clone(),
        id: id.clone(),
    };
    let job = state.merge_jobs.lock().get(&id).cloned();
    let Some(job) = job else {
        return;
    };
    // 取消标志共享给全部参与条目：从任意被勾选条目点"取消"都能取消这次合并
    {
        let mut cancels = state.cancels.lock();
        for jid in &job.ids {
            cancels.insert(jid.clone(), cancel.clone());
        }
    }
    let (inputs, params) = {
        let mut inputs = Vec::new();
        let mut anchor = None;
        {
            let hist = state.history.lock();
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
        let cfg = state.config.lock().clone();
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
            let mut jobs = state.merge_jobs.lock();
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
        // 并发 slot 由 _slot 守卫释放
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
            update_progress(
                &app2,
                ProgressPayload {
                    id: id2.clone(),
                    percent: Some(pct),
                    speed: None,
                    eta: None,
                    file: None,
                },
            );
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
        // 探测失败不再静默吞掉：产物元数据（画质列）会空着，必须让用户看到原因
        let meta = match download::probe_output(&state.resolver(), out, |l| log_item(app, id, l)) {
            Ok(m) => m,
            Err(e) => {
                log_item(app, id, format!("产物解析失败（画质列为空）：{e}"));
                Default::default()
            }
        };
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
        state.history.lock().upsert(prod.clone());
        // 产物缩略图：从最终产物抽帧（与下载完成兜底同链路）
        {
            let app2 = app.clone();
            let pid = prod.id.clone();
            let cache_dir = state.paths.cache_dir();
            let resolver2 = state.resolver();
            let out2 = out.clone();
            let cover_idx = prod.meta.cover_stream_index;
            // spawn_blocking：内部是 ffmpeg 子进程（阻塞）
            tauri::async_runtime::spawn_blocking(move || {
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
        let mut jobs = state.merge_jobs.lock();
        for jid in ids {
            jobs.remove(jid);
        }
    }
    // 合并的取消标志注册在每个参与条目上（run_merge_task），一并清理
    {
        let mut cancels = state.cancels.lock();
        for jid in ids {
            cancels.remove(jid);
        }
    }
    // 并发 slot 由调用方的 SlotGuard 释放
    persist(app);
}

/// 设置时间范围下载（DL-12）：起止 "HH:MM:SS"；空串清除。
#[tauri::command(async)]
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
#[tauri::command(async)]
pub fn start_transcode(app: AppHandle, ids: Vec<String>) -> CmdResult<()> {
    let state = app.state::<AppState>();
    if ids.is_empty() {
        return Err("未选择条目".into());
    }
    let mut to_run = Vec::new();
    // 同 start_merge：跳过日志出锁后再写，避免 history 锁重入死锁
    let mut skipped: Vec<(String, String)> = Vec::new();
    {
        let mut hist = state.history.lock();
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
                    hist = state.history.lock();
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
            let mut q = state.queue.lock();
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
    cancel: &Arc<AtomicBool>,
) {
    let url = item.url.clone().unwrap_or_default();
    if url.is_empty() {
        return;
    }
    match probe::list_playlist_entries(resolver, &url, cookies, network, Some(cancel), |l| {
        log_item(app, id, l)
    }) {
        Ok(entries) => {
            let n = entries.len();
            log_item(app, id, format!("播放列表展开：{n} 集"));
            let mut ids = Vec::with_capacity(entries.len());
            {
                let st = app.state::<AppState>();
                let mut hist = st.history.lock();
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
                    // 逐集顺序解析（每个解析都受全局解析闸门约束，不会因合集过大
                    // 而瞬间起等量子进程）
                    let _slot = ProbeSlot::acquire();
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
    // 并发额度守卫（见 SlotGuard 注释）
    let _slot = SlotGuard {
        app: app.clone(),
        id: id.clone(),
    };
    // 锁纪律（§11.15）：history 锁内只 clone 条目，config/cli/default_output_dir
    // 一律出锁后再取（P1-3：持 history 锁期间嵌套取 config/cli 锁会让 update_item 卡顿）。
    //
    // 同一把锁更不能重入：`parking_lot::Mutex` 不可重入，守卫还活着时调用
    // `update_item` / `persist`（内部都要再取 history 锁）会当场自锁 —— 任务线程
    // 永久持锁 → 此后所有列表操作一起冻死，并发额度也随 SlotGuard 永不归还而少一格。
    // 因此把「取数据」与「改数据」分到两个作用域：守卫在块结束时必定释放。
    let loaded = {
        let hist = state.history.lock();
        hist.get(&id).map(|i| (i.clone(), i.path.clone()))
    };
    let (item, path) = match loaded {
        None => {
            // 条目在排队期间被删除：清取消标志即可（slot 由守卫释放）
            state.cancels.lock().remove(&id);
            return;
        }
        Some((item, Some(p))) => (item, std::path::PathBuf::from(p)),
        Some((_, None)) => {
            // 无本地文件（start_transcode 已校验过，此处为兜底）：判失败并释放，
            // 不能把条目永远留在 Transcoding 状态。此处 history 锁已释放，
            // update_item / persist 可以安全调用。
            update_item(&app, &id, |it| {
                transition_in(it, Status::Failed);
                it.error = Some("无本地输入文件".into());
            });
            state.cancels.lock().remove(&id);
            persist(&app);
            return;
        }
    };
    let cfg = state.config.lock().clone();
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
    if meta.audio_tracks.unwrap_or(0) > 1 {
        // 转码固定 `-map 0:a:0?`（只保留第一条音轨）：多音轨是静默丢数据的场景，
        // 不能让它无声发生 —— 用户至少要在日志里看到这件事。
        log_item(
            &app,
            &id,
            format!(
                "源含 {} 条音轨，转码只保留第 1 条（其余音轨不进入产物）",
                meta.audio_tracks.unwrap_or(0)
            ),
        );
    }
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
            update_progress(
                &app2,
                ProgressPayload {
                    id: id2.clone(),
                    percent: Some(pct),
                    speed: None,
                    eta: None,
                    file: None,
                },
            );
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
        // 探测失败不再静默吞掉：产物元数据（画质列）会空着，必须让用户看到原因
        let meta = match probe_output(&state.resolver(), out, |l| log_item(app, id, l)) {
            Ok(m) => m,
            Err(e) => {
                log_item(app, id, format!("产物解析失败（画质列为空）：{e}"));
                Default::default()
            }
        };
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
        state.history.lock().upsert(prod.clone());
        // 产物缩略图：从最终产物抽帧（与下载完成兜底同链路）
        {
            let app2 = app.clone();
            let pid = prod.id.clone();
            let cache_dir = state.paths.cache_dir();
            let resolver2 = state.resolver();
            let out2 = out.clone();
            let cover_idx = prod.meta.cover_stream_index;
            // spawn_blocking：内部是 ffmpeg 子进程（阻塞）
            tauri::async_runtime::spawn_blocking(move || {
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
    // 清理取消标志（并发 slot 由调用方的 SlotGuard 释放），启动下一个等待任务
    state.cancels.lock().remove(id);
    persist(app);
}

/// 转码结束后原条目恢复状态：本地文件 → 已就绪；下载产物/转码产物 → 已完成（可再转码）。
fn restore_status(app: &AppHandle, id: &str) -> Status {
    let state = app.state::<AppState>();
    let hist = state.history.lock();
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
            let h = st.history.lock();
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

#[tauri::command(async)]
pub fn cancel_item(app: AppHandle, win: tauri::WebviewWindow, id: String) -> CmdResult<()> {
    ensure_main_window(&win)?;
    let state = app.state::<AppState>();

    // 合并作业要整批处理：一次合并只把**锚点条目**提交给队列，其余参与条目既不在
    // 等待队列也不在运行集合里。单条取消若不扩成整批，那些条目会永久停在"合并中"
    // （既没有执行体会碰它们，也不是终态、retry 不接受），merge_jobs 里的作业也无人清理。
    let merge_job = state.merge_jobs.lock().get(&id).cloned();
    if let Some(job) = merge_job {
        let anchor = job
            .ids
            .first()
            .cloned()
            .unwrap_or_else(|| id.clone());
        if let Some(flag) = state.cancel_flag(&anchor) {
            // 运行中：置标志即可，进程树终止与状态收尾由合并任务线程负责
            flag.store(true, Ordering::Relaxed);
            for jid in &job.ids {
                update_item(&app, jid, |it| {
                    it.push_log("正在取消合并…".to_string());
                });
            }
            persist(&app);
            return Ok(());
        }
        // 排队中：锚点移出等待队列，整批置为已取消（终态，可重试）
        {
            let mut q = state.queue.lock();
            q.cancel_waiting(&anchor);
        }
        for jid in &job.ids {
            update_item(&app, jid, |it| {
                transition_in(it, Status::Canceled);
                it.push_log("合并已取消（排队中移除）".to_string());
            });
        }
        {
            let mut jobs = state.merge_jobs.lock();
            for jid in &job.ids {
                jobs.remove(jid);
            }
        }
        persist(&app);
        return Ok(());
    }

    // 等待中：直接移除
    let removed = {
        let mut q = state.queue.lock();
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
    // 运行中（含解析中）：置取消标志，进程树由任务线程/看门狗终止
    // 用 register_cancel（复用已有 flag / 预建新 flag）：任务线程可能还没执行到
    // register_cancel（"已排队/刚启动"窗口），此时若只查不建就会丢失取消意图。
    // 任务线程随后 entry().or_insert_with 复用同一 flag，取消不会因注册顺序蒸发。
    let flag = state.register_cancel(&id);
    flag.store(true, Ordering::Relaxed);
    update_item(&app, &id, |it| {
        transition_in(it, Status::Canceled);
        it.push_log("正在取消…".to_string());
    });
    persist(&app);
    Ok(())
}

#[tauri::command(async)]
pub fn remove_item(app: AppHandle, win: tauri::WebviewWindow, id: String) -> CmdResult<()> {
    ensure_main_window(&win)?;
    let state = app.state::<AppState>();
    // 运行中的条目不允许直接删：任务线程还在跑、产物还在往输出目录写，
    // 删掉条目只会让用户以为"已经删干净了"。先取消、等状态落到终态再删。
    {
        let hist = state.history.lock();
        match hist.get(&id) {
            Some(it) if it.status.is_processing() || it.status == Status::Probing => {
                return Err(format!("任务正在{}，请先取消再删除", it.status.label()));
            }
            Some(_) => {}
            None => return Err("条目不存在".into()),
        }
    }
    // 属于进行中合并作业的条目同样不能单独删（整批是一个执行体）
    if state.merge_jobs.lock().contains_key(&id) {
        return Err("该条目属于一个进行中的合并作业，请先取消合并".into());
    }
    if state.history.lock().remove(&id) {
        // 顺带清缩略图缓存（条目已不存在，图留着只会无限累积）
        ytdlp_core::thumbs::remove_thumb(&state.paths.cache_dir(), &id);
        persist(&app);
        // 载荷统一为对象，与其余 item:* 事件保持一致
        let _ = app.emit("item:removed", serde_json::json!({ "id": id }));
        Ok(())
    } else {
        Err("条目不存在".into())
    }
}

#[tauri::command(async)]
pub fn clear_done(app: AppHandle) -> CmdResult<()> {
    let state = app.state::<AppState>();
    let removed: Vec<String> = {
        let mut hist = state.history.lock();
        let ids: Vec<String> = hist
            .items
            .iter()
            .filter(|i| i.status.is_terminal())
            .map(|i| i.id.clone())
            .collect();
        hist.clear_terminal();
        ids
    };
    // 缩略图缓存同步清理（否则 config/cache/thumbs/ 随使用时长无限增长）
    for id in &removed {
        ytdlp_core::thumbs::remove_thumb(&state.paths.cache_dir(), id);
    }
    persist(&app);
    let _ = app.emit("list:changed", ());
    Ok(())
}

#[tauri::command(async)]
pub fn retry_item(app: AppHandle, id: String) -> CmdResult<()> {
    let state = app.state::<AppState>();
    {
        let mut hist = state.history.lock();
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
        let hist = state.history.lock();
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
    Ok(state.config.lock().clone())
}

#[tauri::command(async)]
pub fn save_config(app: AppHandle, win: tauri::WebviewWindow, mut config: AppConfig) -> CmdResult<()> {
    ensure_main_window(&win)?;
    let state = app.state::<AppState>();
    // 后端必须自己校验：前端只有 UI 层限制（number 输入的 min/max 拦不住手输/粘贴），
    // 越界值会一路流到任务线程里（例如负的 max_gain_db 会让 f32::clamp 直接 panic，
    // 而 panic 发生在任务线程 → 条目卡死 + 并发额度永久少一格）
    config.sanitize();
    {
        let mut cur = state.config.lock();
        *cur = config.clone();
    }
    let path = state.paths.config_file();
    let _ = std::fs::create_dir_all(state.paths.config_dir());
    config.save(&path).map_err(err_string)?;
    // 并发上限即时生效：调高并发时若有任务在排队，立刻放行（A4）。
    // 返回本次提升释放出的、应马上启动的等待任务 id 列表。
    let to_launch = state
        .queue
        .lock()
        .set_concurrency(config.general.concurrency as usize);
    // 历史上限即时生效（超出部分在下一次 upsert 时按"最旧终态优先"裁剪）
    state
        .history
        .lock()
        .set_limit(config.general.history_limit);
    // 必须在释放队列锁之后再逐个启动：launch_next 会读 history + queue 的锁
    for id in to_launch {
        launch_next(&app, id);
    }
    Ok(())
}

// ---------- Cookie ----------

#[tauri::command(async)]
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

#[tauri::command(async)]
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

#[tauri::command(async)]
pub fn delete_cookie(app: AppHandle, win: tauri::WebviewWindow, host: String) -> CmdResult<()> {
    ensure_main_window(&win)?;
    let state = app.state::<AppState>();
    // 与 save_cookies 同口径：库文件存的是规范化后的 host（www.youtube.com.txt），
    // 传站点级域名（youtube.com）来删也必须命中 —— 否则 delete_host 见文件不存在
    // 直接返回 Ok，用户以为删掉了、实际还在，下次下载仍带着旧 Cookie。
    let normalized = normalize_cookie_host(&host);
    let store = CookieStore::new(state.paths.cookies_dir());
    store.delete_host(&normalized).map_err(err_string)?;
    let _ = app.emit("cookies:changed", normalized);
    Ok(())
}

// ---------- 依赖自检 ----------

#[tauri::command]
pub async fn probe_dependencies(app: AppHandle) -> CmdResult<Vec<ToolStatus>> {
    let state = app.state::<AppState>();
    let resolver = state.resolver();
    // 每个工具要起子进程取版本（yt-dlp --version / ffmpeg -version …，秒级 × 4），
    // 放 blocking 线程，避免占住 async worker（A6）。
    tauri::async_runtime::spawn_blocking(move || {
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
    })
    .await
    .map_err(|e| format!("依赖自检任务异常：{e}"))?
}

// ---------- 其他 ----------

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
/// - `merge_<uuid>/`、`norm_<uuid>.mp4`：进行中的合并与音量归一化
/// - `tool_dl/`：进行中的工具下载
/// - `ytdlp-*.txt`：进行中的下载产物定位文件（下载中）
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
    let mut removed = 0usize;
    for e in std::fs::read_dir(&dir)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        let skip = active.contains(&name)
            || (tool_dl_busy && name == "tool_dl")
            || (busy && name.starts_with("merge_"))
            || (busy && name.starts_with("norm_"))
            || (busy && name.starts_with("ytdlp-"));
        if skip {
            continue;
        }
        let path = e.path();
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

/// 设置条目旋转角度（UL-12：随条目保存，转码时生效；M2 使用）。
#[tauri::command(async)]
pub fn rot_item(app: AppHandle, id: String, degrees: u16) -> CmdResult<()> {
    let angle = ytdlp_core::model::RotAngle::from_degrees(degrees);
    update_item(&app, &id, |it| {
        it.rot_angle = angle;
    });
    // "随条目保存"必须落盘，否则重启后丢失
    persist(&app);
    Ok(())
}

// ---------- 检查更新（轻量"检查+通知"，不自动下载安装） ----------

/// 检查更新结果：有新版时返回，无新版返回 None。
#[derive(serde::Serialize)]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: String,
    pub url: String,
}

/// 检查更新：请求 GitHub API 获取最新 release tag，与当前版本比较。
///
/// - 配置 `general.check_update` 为 false 时直接返回 None；
/// - 用系统 curl 发请求（与 tool_download 一致，避免 TLS 交叉编译问题）；
/// - 失败时返回错误，前端静默忽略（检查更新是可选增强，不应阻塞启动）。
#[tauri::command(async)]
pub fn check_update(app: AppHandle) -> CmdResult<Option<UpdateInfo>> {
    let state = app.state::<AppState>();
    if !state.config.lock().general.check_update {
        return Ok(None);
    }
    let current = env!("CARGO_PKG_VERSION").to_string();
    // GitHub API：未认证 60 次/小时，桌面应用启动频率足够
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
        api_url,
    ]);
    // Windows 下隐藏 curl 控制台窗口，否则启动检查更新时会闪一个黑框
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
