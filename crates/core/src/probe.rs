//! 元数据解析（§3.2 MD-01/MD-02/MD-06）：
//! - URL 解析：yt-dlp `-J`（JSON 元数据 + 格式列表 + 缩略图）
//! - 本地解析：ffprobe（流/格式/旋转/封面）+ volumedetect（音量）
//! - 失败分类（网络不可达 / 需要登录 / 链接无效 / 非视频 / 探测失败）
//!
//! 解析 JSON → MediaMeta 的转换均为纯函数，可单测；子进程调用薄封装在顶层。

use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

use crate::config::NetworkConfig;
use crate::exec::{
    decode_text, is_http_url, push_url_arg, ChildGuard, Tool, ToolResolver,
};
use crate::model::{AudioVolume, DownloadFormat, MediaMeta};

/// 解析临时文件 slot 计数器（参考 convert_h265.bat / download_video.bat 的
/// mkdir slot 原子性声明思想）。同进程并发解析时，每个任务获取唯一 slot，
/// 避免临时文件互相覆盖（旧实现只用进程 ID，并发解析会冲突）。
static PROBE_SLOT: AtomicU64 = AtomicU64::new(0);
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
    // URL 用 `--` 终结选项解析：地址以 `-` 开头时（播放列表 JSON 里的恶意条目）
    // 只能是一条下不动的链接，不能变成 `--exec` 这样的选项
    push_url_arg(&mut args, url);
    args
}

/// 展开播放列表（yt-dlp -J --flat-playlist）：快速拿每集 URL 与标题，
/// 命令层据此逐条平铺进统一列表。
pub fn list_playlist_entries(
    resolver: &ToolResolver,
    url: &str,
    cookies_file: Option<&Path>,
    network: &NetworkConfig,
    mut on_log: impl FnMut(String),
) -> std::result::Result<Vec<PlaylistEntry>, ProbeFailure> {
    // 只接受 http(s)：这一层的输入是用户粘贴的链接，产物又会被平铺成条目
    // 再送进 probe_url / run_download（见那里同样的闸口）
    if !is_http_url(url) {
        return Err(ProbeFailure {
            kind: ProbeErrorKind::InvalidLink,
            message: format!("只支持 http(s) 链接：{url}"),
        });
    }
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
    let guard = ChildGuard::spawn(&mut cmd).map_err(|e| ProbeFailure {
        kind: ProbeErrorKind::Failed,
        message: format!("启动 yt-dlp 失败：{}", e),
    })?;
    let output = guard.wait_with_output().map_err(|e| ProbeFailure {
        kind: ProbeErrorKind::Failed,
        message: format!("yt-dlp 退出异常：{}", e),
    })?;
    if !output.status.success() {
        let err = decode_text(&output.stderr);
        let mut f = classify_ytdlp_error(&err);
        f.message = format!("获取播放列表失败：{}", err.trim());
        return Err(f);
    }
    let text = decode_text(&output.stdout);
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
    let mut rejected = 0usize;
    if let Some(entries) = v["entries"].as_array() {
        for e in entries {
            let u = e["url"]
                .as_str()
                .or_else(|| e["webpage_url"].as_str())
                .unwrap_or_default()
                .to_string();
            // 条目地址由站点 JSON 决定，属于不可信输入：非 http(s) 一律丢弃。
            // 它会被平铺成条目、再作为下一条 URL 参数送给 yt-dlp，`--exec=…`
            // 这类字符串在这里被挡下就再也进不去。
            if !is_http_url(&u) {
                if !u.is_empty() {
                    rejected += 1;
                }
                continue;
            }
            let title = e["title"].as_str().unwrap_or("").to_string();
            out.push(PlaylistEntry { url: u, title });
        }
    }
    if rejected > 0 {
        on_log(format!("已忽略 {rejected} 条非 http(s) 播放列表条目"));
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
/// `cookies_file`：Netscape 临时文件路径（None 则不带）。
/// `on_log`：接收实际执行的完整命令行（条目日志展示用）。
pub fn probe_url(
    resolver: &ToolResolver,
    url: &str,
    cookies_file: Option<&Path>,
    network: &NetworkConfig,
    playlist: bool,
    temp_dir: &Path,
    mut on_log: impl FnMut(String),
) -> std::result::Result<UrlProbe, ProbeFailure> {
    // 只接受 http(s)。调用方有两处：UI 添加（已校验）与播放列表平铺
    // （URL 来自站点 JSON）—— 校验必须落在这里，离 yt-dlp 最近的一层
    if !is_http_url(url) {
        return Err(ProbeFailure {
            kind: ProbeErrorKind::InvalidLink,
            message: format!("只支持 http(s) 链接：{url}"),
        });
    }
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
    // stdout/stderr 重定向到临时文件（不用管道）：
    // GUI 子进程管道缓冲区有限（Windows 默认 64KB），yt-dlp 输出大量 JSON 元数据时
    // 会阻塞在 write(stdout)，导致无法及时读取网络数据而超时。手动 CMD 输出直接到终端不会阻塞。
    // 临时文件必须落在 exe 同级 temp 目录（需求：所有产生的文件都存 exe 同级），
    // 禁止用 std::env::temp_dir()（会写到 AppData\Local\Temp）。
    let tmp_dir = temp_dir;
    // 进程 ID + 原子递增 slot：同进程并发解析不冲突，多实例也不冲突
    let slot = PROBE_SLOT.fetch_add(1, Ordering::Relaxed);
    let stdout_file = tmp_dir.join(format!(
        "ytdlp-probe-out-{}-{}.json",
        std::process::id(),
        slot
    ));
    let stderr_file = tmp_dir.join(format!(
        "ytdlp-probe-err-{}-{}.log",
        std::process::id(),
        slot
    ));
    let out_f = std::fs::File::create(&stdout_file).map_err(|e| ProbeFailure {
        kind: ProbeErrorKind::Failed,
        message: format!("创建临时输出文件失败：{e}"),
    })?;
    let err_f = std::fs::File::create(&stderr_file).map_err(|e| ProbeFailure {
        kind: ProbeErrorKind::Failed,
        message: format!("创建临时错误文件失败：{e}"),
    })?;
    cmd.stdout(Stdio::from(out_f));
    cmd.stderr(Stdio::from(err_f));

    let mut guard = ChildGuard::spawn(&mut cmd).map_err(|e| ProbeFailure {
        kind: ProbeErrorKind::Failed,
        message: format!("启动 yt-dlp 失败：{}", e),
    })?;
    let status = guard.wait().map_err(|e| ProbeFailure {
        kind: ProbeErrorKind::Failed,
        message: format!("yt-dlp 退出异常：{e}"),
    })?;
    // 读不回内容 ≠ yt-dlp 没输出：杀软占用/共享违规同样会让 read 失败。
    // 旧写法 unwrap_or_default 把这种情况变成"yt-dlp 返回无法解析的数据"，
    // 用户和排查者都会被引向完全错误的方向。
    let stdout_res = std::fs::read(&stdout_file);
    let stderr_res = std::fs::read(&stderr_file);
    let _ = std::fs::remove_file(&stdout_file);
    let _ = std::fs::remove_file(&stderr_file);
    let stdout_bytes = stdout_res.map_err(|e| ProbeFailure {
        kind: ProbeErrorKind::Failed,
        message: format!("读取 yt-dlp 探测输出失败（文件被占用？）：{e}"),
    })?;
    let stderr_bytes = stderr_res.unwrap_or_default();
    if !status.success() {
        let stderr = decode_text(&stderr_bytes);
        return Err(classify_ytdlp_error(stderr.trim()));
    }
    let text = decode_text(&stdout_bytes);
    parse_ytdlp_json(&text).map_err(|e| ProbeFailure {
        kind: ProbeErrorKind::Failed,
        message: format!("解析 yt-dlp 输出失败：{e}"),
    })
}

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
        // 输入必须挂在 `-i` 的值位上：裸位置参数遇到以 `-` 开头的文件名
        // （拖入 `-foo.mp4`）会被 ffprobe 当选项解析，症状是"探测失败"或
        // 拿到一份完全错误的元数据
        "-i",
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
    cmd.args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let guard = ChildGuard::spawn(&mut cmd).map_err(|e| ProbeFailure {
        kind: ProbeErrorKind::Failed,
        message: format!("启动 ffprobe 失败：{}", e),
    })?;
    let out = guard.wait_with_output().map_err(|e| ProbeFailure {
        kind: ProbeErrorKind::Failed,
        message: format!("ffprobe 退出异常：{}", e),
    })?;
    if !out.status.success() {
        return Err(ProbeFailure {
            kind: ProbeErrorKind::NotVideo,
            message: format!("ffprobe 探测失败：{}", decode_text(&out.stderr).trim()),
        });
    }
    let text = decode_text(&out.stdout);
    let mut meta = parse_ffprobe_json(&text);
    meta.size_bytes = std::fs::metadata(path).ok().map(|m| m.len());

    // 2) volumedetect（有音频流时）
    if meta.acodec.is_some() {
        match probe_volume(resolver, path, &mut on_log) {
            Ok(vol) => meta.audio_volume = vol,
            // 失败要说出来：静默吞掉等于告诉用户"这个文件不需要归一化"
            Err(e) => on_log(format!("音量探测失败，本次跳过归一化：{e}")),
        }
    }
    Ok(LocalProbe { meta })
}

/// 音量探测（ffmpeg volumedetect）。
pub fn probe_volume(
    resolver: &ToolResolver,
    path: &Path,
    on_log: &mut dyn FnMut(String),
) -> Result<AudioVolume> {
    let args: Vec<String> = ["-i"]
        .iter()
        .map(|s| s.to_string())
        .chain(std::iter::once(path.to_string_lossy().into_owned()))
        .chain(
            ["-af", "volumedetect", "-f", "null", "-"]
                .iter()
                .map(|s| s.to_string()),
        )
        .collect();
    on_log(crate::exec::display_command("ffmpeg", &args));
    let mut cmd = resolver.command(Tool::Ffmpeg)?;
    cmd.args(&args);
    cmd.stdout(Stdio::null()).stderr(Stdio::piped());
    let guard = ChildGuard::spawn(&mut cmd)?;
    let out = guard.wait_with_output()?;
    // 不检查退出码：ffmpeg 报 "Unknown encoder"/"Invalid data" 时 stderr 里
    // 根本没有 volumedetect 统计，parse_volumedetect 会返回全 None，
    // 于是"探测失败"被静默解释成"这文件没声音/已经很响"，归一化悄悄跳过
    if !out.status.success() {
        return Err(CoreError::ProcessFailed {
            program: "ffmpeg".into(),
            code: out.status.code(),
            stderr: decode_text(&out.stderr).trim().to_string(),
        });
    }
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
                    // 像素格式：合并直拼判据（MG-02）要拿它比 8bit/10bit 源
                    meta.pix_fmt = s["pix_fmt"].as_str().map(str::to_string);
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
    } else if lower.contains("private")
        || lower.contains("members only")
        || lower.contains("sign in")
        || lower.contains("log in")
        || lower.contains("cookies are required")
        || (lower.contains("account") && lower.contains("unavailable"))
    {
        // 只有真正带"要看登录态"信号的才判 NeedLogin。
        // 裸 "unavailable" 绝大多数是**视频被删/地区限制**——判成 NeedLogin 会给
        // 用户一个"去登录"按钮，他登录完再试还是失败，白白绕一圈。
        ProbeErrorKind::NeedLogin
    } else if lower.contains("video unavailable") || lower.contains("unavailable") {
        ProbeErrorKind::InvalidLink
    } else {
        ProbeErrorKind::Failed
    };
    // 取最后一条含 ERROR 的行：yt-dlp 的结尾行常是 traceback 尾巴或空行，
    // 直接 lines().last() 会把"未知错误"送给用户
    let message = stderr
        .lines()
        .rev()
        .find(|l| l.contains("ERROR"))
        .or_else(|| stderr.lines().rev().find(|l| !l.trim().is_empty()))
        .unwrap_or("未知错误")
        .trim()
        .to_string();
    ProbeFailure { kind, message }
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
    fn bare_unavailable_is_not_need_login() {
        // 被删/地区限制的视频不能给"去登录"按钮 —— 登录完再试还是失败
        let e = classify_ytdlp_error("ERROR: [youtube] abc: Video unavailable");
        assert_eq!(e.kind, ProbeErrorKind::InvalidLink, "{:?}", e);
        let e = classify_ytdlp_error("ERROR: This video is no longer available");
        assert_eq!(e.kind, ProbeErrorKind::InvalidLink);
        // 消息取"最后一条含 ERROR 的行"，不是可能被 traceback 占据的末行
        let e = classify_ytdlp_error(
            "ERROR: [youtube] abc: Video unavailable\nTraceback (inner most last):\n  File x",
        );
        assert!(e.message.starts_with("ERROR:"), "意外消息：{}", e.message);
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
