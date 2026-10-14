//! 下载任务线程：参数组装、yt-dlp 执行、后处理与收尾（产物回填/取消清理）。

use tauri::{AppHandle, Emitter, Manager};
use ytdlp_core::download::{self, DownloadParams, post_process, run_download};
use ytdlp_core::model::Status;
use ytdlp_core::worker::SubmitOutcome;
use ytdlp_core::{transition, CoreError};
use super::{CmdResult, err_string, SlotGuard, ProgressPayload, update_item, update_progress, transition_in, log_item, log_error_lines, persist, default_output_dir, resolve_cookies, resolve_potoken, ensure_main_window};
use crate::state::AppState;

#[tauri::command(async)]
pub fn start_download(
    app: AppHandle,
    win: tauri::WebviewWindow,
    id: String,
    format_id: Option<String>,
    audio_only: bool,
) -> CmdResult<()> {
    ensure_main_window(&win)?;
    let state = app.state::<AppState>();
    // 状态切换必须出锁后 emit item:update：前端 1.5s 轮询有"有 busy 条目才
    // 刷新列表"的优化，若不主动通知，前端永远认为状态是 Ready、永不拉列表，
    // 进度条也不显示（progressCell 仅在 Downloading 等状态渲染）——yt-dlp
    // 实际在跑但 UI 完全无感知。
    let updated = {
        let mut hist = state.history.lock();
        let mut item = hist.get(&id).cloned().ok_or("条目不存在")?;
        if item.status != Status::Ready {
            return Err(format!("当前状态不可下载：{}", item.status.label()));
        }
        if item.url.as_deref().map(str::is_empty).unwrap_or(true) {
            return Err("该条目没有可下载的 URL（本地文件请使用转码）".into());
        }
        let new_status = transition(item.status, Status::Downloading).map_err(err_string)?;
        item.status = new_status;
        // 排队场景下 launch_next 从条目读回这两个参数：必须在提交前落进条目，
        // 否则排队任务会丢失用户选择的格式/仅音频选项
        item.format_id = format_id.clone();
        item.audio_only = audio_only;
        // 用户选了具体格式：把该格式的画质信息写回 meta，使列表"画质/格式"列
        // 显示实际下载的格式（1080p），而不是源视频最高画质（4K）。
        // format_id=None（默认 best）时不覆盖，保持源视频元数据。
        if !audio_only {
            if let Some(fid) = &format_id {
                // 组合格式 "137+140" 取视频流（第一段）匹配
                let primary = fid.split('+').next().unwrap_or(fid);
                if let Some(fmt) = item
                    .meta
                    .download_formats
                    .iter()
                    .find(|f| f.format_id == primary || f.format_id == *fid)
                {
                    if let Some(h) = fmt.height {
                        item.meta.height = Some(h);
                    }
                    if let Some(c) = fmt.vcodec.clone() {
                        item.meta.vcodec = Some(c);
                    }
                    if let Some(f) = fmt.fps {
                        item.meta.fps = Some(f);
                    }
                    if let Some(s) = fmt.filesize_bytes {
                        item.meta.size_bytes = Some(s);
                    }
                    if let Some(e) = fmt.ext.clone() {
                        item.meta.container = Some(e.to_uppercase());
                    }
                    if let Some(t) = fmt.tbr_kbps {
                        item.meta.vbitrate_kbps = Some(t);
                    }
                }
            }
        }
        hist.upsert(item.clone());
        item
    };
    let _ = app.emit("item:update", &updated);
    // 提交并发队列
    let outcome = {
        let mut q = state.queue.lock();
        q.submit(&id)
    };
    if outcome == SubmitOutcome::Queued {
        log_item(&app, &id, "已排队，等待并发 slot…");
        persist(&app);
        return Ok(());
    }
    log_item(&app, &id, "开始下载…");
    persist(&app);
    let app2 = app.clone();
    std::thread::spawn(move || {
        run_download_task(app2, id, format_id, audio_only);
    });
    Ok(())
}

pub(super) fn run_download_task(app: AppHandle, id: String, format_id: Option<String>, audio_only: bool) {
    let state = app.state::<AppState>();
    let cancel = state.register_cancel(&id);
    // 并发额度守卫：panic / 任何提前返回都会释放 slot（手写 release_slot 漏一次
    // 就等于永久少一格并发额度）
    let _slot = SlotGuard {
        app: app.clone(),
        id: id.clone(),
    };

    let (
        url,
        cfg,
        general,
        resolver,
        out_dir,
        template,
        proxy,
        po_token,
        visitor_data,
        netscape,
        sections,
        js_runtime,
        cookie_warn,
    ) = {
        // 只在锁内 clone 条目：后续 Cookie 导出（export_netscape）是文件 IO，
        // 持 history 锁做会阻塞所有 update_item 调用（前端刷新卡顿）
        let item = {
            let hist = state.history.lock();
            hist.get(&id).cloned()
        };
        let Some(item) = item else {
            // 条目在排队期间被删除：清掉取消标志即可（并发 slot 由 _slot 守卫释放）
            state.cancels.lock().remove(&id);
            return;
        };
        let url = item.url.clone().unwrap_or_default();
        // 一次锁内读齐 download + general + network
        let (cfg, general, proxy) = {
            let c = state.config.lock();
            (
                c.download.clone(),
                c.general.clone(),
                c.network.resolve_proxy(&url),
            )
        };
        let resolver = state.resolver();
        // PO Token（YouTube 风控）：手动填入优先，否则自动生成（带缓存/失败回退）
        let (po_token, visitor_data) = resolve_potoken(
            &state,
            &resolver,
            &url,
            proxy.as_deref(),
            |l| log_item(&app, &id, l),
        );
        let out_dir = default_output_dir(&state);
        let template = cfg.filename_template.clone();
        let task_tmp = state.paths.task_temp_dir(&id);
        let (netscape, cookie_warn) = resolve_cookies(&state, &item, &task_tmp);
        let sections = item.sections.clone();
        // JS 运行时（§3.6 依赖）：托管/配置的 deno 必须显式传给 yt-dlp，
        // 否则 YouTube 组件会因"没有 JS 运行时"失败（依赖自检却是通过的）
        let js_runtime = resolver
            .resolve(ytdlp_core::exec::Tool::Deno)
            .ok()
            .map(|p| format!("deno:{}", p.to_string_lossy()));
        (
            url,
            cfg,
            general,
            resolver,
            out_dir,
            template,
            proxy,
            po_token,
            visitor_data,
            netscape,
            sections,
            js_runtime,
            cookie_warn,
        )
    };
    if let Some(w) = cookie_warn {
        log_item(&app, &id, w);
    }

    let params = DownloadParams {
        format_id: format_id.clone(),
        audio_only,
        out_dir: out_dir.clone(),
        filename_template: template,
        embed_cover: cfg.embed_cover,
        proxy,
        cookies_file: netscape,
        sections,
        js_runtime,
        // ffmpeg 位置（§3.6 依赖）：合并容器、嵌入封面、时间范围裁剪
        // （--download-sections 帮助里写明 "Needs ffmpeg"）都要用 ffmpeg，
        // 而 yt-dlp 只在 PATH 与自身目录找 —— 托管模式（tools/）必须显式传入。
        ffmpeg_path: resolver
            .resolve(ytdlp_core::exec::Tool::Ffmpeg)
            .ok()
            .map(|p| p.to_string_lossy().into_owned()),
        po_token,
        visitor_data,
    };

    let app2 = app.clone();
    let id2 = id.clone();
    let app4 = app.clone();
    let id4 = id.clone();
    let prog_result = run_download(
        &resolver,
        &url,
        &params,
        &cfg,
        &state.paths.temp_dir(),
        &cancel,
        move |p| {
            // Destination 行（切换到下一条流，percent 恒为 0）只更新目标文件名，
            // 不把进度打回 0：DASH 双流下载视频 100% → 音频 Destination 会把
            // percent 重置，快速下载看起来就像"一直 0%"
            let progress = if p.file.is_some() {
                ProgressPayload {
                    id: id2.clone(),
                    percent: None,
                    speed: None,
                    eta: None,
                    file: p.file.clone(),
                }
            } else {
                ProgressPayload {
                    id: id2.clone(),
                    percent: Some(p.percent),
                    speed: p.speed,
                    eta: p.eta,
                    file: None,
                }
            };
            update_progress(&app2, progress);
        },
        move |l| log_item(&app4, &id4, l),
    );

    let mut outcome = match prog_result {
        Ok(o) => o,
        Err(e) => {
            finish_download(&app, &id, Err(e));
            return;
        }
    };

    // 后处理（DL-04）
    let first = outcome.output_paths.first().cloned();
    if outcome.matched_by_scan {
        // 产物是"目录里新出现的最新媒体文件"（弱证据）：并发下载写同一个输出目录
        // 时可能是别人的产物，因此只提示、不后处理、不写入条目 path
        log_item(
            &app,
            &id,
            format!(
                "疑似产物（按目录新增文件推断，未做后处理）：{}",
                first
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            ),
        );
    } else if outcome.preexisting {
        // 重复下载同一 URL：yt-dlp 未覆盖既有文件（--no-overwrites），
        // 此时不做后处理，避免原地重编码覆盖用户既有文件
        log_item(&app, &id, "产物已存在（未覆盖），跳过后处理");
    } else if let Some(path) = &first {
        log_item(&app, &id, "下载完成，开始后处理…");
        update_item(&app, &id, |it| {
            transition_in(it, Status::PostProcessing);
        });
        let app2 = app.clone();
        let cfg2 = cfg.clone();
        let general2 = general.clone();
        let cancel2 = cancel.clone();
        let pp = post_process(
            &resolver,
            path,
            &cfg2,
            &general2,
            &cancel2,
            |pct| {
                // 后处理是整片重编码，没有进度就是"卡在 100% 不动"的观感
                update_progress(
                    &app,
                    ProgressPayload {
                        id: id.clone(),
                        percent: Some(pct),
                        speed: None,
                        eta: None,
                        file: None,
                    },
                );
            },
            |line| log_item(&app2, &id, line),
        );
        match pp {
            Ok((final_path, meta)) => {
                outcome.output_paths = vec![final_path];
                update_item(&app, &id, |it| {
                    it.meta = meta;
                });
                // 不在这里落盘：finish_download 收尾时会写一次（同样的快照），
                // 每次 persist 都是 clone + 序列化 + fsync 整份 history
            }
            Err(e) => {
                finish_download(&app, &id, Err(e));
                return;
            }
        }
    }
    finish_download(&app, &id, Ok(outcome));
}

fn finish_download(app: &AppHandle, id: &str, result: Result<download::DownloadOutcome, CoreError>) {
    let state = app.state::<AppState>();
    // 本次任务的最终产物（回填 path + 抽帧封面共用）。
    // 目录扫描来的"弱证据"产物不写进条目：它可能根本不是本次任务的产物。
    let final_path = result.as_ref().ok().and_then(|o| {
        if o.matched_by_scan {
            None
        } else {
            o.output_paths.first().cloned()
        }
    });
    let final_path_str = final_path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    let final_status = match &result {
        Ok(o) => {
            log_item(
                app,
                id,
                format!("下载完成：{} 个文件", o.output_paths.len()),
            );
            Status::Done
        }
        Err(CoreError::Cancelled) => {
            log_item(app, id, "已取消，清理残留");
            Status::Canceled
        }
        Err(e) => {
            log_error_lines(app, id, "下载失败：", e);
            Status::Failed
        }
    };
    update_item(app, id, |it| {
        // P1-3：cancel_item 抢先把状态写成 Canceled，但任务线程此时已跑完、
        // 实际成功。transition(Canceled → Done) 不在白名单内会被静默拒绝，
        // 条目就永久停在"已取消"但产物已落地。显式覆盖，不依赖状态机。
        if it.status == Status::Canceled && final_status == Status::Done {
            it.status = Status::Done;
            it.push_log("取消请求到达时下载已完成".to_string());
        } else {
            transition_in(it, final_status);
        }
        it.percent = if final_status == Status::Done {
            100.0
        } else {
            it.percent
        };
        if let Err(e) = &result {
            it.error = Some(e.to_string());
        }
        if final_status == Status::Done {
            it.speed = None;
            it.eta = None;
            // 产物路径回填（MD-06 / TC-11）：下载产物必须能像本地文件一样
            // 直接进入转码/合并，否则列表里连"转码"按钮都不会出现。
            if let Some(p) = &final_path_str {
                it.path = Some(p.clone());
                it.file = Some(p.clone());
            }
        }
    });
    // 下载完成后：
    // 1. 始终清理 yt-dlp --write-thumbnail 写出的封面文件（否则残留桌面）；
    // 2. 条目还没有缩略图时，优先用这张封面做缩略图，兜底 ffmpeg 抽帧。
    //
    // 此前只在 need_thumb 时才进清理逻辑——条目已有远程缩略图时整个 block 被
    // 跳过，封面文件永久残留。
    if final_status == Status::Done {
        if let Some(out_path) = final_path {
            let (need_thumb, cover_idx) = {
                let hist = state.history.lock();
                match hist.get(id) {
                    Some(it) => (
                        it.thumb.clone().is_none(),
                        it.meta.cover_stream_index,
                    ),
                    None => (false, None),
                }
            };
            let app2 = app.clone();
            let id2 = id.to_string();
            let cache_dir = state.paths.cache_dir();
            let resolver2 = state.resolver();
            // spawn_blocking：内部是 std::fs::copy/remove 与 ffmpeg 子进程（阻塞）
            tauri::async_runtime::spawn_blocking(move || {
                let out = std::path::Path::new(&out_path);
                // 优先取 yt-dlp --write-thumbnail 写在输出目录的封面
                let written = ytdlp_core::thumbs::collect_written_thumbnail(out);
                let mut thumb_ok = false;
                if let Some(thumb_file) = written {
                    // 需要缩略图时复制到缓存；不需要时只清理
                    if need_thumb {
                        let dest = ytdlp_core::thumbs::thumb_path(&cache_dir, &id2);
                        thumb_ok = match std::fs::copy(&thumb_file, &dest) {
                            Ok(_) => {
                                log_item(&app2, &id2, format!("缩略图：取自 yt-dlp 封面 {}", thumb_file.display()));
                                true
                            }
                            Err(e) => {
                                log_item(&app2, &id2, format!("缩略图：复制封面失败（{}），回退 ffmpeg 抽帧", e));
                                false
                            }
                        };
                        if thumb_ok {
                            update_item(&app2, &id2, |it| {
                                it.thumb = Some(dest.to_string_lossy().into_owned());
                            });
                            app2.state::<AppState>().persist();
                        }
                    }
                    // 无论是否需要缩略图、复制成功与否，封面文件都不再需要。
                    // Windows 上刚下载完的文件可能被 AV 锁定，重试 5 次（每次 200ms）。
                    for attempt in 0..5 {
                        match std::fs::remove_file(&thumb_file) {
                            Ok(_) => break,
                            Err(_) if attempt < 4 => {
                                std::thread::sleep(std::time::Duration::from_millis(200));
                            }
                            Err(e) => {
                                log_item(&app2, &id2, format!("缩略图：清理封面文件失败（{}），请手动删除 {}", e, thumb_file.display()));
                            }
                        }
                    }
                } else if need_thumb {
                    log_item(&app2, &id2, "缩略图：输出目录无封面文件，ffmpeg 抽帧".to_string());
                }
                // 兜底：需要缩略图且封面文件不可用时，ffmpeg 从产物抽内嵌封面流，再不行抽首帧
                if need_thumb && !thumb_ok {
                    let dest = ytdlp_core::thumbs::thumb_path(&cache_dir, &id2);
                    if ytdlp_core::thumbs::ensure_thumb(
                        &resolver2,
                        out,
                        &dest,
                        cover_idx.map(|i| i as usize),
                        &mut |l| log_item(&app2, &id2, l),
                    )
                    .is_ok()
                    {
                        update_item(&app2, &id2, |it| {
                            it.thumb = Some(dest.to_string_lossy().into_owned());
                        });
                        app2.state::<AppState>().persist();
                    }
                }
            });
        }
    }
    // 任务结束：清理本任务私有临时目录（cookie 导出文件也在这个目录下，一并清理；
    // config/cookies/ 里的站点库文件不受影响）
    let _ = std::fs::remove_dir_all(state.paths.task_temp_dir(id));
    // 清理取消标志注册表条目（否则随任务数无限累积；也让后续 cancel_item
    // 的 cancel_flag 查询能正确区分"运行中"与"已结束"）
    state.cancels.lock().remove(id);
    // 并发 slot 由调用方（run_download_task）的 SlotGuard 释放：
    // 手写释放 + 守卫释放会重复归还，可能把下一个等待任务"超发"启动
    persist(app);
}

