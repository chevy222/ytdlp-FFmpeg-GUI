//! Tauri 应用壳：全局状态 + 命令注册 + 目录约定（§3.7）。
//! 核心逻辑在 ytdlp-core；本层负责界面桥接与外部进程任务调度。

mod commands;
mod login;
#[cfg(windows)]
mod login_win;
mod state;

use tauri::Manager;
use ytdlp_core::model::Status;

/// 将 CLI 参数应用到主实例：存 overrides（无论是否带 URL）并把 URL 投入列表解析。
/// `--dir`/`--cookies` 等覆盖单独出现时也必须生效，不能只在带 URL 时应用。
fn apply_cli(app: &tauri::AppHandle) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = ytdlp_core::cli::parse_cli_args(&args);
    let st = app.state::<state::AppState>();
    *st.cli.lock() = state::CliOverrides::from(cli.clone());
    if !cli.urls.is_empty() {
        let _ = commands::add_url(app.clone(), cli.urls);
    }
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            // 第二实例：解析 argv 并把 URL/覆盖转发给已运行实例（§UL-09 单实例转发）。
            // 覆盖项单独出现（如仅 --dir）也要转发。
            let args: Vec<String> = argv.iter().skip(1).map(|s| s.to_string()).collect();
            let cli = ytdlp_core::cli::parse_cli_args(&args);
            let st = app.state::<state::AppState>();
            *st.cli.lock() = state::CliOverrides::from(cli.clone());
            if !cli.urls.is_empty() {
                let _ = commands::add_url(app.clone(), cli.urls);
            }
        }))
        .manage(state::AppState::new())
        .invoke_handler(tauri::generate_handler![
            commands::add_url,
            commands::add_local,
            commands::list_items,
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
        ])
        .setup(|app| {
            // 确保绿色便携目录结构（exe 同级）
            let state = app.state::<state::AppState>();
            if let Err(e) = state.paths.ensure_dirs() {
                eprintln!("创建运行时目录失败：{}", e);
            }
            // CLI 启动参数（§UL-09）：首实例带 --url 等直接执行
            apply_cli(app.handle());
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
