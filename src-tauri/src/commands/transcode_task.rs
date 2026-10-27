//! 转码任务线程：批量转码/放大入口、参数组装、执行与收尾（产物回列表）。

use super::{
    add_product_item, default_output_dir, enqueue_or_spawn, ensure_main_window, log_error_lines,
    log_item, persist, prepare_batch, restore_status, transition_in, update_item, update_progress,
    CmdResult, ProgressPayload, SlotGuard,
};
use crate::state::AppState;
use std::collections::HashMap;
use tauri::{AppHandle, Emitter, Manager};
use ytdlp_core::model::{ItemKind, MediaMeta, Status};
use ytdlp_core::transcode::{self, TranscodeParams};
use ytdlp_core::CoreError;

/// 批量转码（TC-05：按 设置-转码/通用 默认参数执行，不弹确认窗）。
#[tauri::command(async)]
pub fn start_transcode(
    app: AppHandle,
    win: tauri::WebviewWindow,
    ids: Vec<String>,
) -> CmdResult<()> {
    ensure_main_window(&win)?;
    begin_transcode(app, ids, false)
}

/// 批量放大到 1080P：长边<1920 且 短边<1080 的视频等比放大到不超过 1920×1080，
/// 已达 1080P 及以上不缩放。其余转码参数同 设置-转码/通用。
#[tauri::command(async)]
pub fn start_upscale(app: AppHandle, win: tauri::WebviewWindow, ids: Vec<String>) -> CmdResult<()> {
    ensure_main_window(&win)?;
    // 放大目标在 begin_transcode 里、与"状态成功改到 Transcoding"同一处登记
    // （只有真正会入队的 id 才登记）：预检跳过 / 条目不存在 / 早退的 id 从不登记，
    // 从根上消除"登记了但没人消费"的残留（历史 bug：残留的 true 会让之后对同一
    // id 的普通转码被误放大）。清理点见 cancel_item，消费点见 run_transcode_task 入口。
    begin_transcode(app, ids, true)
}

/// 放大到 1080P 的目标尺寸（旋转后的输出宽高，偶数对齐）。
/// Err 表示无需放大（已达 1080P）或分辨率未知 —— 调用方应直接跳过任务，
/// 而不是继续走一次普通转码。
fn upscale_target(meta: &MediaMeta, rot_deg: u16) -> Result<(u32, u32), String> {
    let (w, h) = match (meta.width, meta.height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => (w, h),
        _ => return Err("源分辨率未知".into()),
    };
    let long = w.max(h) as f32;
    let short = w.min(h) as f32;
    let s = (1920.0 / long).min(1080.0 / short);
    if s <= 1.0 {
        return Err("源已达 1080P 以上".into());
    }
    let tw = ((w as f32 * s) / 2.0).floor() as u32 * 2;
    let th = ((h as f32 * s) / 2.0).floor() as u32 * 2;
    if tw == 0 || th == 0 {
        return Err("计算出的目标尺寸无效".into());
    }
    // 旋转 90/270° 时输出宽高互换（transpose 后方向相反）
    Ok(if rot_deg == 90 || rot_deg == 270 {
        (th, tw)
    } else {
        (tw, th)
    })
}

/// 转码入口（start_transcode / start_upscale 共用）。
/// `upscale=true` 时把通过的 id 与目标尺寸登记进 `AppState.upscale_targets`。
fn begin_transcode(app: AppHandle, ids: Vec<String>, upscale: bool) -> CmdResult<()> {
    let state = app.state::<AppState>();
    if ids.is_empty() {
        return Err("未选择条目".into());
    }
    // 同 start_merge：跳过日志出锁后再写，避免 history 锁重入死锁
    let (accepted, skipped) = {
        let mut hist = state.history.lock();
        let mut upscale_targets: HashMap<String, (u32, u32)> = HashMap::new();
        let (accepted, skipped) = prepare_batch(
            &mut hist,
            &ids,
            Status::Transcoding,
            "转码被跳过",
            |item| {
                let has_file = item
                    .path
                    .as_deref()
                    .map(|p| std::path::Path::new(p).is_file())
                    .unwrap_or(false);
                if !has_file {
                    return Err("无本地输入文件（先下载或添加本地文件）".into());
                }
                if upscale {
                    // 已达 1080P / 分辨率未知：直接跳过任务，不再走一次完整转码
                    let size = upscale_target(&item.meta, item.rot_angle.degrees())?;
                    upscale_targets.insert(item.id.clone(), size);
                }
                Ok(())
            },
        )?;
        // 放大目标与"真正入队"绑定登记：只登记成功 transition 的条目
        if upscale {
            let mut targets = state.upscale_targets.lock();
            for item in &accepted {
                if let Some(size) = upscale_targets.get(&item.id) {
                    targets.insert(item.id.clone(), *size);
                }
            }
        }
        (accepted, skipped)
    };
    // 出锁后统一 emit：避免持锁派发事件，也消除了旧写法每条一次
    // drop(hist)→emit→re-lock 的抖动（批量 50 条 = 50 轮锁释放+获取）
    for updated in &accepted {
        let _ = app.emit("item:update", updated);
    }
    for (id, msg) in &skipped {
        log_item(&app, id, msg.as_str());
    }
    let to_run: Vec<String> = accepted.iter().map(|i| i.id.clone()).collect();
    if to_run.is_empty() {
        // 先落盘再返回：上面的 skipped 日志已写入内存 history，不 persist 会在重启后丢失
        persist(&app);
        if upscale {
            // 放大预检全部跳过时，把更具体的原因（已达 1080P / 分辨率未知）带回前端
            if let Some((_, msg)) = skipped.first() {
                return Err(msg.clone());
            }
        }
        return Err("没有可转码的条目（需要已解析的本地文件）".into());
    }
    for id in to_run {
        enqueue_or_spawn(
            &app,
            &id,
            "已排队，等待并发 slot…",
            "开始转码…",
            run_transcode_task,
        );
    }
    persist(&app);
    Ok(())
}

/// 转码任务线程（提交队列后执行；进度经 `item:update` 回推）。
pub(super) fn run_transcode_task(app: AppHandle, id: String) {
    let state = app.state::<AppState>();
    let cancel = state.register_cancel(&id);
    // 并发额度守卫（见 SlotGuard 注释）
    let _slot = SlotGuard {
        app: app.clone(),
        id: id.clone(),
    };
    // 放大目标在入口一次性消费：无论后面走哪条提前返回路径（条目被删 /
    // 无本地文件），登记都已清除；排队中被取消的任务根本进不到这里，由
    // cancel_item 负责清理。残留登记会让后续普通转码被误放大到 1080P。
    let upscale_size = state.upscale_targets.lock().remove(&id);
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
    // 放大目标已在 begin_transcode 预检时算好（无需放大/分辨率未知的条目
    // 根本不会入队）。这里只记录目标尺寸，供任务日志回看。
    let rot_deg = item.rot_angle.degrees();
    let force_scale = upscale_size;
    if let Some((tw, th)) = force_scale {
        log_item(
            &app,
            &id,
            format!(
                "放大到 1080P：源 {}×{}{} → 输出 {tw}×{th}",
                meta.width.unwrap_or(0),
                meta.height.unwrap_or(0),
                if rot_deg != 0 {
                    format!("（旋转{rot_deg}°）")
                } else {
                    String::new()
                },
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
        max_w: cfg.transcode.max_w,
        max_h: cfg.transcode.max_h,
        brcap_kbps: cfg.transcode.brcap_kbps,
        normalize_audio: cfg.general.normalize_audio,
        max_gain_db: cfg.general.max_gain_db,
        rot_angle: item.rot_angle,
        keep_cover: cfg.transcode.keep_cover,
        collision_policy: cfg.general.collision_policy.clone(),
        force_scale,
    };
    let app2 = app.clone();
    let id2 = id.clone();
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
        // cancel_item 抢先写成 Canceled，但转码实际已完成。状态机已允许
        // Canceled → Ready/Done，这里显式覆盖是为了额外写一条完成日志。
        if it.status == Status::Canceled && final_status == Status::Done {
            it.status = orig_status;
            it.push_log("取消请求到达时转码已完成".to_string());
        } else {
            transition_in(it, orig_status);
        }
        it.percent = if final_status == Status::Done {
            100.0
        } else {
            it.percent
        };
        // 取消不算错误；成功必须清掉上一次转码失败留下的文案
        it.error = match &result {
            Ok(_) | Err(CoreError::Cancelled) => None,
            Err(e) => Some(e.to_string()),
        };
    });
    // 成功：产物作为新条目回到列表（TC-11）
    if let Ok(out) = &result {
        add_product_item(app, ItemKind::TranscodeOut, out, "转码产物", id);
    }
    // 清理取消标志（并发 slot 由调用方的 SlotGuard 释放），启动下一个等待任务
    state.cancels.lock().remove(id);
    persist(app);
}
