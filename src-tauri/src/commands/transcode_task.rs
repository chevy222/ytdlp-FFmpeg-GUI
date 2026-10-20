//! 转码任务线程：批量转码/放大入口、参数组装、执行与收尾（产物回列表）。

use super::{
    default_output_dir, ensure_main_window, log_error_lines, log_item, now_str, persist,
    restore_status, transition_in, update_item, update_progress, CmdResult, ProgressPayload,
    SlotGuard,
};
use crate::state::AppState;
use tauri::{AppHandle, Emitter, Manager};
use ytdlp_core::download::probe_output;
use ytdlp_core::model::{ItemKind, MediaItem, Status};
use ytdlp_core::transcode::{self, TranscodeParams};
use ytdlp_core::worker::SubmitOutcome;
use ytdlp_core::{transition, CoreError};

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
    // 放大意图的登记放在 begin_transcode 里、与"状态成功改到 Transcoding"同一处
    // （只有真正会入队的 id 才登记）：预检跳过 / 条目不存在 / 早退的 id 从不登记，
    // 从根上消除"登记了但没人消费"的残留（历史 bug：残留的 true 会让之后对同一
    // id 的普通转码被误放大）。清理点见 cancel_item，消费点见 run_transcode_task 入口。
    begin_transcode(app, ids, true)
}

/// 转码入口（start_transcode / start_upscale 共用）。
/// `upscale=true` 时把通过的 id 登记进 `AppState.upscale_ids`。
fn begin_transcode(app: AppHandle, ids: Vec<String>, upscale: bool) -> CmdResult<()> {
    let state = app.state::<AppState>();
    if ids.is_empty() {
        return Err("未选择条目".into());
    }
    let mut to_run = Vec::new();
    // 同 start_merge：跳过日志出锁后再写，避免 history 锁重入死锁
    let mut skipped: Vec<(String, String)> = Vec::new();
    // 锁内分两段：先做存在性预检（收集条目），全部通过后再改状态。
    // 旧写法在循环内"边校验边改状态"，第 N 个 id 不存在时直接 return，
    // 前 N-1 个已被写成 Transcoding 并 emit，既没进队列也没执行体，
    // 永久卡死（remove/retry 都不接受非终态）。
    let mut updated_items: Vec<MediaItem> = Vec::new();
    {
        let mut hist = state.history.lock();
        // 第一段：存在性预检 + 收集（不改任何状态）
        let mut items: Vec<(String, MediaItem)> = Vec::with_capacity(ids.len());
        for id in &ids {
            let Some(item) = hist.get(id).cloned() else {
                return Err(format!("条目不存在：{id}"));
            };
            items.push((id.clone(), item));
        }
        // 第二段：统一改状态
        for (id, item) in items {
            let has_file = item
                .path
                .as_deref()
                .map(|p| std::path::Path::new(p).is_file())
                .unwrap_or(false);
            if !has_file {
                skipped.push((
                    id,
                    "转码被跳过：无本地输入文件（先下载或添加本地文件）".into(),
                ));
                continue;
            }
            match transition(item.status, Status::Transcoding) {
                Ok(to) => {
                    // 放大意图与"真正入队"绑定登记（见 start_upscale 的注释）：
                    // 登记与消费之间不存在被跳过/早退的悬空窗口
                    if upscale {
                        state.upscale_ids.lock().insert(id.clone());
                    }
                    // 进入新任务前进度归零：否则上一轮下载/转码的 100% 会一直挂在进度列
                    let updated = MediaItem {
                        status: to,
                        percent: 0.0,
                        ..item
                    };
                    hist.upsert(updated.clone());
                    updated_items.push(updated);
                    to_run.push(id);
                }
                Err(e) => skipped.push((id, format!("转码被跳过：{e}"))),
            }
        }
    }
    // 出锁后统一 emit：避免持锁派发事件，也消除了旧写法每条一次
    // drop(hist)→emit→re-lock 的抖动（批量 50 条 = 50 轮锁释放+获取）
    for updated in &updated_items {
        let _ = app.emit("item:update", updated);
    }
    for (id, msg) in &skipped {
        log_item(&app, id, msg.as_str());
    }
    if to_run.is_empty() {
        // 先落盘再返回：上面的 skipped 日志已写入内存 history，不 persist 会在重启后丢失
        persist(&app);
        return Err("没有可转码的条目（需要已解析的本地文件）".into());
    }
    for id in to_run {
        let outcome = {
            let mut q = state.queue.lock();
            q.submit(&id)
        };
        if matches!(
            outcome,
            SubmitOutcome::Queued | SubmitOutcome::AlreadyRunning
        ) {
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

/// 转码任务线程（提交队列后执行；进度经 `item:update` 回推）。
pub(super) fn run_transcode_task(app: AppHandle, id: String) {
    let state = app.state::<AppState>();
    let cancel = state.register_cancel(&id);
    // 并发额度守卫（见 SlotGuard 注释）
    let _slot = SlotGuard {
        app: app.clone(),
        id: id.clone(),
    };
    // 放大意图在入口一次性消费：无论后面走哪条提前返回路径（条目被删 /
    // 无本地文件），登记都已清除；排队中被取消的任务根本进不到这里，由
    // cancel_item 负责清理。残留登记会让后续普通转码被误放大到 1080P。
    let upscale = state.upscale_ids.lock().remove(&id);
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
    // 放大到 1080P：upscale 标记已在函数入口一次性消费（见上）。
    // 在 Rust 侧用 ffprobe 已探测的分辨率计算固定目标宽高（偶数对齐），
    // 不用 ffmpeg 表达式——表达式里 if()/min() 的逗号会被 filtergraph 解析器
    // 误判为滤波器分隔符（scale_qsv 选项值的单引号不转义逗号），导致 EXIT=127。
    let rot_deg = item.rot_angle.degrees();
    let force_scale = if upscale {
        match (meta.width, meta.height) {
            (Some(w), Some(h)) if w > 0 && h > 0 => {
                let long = w.max(h) as f32;
                let short = w.min(h) as f32;
                let s = (1920.0 / long).min(1080.0 / short);
                if s > 1.0 {
                    // 不旋转时的目标宽高（偶数对齐）
                    let tw = ((w as f32 * s) / 2.0).floor() as u32 * 2;
                    let th = ((h as f32 * s) / 2.0).floor() as u32 * 2;
                    // 旋转 90/270° 时输出宽高互换（vpp_qsv/transpose 后方向相反）；
                    // 0/180° 不互换。force_scale 存的是"旋转后的输出尺寸"。
                    let (tw, th) = if rot_deg == 90 || rot_deg == 270 {
                        (th, tw)
                    } else {
                        (tw, th)
                    };
                    if tw > 0 && th > 0 {
                        log_item(
                            &app,
                            &id,
                            format!(
                                "放大到 1080P：源 {w}×{h}{} → 输出 {tw}×{th}（缩放比 {:.2}x）",
                                if rot_deg != 0 {
                                    format!("（旋转{rot_deg}°）")
                                } else {
                                    String::new()
                                },
                                s
                            ),
                        );
                        Some((tw, th))
                    } else {
                        None
                    }
                } else {
                    log_item(
                        &app,
                        &id,
                        format!("放大到 1080P：源 {w}×{h} 已达 1080P 以上，跳过放大"),
                    );
                    None
                }
            }
            _ => {
                log_item(&app, &id, "放大到 1080P：源分辨率未知，跳过放大");
                None
            }
        }
    } else {
        None
    };
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
        // P1-3：cancel_item 抢先写成 Canceled，但转码实际已完成。
        // transition(Canceled → Ready/Done) 不在白名单，静默拒绝后条目永久停在
        // "已取消"，而产物已作为新条目回列表。显式覆盖为恢复状态。
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
