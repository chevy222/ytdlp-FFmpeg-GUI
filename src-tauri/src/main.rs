// 入口：初始化日志/配置后启动 Tauri。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // 日志与 panic 钩子要最先初始化：AppState::new() 里的配置/历史加载告警、
    // 以及任务线程的 panic 现场都依赖它（GUI 程序 stderr 不可见）
    ytdlp_gui_lib::init_logging();
    ytdlp_gui_lib::run();
}
