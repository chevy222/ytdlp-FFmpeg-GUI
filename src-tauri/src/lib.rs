//! Tauri 应用壳：全局状态 + 命令注册 + 目录约定（§3.7）。
//! 核心逻辑在 ytdlp-core；本层负责界面桥接与外部进程任务调度。

mod commands;
mod login;
#[cfg(windows)]
mod login_win;
mod state;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tauri::Manager;
use ytdlp_core::model::Status;

/// 退出流程只允许进入一次：`app.exit(0)` 会再次触发 `ExitRequested`，
/// 第二次不能再 `prevent_exit`，否则永远退不出去。
static EXITING: AtomicBool = AtomicBool::new(false);

/// 初始化落盘日志与 panic 钩子（GUI 无控制台，异常现场只能落盘）。
///
/// 必须在 `run()` 之前调用：`AppState::new()`（配置/历史加载）与任务线程的
/// 收尾告警都走这套日志。重复调用安全。
pub fn init_logging() {
    ytdlp_core::log::init(ytdlp_core::paths::Paths::from_exe().logs_dir());
    ytdlp_core::log::install_panic_hook();
}

/// 进程启动参数（OS 字符串 → 字符串：非 UTF-8 参数不再 panic）。
fn env_cli_args() -> Vec<String> {
    std::env::args_os()
        .skip(1)
        .map(|s| s.to_string_lossy().into_owned())
        .collect()
}

/// 将 CLI 参数应用到本实例：存 overrides（无论是否带 URL）并把 URL 投入列表解析。
/// `--dir`/`--cookies` 等覆盖单独出现时也必须生效，不能只在带 URL 时应用。
///
/// 注意覆盖项的存活范围：写入 `AppState.cli` 后**持续到进程结束**。单实例转发
/// 场景下，第二个实例传来的 `--dir` 会影响之后新建的所有任务（"本次调用"在
/// 单实例模式下没有天然的边界）；这是已知的语义边界，见 state::CliOverrides。
fn apply_cli_args(app: &tauri::AppHandle, args: Vec<String>) {
    let cli = ytdlp_core::cli::parse_cli_args(&args);
    let has_override = cli.dir.is_some()
        || cli.cookies.is_some()
        || cli.yt_dlp_path.is_some()
        || cli.deno_path.is_some();
    if !cli.urls.is_empty() || has_override {
        ytdlp_core::log::info(format!(
            "应用 CLI 参数：urls={} dir={:?} cookies={:?} yt_dlp={:?} deno={:?}",
            cli.urls.len(),
            cli.dir,
            cli.cookies,
            cli.yt_dlp_path,
            cli.deno_path
        ));
    }
    let st = app.state::<state::AppState>();
    if has_override {
        // 只覆盖本次给的字段（不把既有覆盖清空）：单实例转发时后一次调用
        // 不带 --dir 不应让先前的 --dir 失效
        let mut cur = st.cli.lock();
        let from = state::CliOverrides::from(cli.clone());
        if from.dir.is_some() {
            cur.dir = from.dir;
        }
        if from.cookies.is_some() {
            cur.cookies = from.cookies;
        }
        if from.yt_dlp_path.is_some() {
            cur.yt_dlp_path = from.yt_dlp_path;
        }
        if from.deno_path.is_some() {
            cur.deno_path = from.deno_path;
        }
    }
    if !cli.urls.is_empty() {
        // add_url 内部要原子写落盘（含 fsync）并为每条 URL 起解析线程，而本函数
        // 在 setup 与单实例回调里都由主线程执行 —— 放后台跑，避免启动/转发时
        // 出现一次同步磁盘写把窗口卡住。
        let app2 = app.clone();
        std::thread::spawn(move || {
            let _ = commands::add_url_inner(&app2, cli.urls);
        });
    }
}

/// 运行时目录创建失败时的阻断提示：继续跑下去会以各种费解的方式失败
/// （配置写不进去、临时文件建不出来），必须让用户当场知道原因。
fn fatal_dialog(title: &str, text: &str) {
    ytdlp_core::log::error(format!("{title}：{text}"));
    #[cfg(windows)]
    {
        use windows::core::HSTRING;
        use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
        let caption = HSTRING::from(title);
        let message = HSTRING::from(text);
        unsafe {
            let _ = MessageBoxW(None, &message, &caption, MB_OK | MB_ICONERROR);
        }
    }
    #[cfg(not(windows))]
    {
        eprintln!("{title}：{text}");
    }
}

pub fn run() {
    let app = match tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            // 第二实例：解析 argv 并把 URL/覆盖转发给已运行实例（§UL-09 单实例转发）。
            // 覆盖项单独出现（如仅 --dir）也要转发。
            let args: Vec<String> = argv.iter().skip(1).map(|s| s.to_string()).collect();
            apply_cli_args(app, args);
        }))
        .manage(state::AppState::new())
        .invoke_handler(tauri::generate_handler![
            commands::read_clipboard,
            commands::add_url,
            commands::add_local,
            commands::list_items,
            commands::list_items_lite,
            commands::get_item_log,
            commands::get_item_formats,
            // 拆到 commands/ 子模块的命令必须用**定义模块路径**注册：
            // #[tauri::command] 生成的 __cmd__X 伴随项在定义模块里，pub use
            // 再导出不会带上它们，generate_handler! 按路径前缀找不到。
            commands::download_task::start_download,
            commands::transcode_task::start_transcode,
            commands::transcode_task::start_upscale,
            commands::merge_task::start_merge,
            commands::cancel_item,
            commands::remove_item,
            commands::clear_done,
            commands::retry_item,
            commands::relogin_item,
            commands::open_login_site,
            commands::settings::get_config,
            commands::settings::save_config,
            commands::cookies::list_cookies,
            commands::cookies::delete_cookie,
            commands::tools::probe_dependencies,
            commands::tools::download_tool,
            commands::tools::cancel_tool_download,
            commands::tools::tool_urls,
            commands::tools::probe_hw_encoders,
            commands::misc::open_item_dir,
            commands::misc::clear_temp,
            commands::rot_item,
            commands::set_sections,
            commands::misc::queue_status,
            commands::misc::check_update,
            commands::misc::get_version_info,
            commands::misc::open_url,
        ])
        .setup(|app| {
            // 确保绿色便携目录结构（exe 同级）
            let state = app.state::<state::AppState>();
            if let Err(e) = state.paths.ensure_dirs() {
                fatal_dialog(
                    "影栈：无法创建运行时目录",
                    &format!(
                        "在程序目录下创建 config/ temp/ tools/ logs/ 失败：{e}\n\n\
                         请把程序放到可写目录（如 D:\\影栈\\）后重试，\n\
                         不要放在 Program Files 等需要管理员权限的位置。"
                    ),
                );
            }
            // 启动清理 temp/：上次崩溃/被强杀留下的任务目录、合并中间产物、工具
            // 下载半成品都不该跨实例累积（此时还没有任何任务在跑，豁免表为空，
            // 整目录都是可清的残留；判定与「清空临时文件」按钮共用同一份实现）。
            match commands::clear_temp_inner(&state) {
                Ok(n) if n > 0 => ytdlp_core::log::info(format!("启动清理 temp/：{n} 项残留")),
                Ok(_) => {}
                Err(e) => ytdlp_core::log::warn(format!("启动清理 temp/ 失败：{e}")),
            }
            // 预热本地时区偏移：否则首次创建条目会在"纯数据构造"里起一次 reg query
            ytdlp_core::timefmt::warm_up();
            // asset 协议只放行缩略图缓存目录：静态 scope（tauri.conf.json）表达不了
            // "exe 同级目录"这种运行时路径，这里按真实路径授权。
            // 前端只在缩略图 <img> 上使用 asset 协议，不再需要整盘可读。
            let thumbs = state.paths.cache_dir().join("thumbs");
            let _ = std::fs::create_dir_all(&thumbs);
            let _ = app.asset_protocol_scope().allow_directory(&thumbs, true);
            // CLI 启动参数（§UL-09）：首实例带 --url 等直接执行
            apply_cli_args(app.handle(), env_cli_args());
            // 启动时恢复队列（UL-10）：中断的任务标记失败，可重试。
            // NeedLogin 除外 —— 它不是"中断"，未登录的状态跨重启依然成立
            {
                let mut hist = state.history.lock();
                for item in hist.items.iter_mut() {
                    if !item.status.is_terminal()
                        && item.status != Status::Ready
                        && item.status != Status::NeedLogin
                    {
                        item.status = Status::Failed;
                        item.error = Some("应用重启，任务中断".into());
                        item.push_log("应用重启，任务中断（可重试）".to_string());
                    }
                }
            }
            if state.history_load_failed {
                // 读取失败时 history 只是空占位：此时落盘会把原文件真正覆盖掉。
                // 备份已由 History::load 尽力生成，等用户产生新数据/显式写盘再覆盖。
                ytdlp_core::log::warn("history.json 本次未能读取，已跳过启动写盘以保护原文件");
            } else {
                state.persist();
            }
            Ok(())
        })
        .build(tauri::generate_context!())
    {
        Ok(a) => a,
        Err(e) => {
            // GUI 无控制台：启动失败必须有可见提示，而不是静默 panic（日志 + 弹窗）
            fatal_dialog("启动失败", &format!("Tauri 应用启动失败：{e}"));
            std::process::exit(1);
        }
    };
    // 退出兜底：先拦截退出，取消所有任务、清空等待队列，等运行中的子进程
    // 收尾后再真正退出；同时同步写一次 history（异步 persist 不会在进程退出时等）。
    app.run(|app_handle, event| {
        if let tauri::RunEvent::ExitRequested { api, .. } = event {
            if EXITING.swap(true, Ordering::SeqCst) {
                // app.exit(0) 触发的第二次 ExitRequested：放行
                return;
            }
            api.prevent_exit();
            let app_for_exit = app_handle.clone();
            {
                let st = app_for_exit.state::<state::AppState>();
                // 1) 通知所有任务/工具下载取消（看门狗会终止进程树）
                {
                    let cancels = st.cancels.lock();
                    for flag in cancels.values() {
                        flag.store(true, Ordering::Relaxed);
                    }
                }
                // 2) 清空等待队列：任务结束释放 slot 时不能再启动新的排队任务
                let _ = st.queue.lock().drain_waiting();
            }
            // 3) 立即终止已登记的子进程树（yt-dlp/ffmpeg/ffprobe），
            //    避免进程退出后它们还在后台写盘。
            ytdlp_core::exec::kill_all_children();
            std::thread::spawn(move || {
                // 4) 等待运行中的任务退出（最多 5 秒），再落盘并真正退出。
                // 每轮重新扫一遍 cancels：收尾期间仍可能有任务刚注册取消标志
                // （例如上一个任务释放 slot 时恰好启动了下一个），这些新标志
                // 也必须被置位。
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    // running_count() 返回 usize 不借用 guard，链式调用即可
                    let running = app_for_exit
                        .state::<state::AppState>()
                        .queue
                        .lock()
                        .running_count();
                    // cancels：在一行内 lock → clone Arc flags → 释放锁，
                    // 不把 MutexGuard 存到变量（否则临时 State 生命周期不够）
                    let flags: Vec<Arc<AtomicBool>> = app_for_exit
                        .state::<state::AppState>()
                        .cancels
                        .lock()
                        .values()
                        .cloned()
                        .collect();
                    for f in &flags {
                        f.store(true, Ordering::Relaxed);
                    }
                    let pending = !flags.is_empty();
                    if (running == 0 && !pending) || Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                let st = app_for_exit.state::<state::AppState>();
                // 读取失败的 history 只是空占位，退出时同样不能拿它覆盖原文件。
                if !st.history_load_failed {
                    st.persist();
                }
                app_for_exit.exit(0);
            });
        }
    });
}
