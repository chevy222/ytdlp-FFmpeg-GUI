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
    let path = state.paths.config_file();
    let _ = std::fs::create_dir_all(state.paths.config_dir());
    let deps_changed = {
        // 全量保存与单字段更新共用同一临界区，避免磁盘快照和内存状态交错覆盖。
        let mut cur = state.config.lock();
        let changed = cur.dependencies != config.dependencies;
        config.save(&path).map_err(err_string)?;
        *cur = config.clone();
        changed
    };
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

#[tauri::command(async)]
pub fn save_dependency_path(
    app: AppHandle,
    win: tauri::WebviewWindow,
    tool: String,
    path: String,
) -> CmdResult<()> {
    ensure_main_window(&win)?;
    let state = app.state::<AppState>();
    let mut cur = state.config.lock();
    let mut next = cur.clone();
    match tool.as_str() {
        "yt_dlp_path" => next.dependencies.yt_dlp_path = Some(path),
        "ffmpeg_path" => next.dependencies.ffmpeg_path = Some(path),
        "ffprobe_path" => next.dependencies.ffprobe_path = Some(path),
        "deno_path" => next.dependencies.deno_path = Some(path),
        _ => return Err("未知工具".into()),
    }
    next.save(&state.paths.config_file()).map_err(err_string)?;
    *cur = next;
    drop(cur);
    if tool == "ffmpeg_path" {
        transcode::reset_locked_tier();
    }
    Ok(())
}
