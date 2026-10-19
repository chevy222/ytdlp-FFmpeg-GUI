//! 合并任务线程：合并作业参数、锚点提交、执行与收尾（产物回列表）。

use tauri::{AppHandle, Emitter, Manager};
use ytdlp_core::download;
use ytdlp_core::merge::{self, MergeParams};
use ytdlp_core::model::{ItemKind, MediaItem, Status};
use ytdlp_core::worker::SubmitOutcome;
use ytdlp_core::{transition, CoreError};
use super::{CmdResult, SlotGuard, ProgressPayload, update_item, update_progress, transition_in, log_item, log_error_lines, persist, default_output_dir, now_str, restore_status, today_stamp, ensure_main_window};
use crate::state::AppState;

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
    win: tauri::WebviewWindow,
    ids: Vec<String>,
    filename: Option<String>,
    container: Option<String>,
    encoder: Option<String>,
    normalize: Option<bool>,
) -> CmdResult<()> {
    ensure_main_window(&win)?;
    let state = app.state::<AppState>();
    if ids.len() < 2 {
        return Err("合并至少需要 2 个条目".into());
    }
    let mut jobs = Vec::new();
    // 跳过原因出锁后再写日志：log_item → update_item 会再次 lock history，
    // std::sync::Mutex 不可重入，锁内调用会当场死锁（UI 永久冻结）
    let mut skipped: Vec<(String, String)> = Vec::new();
    // 锁内分两段：先做存在性预检（收集条目），全部通过后再改状态。
    // 旧写法在循环内"边校验边改状态"，第 N 个 id 不存在时直接 return，
    // 前 N-1 个已被写成 Merging 并 emit，既没进队列也没执行体，永久卡死。
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
                skipped.push((id, "合并被跳过：无本地输入文件".into()));
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
                    updated_items.push(updated);
                    jobs.push(id);
                }
                Err(e) => skipped.push((id, format!("合并被跳过：{e}"))),
            }
        }
    }
    // 出锁后统一 emit
    for updated in &updated_items {
        let _ = app.emit("item:update", updated);
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

/// 合并任务线程（提交队列后执行；进度经 `item:update` 回推）。
pub(super) fn run_merge_task(app: AppHandle, id: String) {
    let state = app.state::<AppState>();
    let cancel = state.register_cancel(&id);
    // 并发额度守卫（见 SlotGuard 注释）
    let _slot = SlotGuard {
        app: app.clone(),
        id: id.clone(),
    };
    let job = state.merge_jobs.lock().get(&id).cloned();
    let Some(job) = job else {
        // job 记录缺失（理论不可达）：清掉上面为 id 注册的 cancel flag，
        // 否则会随每次异常调用在 state.cancels 里累积残留条目。
        state.cancels.lock().remove(&id);
        return;
    };
    // 取消标志共享给全部参与条目：从任意被勾选条目点"取消"都能取消这次合并
    {
        let mut cancels = state.cancels.lock();
        for jid in &job.ids {
            cancels.insert(jid.clone(), cancel.clone());
        }
    }
    // 任务私有临时目录名由本层生成并登记到 AppState.merge_tmp：「清空临时文件」
    // 据此精确豁免进行中的合并（而不是"有任务在跑就跳过所有 merge_*"），
    // 崩溃残留在下次启动时清掉。纳秒时间戳足够区分并发合并。
    let subdir = format!(
        "merge_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
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
            temp_subdir: subdir.clone(),
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
        // 与正常收尾 finish_merge 同口径：清掉各参与条目的 cancel flag，
        // 否则这条早退路径会把 cancels 注册表项永久泄漏，并让后续 cancel_item
        // 命中幽灵 flag、clear_temp_inner 因 active 非空而保守跳过清理。
        {
            let mut cancels = state.cancels.lock();
            for jid in &job.ids {
                cancels.remove(jid);
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
    // 登记后才开始跑：合并结束（无论成败）后移除（目录本身由 run_merge 清理）
    state.merge_tmp.lock().insert(subdir.clone());
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
    // 解除临时目录豁免登记
    state.merge_tmp.lock().remove(&subdir);
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
            // P1-3：cancel_item 抢先写成 Canceled，但合并实际已完成。
            // transition(Canceled → Ready/Done) 不在白名单，静默拒绝后条目
            // 永久停在"已取消"，而产物已作为新条目回列表。显式覆盖为恢复状态。
            if it.status == Status::Canceled && final_status == Status::Done {
                it.status = orig;
                if is_anchor {
                    it.push_log("取消请求到达时合并已完成".to_string());
                }
            } else {
                transition_in(it, orig);
            }
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

