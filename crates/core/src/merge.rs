//! 合并引擎（§3.5 MG）：多视频按序拼接。
//!
//! 双模式（MG-02/03）：
//! - 模式 A 同参直拼：视频编码/分辨率/帧率/音频编码/采样率一致 **且视频流
//!   extradata（SPS/PPS）一致** → concat demuxer 零重编码直拼
//! - 模式 B 异参统一：逐段统一转码（编码器跟随 设置-转码 默认编码器、分辨率
//!   统一为各段最大值、音频 aac 48kHz 立体声）后再 concat 直拼
//!
//! 输出：MP4（默认）/MKV，`+faststart`；任务私有临时目录，结束清理（稳定性需求）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::exec::{ChildGuard, Tool, ToolResolver};
use crate::model::MediaMeta;
use crate::{CoreError, Result};

/// 合并参数（合并面板内配置，不落 config.json，§3.5 说明）。
#[derive(Debug, Clone)]
pub struct MergeParams {
    pub inputs: Vec<PathBuf>,
    pub out_dir: PathBuf,
    /// 任务私有临时目录根（`<exe 同级>\temp\`，§3.7）——中间产物不得落在输出目录
    pub temp_dir: PathBuf,
    /// 输出文件名（可编辑，默认 合并_<时间戳>）
    pub filename: String,
    /// 容器：mp4 | mkv
    pub container: String,
    /// 编码器：auto | libx265 | nvenc | amf（跟随 设置-转码 默认编码器）
    pub encoder_mode: String,
    pub collision_policy: String,
    /// 合并后可选后处理：音量归一化（MG-05，面板开关）
    pub normalize_audio: bool,
    pub max_gain_db: f32,
}

impl MergeParams {
    /// 目标扩展名（容器 → 扩展，与转码共用 `paths::container_extension`，C2）。
    pub fn extension(&self) -> &'static str {
        crate::paths::container_extension(&self.container)
    }
}

/// 同参判定（MG-02）：vcodec/height/fps/acodec/sample_rate **且 extradata（SPS/PPS）**
/// 一致；音频缺失视为一致仅当所有段都无音频。
///
/// extradata **未知即判定不一致**：探测拿不到 SPS/PPS 时无法证明"零重编码直拼安全"，
/// 此时走统一转码（模式 B）比冒险直拼更安全 —— concat demuxer 对 SPS 不一致的输入
/// **不会报错**，只会从第 2 段起花屏（实测确认）。
fn same_parameters(metas: &[MediaMeta]) -> bool {
    if metas.len() < 2 {
        return false;
    }
    let first = &metas[0];
    let v = |m: &MediaMeta| {
        (
            m.vcodec.clone(),
            m.height,
            m.fps.map(|f| (f * 100.0).round() as i64),
            m.extradata.clone(),
        )
    };
    let a = |m: &MediaMeta| (m.acodec.clone(), m.sample_rate);
    let ref_v = v(first);
    let ref_a = a(first);
    metas
        .iter()
        .all(|m| m.extradata.is_some() && v(m) == ref_v && a(m) == ref_a)
}

/// 各段时长合计（进度换算基准）。
fn total_duration(metas: &[MediaMeta]) -> f64 {
    metas.iter().filter_map(|m| m.duration_secs).sum()
}

/// 输出路径（碰撞安全命名，同 TC-11；碰撞处理实现在 `paths::unique_output_path`）。
pub fn output_path(params: &MergeParams) -> Result<PathBuf> {
    let base = crate::transcode::sanitize_filename(&params.filename);
    crate::paths::unique_output_path(
        &params.out_dir,
        &base,
        params.extension(),
        &params.collision_policy,
    )
}

/// 编码器参数：委托转码侧共享参数表（P2-7；合并的 auto 语义 = libx265）。
fn encoder_args(mode: &str) -> (String, Vec<String>) {
    crate::transcode::explicit_encoder_args(if mode == "auto" { "libx265" } else { mode })
}
/// 模式 A：concat demuxer 零重编码直拼。
#[allow(clippy::too_many_arguments)]
fn concat_copy(
    resolver: &ToolResolver,
    list_file: &Path,
    out: &Path,
    container: &str,
    cancel: &Arc<AtomicBool>,
    duration: f64,
    on_progress: &mut dyn FnMut(f32),
    on_log: &mut dyn FnMut(String),
) -> Result<()> {
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-f".into(),
        "concat".into(),
        "-safe".into(),
        "0".into(),
        "-i".into(),
        list_file.to_string_lossy().into_owned(),
        "-c".into(),
        "copy".into(),
        "-map_metadata".into(),
        "0".into(),
    ];
    if container == "mp4" {
        args.push("-movflags".into());
        args.push("+faststart".into());
    }
    args.push("-y".into());
    args.push("-progress".into());
    args.push("pipe:1".into());
    args.push("-nostats".into());
    args.push("-loglevel".into());
    args.push("error".into());
    args.push(out.to_string_lossy().into_owned());

    on_log("参数一致，直拼（零重编码）…".into());
    run_piped_progress(resolver, args, cancel, duration, on_progress, on_log)?;
    if !out.exists() {
        return Err(CoreError::ProcessFailed {
            program: "ffmpeg".into(),
            code: None,
            stderr: "直拼结束但未找到输出文件".into(),
        });
    }
    Ok(())
}

/// 模式 B 单段统一转码（H.265 跟随编码器、分辨率统一、音频 aac 48k 立体声）。
#[allow(clippy::too_many_arguments)]
fn transcode_segment(
    resolver: &ToolResolver,
    input: &Path,
    out: &Path,
    main_idx: Option<u32>,
    encoder_mode: &str,
    target_h: u32,
    cancel: &Arc<AtomicBool>,
    on_progress: &mut dyn FnMut(f32),
    on_log: &mut dyn FnMut(String),
) -> Result<()> {
    let (enc, mut enc_args) = encoder_args(encoder_mode);
    if enc == "libx265" {
        enc_args.push("-tag:v:0".into());
        enc_args.push("hvc1".into());
    }
    // 目标高度取偶（奇数高度 + 非偶对齐会让 libx265 报 chroma subsampling 错误）
    let target_h = if target_h > 0 { target_h & !1 } else { 1080 };
    // 主视频用 ffprobe 的**绝对流索引**映射（§11.1）：yt-dlp 产物的封面流常排在
    // 主视频前，`0:v:0` 会选中 mjpeg 封面，转出来的段就是一张图
    let main_map = main_idx
        .map(|i| format!("0:{i}"))
        .unwrap_or_else(|| "0:v:0".into());
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        // 旋转由条目 rot_angle 单一来源（§11.13）：禁用 ffmpeg autorotate，
        // 避免源文件 rotate 标签叠加手动旋转造成双重旋转
        "-noautorotate".into(),
        "-i".into(),
        input.to_string_lossy().into_owned(),
        "-map".into(),
        main_map,
        "-map".into(),
        "0:a?".into(),
        "-vf".into(),
        // 偶数对齐：宽度 -2 自动取偶，高度 min() 封顶 + force_divisible_by=2（§11.5）
        format!(
            "scale=-2:'min(ih,{})':force_original_aspect_ratio=decrease:force_divisible_by=2",
            target_h
        ),
        "-c:v:0".into(),
        enc,
    ];
    args.extend(enc_args);
    args.push("-c:a".into());
    args.push("aac".into());
    args.push("-ar".into());
    args.push("48000".into());
    args.push("-ac".into());
    args.push("2".into());
    args.push("-map_metadata".into());
    args.push("0".into());
    // 清掉源 rotate 标签（-noautorotate 已禁自动旋转，标签残留会让播放器再转一次）
    args.push("-metadata:s:v:0".into());
    args.push("rotate=0".into());
    args.push("-movflags".into());
    args.push("+faststart".into());
    args.push("-y".into());
    args.push("-progress".into());
    args.push("pipe:1".into());
    args.push("-nostats".into());
    args.push("-loglevel".into());
    args.push("error".into());
    args.push(out.to_string_lossy().into_owned());

    on_log(format!("统一参数转码：{}", input.display()));
    run_piped_progress(resolver, args, cancel, 0.0, on_progress, on_log)?;
    if !out.exists() {
        return Err(CoreError::ProcessFailed {
            program: "ffmpeg".into(),
            code: None,
            stderr: "统一转码结束但未找到输出".into(),
        });
    }
    Ok(())
}

fn run_piped_progress(
    resolver: &ToolResolver,
    args: Vec<String>,
    cancel: &Arc<AtomicBool>,
    duration: f64,
    on_progress: &mut dyn FnMut(f32),
    on_log: &mut dyn FnMut(String),
) -> Result<()> {
    use std::process::Stdio;
    // 合并链路的所有 ffmpeg 执行（段转码/拼接/归一化）都走这里：命令行统一入日志
    on_log(crate::exec::display_command("ffmpeg", &args));
    let mut cmd = resolver.command(Tool::Ffmpeg)?;
    cmd.args(&args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut guard = ChildGuard::spawn(&mut cmd)?;
    // 用 exec::stream_lines 统一处理管道：它在独立线程排空 stderr（否则 ffmpeg
    // 报错写满管道缓冲区后不再产出 stdout，父进程也跟着阻塞），并用
    // read_until + decode_text 逐行解码（`lines()` 遇非 UTF-8 会中断整条读取）。
    let outcome = crate::exec::stream_lines(&mut guard, cancel, None, |line| {
        if duration > 0.0 {
            if let Some(us) = crate::transcode::parse_out_time_us(line) {
                let pct = ((us as f64 / 1e6) / duration * 100.0).clamp(0.0, 99.0) as f32;
                on_progress(pct);
            }
        }
    })?;
    if outcome.killed || cancel.load(Ordering::Relaxed) {
        return Err(CoreError::Cancelled);
    }
    if !outcome.status.success() {
        let err = if outcome.stderr.trim().is_empty() {
            "（无错误输出）".to_string()
        } else {
            outcome.stderr.trim().to_string()
        };
        on_log(format!("ffmpeg 失败：{}", err));
        return Err(CoreError::ProcessFailed {
            program: "ffmpeg".into(),
            code: outcome.status.code(),
            stderr: err,
        });
    }
    Ok(())
}

/// 合并主体：参数一致走模式 A 直拼，直拼失败（非取消）自动回落模式 B；
/// 参数不一致直接走模式 B。
#[allow(clippy::too_many_arguments)]
fn run_merge_inner(
    resolver: &ToolResolver,
    params: &MergeParams,
    metas: &[MediaMeta],
    tmp: &Path,
    out: &Path,
    same: bool,
    total: f64,
    cancel: &Arc<AtomicBool>,
    on_progress: &mut dyn FnMut(f32),
    on_log: &mut dyn FnMut(String),
) -> Result<()> {
    if same {
        let list = tmp.join("list.txt");
        write_concat_list(&list, &params.inputs)?;
        // 先把结果落到局部变量再 match：避免在 match 的 scrutinee 里同时借用
        // on_progress / on_log（后续 arm 里还要再用它们，重借用语义容易踩坑）
        let direct = concat_copy(
            resolver,
            &list,
            out,
            &params.container,
            cancel,
            total,
            on_progress,
            on_log,
        );
        match direct {
            Ok(()) => return Ok(()),
            // 取消不是"失败"，绝不能回落重试：用户已经不想做了
            Err(CoreError::Cancelled) => return Err(CoreError::Cancelled),
            Err(e) => {
                // 参数一致只是直拼的**必要**条件：容器、时间基、编码标签仍可能让
                // `-c copy` 失败（MKV 源写 MP4、时间基差异过大等）。此时自动降级到
                // 模式 B（统一转码后拼接），而不是把整次合并判死 —— 与转码的
                // 层级回落（GpuQsv → HybridQsv → Software）同一思路。
                on_log(format!("直拼失败（{e}），自动改为统一参数转码后拼接…"));
            }
        }
    }
    merge_unified(resolver, params, metas, tmp, out, cancel, on_progress, on_log)
}

/// 模式 B：逐段统一转码（分辨率统一为各段最大值）后 concat 直拼。
#[allow(clippy::too_many_arguments)]
fn merge_unified(
    resolver: &ToolResolver,
    params: &MergeParams,
    metas: &[MediaMeta],
    tmp: &Path,
    out: &Path,
    cancel: &Arc<AtomicBool>,
    on_progress: &mut dyn FnMut(f32),
    on_log: &mut dyn FnMut(String),
) -> Result<()> {
    let seg_total = total_duration(metas);
    let target_h = metas.iter().filter_map(|m| m.height).max().unwrap_or(1080);
    let segs: Vec<PathBuf> = (0..params.inputs.len())
        .map(|i| tmp.join(format!("seg_{:02}.mp4", i)))
        .collect();
    let mut acc = 0.0f64;
    for (i, (p, seg)) in params.inputs.iter().zip(&segs).enumerate() {
        let d = metas[i].duration_secs.unwrap_or(0.0);
        let seg_dur = if seg_total > 0.0 { d } else { 1.0 };
        let base = if seg_total > 0.0 {
            (acc / seg_total * 85.0) as f32
        } else {
            (i as f32 / params.inputs.len() as f32) * 85.0
        };
        let span = if seg_total > 0.0 {
            (seg_dur / seg_total * 85.0) as f32
        } else {
            85.0 / params.inputs.len() as f32
        };
        let seg_base = base;
        let seg_span = span.max(0.5);
        let mut prog = |pct: f32| on_progress(seg_base + pct * 0.01 * seg_span);
        transcode_segment(
            resolver,
            p,
            seg,
            metas[i].video_stream_index,
            &params.encoder_mode,
            target_h,
            cancel,
            &mut prog,
            on_log,
        )?;
        acc += d;
    }
    let list = tmp.join("list.txt");
    write_concat_list(&list, &segs)?;
    let mut prog = |pct: f32| on_progress(85.0 + pct * 0.01 * 15.0);
    concat_copy(
        resolver,
        &list,
        out,
        &params.container,
        cancel,
        seg_total,
        &mut prog,
        on_log,
    )
}

/// 执行合并，返回输出路径；取消清理临时目录与输出残留（UL-06 适用合并）。
pub fn run_merge(
    resolver: &ToolResolver,
    params: &MergeParams,
    cancel: &Arc<AtomicBool>,
    mut on_progress: impl FnMut(f32),
    mut on_log: impl FnMut(String),
) -> Result<PathBuf> {
    if params.inputs.len() < 2 {
        return Err(CoreError::Io(std::io::Error::other(
            "合并至少需要 2 个输入",
        )));
    }

    // 1) 探测全部输入
    let mut metas = Vec::with_capacity(params.inputs.len());
    on_log("探测输入参数…".into());
    for p in &params.inputs {
        let m = crate::download::probe_output(resolver, p, &mut on_log)?;
        metas.push(m);
    }
    let total = total_duration(&metas);
    let same = same_parameters(&metas);
    if !same {
        on_log("输入参数不一致（编码/分辨率/帧率/音频/采样率），按统一模式处理".into());
    }

    // 2) 任务私有临时目录（§3.7：中间产物一律在 <exe 同级>\temp\，不污染输出目录）
    let task_id = uuid::Uuid::new_v4().to_string();
    let tmp = params.temp_dir.join(format!("merge_{}", task_id));
    std::fs::create_dir_all(&tmp)?;
    // 输出名放在最后抢占：output_path 内部 create_new 占位，提前占位会让
    // "探测失败"这类早退路径在用户输出目录留下 0 字节垃圾文件
    let out = output_path(params)?;
    let cleanup = |t: &Path, out: &Path| {
        let _ = std::fs::remove_dir_all(t);
        let _ = std::fs::remove_file(out);
    };
    if params.normalize_audio {
        on_log("合并完成前做音量归一化…".into());
    }
    let result = run_merge_inner(
        resolver,
        params,
        &metas,
        &tmp,
        &out,
        same,
        total,
        cancel,
        &mut on_progress,
        &mut on_log,
    )
    .map(|()| out.clone());

    match result {
        Ok(mut o) => {
            let _ = std::fs::remove_dir_all(&tmp);
            if params.normalize_audio {
                match post_normalize(
                    resolver,
                    &o,
                    params.max_gain_db,
                    &params.container,
                    &params.temp_dir,
                    cancel,
                    &mut on_log,
                ) {
                    Ok(n) => o = n,
                    Err(CoreError::Cancelled) => {
                        let _ = std::fs::remove_file(&o);
                        on_log("音量归一化已取消，清理输出".into());
                        return Err(CoreError::Cancelled);
                    }
                    Err(e) => {
                        on_log(format!("音量归一化失败，保留合并产物：{}", e));
                    }
                }
            }
            on_log(format!("合并完成：{}", o.display()));
            Ok(o)
        }
        Err(CoreError::Cancelled) => {
            cleanup(&tmp, &out);
            on_log("合并已取消，清理临时目录与输出残留".into());
            Err(CoreError::Cancelled)
        }
        Err(e) => {
            cleanup(&tmp, &out);
            on_log(format!("合并失败，已清理临时目录与半成品：{}", e));
            Err(e)
        }
    }
}

/// 合并产物音量归一化（MG-05）：probe 音量 → 增益至峰值 0dBFS（不超过上限），
/// 视频流 copy、音频重编码 aac；中间文件写在任务临时目录，成功后原子替换。
fn post_normalize(
    resolver: &ToolResolver,
    input: &Path,
    max_gain_db: f32,
    container: &str,
    temp_dir: &Path,
    cancel: &Arc<AtomicBool>,
    on_log: &mut dyn FnMut(String),
) -> Result<PathBuf> {
    let meta = crate::download::probe_output(resolver, input, &mut *on_log)?;
    let Some(max_v) = meta.audio_volume.max_volume_db else {
        return Ok(input.to_path_buf());
    };
    if max_v >= -0.5 || max_v <= -100.0 {
        return Ok(input.to_path_buf());
    }
    let gain = (-max_v).clamp(0.0, max_gain_db.max(0.0));
    if gain < 0.1 {
        return Ok(input.to_path_buf());
    }
    let ext = input.extension().and_then(|e| e.to_str()).unwrap_or("mp4");
    std::fs::create_dir_all(temp_dir)?;
    let tmp = temp_dir.join(format!("norm_{}.{}", uuid::Uuid::new_v4(), ext));
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-i".into(),
        input.to_string_lossy().into_owned(),
        "-map".into(),
        "0".into(),
        "-c:v".into(),
        "copy".into(),
        "-c:a".into(),
        "aac".into(),
        "-af".into(),
        format!("volume={:.2}dB", gain),
    ];
    // `-movflags` 只对 mp4/mov 系列封装有意义；MKV 下实测被静默忽略（exit 0、无告警），
    // 传了无害，但没必要 —— 这里只在非 mkv 时附加
    if container != "mkv" {
        args.push("-movflags".into());
        args.push("+faststart".into());
    }
    args.push("-y".into());
    args.push(tmp.to_string_lossy().into_owned());
    on_log(format!("音量归一化：+{:.1}dB", gain));
    run_piped_progress(resolver, args, cancel, 0.0, &mut |_| {}, on_log)?;
    if !tmp.exists() {
        return Err(CoreError::ProcessFailed {
            program: "ffmpeg".into(),
            code: None,
            stderr: "音量归一化未生成输出".into(),
        });
    }
    // 原子替换：rename 直接覆盖（Windows MOVEFILE_REPLACE_EXISTING），
    // 不再"先删后改名"（中途失败会连合并产物一起丢）
    if let Err(e) = std::fs::rename(&tmp, input) {
        let _ = std::fs::remove_file(&tmp);
        return Err(CoreError::Io(std::io::Error::other(format!(
            "音量归一化产物替换失败：{e}"
        ))));
    }
    Ok(input.to_path_buf())
}

fn write_concat_list(path: &Path, inputs: &[PathBuf]) -> Result<()> {
    let mut content = String::new();
    for p in inputs {
        let s = p.to_string_lossy();
        // 单引号转义（ffmpeg concat 文件内转义规则）
        let escaped = s.replace('\'', "'\\''");
        content.push_str(&format!("file '{}'\n", escaped));
    }
    std::fs::write(path, content)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AudioVolume;

    fn meta(
        vcodec: &str,
        h: u32,
        fps: f64,
        acodec: &str,
        sr: u32,
        ext: &str,
        dur: f64,
    ) -> MediaMeta {
        MediaMeta {
            vcodec: Some(vcodec.into()),
            height: Some(h),
            fps: Some(fps),
            acodec: Some(acodec.into()),
            sample_rate: Some(sr),
            extradata: Some(ext.into()),
            duration_secs: Some(dur),
            audio_volume: AudioVolume::default(),
            ..Default::default()
        }
    }

    #[test]
    fn same_params_true_when_identical() {
        let ms = vec![
            meta("h264", 1080, 30.0, "aac", 48000, "aabb", 10.0),
            meta("h264", 1080, 30.0, "aac", 48000, "aabb", 20.0),
        ];
        assert!(same_parameters(&ms));
    }

    #[test]
    fn same_params_false_on_extradata_diff() {
        let ms = vec![
            meta("h264", 1080, 30.0, "aac", 48000, "aabb", 10.0),
            meta("h264", 1080, 30.0, "aac", 48000, "ccdd", 20.0),
        ];
        assert!(!same_parameters(&ms));
    }

    #[test]
    fn same_params_false_on_res_diff() {
        let ms = vec![
            meta("h264", 1080, 30.0, "aac", 48000, "aabb", 10.0),
            meta("h264", 720, 30.0, "aac", 48000, "aabb", 20.0),
        ];
        assert!(!same_parameters(&ms));
    }

    #[test]
    fn same_params_false_on_sample_rate_diff() {
        let ms = vec![
            meta("h264", 1080, 30.0, "aac", 48000, "aabb", 10.0),
            meta("h264", 1080, 30.0, "aac", 44100, "aabb", 20.0),
        ];
        assert!(!same_parameters(&ms));
    }

    #[test]
    fn same_params_false_when_extradata_unknown() {
        // 探测拿不到 SPS/PPS 时不能宣称"参数一致"（否则直拼可能花屏且 ffmpeg 不报错），
        // 必须退回统一转码路径
        let mut a = meta("h264", 1080, 30.0, "aac", 48000, "aabb", 10.0);
        a.extradata = None;
        let b = meta("h264", 1080, 30.0, "aac", 48000, "aabb", 20.0);
        assert!(!same_parameters(&[a, b]));
    }

    /// 测试用合并参数（默认 mp4 / auto）。
    fn merge_params_mk(dir: &Path) -> MergeParams {
        MergeParams {
            inputs: vec![],
            out_dir: dir.to_path_buf(),
            temp_dir: dir.join("temp"),
            filename: "合并_x".into(),
            container: "mp4".into(),
            encoder_mode: "auto".into(),
            collision_policy: "auto_inc".into(),
            normalize_audio: false,
            max_gain_db: 24.0,
        }
    }

    #[test]
    fn output_path_auto_inc() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = merge_params_mk(dir.path());
        p.filename = "合并_20260917".into();
        let a = output_path(&p).unwrap();
        std::fs::write(&a, b"x").unwrap();
        let b = output_path(&p).unwrap();
        assert_eq!(b.file_name().unwrap(), "合并_20260917 (1).mp4");
    }

    #[test]
    fn output_path_skip() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = merge_params_mk(dir.path());
        p.filename = "合并_x".into();
        p.container = "mkv".into();
        p.collision_policy = "skip".into();
        let a = output_path(&p).unwrap();
        std::fs::write(&a, b"x").unwrap();
        assert!(output_path(&p).is_err());
    }

    #[test]
    fn concat_list_escapes_quote() {
        let dir = tempfile::tempdir().unwrap();
        let list = dir.path().join("list.txt");
        write_concat_list(&list, &[PathBuf::from("a'b.mp4")]).unwrap();
        let s = std::fs::read_to_string(&list).unwrap();
        assert_eq!(s, "file 'a'\\''b.mp4'\n");
    }
}
