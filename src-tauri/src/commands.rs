//! Tauri 命令层：统一列表 CRUD + 解析/下载/取消/删除 + 配置 + Cookie + 依赖自检。
//!
//! **模块布局**：本文件只保留共享基建（锁/守卫/事件/持久化）与列表、解析、
//! 登录域；下载/转码/合并三条任务链、工具链、Cookie、设置、杂项按域拆在
//! `commands/` 子模块。命令经 lib.rs 的 `generate_handler!` 用**定义模块路径**
//! 注册（`#[tauri::command]` 生成的 `__cmd__X` 伴随项位于定义模块，不随
//! `pub use` 再导出）；被 Rust 内部按 `commands::X` 引用的非命令项
//! （`save_cookies_inner` / `MergeJob` / `clear_temp_inner`）在本文件再导出。
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
//! **阻塞调用分流（A6）**：`(async)` 只保证命令不跑在主线程。短 IO（一次 persist 的
//! fsync、几百字节配置写）在 `(async)` 命令体内直接执行即可——add_url /
//! start_download / begin_transcode / cancel_item / save_config 等均此口径；
//! **分钟级**阻塞（子进程、目录递归、大 JSON 序列化、persist 整份 history）
//! 必须再包一层 `spawn_blocking`，避免占住执行池拖慢同池命令。已收敛的命令见
//! `probe_dependencies`、`probe_hw_encoders`、`clear_temp`、`download_tool`、
//! `add_local`、三处缩略图。
//!
//! 常见错误：给同步 `pub fn` 加了 `#[tauri::command(async)]` 却又在函数体里 `.await`
//! （`(async)` 不会把函数变成 async），会直接 E0728 编译失败。

pub mod cookies;
pub mod download_task;
pub mod merge_task;
pub mod misc;
pub mod settings;
pub mod tools;
pub mod transcode_task;

// 命令本体经 lib.rs 的 generate_handler! 用**定义模块路径**注册：
// #[tauri::command] 生成的 __cmd__X 隐藏伴随项位于定义模块，pub use 再导出
// 不会带上它们。这里只再导出被 Rust 内部按 `commands::X` 路径引用的非命令项：
// - save_cookies_inner：login.rs 登录完成回调直接调用（不走 IPC）
// - MergeJob：state.rs 合并作业注册表的类型
// - clear_temp_inner：lib.rs setup 的启动清理
pub(crate) use cookies::save_cookies_inner;
pub use merge_task::MergeJob;
pub use misc::clear_temp_inner;

use crate::login;
use crate::state::AppState;
use download_task::run_download_task;
use merge_task::run_merge_task;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use tauri::{AppHandle, Emitter, Manager, State};
use transcode_task::run_transcode_task;
use ytdlp_core::cookies::CookieStore;
use ytdlp_core::download;
use ytdlp_core::exec::ToolResolver;
use ytdlp_core::model::{ItemKind, MediaItem, Status};
use ytdlp_core::probe::{self, ProbeErrorKind};
use ytdlp_core::worker::SubmitOutcome;
use ytdlp_core::{transition, CoreError};

/// 解析（探测）并发上限：粘贴 100 条链接时不能瞬间起 100 个 yt-dlp 子进程
/// ——打满本机资源，也必然触发站点风控。播放列表展开（DL-09）本就是顺序解析，
/// 这里把"多条 URL 同时入列"收进同一条约束。
const PROBE_CONCURRENCY: u64 = 3;

/// A1：敏感命令的调用方守卫。自定义命令不受 capability 的 `windows` 白名单约束，
/// 任何窗口/webview（含登录窗里承载的第三方页面）都能 `invoke`。当前 capability 未声明
/// `remote.urls`（登录窗加载的是远程站点，本就不能用 IPC），但为防未来误加 remote / 误建
/// 可执行窗口，给真正敏感的命令加一层"仅主窗口可调用"的硬校验。
///
/// 只对需要从主窗口触发的命令用；登录流程的 cookie 保存由 login.rs 直接调用
/// `save_cookies_inner`（不走 IPC），不经过这层守卫。
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

/// 解析失败收尾（URL 解析失败与播放列表展开失败共用）：
/// 取消 → 已取消；需要登录 → 需登录；其余 → 失败。
fn handle_probe_failure(
    app: &AppHandle,
    id: &str,
    f: &probe::ProbeFailure,
    log_prefix: &str,
) {
    let status = match f.kind {
        ProbeErrorKind::NeedLogin => Status::NeedLogin,
        ProbeErrorKind::Cancelled => Status::Canceled,
        _ => Status::Failed,
    };
    update_item(app, id, |it| {
        transition_in(it, status);
        if f.kind != ProbeErrorKind::Cancelled {
            it.error = Some(f.to_string());
        }
        // 多行错误逐行落日志：单条塞多行在日志弹窗里会被错误截断
        let s = f.to_string();
        let mut lines = s.lines();
        if let Some(first) = lines.next() {
            it.push_log(format!("{log_prefix}：{first}"));
        }
        for l in lines {
            if !l.trim().is_empty() {
                it.push_log(l.to_string());
            }
        }
    });
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

/// 用 yt-dlp 拉远程缩略图（解析链路取封面的唯一路径）。
///
/// 不走 curl：curl 不走 Windows 系统代理（WinINET），且 schannel + socks5 兼容性差
/// （经常 server closed abruptly）；yt-dlp 与主下载共用同一套代理/TLS（openssl），
/// 行为最一致。`--write-thumbnail --skip-download` 只拉封面不下载视频。
///
/// **Cookie**：按视频页面 host 从站点 Cookie 库导出到本任务临时目录，随 tmp 清理。
/// YouTube 对无 Cookie 的缩略图请求会触发 bot 校验（"Sign in to confirm you're
/// not a bot"），因此缩略图与解析/下载同口径带 Cookie；库文件本身只读，导出文件
/// 用完即删，不会污染 `config/cookies/`。
// 参数较多是封面链路的固有形态（路径/代理/Cookie 来源/日志），按项目惯例放行。
#[allow(clippy::too_many_arguments)]
fn fetch_thumb_ytdlp(
    resolver: &ToolResolver,
    video_url: &str,
    dest: &Path,
    temp_root: &Path,
    proxy: Option<&str>,
    cookies_host: Option<&str>,
    cookies_dir: &Path,
    on_log: &mut dyn FnMut(String),
) -> Result<(), String> {
    // 运行时文件必须落 <exe 同级>\temp\（§3.7），禁止系统 temp；
    // 目录名带纳秒时间戳，避免同进程内多次调用撞名
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp_dir = temp_root.join(format!("thumb-{ts}"));
    std::fs::create_dir_all(&tmp_dir).map_err(|e| format!("创建临时目录失败：{e}"))?;
    // Cookie 导出到任务私有目录：yt-dlp 退出时可能回写该文件，tmp 删除即可，
    // 不触碰 config/cookies/ 的站点库文件。
    let cookies_file = match cookies_host {
        Some(host) => match CookieStore::new(cookies_dir)
            .export_for_task(host, &tmp_dir.join("cookies.txt"))
        {
            Ok(p) => p,
            Err(e) => {
                on_log(format!("缩略图 Cookie 导出失败（改为不带 Cookie）：{e}"));
                None
            }
        },
        None => None,
    };
    let out_tmpl = tmp_dir.join("%(id)s.%(ext)s");
    let out_tmpl_str = out_tmpl.to_string_lossy().into_owned();
    let mut args: Vec<String> = vec![
        // 忽略用户级/便携级配置，避免 -o/--paths 被覆盖（与主下载链路同口径）
        "--ignore-config".into(),
        "--write-thumbnail".into(),
        "--skip-download".into(),
        "--no-warnings".into(),
        "--no-playlist".into(),
        "--socket-timeout".into(),
        "30".into(),
    ];
    if let Some(p) = proxy {
        if !p.is_empty() {
            args.push("--proxy".into());
            args.push(p.to_string());
        }
    }
    if let Some(cf) = &cookies_file {
        args.push("--cookies".into());
        args.push(cf.to_string_lossy().into_owned());
    }
    if let Ok(deno) = resolver.resolve(ytdlp_core::exec::Tool::Deno) {
        args.push("--js-runtimes".into());
        args.push(format!("deno:{}", deno.to_string_lossy()));
    }
    if let Ok(ffmpeg) = resolver.resolve(ytdlp_core::exec::Tool::Ffmpeg) {
        args.push("--ffmpeg-location".into());
        args.push(ffmpeg.to_string_lossy().into_owned());
        // dest 固定 .jpg：让 yt-dlp 转成 jpg，避免 webp/png 内容配 .jpg 扩展名
        args.push("--convert-thumbnails".into());
        args.push("jpg".into());
    }
    args.extend(ytdlp_core::exec::browser_headers());
    args.push("-o".into());
    args.push(out_tmpl_str);
    args.push(video_url.to_string());
    on_log(ytdlp_core::exec::display_command("yt-dlp", &args));
    let mut cmd = match resolver.command(ytdlp_core::exec::Tool::YtDlp) {
        Ok(c) => c,
        Err(e) => {
            // 已导出的 Cookie 文件必须随临时目录一起清掉
            let _ = std::fs::remove_dir_all(&tmp_dir);
            return Err(e.to_string());
        }
    };
    cmd.args(&args);
    // 总超时兜底：--socket-timeout 只管读超时，管不了"连上之后不产数据"的挂起；
    // 与全项目其它子进程调用同口径（60s 量级）。run_capture_deadline 只在退出码
    // 为 0 时返回 Ok，非零退出的 stderr 由它带进错误信息，无需再手动判退出码。
    match ytdlp_core::exec::run_capture_deadline(cmd, std::time::Duration::from_secs(60)) {
        Ok(_) => {}
        Err(CoreError::ProcessFailed { stderr, .. }) => {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            let detail = stderr.trim();
            return Err(if detail.is_empty() {
                "yt-dlp 缩略图下载失败".to_string()
            } else {
                format!("yt-dlp 缩略图下载失败：{detail}")
            });
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            return Err(format!("yt-dlp 缩略图下载失败：{e}"));
        }
    }
    // 封面扩展名可能是 webp/jpg/jpeg/png/avif（取决于源与转换结果）。
    // 只按扩展名白名单挑：目录里可能还躺着 .part/点文件等其它条目，而 read_dir
    // 顺序不定 —— "随便取第一个"会把非封面文件复制过去。
    let thumb_file = std::fs::read_dir(&tmp_dir).ok().and_then(|rd| {
        let mut images: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && matches!(
                        p.extension()
                            .and_then(|e| e.to_str())
                            .map(str::to_ascii_lowercase)
                            .as_deref(),
                        Some("webp" | "jpg" | "jpeg" | "png" | "avif")
                    )
            })
            .collect();
        // dest 固定 .jpg：优先 jpg/jpeg（转换后的），避免复制转换前的 webp/png
        images.sort_by_key(|p| {
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
            !ext.eq_ignore_ascii_case("jpg") && !ext.eq_ignore_ascii_case("jpeg")
        });
        images.into_iter().next()
    });
    let Some(thumb_file) = thumb_file else {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Err("yt-dlp 未生成缩略图文件".into());
    };
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    // 复制失败（磁盘满/权限）也必须清掉临时目录，否则 temp/thumb-<ts>/ 会残留
    if let Err(e) = std::fs::copy(&thumb_file, dest) {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Err(format!("复制缩略图失败：{e}"));
    }
    let _ = std::fs::remove_dir_all(&tmp_dir);
    Ok(())
}

/// persist 去抖窗口：500ms 内的多次变更合并为一次写盘。
///
/// P1-5：批量转码 50 条 = 每条收尾调一次 persist，此前是 50 次全量
/// clone + 序列化 + fsync。去抖后收敛为 ~1 次写盘。
const PERSIST_DEBOUNCE_MS: u64 = 500;

/// 持久化（状态迁移后调用）。
///
/// **永远异步写盘**：命令线程不能被同步 IO 阻塞——history.json 随条目数增长
/// 可达数 MB，机械硬盘上同步写能卡 1-2 秒，用户点"取消/重试"会明显感觉延迟。
/// state.persist() 内部有序列号去重（persist_req/persist_done），并发写安全。
///
/// 去抖：距上次写盘 >= 500ms 立即 spawn 写；< 500ms 则 spawn 延迟任务，
/// 500ms 后合并写。`pending_flush` 保证同一窗口内只有一个延迟任务。
fn persist(app: &AppHandle) {
    let state = app.state::<AppState>();
    if state.ms_since_last_persist() >= PERSIST_DEBOUNCE_MS {
        let app = app.clone();
        std::thread::spawn(move || {
            app.state::<AppState>().persist();
        });
        return;
    }
    // 去抖窗口内：确保有且仅有一个延迟 flush 任务
    if !state.pending_flush().swap(true, Ordering::SeqCst) {
        let app = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(PERSIST_DEBOUNCE_MS));
            let state = app.state::<AppState>();
            state.pending_flush().store(false, Ordering::SeqCst);
            state.persist();
        });
    }
}

/// 条目缩略图（下载/转码/合并产物共用）：后台抽帧/内嵌封面，成功后回填并落盘。
///
/// `log_id`：缩略图命令回显与失败原因写入哪条目的日志。
pub(super) fn spawn_product_thumb(
    app: &AppHandle,
    product_id: String,
    out: PathBuf,
    cover_idx: Option<u32>,
    log_id: String,
) {
    let app2 = app.clone();
    let state = app.state::<AppState>();
    let cache_dir = state.paths.cache_dir();
    let resolver = state.resolver();
    tauri::async_runtime::spawn_blocking(move || {
        let dest = ytdlp_core::thumbs::thumb_path(&cache_dir, &product_id);
        // 已有缩略图（下载条目在解析阶段生成过）不重复抽帧；也不做无意义的落盘
        if dest.is_file() {
            return;
        }
        if ytdlp_core::thumbs::ensure_thumb(
            &resolver,
            &out,
            &dest,
            cover_idx.map(|i| i as usize),
            &mut |l| log_item(&app2, &log_id, l),
        )
        .is_ok()
        {
            update_item(&app2, &product_id, |it| {
                it.thumb = Some(dest.to_string_lossy().into_owned());
            });
            app2.state::<AppState>().persist();
        }
    });
}

/// 产物条目回列表（转码/合并共用）：探测元数据、建条目、抽缩略图、通知前端。
///
/// `log_id`：日志归属条目（产物解析命令回显到发起任务的条目日志里）。
pub(super) fn add_product_item(
    app: &AppHandle,
    kind: ItemKind,
    out: &Path,
    fallback_title: &str,
    log_id: &str,
) {
    let state = app.state::<AppState>();
    let meta = match ytdlp_core::download::probe_output(&state.resolver(), out, |l| {
        log_item(app, log_id, l)
    }) {
        Ok(m) => m,
        Err(e) => {
            // 探测失败不再静默吞掉：产物元数据（画质列）会空着，必须让用户看到原因
            log_item(app, log_id, format!("产物解析失败（画质列为空）：{e}"));
            Default::default()
        }
    };
    let title = out
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| fallback_title.to_string());
    let mut prod = MediaItem::new(kind, title);
    prod.path = Some(out.to_string_lossy().into_owned());
    prod.status = Status::Done;
    prod.percent = 100.0;
    prod.meta = meta;
    prod.updated_at = now_str();
    state.history.lock().upsert(prod.clone());
    // 产物缩略图：从最终产物抽帧（与下载完成兜底同链路）
    spawn_product_thumb(
        app,
        prod.id.clone(),
        out.to_path_buf(),
        prod.meta.cover_stream_index,
        prod.id.clone(),
    );
    let _ = app.emit("list:changed", ());
}

/// 批量预检结果：(接受的条目, 跳过的条目(id, 原因))。
type BatchOutcome = Result<(Vec<MediaItem>, Vec<(String, String)>), String>;

/// 批量任务预检 + 状态迁移（转码/合并共用）。
///
/// 两段式：先确认所有 id 都存在（任一缺失直接报错，避免前 N-1 个已被改成
/// 处理中却没人执行），再逐条跑 `check` 并做 transition；被跳过的条目带原因返回。
/// 调用方负责在出锁后 emit `item:update` / 写跳过日志。
pub(super) fn prepare_batch(
    hist: &mut ytdlp_core::history::History,
    ids: &[String],
    target: Status,
    skip_prefix: &str,
    mut check: impl FnMut(&MediaItem) -> Result<(), String>,
) -> BatchOutcome {
    let mut items = Vec::with_capacity(ids.len());
    for id in ids {
        let Some(item) = hist.get(id).cloned() else {
            return Err(format!("条目不存在：{id}"));
        };
        items.push((id.clone(), item));
    }
    let mut accepted = Vec::new();
    let mut skipped = Vec::new();
    for (id, item) in items {
        if let Err(msg) = check(&item) {
            skipped.push((id, format!("{skip_prefix}：{msg}")));
            continue;
        }
        match transition(item.status, target) {
            Ok(to) => {
                // 进入新任务前进度归零：否则上一轮下载/转码的 100% 会一直挂在进度列
                let updated = MediaItem {
                    status: to,
                    percent: 0.0,
                    ..item
                };
                hist.upsert(updated.clone());
                accepted.push(updated);
            }
            Err(e) => skipped.push((id, format!("{skip_prefix}：{e}"))),
        }
    }
    Ok((accepted, skipped))
}

// ---------- 添加与解析 ----------

/// 读取剪贴板文本。首页"添加"按钮在输入框为空时自动调用，
/// 把剪贴板里的 URL 直接添加，省去手动粘贴步骤。
///
/// 剪贴板被其它进程占用时 `OpenClipboard` 会阻塞数秒，同步命令跑在主线程会卡窗口，
/// 因此走 `spawn_blocking`（A6：不得在主线程做阻塞 IO）。
#[tauri::command]
pub async fn read_clipboard(win: tauri::WebviewWindow) -> CmdResult<String> {
    ensure_main_window(&win)?;
    tauri::async_runtime::spawn_blocking(|| -> CmdResult<String> {
        let mut clipboard =
            arboard::Clipboard::new().map_err(|e| format!("打开剪贴板失败：{e}"))?;
        Ok(clipboard.get_text().unwrap_or_default())
    })
    .await
    .map_err(|e| format!("读取剪贴板任务异常：{e}"))?
}

#[tauri::command(async)]
pub fn add_url(app: AppHandle, win: tauri::WebviewWindow, urls: Vec<String>) -> CmdResult<()> {
    // 敏感命令仅允许主窗口调用（与 start_download 等同口径）
    ensure_main_window(&win)?;
    add_url_inner(&app, urls)
}

/// 实际入列逻辑（IPC 命令与 CLI 单实例转发共用；不含窗口守卫）。
pub(crate) fn add_url_inner(app: &AppHandle, urls: Vec<String>) -> CmdResult<()> {
    let state = app.state::<AppState>();
    // 新条目继承"仅音频默认"（设置-下载）：出 history 锁之前先取，避免嵌套锁
    let default_audio_only = state.config.lock().download.audio_only;
    // 先在锁内收集 id，出锁后再起解析线程：否则新增线程会全部阻塞在 history 锁上
    let mut ids: Vec<String> = Vec::new();
    {
        let mut hist = state.history.lock();
        for raw in urls {
            let url = clean_url(&raw);
            // DL-01：只接受 http(s)://（UI 与 CLI 共用同一校验）
            if url.is_empty() || (!url.starts_with("http://") && !url.starts_with("https://")) {
                continue;
            }
            let mut item = MediaItem::from_url(url);
            item.audio_only = default_audio_only;
            ids.push(item.id.clone());
            hist.upsert(item);
        }
    }
    // 全部非法时给出明确反馈（此前静默丢弃，用户看不出为什么没反应）
    if ids.is_empty() {
        return Err("没有可添加的链接（仅支持 http/https 链接）".into());
    }
    // 解析线程不占并发 slot，但受解析闸门约束：一次粘贴 100 条不能瞬间起 100 个 yt-dlp
    for id in ids {
        let app2 = app.clone();
        std::thread::spawn(move || {
            let _slot = ProbeSlot::acquire();
            run_probe(app2, id, true);
        });
    }
    persist(app);
    let _ = app.emit("list:changed", ());
    Ok(())
}

#[tauri::command]
pub async fn add_local(
    app: AppHandle,
    win: tauri::WebviewWindow,
    paths: Vec<String>,
    recursive: bool,
) -> CmdResult<()> {
    ensure_main_window(&win)?;
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
    // 锁内收集 id，出锁后再起解析线程，避免新线程阻塞在 history 锁上
    let mut ids: Vec<String> = Vec::new();
    {
        let mut hist = state.history.lock();
        for f in files {
            let item = MediaItem::from_path(f.to_string_lossy().into_owned());
            ids.push(item.id.clone());
            hist.upsert(item);
        }
    }
    for id in ids {
        let app2 = app.clone();
        std::thread::spawn(move || {
            let _slot = ProbeSlot::acquire();
            run_probe(app2, id, true);
        });
    }
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
/// `auto_ready`：解析成功后是否发 `item:ready` 触发前端 5 秒自动下载。
/// 登录后刷新已就绪条目时传 false（用户可能还要挑格式）；
/// 新增/重试/播放列表展开条目传 true（与"解析完成默认自动下载"一致）。
fn run_probe(app: AppHandle, id: String, auto_ready: bool) {
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
    // 一次锁内读齐 network + download.playlist，避免多次加锁
    let (network, playlist_on) = {
        let cfg = state.config.lock();
        (cfg.network.clone(), cfg.download.playlist)
    };
    // 任务私有临时目录：cookie 导出落这里，任务结束整体清理
    let task_tmp = state.paths.task_temp_dir(&id);
    let _ = std::fs::create_dir_all(&task_tmp);
    let (netscape, cookie_warn) = resolve_cookies(&state, &item, &task_tmp);
    if let Some(w) = cookie_warn {
        log_item(&app, &id, w);
    }

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
                entries: Vec::new(),
                skipped_entries: 0,
            }
        })
    };

    match result {
        Ok(p) => {
            if p.skipped_entries > 0 {
                log_item(
                    &app,
                    &id,
                    format!(
                        "播放列表有 {} 条未提供完整 URL（flat 模式限制），已跳过",
                        p.skipped_entries
                    ),
                );
            }
            let url_src = item.url.is_some();
            // 播放列表（DL-09）：开启时把合集展开为逐集条目平铺进列表。
            // 父条目保持"解析中"直到展开结束（不触发倒计时、不生成封面），
            // 展开成功后从列表移除；失败则落到 NeedLogin/失败/已取消。
            // entries 来自本次 flat-playlist 解析结果，不再二次调用 yt-dlp。
            let playlist_task = url_src && playlist_on && p.is_playlist;
            if playlist_task {
                match expand_playlist(&app, &id, p.entries) {
                    Ok(n) => ytdlp_core::log::info(format!("播放列表 {id} 展开为 {n} 集")),
                    Err(f) => handle_probe_failure(&app, &id, &f, "播放列表展开失败"),
                }
            } else {
                let cover_idx = p.meta.cover_stream_index;
                // 缩略图 Cookie 的 host：优先本次解析出的页面 host，其次条目旧值，
                // 最后从 URL 兜底（解析成功前 item.host 可能还是 None）。
                let thumb_host = p
                    .host
                    .clone()
                    .or_else(|| item.host.clone())
                    .or_else(|| {
                        item.url
                            .as_deref()
                            .and_then(ytdlp_core::cookies::host_from_url)
                    });
                update_item(&app, &id, |it| {
                    // 先取标题再 move meta
                    let meta_title = p.meta.title.clone();
                    it.meta = p.meta;
                    it.site = p.site.clone();
                    if p.host.is_some() {
                        it.host = p.host.clone();
                    }
                    // 解析出视频标题后同步到条目标题（前端名称列显示）
                    if let Some(t) = meta_title {
                        it.title = t;
                    }
                    transition_in(it, Status::Ready);
                    it.percent = 0.0;
                    it.error = None;
                    it.push_log("解析完成，已就绪".to_string());
                });
                // 封面缩略图（异步生成，不阻塞就绪）。
                // spawn_blocking：里面是 yt-dlp / ffmpeg 子进程（阻塞），
                // 放在 async 执行器的 worker 上会占住线程（执行器线程数 ≈ CPU 核数）
                {
                    let app2 = app.clone();
                    let id2 = id.clone();
                    let cache_dir = state.paths.cache_dir();
                    let local_path = item.path.clone();
                    let video_url = item.url.clone();
                    // 本地文件优先取内嵌封面（元数据），与桌面缩略图同源
                    let resolver2 = resolver.clone();
                    // 缩略图代理按视频页面 URL 解析（而非缩略图 URL i.ytimg.com）——
                    // 用户勾的是 youtube.com，i.ytimg.com 不匹配；缩略图属于该视频，
                    // 应跟视频走同一代理。
                    let thumb_proxy = video_url.as_deref().and_then(|u| network.resolve_proxy(u));
                    // 缩略图与解析/下载同口径带站点 Cookie：YouTube 对无 Cookie 的
                    // 缩略图请求会触发 bot 校验。Cookie 由 fetch_thumb_ytdlp 导出到
                    // 任务临时目录，随 tmp 清理，不污染站点库。
                    let thumb_tmp_root = state.paths.temp_dir();
                    let cookies_dir = state.paths.cookies_dir();
                    tauri::async_runtime::spawn_blocking(move || {
                        let dest = ytdlp_core::thumbs::thumb_path(&cache_dir, &id2);
                        // URL 视频：直接用 yt-dlp --write-thumbnail --skip-download 下载缩略图。
                        // 与下载视频走同一套代理/cookies/TLS(openssl)，不走 curl（schannel
                        // + socks5 兼容性差，经常 server closed abruptly）。
                        // 本地文件：优先取内嵌封面（ffmpeg），无封面流才抽帧。
                        let r = if video_url.is_some() {
                            if let Some(vurl) = &video_url {
                                fetch_thumb_ytdlp(
                                    &resolver2,
                                    vurl,
                                    &dest,
                                    &thumb_tmp_root,
                                    thumb_proxy.as_deref(),
                                    thumb_host.as_deref(),
                                    &cookies_dir,
                                    &mut |l| log_item(&app2, &id2, l),
                                )
                            } else {
                                Ok(())
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
                        match r {
                            Ok(()) => {
                                // 缩略图下载是异步的：用户可能在解析完成后立即删除条目，
                                // 此时 update_item 返回 None、persist 做无意义全量序列化。
                                // 先确认条目仍存在再更新，避免批量删除时触发多次 persist。
                                let exists =
                                    app2.state::<AppState>().history.lock().get(&id2).is_some();
                                if exists {
                                    update_item(&app2, &id2, |it| {
                                        it.thumb = Some(dest.to_string_lossy().into_owned());
                                    });
                                    app2.state::<AppState>().persist();
                                }
                            }
                            Err(e) => {
                                log_item(&app2, &id2, format!("缩略图生成失败：{e}"));
                            }
                        }
                    });
                }
                // URL 已就绪：触发 5 秒倒计时自动下载（前端计时，后端只发可下载信号）
                if url_src && auto_ready {
                    let _ = app.emit("item:ready", serde_json::json!({ "id": id }));
                }
            }
        }
        Err(f) => {
            handle_probe_failure(&app, &id, &f, "解析失败");
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

/// 登录完成后的重解析：把该站点下 `NeedLogin`/`Ready` 的 URL 条目重新解析一遍。
///
/// 很多站点未登录也能解析出受限清晰度（B 站只给低码率），条目状态是 Ready
/// 而不是 NeedLogin；登录后若不重解析，格式列表仍是旧的低清晰度版本。
/// 这里按 host 的 Cookie 候选域匹配，清掉旧的手选格式并重新探测。
pub(crate) fn reprobe_items_for_host(app: &AppHandle, host: &str) {
    let state = app.state::<AppState>();
    let login_candidates = ytdlp_core::cookies::cookie_candidates(&host.to_lowercase());
    let mut updated: Vec<MediaItem> = Vec::new();
    let mut spawns: Vec<(String, bool)> = Vec::new();
    {
        let mut hist = state.history.lock();
        let candidates: Vec<MediaItem> = hist
            .items
            .iter()
            .filter(|it| {
                if it.kind != ItemKind::UrlTask
                    || !matches!(it.status, Status::NeedLogin | Status::Ready)
                {
                    return false;
                }
                let item_host = it
                    .host
                    .clone()
                    .or_else(|| it.url.as_deref().and_then(ytdlp_core::cookies::host_from_url));
                item_host
                    .map(|h| {
                        ytdlp_core::cookies::cookie_candidates(&h)
                            .iter()
                            .any(|c| login_candidates.contains(c))
                    })
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        for mut item in candidates {
            // NeedLogin 登录后解析成功可以延续"自动下载"习惯；
            // Ready 条目只是刷新格式，解析完不应自动开始下载。
            let auto_ready = item.status == Status::NeedLogin;
            let Ok(to) = transition(item.status, Status::Probing) else {
                continue;
            };
            item.status = to;
            item.error = None;
            item.percent = 0.0;
            // 登录后格式集合会变：旧的手选 format_id 可能已不存在
            item.format_id = None;
            item.push_log("登录完成，重新解析…".to_string());
            let id = item.id.clone();
            hist.upsert(item.clone());
            updated.push(item);
            spawns.push((id, auto_ready));
        }
    }
    if updated.is_empty() {
        return;
    }
    for it in &updated {
        let _ = app.emit("item:update", it);
    }
    let _ = app.emit("list:changed", ());
    ytdlp_core::log::info(format!(
        "登录完成：{host} 下 {} 条条目重新解析",
        updated.len()
    ));
    for (id, auto_ready) in spawns {
        let app2 = app.clone();
        std::thread::spawn(move || {
            let _slot = ProbeSlot::acquire();
            run_probe(app2, id, auto_ready);
        });
    }
    persist(app);
}

// ---------- 列表 ----------

/// 列表全量返回（含每条最多 300 行日志）。
/// `(async)`：克隆 + 序列化整份历史是"MB 级"操作，前端在活动任务期间每 1.5s
/// 轮询一次，留在主线程会周期性卡住窗口。
/// 轮询请用 [`list_items_lite`]：这一份只用于首次加载（日志弹窗需要历史）。
#[tauri::command(async)]
pub fn list_items(
    win: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> CmdResult<Vec<MediaItem>> {
    ensure_main_window(&win)?;
    let hist = state.history.lock();
    Ok(hist.items.clone())
}

/// 列表轻量快照：**不含**日志与下载格式列表。
///
/// 列表渲染只需要状态/进度/元数据；每条最多 300 行的日志（100 条时可达 MB 级）
/// 让每次轮询的 IPC 与 JSON 解析成本凭空翻几倍。日志有两条正式通路：
/// `item:log` 增量事件 + 打开弹窗时的 [`get_item_log`]。
///
/// `download_formats` 同理：YouTube 单条 40~60 个 format ≈ 6KB，200 条满列表
/// ≈ 1.2MB，每 1.5s 轮询一次就是 1MB IPC。它只被"格式选择"弹窗使用，
/// 打开时通过 [`get_item_formats`] 按需拉取。
#[tauri::command(async)]
pub fn list_items_lite(
    win: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> CmdResult<Vec<MediaItem>> {
    ensure_main_window(&win)?;
    let hist = state.history.lock();
    Ok(hist.items.iter().map(MediaItem::to_lite).collect())
}

/// 取某条目的下载格式列表（格式选择弹窗打开时按需拉取）。
///
/// `list_items_lite` 已清空 `download_formats` 以压缩轮询体积，弹窗打开时
/// 调此命令取回完整格式列表。
#[tauri::command(async)]
pub fn get_item_formats(
    win: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
) -> CmdResult<Vec<ytdlp_core::model::DownloadFormat>> {
    ensure_main_window(&win)?;
    let hist = state.history.lock();
    Ok(hist
        .get(&id)
        .map(|i| i.meta.download_formats.clone())
        .unwrap_or_default())
}

/// 取某条目的完整日志（打开日志弹窗时按需拉取的权威副本）。
#[tauri::command(async)]
pub fn get_item_log(
    app: AppHandle,
    win: tauri::WebviewWindow,
    id: String,
) -> CmdResult<Vec<String>> {
    ensure_main_window(&win)?;
    let state = app.state::<AppState>();
    let hist = state.history.lock();
    Ok(hist
        .get(&id)
        .map(|i| i.log.iter().cloned().collect())
        .unwrap_or_default())
}

// ---------- 动作 ----------

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

/// 任务入队/起线程的统一收口（下载/转码/合并共用）。
///
/// `Queued` 与 `AlreadyRunning` 都只记排队日志、不起新线程；真正拿到 slot 时才
/// 调用 `spawn`（调用方传入具体执行体），避免三处各写一遍 submit/日志/spawn。
pub(super) fn enqueue_or_spawn(
    app: &AppHandle,
    id: &str,
    queued_log: &str,
    start_log: &str,
    spawn: impl FnOnce(AppHandle, String) + Send + 'static,
) {
    let state = app.state::<AppState>();
    let outcome = {
        let mut q = state.queue.lock();
        q.submit(id)
    };
    if matches!(
        outcome,
        SubmitOutcome::Queued | SubmitOutcome::AlreadyRunning
    ) {
        log_item(app, id, queued_log);
        return;
    }
    log_item(app, id, start_log);
    let app2 = app.clone();
    let id2 = id.to_string();
    std::thread::spawn(move || spawn(app2, id2));
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

fn today_stamp() -> String {
    ytdlp_core::timefmt::date_stamp(
        ytdlp_core::timefmt::now_secs(),
        ytdlp_core::timefmt::local_offset_secs(),
    )
}

/// 解析 "HH:MM:SS" 为秒。空串由上层的"开放区间"语义处理；这里拒绝
/// 越界分钟/秒、超过 2 位的小时和非数字，避免把 99:99:99 直接传给 yt-dlp。
fn parse_section_time(t: &str) -> Option<u32> {
    let mut parts = t.split(':');
    let h_str = parts.next()?;
    let m_str = parts.next()?;
    let s_str = parts.next()?;
    if parts.next().is_some()
        || h_str.len() > 2
        || m_str.len() > 2
        || s_str.len() > 2
    {
        return None;
    }
    let h: u32 = h_str.parse().ok()?;
    let m: u32 = m_str.parse().ok()?;
    let s: u32 = s_str.parse().ok()?;
    if h > 99 || m > 59 || s > 59 {
        return None;
    }
    Some(h * 3600 + m * 60 + s)
}

/// 设置时间范围下载（DL-12）：起止 "HH:MM:SS"；空串清除。
#[tauri::command(async)]
pub fn set_sections(
    app: AppHandle,
    win: tauri::WebviewWindow,
    id: String,
    start: String,
    end: String,
) -> CmdResult<()> {
    ensure_main_window(&win)?;
    let err_fmt = "时间格式应为 HH:MM:SS（如 00:01:00）";
    let start_secs = if start.is_empty() {
        None
    } else {
        Some(parse_section_time(&start).ok_or(err_fmt)?)
    };
    let end_secs = if end.is_empty() {
        None
    } else {
        Some(parse_section_time(&end).ok_or(err_fmt)?)
    };
    if let (Some(s), Some(e)) = (start_secs, end_secs) {
        if e <= s {
            return Err("结束时间必须晚于开始时间".into());
        }
    }
    let sections = if start.is_empty() && end.is_empty() {
        None
    } else {
        Some((start, end))
    };
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
    // 走 500ms 去抖 + 后台线程落盘（与同类低频状态变更一致），
    // 不再同步阻塞命令线程做整份 history 序列化 + fsync。
    persist(&app);
    Ok(())
}

/// 播放列表展开（DL-09）：使用解析阶段已拿到的 flat-playlist 条目，
/// 逐条作为独立条目解析平铺（不再二次请求 yt-dlp）。
///
/// 逐集在单个后台线程里**顺序**解析：大合集（数百集）若每集各开线程，
/// 会瞬间并发起等量 yt-dlp 子进程，打满系统资源并容易触发站点风控；
/// 顺序解析每集就绪即回显（item:update），体验可接受。
fn expand_playlist(
    app: &AppHandle,
    id: &str,
    entries: Vec<probe::PlaylistEntry>,
) -> Result<usize, probe::ProbeFailure> {
    if entries.is_empty() {
        return Err(probe::ProbeFailure {
            kind: ProbeErrorKind::Failed,
            message: "播放列表为空或没有任何可下载的条目".into(),
        });
    }
    let n = entries.len();
    let mut ids = Vec::with_capacity(n);
    {
        let st = app.state::<AppState>();
        // 展开出的逐集条目继承"仅音频默认"（与 add_url 同口径）
        let default_audio_only = st.config.lock().download.audio_only;
        let mut hist = st.history.lock();
        for e in entries {
            let mut entry = MediaItem::from_url(e.url);
            entry.audio_only = default_audio_only;
            ids.push(entry.id.clone());
            hist.upsert(entry);
        }
        // 父条目只承担"展开播放列表"这一件事：成功后从列表移除，
        // 避免它被当成普通视频再次下载整个列表（双下载）。
        hist.remove(id);
    }
    ytdlp_core::thumbs::remove_thumb(&app.state::<AppState>().paths.cache_dir(), id);
    // 先移除父条目再发 list:changed：否则前端可能在刷新时把父条目又画回来
    let _ = app.emit("item:removed", serde_json::json!({ "id": id }));
    let _ = app.emit("list:changed", ());
    let app2 = app.clone();
    std::thread::spawn(move || {
        for eid in ids {
            // 逐集顺序解析（每个解析都受全局解析闸门约束，不会因合集过大
            // 而瞬间起等量子进程）
            let _slot = ProbeSlot::acquire();
            run_probe(app2.clone(), eid, true);
        }
    });
    Ok(n)
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

    // 取消即放弃任何待执行的放大意图：排队中被取消的放大任务永远不会进到
    // 任务线程消费 upscale_targets，不清掉就会让之后对同一 id 的普通转码被误放大
    state.upscale_targets.lock().remove(&id);

    // 合并作业要整批处理：一次合并只把**锚点条目**提交给队列，其余参与条目既不在
    // 等待队列也不在运行集合里。单条取消若不扩成整批，那些条目会永久停在"合并中"
    // （既没有执行体会碰它们，也不是终态、retry 不接受），merge_jobs 里的作业也无人清理。
    let merge_job = state.merge_jobs.lock().get(&id).cloned();
    if let Some(job) = merge_job {
        // 参与条目同样放弃待执行的放大意图（整批取消/执行都不经过各自的放大登记）
        {
            let mut set = state.upscale_targets.lock();
            for jid in &job.ids {
                set.remove(jid);
            }
        }
        let anchor = job.ids.first().cloned().unwrap_or_else(|| id.clone());
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
pub fn clear_done(app: AppHandle, win: tauri::WebviewWindow) -> CmdResult<()> {
    ensure_main_window(&win)?;
    let state = app.state::<AppState>();
    let removed: Vec<String> = {
        let mut hist = state.history.lock();
        // 清除所有非活跃条目（Ready/Done/Failed/Canceled/NeedLogin），
        // 保留解析中和处理中的。按钮叫"清空列表"，用户期望清掉本地添加的
        // Ready 条目，而不是只清终态。
        let ids: Vec<String> = hist
            .items
            .iter()
            .filter(|i| !i.status.is_active())
            .map(|i| i.id.clone())
            .collect();
        hist.clear_idle();
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
pub fn retry_item(app: AppHandle, win: tauri::WebviewWindow, id: String) -> CmdResult<()> {
    ensure_main_window(&win)?;
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
    std::thread::spawn(move || {
        // 与 add_url/add_local/expand_playlist 同口径：解析线程也受 ProbeSlot 闸门约束，
        // 否则批量"重试失败条目"会瞬间起等量 yt-dlp，突破 PROBE_CONCURRENCY。
        let _slot = ProbeSlot::acquire();
        run_probe(app2, id, true);
    });
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
pub async fn relogin_item(app: AppHandle, win: tauri::WebviewWindow, id: String) -> CmdResult<()> {
    ensure_main_window(&win)?;
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
///
/// `url` 为自定义站点的登录页完整 URL（如 `https://www.example.com/login`）；
/// 预设站点只传 host，由 `login_url_for_host` 映射到固定登录页。自定义站点
/// 传 url 时不再查硬编码列表，任何站点都能登录。
#[tauri::command]
pub async fn open_login_site(
    app: AppHandle,
    win: tauri::WebviewWindow,
    host: String,
    url: Option<String>,
) -> CmdResult<()> {
    ensure_main_window(&win)?;
    let login_url = match url {
        Some(u) if !u.trim().is_empty() => {
            let u = u.trim().to_string();
            // P1-4：自定义 URL 仅允许 https，防止登录窗被引导到 http/恶意 scheme
            let parsed =
                url::Url::parse(&u).map_err(|_| "自定义登录 URL 格式不正确".to_string())?;
            if parsed.scheme() != "https" {
                return Err("自定义登录 URL 仅支持 https".into());
            }
            u
        }
        _ => login::login_url_for_host(&host).ok_or_else(|| {
            format!("站点 {host} 不支持内置登录：请在下方的 Cookie 列表中直接导入")
        })?,
    };
    tauri::async_runtime::spawn_blocking(move || {
        login::open_login(&app, &host, &login_url).map_err(err_string)
    })
    .await
    .map_err(|e| format!("登录窗口启动失败：{e}"))??;
    Ok(())
}

// ---------- 配置 ----------

/// 设置条目旋转角度（UL-12：随条目保存，转码时生效；M2 使用）。
#[tauri::command(async)]
pub fn rot_item(
    app: AppHandle,
    win: tauri::WebviewWindow,
    id: String,
    degrees: u16,
) -> CmdResult<()> {
    ensure_main_window(&win)?;
    let angle = ytdlp_core::model::RotAngle::from_degrees(degrees);
    update_item(&app, &id, |it| {
        it.rot_angle = angle;
    });
    // "随条目保存"必须落盘，否则重启后丢失
    persist(&app);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_section_time;

    #[test]
    fn section_time_accepts_valid_ranges() {
        assert_eq!(parse_section_time("00:00:00"), Some(0));
        assert_eq!(parse_section_time("01:01:01"), Some(3661));
        assert_eq!(parse_section_time("99:59:59"), Some(359_999));
    }

    #[test]
    fn section_time_rejects_invalid_ranges() {
        for bad in [
            "100:00:00",
            "000:00:00",
            "00:60:00",
            "00:00:60",
            "00:00",
            "00:00:00:00",
            "aa:00:00",
            "",
        ] {
            assert_eq!(parse_section_time(bad), None, "{bad}");
        }
    }
}
