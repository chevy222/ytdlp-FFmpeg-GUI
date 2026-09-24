//! 元数据解析（§3.2 MD-01/MD-02/MD-06）：
//! - URL 解析：yt-dlp `-J`（JSON 元数据 + 格式列表 + 缩略图）
//! - 本地解析：ffprobe（流/格式/旋转/封面）+ volumedetect（音量）
//! - 失败分类（网络不可达 / 需要登录 / 链接无效 / 非视频 / 探测失败）
//!
//! 解析 JSON → MediaMeta 的转换均为纯函数，可单测；子进程调用薄封装在顶层。

use std::path::Path;
use std::process::Stdio;

use serde_json::Value;

use crate::config::NetworkConfig;
use crate::exec::{decode_text, ChildGuard, Tool, ToolResolver};
use crate::model::{AudioVolume, DownloadFormat, MediaMeta};
use crate::Result;

/// 解析失败分类（MD-05）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeErrorKind {
    /// 网络不可达 / 超时
    Network,
    /// 需要登录（Cookie 缺失/过期/风控）
    NeedLogin,
    /// 链接无效 / 不存在 / 404
    InvalidLink,
    /// 非视频文件 / 探测失败（本地）
    NotVideo,
    /// 用户取消（解析中的条目也能取消，见 run_probe 的取消注册）
    Cancelled,
    /// 其他失败
    Failed,
}

/// URL 解析结果。
#[derive(Debug, Clone)]
pub struct UrlProbe {
    pub meta: MediaMeta,
    pub site: Option<String>,
    pub host: Option<String>,
    /// 是否合集/多 P
    pub is_playlist: bool,
    /// 缩略图 URL（缓存/封面用）
    pub thumbnail_url: Option<String>,
}

/// 本地解析结果。
#[derive(Debug, Clone)]
pub struct LocalProbe {
    pub meta: MediaMeta,
}

/// 播放列表单集条目（DL-09 平铺）。
#[derive(Debug, Clone)]
pub struct PlaylistEntry {
    pub url: String,
    pub title: String,
}

/// 组装 yt-dlp 公共参数（`-J` 家族共用）：extra 前缀 + js 运行时 + cookies + 代理 + URL。
/// 命令行展示（`display_command`）与 Command 构造共用这一份，保证日志与实际执行一致。
///
/// js 运行时说明（§3.6/DL-14）：yt-dlp 的 YouTube 组件需要 JS 运行时，默认
/// **只认 PATH 里的 deno**；依赖配置/`tools\` 托管目录里的 deno 必须显式传给它
/// （`--js-runtimes deno:<路径>`），否则即使依赖自检通过，YouTube 仍会因缺
/// JS 运行时失败。
fn ytdlp_args(
    resolver: &ToolResolver,
    url: &str,
    cookies_file: Option<&Path>,
    network: &NetworkConfig,
    extra: &[&str],
) -> Vec<String> {
    let mut args: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
    if let Ok(deno) = resolver.resolve(Tool::Deno) {
        args.push("--js-runtimes".into());
        args.push(format!("deno:{}", deno.to_string_lossy()));
    }
    if let Some(cf) = cookies_file {
        args.push("--cookies".into());
        args.push(cf.to_string_lossy().into_owned());
    }
    if let Some(p) = network.resolve_proxy(url) {
        args.push("--proxy".into());
        args.push(p);
    }
    args.push(url.to_string());
    args
}

/// 展开播放列表（yt-dlp -J --flat-playlist）：快速拿每集 URL 与标题，
/// 命令层据此逐条平铺进统一列表。
///
/// 与 [`probe_url`] 同一套读取方式：边下边读、可由 `cancel` 终止、300 秒兜底。
/// 大合集的 JSON 可达数十 MB，持续排空 stdout 才不会把自己卡在管道缓冲区上。
pub fn list_playlist_entries(
    resolver: &ToolResolver,
    url: &str,
    cookies_file: Option<&Path>,
    network: &NetworkConfig,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    mut on_log: impl FnMut(String),
) -> std::result::Result<Vec<PlaylistEntry>, ProbeFailure> {
    use std::sync::atomic::Ordering;
    let args = ytdlp_args(
        resolver,
        url,
        cookies_file,
        network,
        &["-J", "--flat-playlist", "--no-warnings"],
    );
    on_log(crate::exec::display_command("yt-dlp", &args));
    let mut cmd = resolver.command(Tool::YtDlp).map_err(|e| ProbeFailure {
        kind: ProbeErrorKind::Failed,
        message: e.to_string(),
    })?;
    cmd.args(&args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut guard = ChildGuard::spawn(&mut cmd).map_err(|e| ProbeFailure {
        kind: ProbeErrorKind::Failed,
        message: format!("启动 yt-dlp 失败：{}", e),
    })?;

    let cancel = cancel
        .cloned()
        .unwrap_or_else(|| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    let mut text = String::new();
    let outcome = crate::exec::stream_lines(&mut guard, &cancel, Some(deadline), |line| {
        text.push_str(line);
        text.push('\n');
    })
    .map_err(|e| ProbeFailure {
        kind: ProbeErrorKind::Failed,
        message: format!("yt-dlp 执行异常：{e}"),
    })?;
    if outcome.killed {
        let (kind, message) = if cancel.load(Ordering::Relaxed) {
            (ProbeErrorKind::Cancelled, "获取播放列表已取消".to_string())
        } else {
            (
                ProbeErrorKind::Network,
                "获取播放列表超时（超过 300 秒），可重试或检查网络/代理".to_string(),
            )
        };
        return Err(ProbeFailure { kind, message });
    }
    if !outcome.status.success() {
        let mut f = classify_ytdlp_error(outcome.stderr.trim());
        f.message = format!("获取播放列表失败：{}", outcome.stderr.trim());
        return Err(f);
    }
    let v: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            return Err(ProbeFailure {
                kind: ProbeErrorKind::Failed,
                message: "yt-dlp 返回无法解析的播放列表数据".into(),
            })
        }
    };
    let mut out = Vec::new();
    if let Some(entries) = v["entries"].as_array() {
        for e in entries {
            let u = e["url"]
                .as_str()
                .or_else(|| e["webpage_url"].as_str())
                .unwrap_or_default()
                .to_string();
            if u.is_empty() {
                continue;
            }
            let title = e["title"].as_str().unwrap_or("").to_string();
            out.push(PlaylistEntry { url: u, title });
        }
    }
    Ok(out)
}

/// 解析失败（分类 + 摘要）。
#[derive(Debug, Clone)]
pub struct ProbeFailure {
    pub kind: ProbeErrorKind,
    pub message: String,
}

impl std::fmt::Display for ProbeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// URL 解析（yt-dlp -J）。
///
/// 输出用 [`crate::exec::stream_lines`] **边下边读**（不再落 temp 临时文件）：
/// 整份元数据 JSON 可能有几十 MB，持续排空 stdout 就不会触发管道缓冲区阻塞，
/// 同时天然获得"取消/超时由看门狗落实"的能力（旧实现用临时文件是为了绕开管道
/// 阻塞，代价是 temp 目录里多出一套需要维护生命周期的文件）。
///
/// - `cookies_file`：Netscape 临时文件路径（None 则不带）；
/// - `cancel`：置位后终止 yt-dlp 并返回 [`ProbeErrorKind::Cancelled`]；
/// - `on_log`：接收实际执行的完整命令行（条目日志展示用）。
pub fn probe_url(
    resolver: &ToolResolver,
    url: &str,
    cookies_file: Option<&Path>,
    network: &NetworkConfig,
    playlist: bool,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    mut on_log: impl FnMut(String),
) -> std::result::Result<UrlProbe, ProbeFailure> {
    use std::sync::atomic::Ordering;
    let extra: &[&str] = if playlist {
        &["-J", "--no-warnings", "--yes-playlist", "--socket-timeout", "60"]
    } else {
        &["-J", "--no-warnings", "--no-playlist", "--socket-timeout", "60"]
    };
    let args = ytdlp_args(resolver, url, cookies_file, network, extra);
    on_log(crate::exec::display_command("yt-dlp", &args));
    let mut cmd = resolver.command(Tool::YtDlp).map_err(|e| ProbeFailure {
        kind: ProbeErrorKind::Failed,
        message: e.to_string(),
    })?;
    cmd.args(&args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut guard = ChildGuard::spawn(&mut cmd).map_err(|e| ProbeFailure {
        kind: ProbeErrorKind::Failed,
        message: format!("启动 yt-dlp 失败：{}", e),
    })?;

    let cancel = cancel
        .cloned()
        .unwrap_or_else(|| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)));
    // 300 秒是纯兜底（正常解析是秒级）：卡住时不要永远占着解析线程
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    let mut text = String::new();
    let outcome = crate::exec::stream_lines(&mut guard, &cancel, Some(deadline), |line| {
        text.push_str(line);
        text.push('\n');
    })
    .map_err(|e| ProbeFailure {
        kind: ProbeErrorKind::Failed,
        message: format!("yt-dlp 执行异常：{e}"),
    })?;

    if outcome.killed {
        // 区分"用户取消"与"超时兜底"：前者进已取消，后者进失败（带可读原因）
        if cancel.load(Ordering::Relaxed) {
            return Err(ProbeFailure {
                kind: ProbeErrorKind::Cancelled,
                message: "解析已取消".into(),
            });
        }
        return Err(ProbeFailure {
            kind: ProbeErrorKind::Network,
            message: "解析超时（超过 300 秒仍未返回元数据），可重试或检查网络/代理".into(),
        });
    }
    if !outcome.status.success() {
        return Err(classify_ytdlp_error(outcome.stderr.trim()));
    }
    parse_ytdlp_json(&text).map_err(|e| ProbeFailure {
        kind: ProbeErrorKind::Failed,
        message: format!("解析 yt-dlp 输出失败：{e}"),
    })
}

/// 本地探测（ffprobe / volumedetect）的墙钟上限。
///
/// 这类"看着一定很快"的命令在损坏容器、网络盘、被安全软件拦截的文件上会永久
/// 挂住，而它们跑在解析线程里、还占着全局解析闸位 —— 挂一次就少一路解析能力。
/// 60 秒对本地探测是极宽松的上限（正常是毫秒级）。
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// 本地文件解析（ffprobe + volumedetect）。
pub fn probe_local(
    resolver: &ToolResolver,
    path: &Path,
    mut on_log: impl FnMut(String),
) -> std::result::Result<LocalProbe, ProbeFailure> {
    if !path.is_file() {
        return Err(ProbeFailure {
            kind: ProbeErrorKind::NotVideo,
            message: format!("文件不存在：{}", path.display()),
        });
    }
    // 1) ffprobe 基础信息
    let args: Vec<String> = [
        "-v",
        "error",
        "-print_format",
        "json",
        "-show_format",
        "-show_streams",
        // -show_data 才会输出流级 `extradata`（否则只有 extradata_size）：
        // 合并直拼判据 MG-02 需要真实 SPS/PPS 十六进制对比
        "-show_data",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain(std::iter::once(path.to_string_lossy().into_owned()))
    .collect();
    on_log(crate::exec::display_command("ffprobe", &args));
    let mut cmd = resolver.command(Tool::Ffprobe).map_err(|e| ProbeFailure {
        kind: ProbeErrorKind::Failed,
        message: e.to_string(),
    })?;
    cmd.args(&args);
    // run_capture_deadline 自带"到点杀进程树"的看门狗，并在非零退出时把
    // stderr 带进错误信息（不再需要手动 drain_stderr）
    let out = crate::exec::run_capture_deadline(cmd, PROBE_TIMEOUT).map_err(|e| ProbeFailure {
        kind: ProbeErrorKind::NotVideo,
        message: format!("ffprobe 探测失败：{e}"),
    })?;
    let text = decode_text(&out.stdout);
    let mut meta = parse_ffprobe_json(&text);
    meta.size_bytes = std::fs::metadata(path).ok().map(|m| m.len());

    // 2) volumedetect（有音频流时）
    if meta.acodec.is_some() {
        // 失败要说出来：静默吞掉时用户只看到"音量未知"，无从判断是探测失败
        // 还是文件真的没有可测音量
        match probe_volume(resolver, path, &mut on_log) {
            Ok(vol) => meta.audio_volume = vol,
            Err(e) => on_log(format!("音量探测失败（增益按保守值处理）：{e}")),
        }
    }
    Ok(LocalProbe { meta })
}

/// 音量探测（ffmpeg volumedetect）。
///
/// 只解**主音轨**（`-map 0:a:0`）且只取**前 10 分钟**（`-t 600`）：
/// volumedetect 是统计型滤镜，要解完全部采样才有均值——不加限制时，一部 2 小时
/// 4K 视频（含视频解码！`-vn` 关掉）会在这里耗掉几十秒到几分钟，而增益决策
/// 只需要一个足够有代表性的峰值。原实现还不看退出码，失败时静默返回"音量未知"。
pub fn probe_volume(
    resolver: &ToolResolver,
    path: &Path,
    on_log: &mut dyn FnMut(String),
) -> Result<AudioVolume> {
    // ffmpeg 选项分输入/输出两段：-i 之前是输入选项，之后是输出选项。
    // -map / -vn / -af / -f 都是输出选项，必须放在 -i 之后；否则 ffmpeg 报
    // "Option map cannot be applied to input url"（退出码 -22/EINVAL）。
    let args: Vec<String> = ["-i"]
        .iter()
        .map(|s| s.to_string())
        .chain(std::iter::once(path.to_string_lossy().into_owned()))
        .chain(
            ["-vn", "-map", "0:a:0", "-t", "600", "-af", "volumedetect", "-f", "null", "-"]
                .iter()
                .map(|s| s.to_string()),
        )
        .collect();
    on_log(crate::exec::display_command("ffmpeg", &args));
    let mut cmd = resolver.command(Tool::Ffmpeg)?;
    cmd.args(&args);
    // 带截止时间：volumedetect 是统计型滤镜，要解完采样才有结果；坏文件/网络盘上
    // 可能长期不出结果，而本函数在解析线程里同步执行，没有兜底会把解析链路一起占死。
    // 非零退出时 run_capture_deadline 会把 stderr 一并带进错误信息。
    let out = crate::exec::run_capture_deadline(cmd, PROBE_TIMEOUT)?;
    Ok(parse_volumedetect(&decode_text(&out.stderr)))
}

/// 解析 yt-dlp `-J` JSON → UrlProbe（纯函数）。
pub fn parse_ytdlp_json(text: &str) -> Result<UrlProbe> {
    let v: Value = serde_json::from_str(text)?;
    let title = v["title"].as_str().map(str::to_string);
    let duration = v["duration"].as_f64();
    let thumbnail = v["thumbnail"].as_str().map(str::to_string);
    let is_playlist = v["_type"].as_str() == Some("playlist")
        || v["playlist_count"].as_u64().map(|c| c > 1).unwrap_or(false);
    let webpage_url = v["webpage_url"].as_str().unwrap_or_default();
    let host = crate::cookies::host_from_url(webpage_url);
    let extractor = v["extractor"].as_str().unwrap_or_default();
    let site = if extractor.is_empty() {
        None
    } else {
        Some(extractor.to_string())
    };

    // 格式列表（fps/tbr/filesize 完整时才进入列表；带协议 video+audio 合并项）
    let mut formats: Vec<DownloadFormat> = Vec::new();
    if let Some(arr) = v["formats"].as_array() {
        for f in arr {
            let format_id = f["format_id"].as_str().unwrap_or_default().to_string();
            if format_id.is_empty() {
                continue;
            }
            let vcodec = f["vcodec"].as_str().map(str::to_string);
            let acodec = f["acodec"].as_str().map(str::to_string);
            let height = f["height"].as_u64().map(|h| h as u32);
            let fps = f["fps"].as_f64();
            let filesize = f.as_object().and_then(|o| {
                o.get("filesize")
                    .or_else(|| o.get("filesize_approx"))
                    .and_then(|x| x.as_u64())
            });
            let tbr = f["tbr"].as_f64().map(|t| t as u32);
            let ext = f["ext"].as_str().map(str::to_string);
            let has_video = match vcodec.as_deref() {
                None => false,
                Some(c) => !matches!(c.to_lowercase().as_str(), "none" | "n/a" | "images"),
            };
            let audio_only = !has_video && acodec.is_some();
            let note = if f["format_note"].as_str().is_some() {
                f["format_note"].as_str().map(str::to_string)
            } else {
                None
            };
            let df = DownloadFormat {
                label: String::new(),
                format_id,
                height,
                ext,
                vcodec,
                acodec,
                filesize_bytes: filesize,
                fps,
                tbr_kbps: tbr,
                note,
                audio_only,
            };
            let label = df.make_label();
            formats.push(DownloadFormat { label, ..df });
        }
    }
    // 去重（同 format_id 保留首个）
    let mut seen = std::collections::HashSet::new();
    formats.retain(|f| seen.insert(f.format_id.clone()));

    let meta = MediaMeta {
        title: title.clone(),
        duration_secs: duration,
        height: formats
            .iter()
            .filter(|f| !f.audio_only)
            .map(|f| f.height.unwrap_or(0))
            .max(),
        container: None, // 下载容器由所选格式决定，展示列用 URL 行内格式按钮
        download_formats: formats,
        has_cover: thumbnail.is_some(),
        ..Default::default()
    };
    Ok(UrlProbe {
        meta,
        site,
        host,
        is_playlist,
        thumbnail_url: thumbnail,
    })
}

/// 解析 ffprobe JSON → MediaMeta（纯函数）。
pub fn parse_ffprobe_json(text: &str) -> MediaMeta {
    let v: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return MediaMeta::default(),
    };
    let mut meta = MediaMeta::default();
    if let Some(fmt) = v["format"].as_object() {
        meta.container = fmt.get("format_name").and_then(|x| x.as_str()).map(|s| {
            let lower = s.to_lowercase();
            // mov,mp4,m4a,… 优先显示 MP4（用户习惯），否则取首个格式名大写
            if lower.contains("mp4") {
                "MP4".to_string()
            } else {
                let base = lower.split(',').next().unwrap_or(&lower);
                base.to_uppercase()
            }
        });
        meta.duration_secs = fmt
            .get("duration")
            .and_then(|x| x.as_str())
            .and_then(|s| s.parse().ok());
        meta.size_bytes = fmt
            .get("size")
            .and_then(|x| x.as_str())
            .and_then(|s| s.parse().ok());
        meta.has_cover = false;
    }
    let streams = v["streams"].as_array().cloned().unwrap_or_default();
    let mut has_audio = false;
    let mut audio_tracks = 0u32;
    for (pos, s) in streams.iter().enumerate() {
        // 绝对流索引（映射用；`index` 缺失时按出现顺序回退）
        let abs_index = s["index"].as_u64().map(|i| i as u32).unwrap_or(pos as u32);
        match s["codec_type"].as_str() {
            Some("video") => {
                let is_cover = s["disposition"]["attached_pic"].as_u64() == Some(1)
                    || s["disposition"]["attached_pic"].as_str() == Some("1");
                if is_cover {
                    // 封面流（attached pic）只标记 has_cover 与索引，不参与主视频元数据，
                    // 否则 mjpeg 封面会覆盖主视频的高度/编码器/码率/帧率
                    meta.has_cover = true;
                    if meta.cover_stream_index.is_none() {
                        meta.cover_stream_index = Some(abs_index);
                    }
                    continue;
                }
                // 只取首个主视频流：保证 mapping 用的绝对索引与元数据来自同一条流
                if meta.video_stream_index.is_none() {
                    meta.video_stream_index = Some(abs_index);
                    meta.height = s["height"].as_u64().map(|h| h as u32);
                    // 宽度采集（MD-02）：竖屏源 short_edge()/画质列/后处理短边判据都依赖它
                    meta.width = s["width"].as_u64().map(|w| w as u32);
                    meta.vcodec = s["codec_name"].as_str().map(str::to_string);
                    meta.fps = s["avg_frame_rate"].as_str().and_then(|r| {
                        let mut it = r.split('/');
                        let n: f64 = it.next()?.parse().ok()?;
                        let d: f64 = it.next()?.parse().ok()?;
                        if d == 0.0 {
                            None
                        } else {
                            Some(n / d)
                        }
                    });
                    meta.vbitrate_kbps = s["bit_rate"]
                        .as_str()
                        .and_then(|b| b.parse::<f64>().ok())
                        .map(|b| (b / 1000.0) as u32);
                    meta.extradata = s["extradata"].as_str().map(str::to_string);
                    // 旋转标记（MD-02 采集；转码是否据此自动纠正见 TC-04 手动旋转约定）
                    if let Some(tags) = s["tags"].as_object() {
                        if let Some(r) = tags.get("rotate").and_then(|x| x.as_str()) {
                            meta.rotate_tag = r.parse::<i32>().ok();
                        }
                    }
                    // 新版 ffprobe 用 side_data_list.Display Matrix.rotation 代替 tags.rotate
                    if meta.rotate_tag.is_none() {
                        if let Some(sd) = s["side_data_list"].as_array() {
                            for item in sd {
                                if let Some(r) = item["rotation"].as_f64() {
                                    // rotation 是弧度（如 -1.570796），转成角度
                                    let deg = (r * 180.0 / std::f64::consts::PI).round() as i32;
                                    if deg != 0 {
                                        meta.rotate_tag = Some(deg.rem_euclid(360));
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Some("audio") => {
                has_audio = true;
                audio_tracks += 1;
                // 保留首个音频流的信息（后续流仅计数）
                if meta.acodec.is_none() {
                    meta.acodec = s["codec_name"].as_str().map(str::to_string);
                    meta.audio_channels = s["channels"].as_u64().map(|c| c as u32);
                    meta.abitrate_kbps = s["bit_rate"]
                        .as_str()
                        .and_then(|b| b.parse::<f64>().ok())
                        .map(|b| (b / 1000.0) as u32);
                    meta.sample_rate = s["sample_rate"].as_str().and_then(|r| r.parse().ok());
                }
            }
            _ => {}
        }
    }
    meta.audio_tracks = if has_audio { Some(audio_tracks) } else { None };
    if meta.acodec.is_some() && meta.audio_tracks == Some(0) {
        meta.audio_tracks = Some(1);
    }
    // 视频码率兜底：流级 bit_rate 缺失时用 总比特率-音频比特率 估算（Windows 属性同口径）
    if meta.vbitrate_kbps.is_none() {
        if let Some(fmt_br) = v["format"]["bit_rate"]
            .as_str()
            .and_then(|b| b.parse::<f64>().ok())
        {
            let a_br = meta.abitrate_kbps.unwrap_or(0) as f64 * 1000.0;
            let v_br = fmt_br - a_br;
            if v_br > 0.0 {
                meta.vbitrate_kbps = Some((v_br / 1000.0) as u32);
            }
        }
    }
    // 无主视频流（如纯音频文件）：不给出映射索引，调用方按 `0:v:0?` 兜底
    meta
}

/// 解析 volumedetect 输出 → AudioVolume（纯函数）。
pub fn parse_volumedetect(stderr: &str) -> AudioVolume {
    let mut v = AudioVolume::default();
    for line in stderr.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("[Parsed_volumedetect") {
            if let Some(rest) = rest.split("] ").nth(1) {
                if let Some(val) = rest.strip_prefix("mean_volume: ") {
                    v.mean_volume_db = val.trim_end_matches(" dB").trim().parse().ok();
                } else if let Some(val) = rest.strip_prefix("max_volume: ") {
                    v.max_volume_db = val.trim_end_matches(" dB").trim().parse().ok();
                }
            }
        }
    }
    v
}

/// 分类 yt-dlp 错误（MD-05）。
fn classify_ytdlp_error(stderr: &str) -> ProbeFailure {
    let lower = stderr.to_lowercase();
    let kind = if lower.contains("sign in")
        || lower.contains("login")
        || lower.contains("需要登录")
        || lower.contains("private")
        || lower.contains("log in")
        || lower.contains("members only")
        || lower.contains("cookies are needed")
        || lower.contains("fresh cookies")
    {
        ProbeErrorKind::NeedLogin
    } else if lower.contains("unable to download webpage")
        || lower.contains("timed out")
        || lower.contains("connection")
        || lower.contains("网络不可达")
    {
        ProbeErrorKind::Network
    } else if lower.contains("does not exist")
        || lower.contains("404")
        || lower.contains("invalid url")
    {
        ProbeErrorKind::InvalidLink
    } else if lower.contains("video unavailable") || lower.contains("unavailable") {
        ProbeErrorKind::NeedLogin
    } else {
        ProbeErrorKind::Failed
    };
    ProbeFailure {
        kind,
        message: stderr.lines().last().unwrap_or("未知错误").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const YTDLP_JSON: &str = r#"{
      "id": "abc123",
      "title": "示例视频标题",
      "duration": 332.5,
      "thumbnail": "https://i.ytimg.com/vi/abc123/maxresdefault.jpg",
      "extractor": "youtube",
      "webpage_url": "https://www.youtube.com/watch?v=abc123",
      "formats": [
        {"format_id": "137", "ext": "mp4", "height": 1080, "vcodec": "avc1", "acodec": "none", "fps": 30.0, "filesize": 35651584, "tbr": 1200.0, "format_note": "1080p"},
        {"format_id": "140", "ext": "m4a", "vcodec": "none", "acodec": "mp4a", "fps": null, "filesize": 5242880, "tbr": 128.0, "format_note": "medium"}
      ]
    }"#;

    #[test]
    fn parse_ytdlp_json_basic() {
        let p = parse_ytdlp_json(YTDLP_JSON).unwrap();
        assert_eq!(p.meta.title.as_deref(), Some("示例视频标题"));
        assert_eq!(p.meta.duration_secs, Some(332.5));
        assert_eq!(p.host.as_deref(), Some("www.youtube.com"));
        assert_eq!(p.site.as_deref(), Some("youtube"));
        assert_eq!(
            p.thumbnail_url.as_deref(),
            Some("https://i.ytimg.com/vi/abc123/maxresdefault.jpg")
        );
        assert!(!p.is_playlist);
    }

    #[test]
    fn parse_ytdlp_json_formats() {
        let p = parse_ytdlp_json(YTDLP_JSON).unwrap();
        assert_eq!(p.meta.download_formats.len(), 2);
        let video = &p.meta.download_formats[0];
        assert_eq!(video.format_id, "137");
        assert_eq!(video.height, Some(1080));
        assert!(!video.audio_only);
        assert!(video.label.contains("1080P"));
        assert!(video.label.contains("H.264"));
        let audio = &p.meta.download_formats[1];
        assert!(audio.audio_only);
        assert!(audio.label.contains("仅音频"));
        assert!(audio.label.contains("AAC"));
    }

    #[test]
    fn parse_ytdlp_json_playlist() {
        let text = r#"{"_type":"playlist","playlist_count":5,"title":"合集","formats":[]}"#;
        let p = parse_ytdlp_json(text).unwrap();
        assert!(p.is_playlist);
    }

    #[test]
    fn parse_ytdlp_json_dedup_format_ids() {
        let text = r#"{"title":"t","formats":[{"format_id":"a","vcodec":"avc1","acodec":"none"},{"format_id":"a","vcodec":"avc1","acodec":"none"}]}"#;
        let p = parse_ytdlp_json(text).unwrap();
        assert_eq!(p.meta.download_formats.len(), 1);
    }

    #[test]
    fn parse_ffprobe_json_media() {
        let json = r#"{
          "streams": [
            {"codec_type":"video","codec_name":"hevc","width":1920,"height":1080,"avg_frame_rate":"60/1","bit_rate":"12000000","tags":{"rotate":"90"}},
            {"codec_type":"audio","codec_name":"aac","bit_rate":"320000"},
            {"codec_type":"audio","codec_name":"aac","bit_rate":"128000"}
          ],
          "format": {"format_name":"mov,mp4,m4a","duration":"332.000000","size":"35651584"}
        }"#;
        let m = parse_ffprobe_json(json);
        assert_eq!(m.height, Some(1080));
        // 宽度采集（MD-02/P0-2）：竖屏源 short_edge 与画质列都依赖它
        assert_eq!(m.width, Some(1920));
        assert_eq!(m.vcodec.as_deref(), Some("hevc"));
        assert_eq!(m.fps, Some(60.0));
        assert_eq!(m.vbitrate_kbps, Some(12000));
        assert_eq!(m.acodec.as_deref(), Some("aac"));
        assert_eq!(m.audio_tracks, Some(2));
        assert_eq!(m.abitrate_kbps, Some(320));
        assert_eq!(m.container.as_deref(), Some("MP4"));
        assert_eq!(m.duration_secs, Some(332.0));
        assert_eq!(m.size_bytes, Some(35651584));
        assert!(!m.has_cover);
    }

    #[test]
    fn parse_ffprobe_json_attached_pic_cover() {
        let json = r#"{"streams":[{"codec_type":"video","codec_name":"mjpeg","disposition":{"attached_pic":1}},{"codec_type":"video","codec_name":"hevc","height":1080},{"codec_type":"audio","codec_name":"aac"}]}"#;
        let m = parse_ffprobe_json(json);
        assert!(m.has_cover);
        assert_eq!(m.height, Some(1080));
    }

    #[test]
    fn parse_ffprobe_json_stream_index_extradata_rotate() {
        let json = r#"{
          "streams": [
            {"index":0,"codec_type":"video","codec_name":"hevc","height":1080,"extradata":"0a0b0c","tags":{"rotate":"90"}},
            {"index":1,"codec_type":"audio","codec_name":"aac","sample_rate":"48000","bit_rate":"128000"},
            {"index":2,"codec_type":"video","codec_name":"mjpeg","disposition":{"attached_pic":1}}
          ]
        }"#;
        let m = parse_ffprobe_json(json);
        assert_eq!(m.video_stream_index, Some(0));
        assert_eq!(m.cover_stream_index, Some(2));
        assert_eq!(m.extradata.as_deref(), Some("0a0b0c"));
        assert_eq!(m.rotate_tag, Some(90));
        assert_eq!(m.height, Some(1080));
        assert_eq!(m.vcodec.as_deref(), Some("hevc"));
    }

    #[test]
    fn parse_ffprobe_json_cover_first_main_index_is_second() {
        // 封面流排在主视频之前时，绝对索引必须指向真正的主视频
        let json = r#"{"streams":[
          {"index":0,"codec_type":"video","codec_name":"mjpeg","disposition":{"attached_pic":1}},
          {"index":1,"codec_type":"video","codec_name":"hevc","height":1080},
          {"index":2,"codec_type":"audio","codec_name":"aac"}
        ]}"#;
        let m = parse_ffprobe_json(json);
        assert_eq!(m.cover_stream_index, Some(0));
        assert_eq!(m.video_stream_index, Some(1));
        assert_eq!(m.height, Some(1080));
        assert_eq!(m.vcodec.as_deref(), Some("hevc"));
    }

    #[test]
    fn parse_ffprobe_json_missing_index_falls_back_to_position() {
        // 不带 index 字段（老样本）时按出现顺序回退，保证映射不丢
        let json = r#"{"streams":[{"codec_type":"video","codec_name":"h264","height":720},{"codec_type":"audio","codec_name":"aac"}]}"#;
        let m = parse_ffprobe_json(json);
        assert_eq!(m.video_stream_index, Some(0));
        assert_eq!(m.cover_stream_index, None);
    }

    #[test]
    fn parse_volumedetect_ok() {
        let stderr = "\
[Parsed_volumedetect_0 @ 0x7f] n_samples: 1000
[Parsed_volumedetect_0 @ 0x7f] mean_volume: -15.3 dB
[Parsed_volumedetect_0 @ 0x7f] max_volume: -8.2 dB
";
        let v = parse_volumedetect(stderr);
        assert_eq!(v.mean_volume_db, Some(-15.3));
        assert_eq!(v.max_volume_db, Some(-8.2));
    }

    #[test]
    fn parse_volumedetect_empty() {
        assert_eq!(parse_volumedetect("").max_volume_db, None);
    }

    #[test]
    fn classify_errors() {
        let e = classify_ytdlp_error("ERROR: Please sign in to view this video");
        assert_eq!(e.kind, ProbeErrorKind::NeedLogin);
        let e = classify_ytdlp_error("ERROR: Video unavailable. This video is private");
        assert_eq!(e.kind, ProbeErrorKind::NeedLogin);
        let e = classify_ytdlp_error("ERROR: Unable to download webpage: timed out");
        assert_eq!(e.kind, ProbeErrorKind::Network);
        let e = classify_ytdlp_error("ERROR: This video does not exist");
        assert_eq!(e.kind, ProbeErrorKind::InvalidLink);
        let e = classify_ytdlp_error("ERROR: something else happened");
        assert_eq!(e.kind, ProbeErrorKind::Failed);
        let e = classify_ytdlp_error(
            "ERROR: [Douyin] 7686436507753794843: Fresh cookies (not necessarily logged in) are needed",
        );
        assert_eq!(e.kind, ProbeErrorKind::NeedLogin);
    }

    #[test]
    fn download_format_label_audio() {
        let f = DownloadFormat {
            format_id: "140".into(),
            label: String::new(),
            height: None,
            ext: Some("m4a".into()),
            vcodec: None,
            acodec: Some("mp4a".into()),
            filesize_bytes: Some(5242880),
            fps: None,
            tbr_kbps: Some(128),
            note: None,
            audio_only: true,
        };
        let l = f.make_label();
        assert!(l.contains("仅音频"));
        assert!(l.contains("M4A"));
        assert!(l.contains("AAC"));
        assert!(l.contains("5.0MB"));
    }
}
