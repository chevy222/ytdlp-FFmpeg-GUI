//! 设置命令：读取/保存 config.json（后端 sanitize + 原子写 + 并发/历史上限即时生效）。

use super::{ensure_main_window, err_string, launch_next, CmdResult};
use crate::state::AppState;
use tauri::{AppHandle, Manager};
use ytdlp_core::config::AppConfig;
use ytdlp_core::transcode;

#[tauri::command(async)]
pub fn get_config(app: AppHandle, win: tauri::WebviewWindow) -> CmdResult<AppConfig> {
    ensure_main_window(&win)?;
    let state = app.state::<AppState>();
    let config = state.config.lock().clone();
    Ok(config)
}

#[tauri::command(async)]
pub fn save_config(
    app: AppHandle,
    win: tauri::WebviewWindow,
    mut config: AppConfig,
) -> CmdResult<()> {
    ensure_main_window(&win)?;
    let state = app.state::<AppState>();
    // 后端必须自己校验：前端只有 UI 层限制（number 输入的 min/max 拦不住手输/粘贴），
    // 越界值会一路流到任务线程里（例如负的 max_gain_db 会让 f32::clamp 直接 panic，
    // 而 panic 发生在任务线程 → 条目卡死 + 并发额度永久少一格）
    config.sanitize();
    // 先落盘再替换内存：落盘失败时错误上抛、内存保持旧值。原顺序（先换内存
    // 再落盘）在写盘失败时会分叉——本次会话已按新配置运行，重启后却丢失，
    // 用户看到"保存失败"却解释不了当前行为为何已变化。
    let path = state.paths.config_file();
    let _ = std::fs::create_dir_all(state.paths.config_dir());
    config.save(&path).map_err(err_string)?;
    // 依赖路径变更即重置转码层级锁定：用户换了一个有无 QSV 不同的 ffmpeg
    // 构建后，旧的进程级锁定结论要等到重启才失效（保存设置时主动清掉，
    // 最坏从头试一层；编码器探测有 TTL 缓存兜底）
    let deps_changed = {
        let cur = state.config.lock();
        cur.dependencies != config.dependencies
    };
    {
        let mut cur = state.config.lock();
        *cur = config.clone();
    }
    if deps_changed {
        transcode::reset_locked_tier();
    }
    // 并发上限即时生效：调高并发时若有任务在排队，立刻放行（A4）。
    // 返回本次提升释放出的、应马上启动的等待任务 id 列表。
    let to_launch = state
        .queue
        .lock()
        .set_concurrency(config.general.concurrency as usize);
    // 历史上限即时生效（超出部分在下一次 upsert 时按"最旧终态优先"裁剪）
    state.history.lock().set_limit(config.general.history_limit);
    // 必须在释放队列锁之后再逐个启动：launch_next 会读 history + queue 的锁
    for id in to_launch {
        launch_next(&app, id);
    }
    Ok(())
}
