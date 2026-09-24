//! Tauri 应用壳：全局状态 + 命令注册 + 目录约定（§3.7）。
//! 核心逻辑在 ytdlp-core；本层负责界面桥接与外部进程任务调度。

mod commands;
mod login;
#[cfg(windows)]
mod login_win;
mod state;

use tauri::Manager;
use ytdlp_core::model::Status;

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
            let _ = commands::add_url(app2, cli.urls);
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
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            // 第二实例：解析 argv 并把 URL/覆盖转发给已运行实例（§UL-09 单实例转发）。
            // 覆盖项单独出现（如仅 --dir）也要转发。
            let args: Vec<String> = argv.iter().skip(1).map(|s| s.to_string()).collect();
            apply_cli_args(app, args);
        }))
        .manage(state::AppState::new())
        .invoke_handler(tauri::generate_handler![
            commands::add_url,
            commands::add_local,
            commands::list_items,
            commands::list_items_lite,
            commands::get_item_log,
            commands::start_download,
            commands::start_transcode,
            commands::start_merge,
            commands::cancel_item,
            commands::remove_item,
            commands::clear_done,
            commands::retry_item,
            commands::relogin_item,
            commands::open_login_site,
            commands::get_config,
            commands::save_config,
            commands::list_cookies,
            commands::save_cookies,
            commands::delete_cookie,
            commands::probe_dependencies,
            commands::download_tool,
            commands::cancel_tool_download,
            commands::tool_urls,
            commands::probe_hw_encoders,
            commands::open_item_dir,
            commands::clear_temp,
            commands::rot_item,
            commands::set_sections,
            commands::queue_status,
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
            state.persist();
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("Tauri 应用启动失败");
}
