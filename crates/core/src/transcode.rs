//! 转码引擎（§3.4 TC）：ffmpeg 子进程。
//!
//! 输出 H.265（QSV/NVENC/AMF/libx265），支持：手动旋转（条目 rot_angle，TC-04）、
//! 分辨率上限（MAXW/MAXH）、码率封顶、音量归一化（增益上限，通用段）、保留封面、
//! 文件名模板（下载段共用）+ 碰撞安全命名（auto_inc/skip）、取消清理输出残留（UL-06）。
//! x265 CRF 固定 23（不落配置，TC 需求）。
//!
//! 进度：`ffmpeg -progress pipe:1 -nostats`，按 `out_time_us` 相对探测时长换算百分比。

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use parking_lot::Mutex;

use crate::exec::{decode_text, ChildGuard, Tool, ToolResolver};
use crate::model::{MediaMeta, RotAngle};
use crate::{CoreError, Result};

/// 全局锁定的转码层级（参考 convert_h265.bat 的 MODE 锁定思想）。
///
/// 第一个文件成功编码后锁定层级，后续文件直接从该层级开始，
/// 避免每个文件都先尝试 QSV（在不支持的机器上每次浪费数秒）。
/// `force_encoder_mode != "auto"` 时不锁定（用户显式指定了编码器）。
static LOCKED_TIER: OnceLock<Mutex<Option<TranscodeTier>>> = OnceLock::new();

fn locked_tier() -> Option<TranscodeTier> {
    *LOCKED_TIER.get_or_init(|| Mutex::new(None)).lock()
}

fn set_locked_tier(tier: TranscodeTier) {
    let mut cur = LOCKED_TIER.get_or_init(|| Mutex::new(None)).lock();
    // 只由第一个成功的层级写入：并发任务各自"先读后写"时，后来的写入者不得
    // 覆盖已有结论（两者通常相同，但确定性比"最后写入者胜"更可解释）
    if cur.is_none() {
        *cur = Some(tier);
    }
}

/// 转码参数（来自 设置-转码/通用/下载 + 条目 rot_angle，TC-05）。
#[derive(Debug, Clone)]
pub struct TranscodeParams {
    pub input: PathBuf,
    pub out_dir: PathBuf,
    /// 输出命名标题（文件名模板输入）
    pub title: String,
    /// 文件名模板：纯标题/标题+ID/UP主-标题/日期-标题（下载段共用）
    pub filename_template: String,
    /// 容器：mp4 | mkv（P0 仅两者，§TC-12）
    pub container: String,
    /// 编码器模式：auto | libx265 | nvenc | amf（自动 = QSV → libx265 兜底）
    pub encoder_mode: String,
    /// QSV low_power（仅 auto→QSV 时生效）
    pub low_power: bool,
    pub max_w: u32,
    pub max_h: u32,
    pub brcap_kbps: Option<u32>,
    /// 兜底码率 kbps：封顶留空时的 maxrate 兜底（0 = 不兜底）
    pub br_default_kbps: u32,
    pub normalize_audio: bool,
    pub max_gain_db: f32,
    pub rot_angle: RotAngle,
    pub keep_cover: bool,
    /// auto_inc | skip（§TC-11 碰撞命名策略）
    pub collision_policy: String,
}

impl TranscodeParams {
    /// 目标扩展名（容器 → 扩展，与合并共用 `paths::container_extension`，C2）。
    pub fn extension(&self) -> &'static str {
        crate::paths::container_extension(&self.container)
    }

    /// 输出路径（模板命名 + 碰撞策略；skip 且已存在时返回 Err。
    /// 碰撞处理实现在 `paths::unique_output_path`，与合并共用，C2）。
    pub fn output_path(&self) -> Result<PathBuf> {
        let base = apply_filename_template(&self.filename_template, &self.title);
        crate::paths::unique_output_path(
            &self.out_dir,
            &base,
            self.extension(),
            &self.collision_policy,
        )
    }
}

/// 文件名模板（§设置-下载，下载/转码/合并输出共用）。
///
/// - 纯标题：`{title}`
/// - 标题+ID：`{title}-{id 前 8 位}`
/// - UP主-标题：本地条目无 UP 主字段，回退为 `{title}`（下载侧由 yt-dlp 模板实现）
/// - 日期-标题：`{YYYY-MM-DD}-{title}`（本地时区，见 timefmt 模块说明）
pub fn apply_filename_template(tmpl: &str, title: &str) -> String {
    let title = strip_media_ext(&sanitize_filename(title));
    match tmpl {
        "标题+ID" => format!("{}-{}", title, id_hint(title.as_str())),
        "日期-标题" => format!(
            "{}-{}",
            crate::timefmt::date_str(
                crate::timefmt::now_secs(),
                crate::timefmt::local_offset_secs()
            ),
            title
        ),
        "UP主-标题" => title,
        _ => title,
    }
}

/// 去掉常见媒体扩展名（本地条目标题带 .mp4 时避免输出 "x.mp4.mp4"）。
fn strip_media_ext(name: &str) -> String {
    const MEDIA_EXTS: &[&str] = &[
        "mp4", "mkv", "mov", "avi", "wmv", "flv", "webm", "m4v", "mpg", "mpeg", "ts", "m2ts",
        "3gp", "rm", "rmvb", "vob", "mts", "m4a", "aac", "mp3", "flac", "wav", "ogg", "opus",
    ];
    let p = std::path::Path::new(name);
    if let (Some(stem), Some(ext)) = (p.file_stem(), p.extension()) {
        let e = ext.to_string_lossy().to_lowercase();
        if MEDIA_EXTS.contains(&e.as_str()) {
            return stem.to_string_lossy().into_owned();
        }
    }
    name.to_string()
}

/// 文件名清洗（Windows 非法字符 / 截断）。
pub fn sanitize_filename(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    let t = cleaned.trim().trim_end_matches('.').to_string();
    if t.is_empty() {
        "未命名".into()
    } else {
        t.chars().take(120).collect()
    }
}

fn id_hint(title: &str) -> String {
    // 本地转码无稳定 ID 字段：用标题长度做短指纹，保证不同文件不互相覆盖
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    title.hash(&mut h);
    format!("{:08x}", h.finish() & 0xFFFF_FFFF)
}

fn sv(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// 显式模式的编码器参数表（libx265 / nvenc / amf；auto 由调用方决定语义，P2-7）。
/// 转码与合并共用，消除两张逐字相同的参数表。
pub fn explicit_encoder_args(mode: &str) -> (String, Vec<String>) {
    match mode {
        "nvenc" => (
            "hevc_nvenc".into(),
            sv(&["-rc", "vbr", "-cq", "23", "-preset", "p5"]),
        ),
        "amf" => (
            "hevc_amf".into(),
            sv(&["-qp_i", "23", "-qp_p", "23", "-quality", "balanced"]),
        ),
        _ => ("libx265".into(), sv(&["-crf", "23", "-preset", "medium"])),
    }
}

/// 编码器选择（TC-14：auto = QSV 可用则 hevc_qsv，否则 libx265 兜底；
/// 显式 nvenc/amf/libx265 不被自动覆盖）。
fn pick_encoder(
    resolver: &ToolResolver,
    mode: &str,
    low_power: bool,
) -> Result<(String, Vec<String>)> {
    let (enc, args) = explicit_encoder_args(mode);
    match mode {
        "libx265" => Ok((enc, args)),
        "nvenc" => Ok((enc, args)),
        "amf" => Ok((enc, args)),
        _ => {
            if qsv_available(resolver)? {
                let mut args = sv(&["-global_quality", "23"]);
                if low_power {
                    args.push("-low_power".into());
                    args.push("1".into());
                }
                Ok(("hevc_qsv".into(), args))
            } else {
                Ok(("libx265".into(), sv(&["-crf", "23", "-preset", "medium"])))
            }
        }
    }
}

/// 可用硬件编码器探测结果（TC-14）。
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct HwEncoders {
    pub qsv: bool,
    pub nvenc: bool,
    pub amf: bool,
}

/// 解析 `ffmpeg -encoders` 输出（纯函数，可单测）。
pub fn parse_encoders_output(text: &str) -> HwEncoders {
    let mut hw = HwEncoders::default();
    for line in text.lines() {
        if line.contains("hevc_qsv") {
            hw.qsv = true;
        } else if line.contains("hevc_nvenc") {
            hw.nvenc = true;
        } else if line.contains("hevc_amf") {
            hw.amf = true;
        }
    }
    hw
}

/// 探测 ffmpeg 是否内置 QSV（hevc_qsv）编码器。
pub fn qsv_available(resolver: &ToolResolver) -> Result<bool> {
    Ok(detect_hw_encoders(resolver)?.qsv)
}

/// 探测可用硬件编码器（QSV/NVENC/AMF，TC-14）。
/// 带截止时间：设置页"编码器探测"卡死会连带把探测命令所在线程一起占住。
pub fn detect_hw_encoders(resolver: &ToolResolver) -> Result<HwEncoders> {
    let mut cmd = resolver.command(Tool::Ffmpeg)?;
    cmd.arg("-encoders");
    let out = crate::exec::run_capture_deadline(cmd, std::time::Duration::from_secs(30))?;
    let text = decode_text(&out.stdout);
    Ok(parse_encoders_output(&text))
}

/// 视频滤镜链：旋转（transpose）→ 分辨率上限（不放大）。
///
/// 上限语义：`max_h` = **短边**上限、`max_w` = 长边上限（§TC-05）。
/// 关键：transpose 之后 `iw`/`ih` 是**互换过**的（1920×1080 转 90° 后 ih=1920），
/// 直接写 `min(ih,max_h)` 会把原视频的**长边**当短边砍（1080P 转 90° 得 606×1080）。
/// 因此用旋转不变量表达：短边 = `min(iw,ih)`、长边 = `max(iw,ih)`——无论转不转都成立。
/// 缩放系数 s = min(1, max_h/短边, max_w/长边)，宽高各自 `trunc(*s/2)*2` 保偶数边长
/// （奇数宽会让 libx265/QSV 直接报 chroma subsampling 错误）。
fn build_vf(rot: RotAngle, max_w: u32, max_h: u32) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    match rot.degrees() {
        90 => parts.push("transpose=1".into()),
        180 => parts.push("transpose=1,transpose=1".into()),
        270 => parts.push("transpose=2".into()),
        _ => {}
    }
    if max_w > 0 || max_h > 0 {
        // 缩放系数 s = min(1, MAXH/短边, MAXW/长边)。
        // 注意：ffmpeg 表达式求值器的 min()/max() **只接受两个参数**，三参数写法
        // 会在滤镜初始化时报 "Cannot parse expression for width"（实测）——必须两两嵌套。
        let mut s = "1".to_string();
        if max_h > 0 {
            s = format!("min({},{}/min(iw,ih))", s, max_h);
        }
        if max_w > 0 {
            s = format!("min({},{}/max(iw,ih))", s, max_w);
        }
        parts.push(format!("scale='trunc(iw*{s}/2)*2':'trunc(ih*{s}/2)*2'"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(","))
    }
}

/// 转码执行层级（参考 BatchConverter convert_h265.bat MODE 1/2/3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscodeTier {
    /// MODE 1 全 GPU：QSV 解码 → vpp_qsv/scale_qsv → hevc_qsv（最快）
    GpuQsv,
    /// MODE 2 混合：QSV 解码 → hwdownload → CPU 滤镜 → hevc_qsv
    HybridQsv,
    /// MODE 3 全软件：CPU 解码 → CPU 滤镜 → libx265（总是可用）
    Software,
}

impl TranscodeTier {
    pub fn label(self) -> &'static str {
        match self {
            Self::GpuQsv => "全 GPU（QSV 解码 → vpp_qsv/scale_qsv → hevc_qsv）",
            Self::HybridQsv => "混合（QSV 解码 → hwdownload → CPU 滤镜 → hevc_qsv）",
            Self::Software => "全软件（CPU 解码 → CPU 滤镜 → libx265）",
        }
    }
}

/// 构造指定层级的 ffmpeg 参数（TC-03/TC-05/TC-07/TC-08/TC-10）。
///
/// 层级语义（与 convert_h265.bat 对齐）：
/// - `GpuQsv`：`-hwaccel qsv -hwaccel_output_format qsv` → `vpp_qsv`（旋转）/
///   `scale_qsv`（缩放）→ `hevc_qsv`，全程 GPU；
/// - `HybridQsv`：`-hwaccel qsv`（QSV 帧进滤镜图）→ `hwdownload,format=nv12` →
///   CPU 滤镜 → `hevc_qsv`；
/// - `Software`：CPU 解码 → CPU 滤镜 → `libx265`（显式 nvenc/amf 也走本层，
///   仅替换编码器，解码与滤镜留在 CPU）。
///
/// 旋转/上限表达式：`-noautorotate` + `-display_rotation 0` 双重禁用自动旋转，
/// 旋转只由条目 `rot_angle` 驱动；vpp_qsv 先缩放后转置（尺寸表达式取转置前
/// 的 iw/ih，与 CPU 链的转置前语义一致，见 bat 内注释）。
pub fn build_args_for_tier(
    resolver: &ToolResolver,
    params: &TranscodeParams,
    meta: &MediaMeta,
    tier: TranscodeTier,
) -> Result<Vec<String>> {
    let rotated = params.rot_angle.degrees() != 0;
    let hw = !matches!(tier, TranscodeTier::Software);

    // —— 编码器与编码参数 ——
    // QSV 层固定 hevc_qsv（ABR 码率三件套在下方按源码率装配）；
    // 软件层按显式模式选择（auto 在软件层就是 libx265——QSV 已由上层尝试过）。
    let (encoder, mut enc_args) = match tier {
        TranscodeTier::Software => {
            let mode = match params.encoder_mode.as_str() {
                "nvenc" | "amf" => params.encoder_mode.as_str(),
                _ => "libx265",
            };
            pick_encoder(resolver, mode, params.low_power)?
        }
        _ => {
            let mut a: Vec<String> = vec![
                "-preset".into(),
                "veryfast".into(),
                "-extbrc".into(),
                "1".into(),
            ];
            if tier == TranscodeTier::GpuQsv && params.low_power {
                a.push("-low_power".into());
                a.push("1".into());
            }
            ("hevc_qsv".into(), a)
        }
    };
    if params.container == "mp4" && encoder == "libx265" {
        // hvc1 标签（Apple 兼容）。必须带 `:v:0`：不带流后缀的 `-tag:v`
        // 会落到封面流（mjpeg）上，mp4 封装报
        // `Tag hvc1 incompatible with output codec id '7' (mp4v)` 并失败
        enc_args.push("-tag:v:0".into());
        enc_args.push("hvc1".into());
    }

    // —— 音频增益（normalize_audio + 解析音量；接近满度/无音量不处理）——
    // 只处理主音频：-map 0:a:0? 只保留第一条音轨，其余抛弃，
    // volumedetect 峰值与增益目标始终是同一条流。
    let need_gain = meta.needs_audio_gain(params.normalize_audio);
    let gain = if need_gain {
        let max_v = meta.audio_volume.max_volume_db.unwrap_or(0.0);
        Some((-max_v).clamp(0.0, params.max_gain_db.max(0.0)))
    } else {
        None
    };

    // —— 滤镜链 ——
    // CPU 链：旋转（transpose）→ 分辨率上限（旋转不变量 min(iw,ih)/max(iw,ih)，
    // min 两两嵌套——ffmpeg 求值器 min/max 只收两个参数）。
    let cpu_vf = build_vf(params.rot_angle, params.max_w, params.max_h);
    // GPU 链：vpp_qsv 先缩放后转置（尺寸表达式取转置前的 iw/ih），180° 用两次
    // transpose；scale_qsv 的 w/h 与 vpp_qsv 同式（表达式由 ffmpeg 求值）。
    let gpu_vf: Option<String> = (tier == TranscodeTier::GpuQsv).then(|| {
        let mut sc = "1".to_string();
        if params.max_h > 0 {
            sc = format!("min({},{}/min(iw,ih))", sc, params.max_h);
        }
        if params.max_w > 0 {
            sc = format!("min({},{}/max(iw,ih))", sc, params.max_w);
        }
        let wsc = format!("floor(iw*{sc}/2)*2");
        let hsc = format!("floor(ih*{sc}/2)*2");
        match params.rot_angle.degrees() {
            90 => format!("vpp_qsv=transpose=clock:w='{wsc}':h='{hsc}'"),
            270 => format!("vpp_qsv=transpose=cclock:w='{wsc}':h='{hsc}'"),
            180 => format!(
                "vpp_qsv=transpose=clock,vpp_qsv=transpose=clock,scale_qsv=w='{wsc}':h='{hsc}'"
            ),
            _ => format!("scale_qsv=w='{wsc}':h='{hsc}'"),
        }
    });
    let hybrid_main = (tier == TranscodeTier::HybridQsv)
        .then(|| {
            cpu_vf
                .as_ref()
                .map(|vf| format!("hwdownload,format=nv12,{vf}"))
        })
        .flatten();

    // —— 流定位 ——
    // 封面流（attached_pic）可能排在主视频前（yt-dlp 常见），绝对索引必须指向
    // 真正的主视频；探测不到封面流（MKV 附件型）时回退 `0:t?` + `-c:t copy`。
    let main_idx = meta.video_stream_index;
    let cover_idx = if params.keep_cover {
        meta.cover_stream_index
    } else {
        None
    };
    let map_attachments = params.keep_cover && cover_idx.is_none();
    let main_label = main_idx
        .map(|i| format!("0:{i}"))
        .unwrap_or_else(|| "0:v:0".into());

    // —— 输入（解码层）——
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        // 旋转由条目 rot_angle 单一来源（§11.13）：-noautorotate 禁 ffmpeg 自动
        // 转置，-display_rotation 0 清除源显示矩阵（ffmpeg 仍会复制该矩阵到
        // 输出流，不清掉播放器会再转一次，实测见 bat 注释）
        "-noautorotate".into(),
        "-display_rotation".into(),
        "0".into(),
    ];
    if hw {
        args.push("-hwaccel".into());
        args.push("qsv".into());
        if tier == TranscodeTier::GpuQsv {
            args.push("-hwaccel_output_format".into());
            args.push("qsv".into());
        }
    }
    args.push("-i".into());
    args.push(params.input.to_string_lossy().into_owned());
    // 旋转 + 保留封面（hw 层）：QSV 帧无法进 CPU 滤镜，封面改由第二个软件解码
    // 输入取出并旋转（与 bat 的 DIN 第二输入同思路）
    let cover_reencode = rotated && cover_idx.is_some();
    if cover_reencode {
        args.push("-i".into());
        args.push(params.input.to_string_lossy().into_owned());
    }

    // —— 主视频滤镜 + 流映射 ——
    let cpu_vf_for_cover = || {
        cpu_vf
            .as_ref()
            .cloned()
            .unwrap_or_else(|| "transpose=1".into())
    };
    match tier {
        TranscodeTier::Software => {
            // 软件层维持既有结构：旋转封面与主视频共用 filter_complex
            match (&cpu_vf, cover_idx) {
                (Some(filter), Some(ci)) => {
                    let src = main_idx
                        .map(|i| format!("0:{i}"))
                        .unwrap_or_else(|| "0:v:0".to_string());
                    args.push("-filter_complex".into());
                    if rotated {
                        // 封面流 copy 不会跟随 transpose —— 旋转时封面也过同一滤镜链，
                        // 重编码 mjpeg 保持 attached_pic 语义（见下方 -c:v:1）
                        args.push(format!("[{src}]{filter}[v];[0:{ci}]{filter}[cv]"));
                        args.push("-map".into());
                        args.push("[v]".into());
                        args.push("-map".into());
                        args.push("0:a:0?".into());
                        args.push("-map".into());
                        args.push("[cv]".into());
                    } else {
                        args.push(format!("[{src}]{filter}[v]"));
                        args.push("-map".into());
                        args.push("[v]".into());
                        args.push("-map".into());
                        args.push("0:a:0?".into());
                        args.push("-map".into());
                        args.push(format!("0:{ci}?"));
                    }
                }
                _ => {
                    if let Some(filter) = &cpu_vf {
                        args.push("-vf".into());
                        args.push(filter.clone());
                    }
                    args.push("-map".into());
                    args.push(
                        main_idx
                            .map(|i| format!("0:{i}?"))
                            .unwrap_or_else(|| "0:v:0?".to_string()),
                    );
                    args.push("-map".into());
                    args.push("0:a:0?".into());
                    if let Some(ci) = cover_idx {
                        args.push("-map".into());
                        args.push(format!("0:{ci}?"));
                    }
                }
            }
        }
        TranscodeTier::GpuQsv => {
            if let Some(f) = &gpu_vf {
                args.push("-filter:v:0".into());
                args.push(f.clone());
            }
            args.push("-map".into());
            args.push(format!("{main_label}?"));
            args.push("-map".into());
            args.push("0:a:0?".into());
            if rotated {
                if let Some(ci) = cover_idx {
                    // 封面从第二软件输入取，CPU 滤镜旋转
                    args.push("-map".into());
                    args.push(format!("1:{ci}?"));
                    args.push("-filter:v:1".into());
                    args.push(cpu_vf_for_cover());
                }
            } else if let Some(ci) = cover_idx {
                args.push("-map".into());
                args.push(format!("0:{ci}?"));
            }
        }
        TranscodeTier::HybridQsv => {
            if let Some(f) = &hybrid_main {
                args.push("-filter:v:0".into());
                args.push(f.clone());
            }
            args.push("-map".into());
            args.push(format!("{main_label}?"));
            args.push("-map".into());
            args.push("0:a:0?".into());
            if rotated {
                if let Some(ci) = cover_idx {
                    args.push("-map".into());
                    args.push(format!("1:{ci}?"));
                    args.push("-filter:v:1".into());
                    args.push(cpu_vf_for_cover());
                }
            } else if let Some(ci) = cover_idx {
                args.push("-map".into());
                args.push(format!("0:{ci}?"));
            }
        }
    }
    if map_attachments {
        args.push("-map".into());
        args.push("0:t?".into());
    }

    // —— 编码器 / 封面编码 / 音频 ——
    args.push("-c:v:0".into());
    args.push(encoder);
    args.extend(enc_args);
    if hw {
        // QSV 层码率三件套：源码率（缺省用兜底码率），封顶截断；
        // maxrate = 1.2×、bufsize = 2×（bat 同款）
        let src = meta
            .vbitrate_kbps
            .or(if params.br_default_kbps > 0 {
                Some(params.br_default_kbps)
            } else {
                None
            })
            .map(|src| match params.brcap_kbps.filter(|c| *c > 0) {
                Some(cap) => src.min(cap),
                None => src,
            })
            .filter(|kb| *kb > 0);
        if let Some(kb) = src {
            let maxkb = kb * 12 / 10;
            args.push("-b:v".into());
            args.push(format!("{kb}k"));
            args.push("-maxrate".into());
            args.push(format!("{maxkb}k"));
            args.push("-bufsize".into());
            args.push(format!("{}k", kb * 2));
        }
        if params.container == "mp4" {
            args.push("-tag:v:0".into());
            args.push("hvc1".into());
        }
        if rotated && cover_idx.is_some() {
            // 旋转过的封面已重编码，显式标回 attached_pic
            args.push("-c:v:1".into());
            args.push("mjpeg".into());
            args.push("-q:v:1".into());
            args.push("2".into());
            args.push("-disposition:v:1".into());
            args.push("attached_pic".into());
        } else if cover_idx.is_some() {
            args.push("-c:v:1".into());
            args.push("copy".into());
        }
    } else if cover_idx.is_some() {
        if rotated {
            args.push("-c:v:1".into());
            args.push("mjpeg".into());
            args.push("-q:v:1".into());
            args.push("2".into());
            args.push("-disposition:v:1".into());
            args.push("attached_pic".into());
        } else {
            args.push("-c:v:1".into());
            args.push("copy".into());
        }
    }
    args.push("-c:a".into());
    args.push(if gain.is_some() { "aac" } else { "copy" }.into());
    if let Some(g) = gain {
        if g > 0.1 {
            args.push("-af".into());
            args.push(format!("volume={:.2}dB", g));
        }
        // 音频码率跟随源，clamp 64-192k（MediaMeta::audio_bitrate_kbps，
        // convert_h265.bat 同款：低码率源不膨胀，高码率源不浪费）
        let abr = meta.audio_bitrate_kbps();
        args.push("-b:a".into());
        args.push(format!("{}k", abr));
    }
    if map_attachments {
        args.push("-c:t".into());
        args.push("copy".into());
    }
    args.push("-map_metadata".into());
    args.push("0".into());
    // 配合 -noautorotate / -display_rotation：清除源 rotate 标签，
    // 防止播放器再按标签自动转一次
    args.push("-metadata:s:v:0".into());
    args.push("rotate=0".into());
    match params.container.as_str() {
        "mkv" => args.push("-f".into()),
        _ => {
            args.push("-movflags".into());
            args.push("+faststart".into());
            args.push("-f".into());
        }
    }
    if params.container == "mkv" {
        args.push("matroska".into());
    } else {
        args.push("mp4".into());
    }
    args.push("-y".into());
    args.push("-progress".into());
    args.push("pipe:1".into());
    args.push("-nostats".into());
    args.push("-loglevel".into());
    args.push("error".into());
    Ok(args)
}

/// 软件层参数（既有入口；单测与旧调用方共用）。
pub fn build_args(
    resolver: &ToolResolver,
    params: &TranscodeParams,
    meta: &MediaMeta,
) -> Result<Vec<String>> {
    build_args_for_tier(resolver, params, meta, TranscodeTier::Software)
}

/// 解析 `-progress` 输出中的 `out_time_us=`（微秒）。
pub(crate) fn parse_out_time_us(line: &str) -> Option<u64> {
    let line = line.trim();
    if let Some(v) = line.strip_prefix("out_time_us=") {
        return v.trim().parse().ok();
    }
    if let Some(v) = line.strip_prefix("out_time_ms=") {
        return v.trim().parse::<u64>().ok().map(|ms| ms * 1000);
    }
    // 新版 ffmpeg 输出 out_time=HH:MM:SS.xx 格式
    if let Some(v) = line.strip_prefix("out_time=") {
        let v = v.trim();
        let parts: Vec<&str> = v.split(':').collect();
        if parts.len() == 3 {
            let h: f64 = parts[0].parse().ok()?;
            let m: f64 = parts[1].parse().ok()?;
            let s: f64 = parts[2].parse().ok()?;
            let total_us = ((h * 3600.0 + m * 60.0 + s) * 1_000_000.0) as u64;
            return Some(total_us);
        }
    }
    None
}

/// 执行转码。
///
/// 返回输出路径；取消时终止子进程树并删除输出残留（UL-06）；失败删除半成品保留原文件。
/// 层级协商（TC-14，参考 bat 的 MODE 1/2/3）：`auto` 且本机 QSV 可用时依次尝试
/// 全 GPU → 混合 → 全软件，任一层成功即锁定产物；显式 nvenc/amf/libx265 只跑
/// 软件解码+滤镜的单层（解码与滤镜留在 CPU，按用户选择不自动回落）。
pub fn run_transcode(
    resolver: &ToolResolver,
    params: &TranscodeParams,
    meta: &MediaMeta,
    cancel: &Arc<AtomicBool>,
    mut on_progress: impl FnMut(f32),
    mut on_log: impl FnMut(String),
) -> Result<PathBuf> {
    let explicit = matches!(params.encoder_mode.as_str(), "libx265" | "nvenc" | "amf");
    let auto_qsv = !explicit && qsv_available(resolver).unwrap_or(false);
    // 层级序列：auto 模式且 QSV 可用时三级回落，否则纯软件。
    // 参考 convert_h265.bat：full GPU QSV → hybrid → software libx265。
    let all_tiers: &[TranscodeTier] = if explicit || !auto_qsv {
        &[TranscodeTier::Software]
    } else {
        &[
            TranscodeTier::GpuQsv,
            TranscodeTier::HybridQsv,
            TranscodeTier::Software,
        ]
    };
    // 层级锁定（参考 bat 的 MODE 锁定）：第一个文件成功后记录层级，
    // 后续文件从锁定层级开始，避免在不支持 QSV 的机器上每个文件都先试 QSV。
    // explicit 模式不锁定（用户显式指定了编码器）。
    let start_idx = if !explicit && auto_qsv {
        if let Some(locked) = locked_tier() {
            all_tiers.iter().position(|t| *t == locked).unwrap_or(0)
        } else {
            0
        }
    } else {
        0
    };
    for (i, tier) in all_tiers[start_idx..].iter().enumerate() {
        let r = run_transcode_once(
            resolver,
            params,
            meta,
            cancel,
            *tier,
            &mut on_progress,
            &mut on_log,
        );
        match r {
            Ok(p) => {
                if !explicit && auto_qsv {
                    set_locked_tier(*tier);
                }
                return Ok(p);
            }
            Err(CoreError::Cancelled) => return Err(CoreError::Cancelled),
            Err(e) => {
                if i + 1 < all_tiers[start_idx..].len() {
                    on_log(format!("{} 失败（{e}），自动回落下一层级…", tier.label()));
                } else {
                    return Err(e);
                }
            }
        }
    }
    unreachable!("层级序列非空")
}

fn run_transcode_once(
    resolver: &ToolResolver,
    params: &TranscodeParams,
    meta: &MediaMeta,
    cancel: &Arc<AtomicBool>,
    tier: TranscodeTier,
    on_progress: &mut dyn FnMut(f32),
    on_log: &mut dyn FnMut(String),
) -> Result<PathBuf> {
    // 顺序很重要：先把所有可能失败的准备做完（参数构造 / ffmpeg 定位），再抢占
    // 输出名。output_path() 内部 create_new 占位，提前占位会在失败路径留下
    // 0 字节垃圾文件在用户的输出目录里。
    let args = build_args_for_tier(resolver, params, meta, tier)?;
    let mut cmd = resolver.command(Tool::Ffmpeg)?;
    let out = params.output_path()?;
    on_log(format!(
        "转码 {} → {}（{}，{}）",
        params
            .input
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        out.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        params.encoder_mode,
        tier.label(),
    ));
    // 实际执行的完整命令（含输出路径，build_args 不含）
    let mut full = args.clone();
    full.push(out.to_string_lossy().into_owned());
    on_log(crate::exec::display_command("ffmpeg", &full));

    cmd.args(&args);
    cmd.arg(&out);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut guard = match ChildGuard::spawn(&mut cmd) {
        Ok(g) => g,
        Err(e) => {
            // 起不来就把占位文件清掉，不在用户输出目录留 0 字节垃圾
            let _ = std::fs::remove_file(&out);
            return Err(e);
        }
    };

    let duration = meta.duration_secs.unwrap_or(0.0);
    if duration <= 0.0 {
        // 无时长就无法把 out_time_us 换算成百分比：现象是进度 0% 直跳 100%
        // （等待结束时无条件 on_progress(100)）。记录一条日志帮助定位 meta 缺时长
        on_log("源时长未知，无法换算百分比进度".to_string());
    }
    // 管道读取统一走 exec::stream_lines（独立线程排空 stderr + read_until + decode_text）：
    // ffmpeg 转码中往 stderr 输出 warning/info 时若不排空会写满 64KB 缓冲区，
    // 子进程阻塞在写 stderr 上、stdout 的进度行也不再产出。
    let mut debug_lines = 0u8;
    let outcome = crate::exec::stream_lines(&mut guard, cancel, None, |line| {
        // 前 5 行原始 progress 输出入日志，便于排查"进度为什么不动"
        if debug_lines < 5 && !line.trim().is_empty() {
            on_log(format!("[progress] {}", line.trim()));
            debug_lines += 1;
        }
        if line.trim() == "progress=end" {
            on_progress(100.0);
            return;
        }
        if let Some(us) = parse_out_time_us(line) {
            if duration > 0.0 {
                let pct = ((us as f64 / 1e6) / duration * 100.0).clamp(0.0, 100.0) as f32;
                on_progress(pct);
            }
        }
    })?;

    on_progress(100.0);
    if outcome.killed || cancel.load(Ordering::Relaxed) {
        let _ = std::fs::remove_file(&out);
        return Err(CoreError::Cancelled);
    }
    if !outcome.status.success() {
        let err = outcome.stderr;
        // 多行 stderr 拆成逐条日志：单条塞进一个 entry 时 UI 侧易被截断观感，
        // 逐行落日志才能完整回看 ffmpeg 的报错原因
        let mut lines = err.lines();
        if let Some(first) = lines.next() {
            on_log(format!("转码失败（已删除半成品，保留原文件）：{first}"));
        }
        for l in lines {
            if !l.trim().is_empty() {
                on_log(l.to_string());
            }
        }
        let _ = std::fs::remove_file(&out);
        return Err(CoreError::ProcessFailed {
            program: "ffmpeg".into(),
            code: outcome.status.code(),
            stderr: err,
        });
    }
    // 产物校验：文件必须存在且 ≥1024 字节（convert_h265.bat 同款：ffmpeg 可能
    // exit 0 却只写出空/截断文件，报"成功"但产物不可用）
    let out_ok = std::fs::metadata(&out)
        .map(|m| m.len() >= 1024)
        .unwrap_or(false);
    if !out_ok {
        let _ = std::fs::remove_file(&out);
        return Err(CoreError::ProcessFailed {
            program: "ffmpeg".into(),
            code: None,
            stderr: "转码结束但输出文件缺失或过小（<1024 字节，已删除半成品保留原文件）".into(),
        });
    }
    on_log(format!("转码完成：{}", out.display()));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_pure_title() {
        // 本地条目标题带扩展名 → 输出不带双扩展名
        assert_eq!(apply_filename_template("纯标题", "a/b:c.mp4"), "a_b_c");
        assert_eq!(
            apply_filename_template("纯标题", "你好世界.mp4"),
            "你好世界"
        );
        assert_eq!(apply_filename_template("纯标题", "无扩展名"), "无扩展名");
    }

    #[test]
    fn template_date_prefix() {
        let s = apply_filename_template("日期-标题", "你好");
        assert!(
            s.starts_with("20") && s.contains('-') && s.ends_with("你好"),
            "{}",
            s
        );
    }

    #[test]
    fn sanitize_windows_chars() {
        assert_eq!(
            sanitize_filename("a<b>:c\"d/e\\f|g?h*i"),
            "a_b__c_d_e_f_g_h_i"
        );
        assert_eq!(sanitize_filename("  "), "未命名");
        assert_eq!(sanitize_filename("abc."), "abc");
    }

    #[test]
    fn output_collision_auto_inc() {
        let dir = tempfile::tempdir().unwrap();
        let p = TranscodeParams {
            input: "x.mp4".into(),
            out_dir: dir.path().to_path_buf(),
            title: "t".into(),
            filename_template: "纯标题".into(),
            container: "mp4".into(),
            encoder_mode: "libx265".into(),
            low_power: false,
            max_w: 0,
            max_h: 0,
            brcap_kbps: None,
            br_default_kbps: 0,
            normalize_audio: false,
            max_gain_db: 24.0,
            rot_angle: RotAngle::ZERO,
            keep_cover: true,
            collision_policy: "auto_inc".into(),
        };
        let a = p.output_path().unwrap();
        assert_eq!(a.file_name().unwrap(), "t.mp4");
        std::fs::write(&a, b"x").unwrap();
        let b = p.output_path().unwrap();
        assert_eq!(b.file_name().unwrap(), "t (1).mp4");
    }

    #[test]
    fn output_collision_skip() {
        let dir = tempfile::tempdir().unwrap();
        let p = TranscodeParams {
            out_dir: dir.path().to_path_buf(),
            title: "t".into(),
            filename_template: "纯标题".into(),
            container: "mkv".into(),
            collision_policy: "skip".into(),
            ..mk()
        };
        let a = p.output_path().unwrap();
        std::fs::write(&a, b"x").unwrap();
        assert!(p.output_path().is_err());
    }

    fn mk() -> TranscodeParams {
        TranscodeParams {
            input: "x.mp4".into(),
            out_dir: PathBuf::new(),
            title: "t".into(),
            filename_template: "纯标题".into(),
            container: "mp4".into(),
            encoder_mode: "libx265".into(),
            low_power: false,
            max_w: 0,
            max_h: 0,
            brcap_kbps: None,
            br_default_kbps: 0,
            normalize_audio: false,
            max_gain_db: 24.0,
            rot_angle: RotAngle::ZERO,
            keep_cover: true,
            collision_policy: "auto_inc".into(),
        }
    }

    #[test]
    fn vf_rotate_and_scale() {
        assert_eq!(build_vf(RotAngle::ZERO, 0, 0), None);
        let vf = build_vf(RotAngle::from_degrees(90), 1920, 1080).unwrap();
        assert!(vf.starts_with("transpose=1,"), "{}", vf);
        // 短边/长边上限必须用旋转不变量 min(iw,ih)/max(iw,ih)：
        // transpose 后 iw/ih 互换，写 min(ih,max_h) 会把原长边当短边砍
        // （1920×1080 转 90° 后 ih=1920 → 被压成 606×1080 的历史 bug）
        let expected = concat!(
            "scale='trunc(iw*min(min(1,1080/min(iw,ih)),1920/max(iw,ih))/2)*2':",
            "'trunc(ih*min(min(1,1080/min(iw,ih)),1920/max(iw,ih))/2)*2'"
        );
        assert_eq!(vf, format!("transpose=1,{}", expected), "{}", vf);
        // 旋转与不旋转时 scale 表达式完全一致（旋转不变性）
        assert_eq!(
            build_vf(RotAngle::ZERO, 1920, 1080).unwrap(),
            expected,
            "转 0° 与转 90° 的缩放上限表达式必须相同"
        );
        let vf = build_vf(RotAngle::from_degrees(270), 0, 0).unwrap();
        assert_eq!(vf, "transpose=2");
        // 只设一个上限时其余因子不出现（max_w=0 不参与 min()，避免除零）
        let vf = build_vf(RotAngle::ZERO, 0, 1080).unwrap();
        assert_eq!(
            vf,
            "scale='trunc(iw*min(1,1080/min(iw,ih))/2)*2':'trunc(ih*min(1,1080/min(iw,ih))/2)*2'"
        );
    }

    #[test]
    fn build_args_maps_main_and_cover_by_absolute_index() {
        // 主视频 0、封面 2（典型：视频/音频/attached_pic 封面）
        let meta = MediaMeta {
            video_stream_index: Some(0),
            cover_stream_index: Some(2),
            ..Default::default()
        };
        let args = build_args(&ToolResolver::default(), &mk(), &meta).unwrap();
        let joined = args.join(" ");
        assert!(joined.contains("-map 0:0?"), "{}", joined);
        assert!(joined.contains("-map 0:2?"), "{}", joined);
        assert!(joined.contains("-c:v:0 libx265"), "{}", joined);
        assert!(joined.contains("-c:v:1 copy"), "{}", joined);
        // 标签必须限定到 v:0，否则会落到 mjpeg 封面流导致 mp4 写头失败
        assert!(joined.contains("-tag:v:0 hvc1"), "{}", joined);
        assert!(!joined.contains("-map 0:t?"), "{}", joined);
    }

    #[test]
    fn build_args_uses_filter_complex_when_filter_and_cover() {
        // 封面排在主视频之前（封面 index 0、主视频 index 1）：绝对索引必须指向真正的主视频
        let meta = MediaMeta {
            video_stream_index: Some(1),
            cover_stream_index: Some(0),
            ..Default::default()
        };
        let mut params = mk();
        params.rot_angle = RotAngle::from_degrees(90);
        let args = build_args(&ToolResolver::default(), &params, &meta).unwrap();
        let joined = args.join(" ");
        // 旋转时封面也过 transpose 链并重编码 mjpeg（copy 的封面不跟随旋转）
        assert!(
            joined.contains("-filter_complex [0:1]transpose=1[v];[0:0]transpose=1[cv]"),
            "{}",
            joined
        );
        assert!(joined.contains("-map [v]"), "{}", joined);
        assert!(joined.contains("-map [cv]"), "{}", joined);
        assert!(!joined.contains("-map 0:0?"), "{}", joined);
        assert!(joined.contains("-c:v:1 mjpeg"), "{}", joined);
        assert!(
            joined.contains("-disposition:v:1 attached_pic"),
            "{}",
            joined
        );
        assert!(!joined.contains("-vf "), "{}", joined);

        // 不旋转：封面照旧 copy，保持原质量
        let mut params = mk();
        params.max_w = 0;
        params.max_h = 0;
        params.rot_angle = RotAngle::ZERO;
        let args = build_args(&ToolResolver::default(), &params, &meta).unwrap();
        let joined = args.join(" ");
        // 无滤镜无上限 → 走 -vf 分支之外的单链；封面映射 0:0? 且 copy
        assert!(joined.contains("-map 0:0?"), "{}", joined);
        assert!(joined.contains("-c:v:1 copy"), "{}", joined);
        assert!(!joined.contains("-disposition:v:1"), "{}", joined);
    }

    #[test]
    fn build_args_falls_back_to_attachments_without_cover_index() {
        // 探测不到封面流（如 MKV 附件型封面）→ 回退 0:t? + -c:t copy
        let meta = MediaMeta {
            video_stream_index: Some(0),
            ..Default::default()
        };
        let args = build_args(&ToolResolver::default(), &mk(), &meta).unwrap();
        let joined = args.join(" ");
        assert!(joined.contains("-map 0:t?"), "{}", joined);
        assert!(joined.contains("-c:t copy"), "{}", joined);
    }

    #[test]
    fn parse_encoders_detects_hw() {
        let text = [
            " V....D hevc_qsv            HEVC (Intel Quick Sync Video acceleration)",
            " V....D hevc_nvenc          HEVC (NVIDIA NVENC)",
            " V....D libx265             libx265 H.265 / HEVC",
        ]
        .join("\n");
        let hw = parse_encoders_output(&text);
        assert!(hw.qsv && hw.nvenc && !hw.amf);
        assert!(!parse_encoders_output("V....D libx265").qsv);
    }

    #[test]
    fn parse_us() {
        assert_eq!(parse_out_time_us("out_time_us=1234567"), Some(1234567));
        assert_eq!(parse_out_time_us("out_time_ms=999"), Some(999000));
        assert_eq!(parse_out_time_us("progress=end"), None);
    }

    #[test]
    fn gpu_tier_uses_full_qsv_pipeline() {
        let mut params = mk();
        params.rot_angle = RotAngle::from_degrees(90);
        let meta = MediaMeta {
            video_stream_index: Some(0),
            ..Default::default()
        };
        let args = build_args_for_tier(
            &ToolResolver::default(),
            &params,
            &meta,
            TranscodeTier::GpuQsv,
        )
        .unwrap();
        let j = args.join(" ");
        assert!(
            j.contains("-hwaccel qsv -hwaccel_output_format qsv"),
            "{}",
            j
        );
        assert!(j.contains("-filter:v:0 vpp_qsv=transpose=clock"), "{}", j);
        assert!(j.contains("hevc_qsv"), "{}", j);
        assert!(j.contains("-display_rotation 0"), "{}", j);
    }

    #[test]
    fn hybrid_tier_downloads_frames_to_cpu() {
        let meta = MediaMeta {
            video_stream_index: Some(0),
            ..Default::default()
        };
        let mut params = mk();
        params.max_w = 1920;
        params.max_h = 1080;
        let args = build_args_for_tier(
            &ToolResolver::default(),
            &params,
            &meta,
            TranscodeTier::HybridQsv,
        )
        .unwrap();
        let j = args.join(" ");
        assert!(j.contains("-hwaccel qsv "), "{}", j);
        assert!(!j.contains("-hwaccel_output_format"), "{}", j);
        assert!(j.contains("hwdownload,format=nv12,"), "{}", j);
        assert!(j.contains("hevc_qsv"), "{}", j);
    }
}
