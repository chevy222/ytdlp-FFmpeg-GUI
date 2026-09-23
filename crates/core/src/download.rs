//! 下载能力（§3.3 DL-01~DL-11 + DL-04 后处理 + §3.2 MD-06 产物解析）。
//!
//! - 参数构造（纯函数可测）：格式回落 / 排序串 / 画质上限 / MP4 统一 / 不覆盖 / 模板
//! - 进度解析（纯函数可测）：`[download] xx% of xx at xx/s ETA xx`
//! - 执行：yt-dlp 子进程（--newline 逐行回调），取消杀进程树 + 清理输出残留
//! - 后处理（M1 基础版）：超画质上限降分辨率（libx265，QSV 协商留 M2）+ 音量归一化

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::config::{DownloadConfig, GeneralConfig};
use crate::exec::{
    decode_text, insert_before_url, is_http_url, progress_tail, push_url_arg, remove_with_retry,
    Monitored, Tool, ToolResolver,
};
use crate::model::MediaMeta;
use crate::probe;
use crate::{CoreError, Result};

/// 下载进度回调数据。
#[derive(Debug, Clone)]
pub struct Progress {
    pub percent: f32,
    pub speed: Option<String>,
    pub eta: Option<String>,
    pub file: Option<String>,
}

/// 下载参数（由 config 与用户选择组装）。
#[derive(Debug, Clone)]
pub struct DownloadParams {
    pub format_id: Option<String>,
    pub audio_only: bool,
    pub out_dir: PathBuf,
    pub filename_template: String,
    pub embed_cover: bool,
    pub proxy: Option<String>,
    pub cookies_file: Option<PathBuf>,
    pub sections: Option<(String, String)>,
    /// yt-dlp `--js-runtimes` 取值（如 `deno:C:\tools\deno.exe`）。
    /// YouTube 组件必需；留空表示交给 yt-dlp 自行探测 PATH。
    pub js_runtime: Option<String>,
    /// yt-dlp `--ffmpeg-location` 取值（ffmpeg 可执行文件路径）。
    /// 合并容器、嵌入封面、时间范围裁剪、音轨提取都依赖 ffmpeg，而 yt-dlp
    /// 默认只在 PATH 与自身目录查找；托管模式（`<exe 同级>\tools\`）必须显式传入。
    pub ffmpeg_path: Option<String>,
}

/// 文件名模板 → yt-dlp 输出模板（DL-10）。
pub fn output_template(tmpl: &str, playlist: bool) -> &'static str {
    if playlist {
        return "%(playlist_title)s/%(playlist_index)s - %(title)s.%(ext)s";
    }
    match tmpl {
        "标题+ID" => "%(title)s [%(id)s].%(ext)s",
        "UP主-标题" => "%(uploader)s - %(title)s.%(ext)s",
        "日期-标题" => "%(upload_date)s %(title)s.%(ext)s",
        _ => "%(title)s.%(ext)s",
    }
}

/// 长边上限（短边 max_h 的 16:9 对应长边，向上取整）。
///
/// yt-dlp 格式过滤器只能分别测试 width 和 height，用长边上限同时限制
/// 两个维度，可以兼容竖屏源（1080×1920 的 height=1920 不会被 `height<=1080`
/// 误排除）。与 download_video.bat 的 MAX_LONG 同语义。
fn long_side_cap(short_side: u32) -> u32 {
    ((short_side as u64) * 16).div_ceil(9) as u32
}

/// 默认格式串（7 级回落 + 画质上限，短边语义，兼容竖屏；参考 download_video.bat）。
///
/// 回落链：
/// 1. H.264+AAC（直拷进 MP4）→ 2. H.264+best audio → 3. any codec+best audio
/// 4. 单文件 ≤上限 → 5. 下载上限内 best video+best audio（触发后处理降分辨率）
/// 6. 无上限 best video+best audio → 7. 无上限 best single
///
/// 关键：用 `[height<=MAX_LONG][width<=MAX_LONG]` 同时限制两维，竖屏 1080×1920
/// 也算 1080p（旧实现只用 `height<=max_h` 会把竖屏源排除到 480p）。
pub fn default_format(max_h: u32, max_dl_h: u32) -> String {
    let ml = long_side_cap(max_h);
    let mdl = long_side_cap(max_dl_h);
    format!(
        "bv*[height<={ml}][width<={ml}][vcodec*=avc]+ba[acodec*=mp4a]/\
         bv*[height<={ml}][width<={ml}][vcodec*=avc]+ba/\
         bv*[height<={ml}][width<={ml}]+ba/\
         b[height<={ml}][width<={ml}]/\
         bv*[height<={mdl}][width<={mdl}]+ba/\
         bv*+ba/b"
    )
}

/// 排序串（DL-03）。
pub const SORT_SPEC: &str = "vcodec:h264,lang,quality,res,fps,acodec:aac,size,proto,ext";

/// 构造 yt-dlp 下载参数（纯函数）。
pub fn build_args(url: &str, p: &DownloadParams, cfg: &DownloadConfig) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    // 格式
    let format = match &p.format_id {
        Some(fid) if !fid.is_empty() => {
            if p.audio_only {
                format!("{}/bestaudio/best", fid)
            } else {
                format!("{}+ba/b", fid)
            }
        }
        _ => {
            if p.audio_only {
                "bestaudio/best".to_string()
            } else {
                default_format(cfg.max_h, cfg.max_dl_h)
            }
        }
    };
    args.push("--format".into());
    args.push(format);
    args.push("-S".into());
    args.push(SORT_SPEC.into());
    // 合并容器统一 MP4
    args.push("--merge-output-format".into());
    args.push("mp4".into());
    // 输出
    let tmpl = output_template(&p.filename_template, cfg.playlist);
    let out = p.out_dir.join(tmpl);
    args.push("-o".into());
    args.push(out.to_string_lossy().into_owned());
    // 不覆盖
    args.push("--no-overwrites".into());
    // 并发分片
    args.push("-N".into());
    args.push(cfg.fragments.to_string());
    // 重试
    args.push("--retries".into());
    args.push(cfg.retries.to_string());
    args.push("--retry-sleep".into());
    args.push("3".into());
    // 分片/文件访问重试（download_video.bat 同款：DASH/HLS 分片断流是高频失败点，
    // 只靠总重试会浪费整个任务的重试配额）
    args.push("--fragment-retries".into());
    args.push(cfg.retries.to_string());
    args.push("--file-access-retries".into());
    args.push(cfg.retries.to_string());
    // 封面/元数据
    if p.embed_cover {
        args.push("--embed-thumbnail".into());
        args.push("--embed-metadata".into());
        // --write-thumbnail 把封面（webp/jpg）同时写到输出目录，下载完成后
        // 直接取这个文件做列表缩略图，避免再跑 ffmpeg 抽帧（thumbs::collect_written_thumbnail）
        args.push("--write-thumbnail".into());
    }
    // 音频仅提取
    if p.audio_only {
        args.push("-x".into());
        args.push("--audio-format".into());
        args.push("mp3".into());
        args.push("--audio-quality".into());
        args.push("0".into());
    }
    // 播放列表
    if cfg.playlist {
        args.push("--yes-playlist".into());
    } else {
        args.push("--no-playlist".into());
    }
    // 代理
    if let Some(proxy) = &p.proxy {
        if !proxy.is_empty() {
            args.push("--proxy".into());
            args.push(proxy.clone());
        }
    }
    // Cookie
    if let Some(cf) = &p.cookies_file {
        args.push("--cookies".into());
        args.push(cf.to_string_lossy().into_owned());
    }
    // 时间范围（DL-12）
    if let Some((start, end)) = &p.sections {
        args.push("--download-sections".into());
        args.push(format!("*{}-{}", start, end));
    }
    // 进度输出（逐行，供解析）——注意不能加 --no-progress，否则
    // yt-dlp 不输出 [download] 进度行，percent 永远解析不到
    args.push("--newline".into());
    // 文件名安全
    args.push("--windows-filenames".into());
    args.push("--trim-filenames".into());
    args.push("120".into());
    // JS 运行时（§3.6 依赖）：YouTube 组件必需，托管/配置的 deno 必须显式传入
    if let Some(rt) = &p.js_runtime {
        if !rt.is_empty() {
            args.push("--js-runtimes".into());
            args.push(rt.clone());
        }
    }
    // ffmpeg 位置（§3.6 依赖）：合并容器（--merge-output-format）、嵌入封面、
    // 时间范围裁剪（--download-sections，帮助里写明 "Needs ffmpeg"）、音轨提取
    // 都依赖 ffmpeg。yt-dlp 默认只在 PATH 与自身目录查找，托管模式下必须显式传入。
    if let Some(ff) = &p.ffmpeg_path {
        if !ff.is_empty() {
            args.push("--ffmpeg-location".into());
            args.push(ff.clone());
        }
    }
    // 其他
    args.push("--no-warnings".into());
    // 注意：不加 `--ignore-errors` —— 它会让"下载/后处理失败"仍以退出码 0 结束，
    // 使 run_download 的成功判定失效（失败任务会被当成完成）。见 §4 失败安全。
    // URL 放最后，并用 `--` 终结选项解析：地址以 `-` 开头时（例如播放列表
    // JSON 里回来的恶意字符串）只能是一条下不动的链接，不能是 `--exec` 选项
    push_url_arg(&mut args, url);
    args
}

/// 解析 yt-dlp 进度行（纯函数）。返回 None 表示非进度行。
pub fn parse_progress_line(line: &str) -> Option<Progress> {
    let t = line.trim();
    if let Some(rest) = t.strip_prefix("[download]") {
        let rest = rest.trim();
        // Destination: <path>
        if let Some(path) = rest.strip_prefix("Destination:") {
            return Some(Progress {
                percent: 0.0,
                speed: None,
                eta: None,
                file: Some(path.trim().to_string()),
            });
        }
        // 45.2% of 123.4MiB at 5.2MiB/s ETA 00:15
        if let Some(caps) = FULL_RE.get_or_init(full_re).captures(rest) {
            let percent: f32 = caps[1].parse().ok()?;
            let speed = format!("{}{}/s", &caps[4], &caps[5]);
            let eta = caps[6].to_string();
            return Some(Progress {
                percent,
                speed: Some(speed),
                eta: Some(eta),
                file: None,
            });
        }
        // 100% 完成行等无 ETA 情况
        if let Some(caps) = PLAIN_RE.get_or_init(plain_re).captures(rest) {
            let percent: f32 = caps[1].parse().ok()?;
            return Some(Progress {
                percent,
                speed: None,
                eta: None,
                file: None,
            });
        }
        // Merger / has already been downloaded 等忽略
    }
    None
}

/// 完整的进度行正则。
///
/// yt-dlp 的进度行是**列对齐**的，数字字段带前导空格，实测输出形如：
/// `[download]  45.2% of    2.72MiB at   1.02MiB/s ETA 00:00`
/// —— `of` 后有 4 个空格、`at` 后有 2 个空格。早期实现写成 `of `（单个空格），
/// 导致**每一行都失配、进度恒为 0%**。这里一律用 `\s+`/`\s*` 容忍对齐空格。
///
/// 百分号也做成可选小数：完成行是 `100% of ... in 00:00:03 at ...`（无小数点）。
fn full_re() -> regex::Regex {
    regex::Regex::new(
        r"^(\d+(?:\.\d+)?)%\s+of\s+~?([\d.]+)\s*([KMG]i?B|B)\s+at\s+([\d.]+)\s*([KMG]i?B|B)/s\s+ETA\s+(\d+:\d+)",
    )
    .expect("无效正则 full")
}

/// 无 ETA 的进度行（首行 `ETA Unknown`、完成后的汇总行等）。
fn plain_re() -> regex::Regex {
    regex::Regex::new(r"^(\d+(?:\.\d+)?)%\s+of\s+~?([\d.]+)\s*([KMG]i?B|B)")
        .expect("无效正则 plain")
}

static FULL_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
static PLAIN_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();

/// 解析 yt-dlp 合并完成行：`[Merger] Merging formats into "<path>"`。
///
/// 音视频分流（DASH）下载时 `[download] Destination:` 指向随后被 yt-dlp 删除的
/// 中间文件（`xxx.f137.mp4` / `xxx.f140.m4a`），真正的最终产物只出现在这一行 ——
/// 不解析它就会拿到一堆已删除的路径，进而触发错误的兜底定位（MD-06）。
pub fn parse_merger_path(line: &str) -> Option<PathBuf> {
    let t = line.trim();
    let payload = t.split_once("Merging formats into ")?.1.trim();
    let path = match (payload.find('"'), payload.rfind('"')) {
        (Some(a), Some(b)) if b > a => &payload[a + 1..b],
        _ => payload,
    };
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

/// 解析 yt-dlp「已下载过」行：`[download] <path> has already been downloaded`。
///
/// `--no-overwrites` 下重复下载同一 URL 时 yt-dlp 不会产生 Destination 行，
/// 只打印这一行；不解析它会把"文件其实已存在"误判为下载失败。
pub fn parse_already_downloaded_path(line: &str) -> Option<PathBuf> {
    let t = line.trim();
    let rest = t.strip_prefix("[download]")?.trim();
    let path = rest.strip_suffix("has already been downloaded")?.trim();
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

/// 解析仅音频提取完成行：`[ExtractAudio] Destination: <path>`。
///
/// `-x --audio-format mp3`（DL-08）的**最终产物**只出现在这一行；此时
/// `[download] Destination:` 指向的是随后被删除/转码消费掉的中间文件
/// （`.webm`/`.m4a`），不解析 ExtractAudio 行就会拿错路径 → 音频下载
/// 成功却被误判为"未找到产物"。
pub fn parse_extract_audio_path(line: &str) -> Option<PathBuf> {
    let t = line.trim();
    let payload = t.strip_prefix("[ExtractAudio] Destination: ")?.trim();
    if payload.is_empty() {
        None
    } else {
        Some(PathBuf::from(payload))
    }
}

fn push_unique(v: &mut Vec<PathBuf>, p: PathBuf) {
    if !v.contains(&p) {
        v.push(p);
    }
}

/// 目录内"本次任务期间新增/更新"的**最新一个**视频文件（兜底产物定位）。
///
/// 与旧实现的关键差别：只接受 `since` 之后出现的文件，且只返回一个 ——
/// 绝不返回目录里用户原有的视频（旧实现返回目录内全部视频并取最旧的那个，
/// 后处理会把它重编码后原地覆盖）。宁可误报"未找到产物"，也不误伤既有文件。
/// 文件"新近度"时间：优先**创建时间**，非 Windows / 拿不到时回退 mtime。
///
/// yt-dlp 会把输出文件的 mtime 设成服务器 Last-modified 头，回退找"最新产物"时
/// 按 mtime 可能拿错文件（download_video.bat 的 `dir /o-d /t:c` 同款原因）。
fn file_newness(path: &Path) -> Option<std::time::SystemTime> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        // FILETIME：1601-01-01 起 100ns 间隔；与 UNIX 纪元差 11644473600 秒
        const WINDOWS_TO_UNIX_EPOCH_100NS: u64 = 116_444_736_000_000_000;

        let md = std::fs::metadata(path).ok()?;
        let filetime = md.creation_time();
        if filetime < WINDOWS_TO_UNIX_EPOCH_100NS {
            return None;
        }
        let unix_100ns = filetime - WINDOWS_TO_UNIX_EPOCH_100NS;
        let seconds = unix_100ns / 10_000_000;
        let nanos = (unix_100ns % 10_000_000) * 100;
        Some(
            std::time::UNIX_EPOCH
                + std::time::Duration::new(seconds, nanos as u32),
        )
    }
    #[cfg(not(windows))]
    {
        std::fs::metadata(path).and_then(|m| m.modified()).ok()
    }
}

pub fn newest_media_since(dir: &Path, since: std::time::SystemTime) -> Option<PathBuf> {
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let path = e.path();
            if !path.is_file() || !is_media_file(&path) {
                continue;
            }
            if let Some(t) = file_newness(&path) {
                if t >= since {
                    files.push((t, path));
                }
            }
        }
    }
    files.into_iter().max_by_key(|(t, _)| *t).map(|(_, p)| p)
}

/// 取消清理：删除本次运行已经落盘的产物与分片残留（§UL-06）。
///
/// 只处理"修改时间不早于本次启动"（`started`）的文件：`--no-overwrites` 下重复
/// 下载会复用输出目录里的既有文件（yt-dlp 报 "has already been downloaded"），
/// 那种文件绝不能在取消时被删掉。同理，拿不到 mtime 时按"不是本次产物"处理 ——
/// 宁可留下一个半成品，也不误删用户文件。
///
/// yt-dlp 正常收到终止信号时会自行清理 `.part`/`.ytdl`，但进程被
/// `taskkill /T /F` 强杀时往往来不及，这里按同名前缀兜底扫一遍。
fn cleanup_cancelled_outputs(
    extracted: &[PathBuf],
    dests: &[PathBuf],
    merged: &[PathBuf],
    started: std::time::SystemTime,
) {
    let is_new = |p: &Path| -> bool {
        file_newness(p)
            .map(|t| t >= started)
            .unwrap_or(false)
    };
    for p in extracted.iter().chain(merged.iter()).chain(dests.iter()) {
        if p.is_file() && is_new(p) {
            // 刚 taskkill 完句柄常还占着：一次删除失败要重试并留痕，
            // 否则"已取消，清理残留"是句假话，桌上留着几 GB 半成品
            if let Err(e) = remove_with_retry(p) {
                eprintln!("取消清理失败：{e}");
            }
        }
        let dir = match p.parent() {
            Some(d) => d,
            None => continue,
        };
        let name = match p.file_name().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let part_prefix = format!("{name}.part");
        let ytdl_name = format!("{name}.ytdl");
        let rd = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in rd.flatten() {
            let ep = entry.path();
            let en = match ep.file_name().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            if en.starts_with(&part_prefix) || en == ytdl_name {
                let _ = std::fs::remove_file(&ep);
            }
        }
    }
}

/// 下载结果。
#[derive(Debug, Clone)]
pub struct DownloadOutcome {
    pub output_paths: Vec<PathBuf>,
    /// 本次没有任何新下载，"产物"是 yt-dlp 报"已下载过"的既有文件。
    /// 调用方此时**不应**再对它做后处理（否则会原地重编码覆盖用户既有文件）。
    pub preexisting: bool,
}

/// `--print-to-file` 清单文件的进程内递增序号：并发任务必须各用一份，
/// 只用 pid + 时间戳会撞名（`started.elapsed()` 在函数入口恒为 0）。
static PRINT_SLOT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 执行下载（阻塞；逐行回调进度；取消置位后杀进程树）。
/// `on_log`：接收实际执行的完整 yt-dlp 命令行（条目日志展示用）。
/// `temp_dir`：exe 同级 temp 目录（所有运行时文件必须落在这里，禁止用系统 temp）。
#[allow(clippy::too_many_arguments)]
pub fn run_download(
    resolver: &ToolResolver,
    url: &str,
    p: &DownloadParams,
    cfg: &DownloadConfig,
    temp_dir: &Path,
    cancel: &Arc<AtomicBool>,
    mut on_progress: impl FnMut(Progress),
    mut on_log: impl FnMut(String),
) -> Result<DownloadOutcome> {
    let started = std::time::SystemTime::now();
    // 只接受 http(s)。地址可能来自站点返回的播放列表 JSON（见 probe::list_playlist_entries），
    // 这一层是离 yt-dlp 最近的闸口，不能只信 UI 入口的那次校验。
    if !is_http_url(url) {
        return Err(CoreError::InvalidInput(format!("只支持 http(s) 链接：{url}")));
    }
    let mut args = build_args(url, p, cfg);
    // --print-to-file after_move：yt-dlp 把最终产物路径写入此文件（UTF-8 无 BOM）。
    // 与解析输出行互为兜底：某些站点（如仅音频提取）不产生 Destination/Merger 行时，
    // 此文件是最可靠的产物定位来源。参考 download_video.bat 的 LASTFILE 机制。
    // 临时文件必须落在 exe 同级 temp 目录（需求：所有产生的文件都存 exe 同级），
    // 禁止用 std::env::temp_dir()（会写到 AppData\Local\Temp）。
    // 序号必须进程内递增：`started.elapsed()` 恒为 0，只带 pid 会让并发任务
    // 共用同一份清单文件、互相覆盖产物路径（同 probe 的 PROBE_SLOT 口径）。
    let print_file = temp_dir.join(format!(
        "ytdlp-print-{}-{}.txt",
        std::process::id(),
        PRINT_SLOT.fetch_add(1, Ordering::Relaxed)
    ));
    // 这三个是**选项**，必须插在 `--` 之前 —— 追加到尾部会被 yt-dlp 当成第二条 URL
    insert_before_url(
        &mut args,
        [
            "--print-to-file".to_string(),
            "after_move:%(filepath)s".to_string(),
            print_file.to_string_lossy().into_owned(),
        ],
    );
    on_log(crate::exec::display_command("yt-dlp", &args));
    let mut cmd = resolver.command(Tool::YtDlp)?;
    cmd.args(&args);

    // [download] Destination: <中间/最终文件>
    let mut dest_paths: Vec<PathBuf> = Vec::new();
    // [Merger] Merging formats into "<最终文件>"
    let mut merged_paths: Vec<PathBuf> = Vec::new();
    // [download] <文件> has already been downloaded
    let mut already_paths: Vec<PathBuf> = Vec::new();
    // [ExtractAudio] Destination: <最终音频文件>（DL-08 仅音频）
    let mut extracted_paths: Vec<PathBuf> = Vec::new();

    // stderr 由 Monitored 在独立线程持续排空：旧实现等进程退出后才读 stderr，
    // 出错刷屏（重试风暴）时管道缓冲写满、yt-dlp 阻塞在 write()，整个下载连
    // 取消一起挂死。
    let mut monitored = Monitored::spawn(&mut cmd)?;
    let done = match monitored.pump(cancel, |line| {
        if let Some(mp) = parse_merger_path(line) {
            push_unique(&mut merged_paths, mp);
        }
        if let Some(xp) = parse_extract_audio_path(line) {
            push_unique(&mut extracted_paths, xp);
        }
        if let Some(ap) = parse_already_downloaded_path(line) {
            push_unique(&mut already_paths, ap);
        }
        if let Some(prog) = parse_progress_line(line) {
            if let Some(f) = &prog.file {
                push_unique(&mut dest_paths, PathBuf::from(f));
            }
            on_progress(prog);
        }
    }) {
        Ok(done) => done,
        Err(e) => {
            // 取消：清掉本次已经落盘的产物与分片残留。否则"取消"之后输出目录里
            // 仍会多出一个视频，与用户对"取消"的预期不符。
            if matches!(e, CoreError::Cancelled) {
                cleanup_cancelled_outputs(&extracted_paths, &dest_paths, &merged_paths, started);
            }
            let _ = std::fs::remove_file(&print_file);
            return Err(e);
        }
    };
    // 最后一条进度行之后才按下取消：进程已正常结束，但用户要的是"别留东西"
    if cancel.load(Ordering::Relaxed) {
        cleanup_cancelled_outputs(&extracted_paths, &dest_paths, &merged_paths, started);
        let _ = std::fs::remove_file(&print_file);
        return Err(CoreError::Cancelled);
    }
    if !done.status.success() {
        let err = done.stderr.trim().to_string();
        let _ = std::fs::remove_file(&print_file);
        return Err(CoreError::ProcessFailed {
            program: "yt-dlp".into(),
            code: done.status.code(),
            stderr: err,
        });
    }

    // 产物收敛：
    // 1) ExtractAudio（仅音频最终产物）→ 合并产物行（DASH 下载的最终文件只出现在
    //    这里）→ Destination（未合并的单流/中间流）→ "已下载过"行；
    // 2) 这些路径都来自 yt-dlp 自己的输出，`--no-overwrites` 保证它不会去动既有文件，
    //    因此"存在即本次产物"（不叠加 mtime 判断：FAT/exFAT 时间戳粒度 2s 会误杀）；
    // 3) 仅当上面一条路径都没解析到时才做目录扫描兜底，且只认 since 之后新增的**最新一个**
    //    —— 绝不返回目录里用户原有的媒体文件（旧实现返回全部并取最旧的，会覆盖用户文件）。
    let mut output_paths: Vec<PathBuf> = Vec::new();
    for path in extracted_paths
        .iter()
        .chain(merged_paths.iter())
        .chain(dest_paths.iter())
        .chain(already_paths.iter())
    {
        push_unique(&mut output_paths, path.clone());
    }
    output_paths.retain(|path| path.is_file());
    // --print-to-file 兜底：某些站点不产生 Destination/Merger 行时，
    // yt-dlp 仍会把最终路径写入此文件。优先级高于目录扫描（更精确）。
    if output_paths.is_empty() {
        if let Ok(content) = std::fs::read_to_string(&print_file) {
            for line in content.lines() {
                let line = line.trim();
                if !line.is_empty() {
                    let pb = PathBuf::from(line);
                    if pb.is_file() {
                        push_unique(&mut output_paths, pb);
                    }
                }
            }
        }
    }
    let _ = std::fs::remove_file(&print_file);
    if output_paths.is_empty() {
        if let Some(found) = newest_media_since(&p.out_dir, started) {
            // 兜底靠时间戳，并发任务同目录时可能认领到兄弟任务的产物：
            // 命中必须留痕，否则事后无从判断这个文件是不是本任务下的
            on_log(format!("产物行未解析到，按时间兜底认领：{}", found.display()));
            output_paths.push(found);
        }
    }
    if output_paths.is_empty() {
        let err = done.stderr;
        let hint = if err.trim().is_empty() {
            "下载结束但未找到本次任务的产物文件（输出目录内既有文件未被改动）".to_string()
        } else {
            format!("下载结束但未找到产物文件。yt-dlp 输出：{}", err)
        };
        return Err(CoreError::ProcessFailed {
            program: "yt-dlp".into(),
            code: None,
            stderr: hint,
        });
    }
    // 本次没有产生新文件（只有"已下载过"的既有文件）→ 不做后处理，避免覆盖用户既有文件
    let preexisting = merged_paths.is_empty() && dest_paths.is_empty();
    Ok(DownloadOutcome {
        output_paths,
        preexisting,
    })
}

/// 取消/结束清理（§UL-06 取消清理）：
/// **只清理本任务私有目录与本次导出的 Cookie 临时文件**。
///
/// 旧实现直接删除全局 `temp/`，会连带删掉其它并发任务的临时目录与 Cookie 文件。
pub fn cleanup_on_cancel(temp_dir: &Path, cookies_file: Option<&Path>) {
    if temp_dir.is_dir() {
        let _ = std::fs::remove_dir_all(temp_dir);
    }
    if let Some(cf) = cookies_file {
        let _ = std::fs::remove_file(cf);
    }
}

/// 后处理（DL-04 M1 基础版）：超画质上限降分辨率 + 音量归一化。
/// 返回最终产物路径（处理失败时返回原路径并附告警日志）。
pub fn post_process(
    resolver: &ToolResolver,
    input: &Path,
    cfg: &DownloadConfig,
    general: &GeneralConfig,
    cancel: &Arc<AtomicBool>,
    mut on_log: impl FnMut(String),
) -> Result<(PathBuf, MediaMeta)> {
    // 先解析产物（MD-06 与后处理共用一次探测）
    let probe = probe::probe_local(resolver, input, &mut on_log)
        .map_err(|e| CoreError::Io(std::io::Error::other(format!("产物解析失败：{}", e))))?;
    let meta = &probe.meta;

    // 画质上限按**短边**（§6 max_h 语义；竖屏源依赖 width 采集，MD-02/P0-2），
    // 缩放表达式用旋转不变量 min(iw,ih)（与 TC-05 同源），竖屏源不再被砍短边
    let need_downscale = meta
        .short_edge()
        .map(|s| s > cfg.max_h && cfg.max_h > 0)
        .unwrap_or(false);
    // 只处理主音频：-map 0:a:0? 只保留第一条音轨，其余抛弃，
    // volumedetect 峰值与增益目标始终是同一条流。
    let need_gain = meta.needs_audio_gain(general.normalize_audio);

    if !need_downscale && !need_gain {
        return Ok((input.to_path_buf(), meta.clone()));
    }

    let out = input.with_extension("processed.mp4");
    // 宽高各自取偶（trunc */2*2）；短边超上限时等比缩小且不放大
    let vf = if need_downscale {
        Some(format!(
            "scale='trunc(iw*min(1,{}/min(iw,ih))/2)*2':'trunc(ih*min(1,{}/min(iw,ih))/2)*2'",
            cfg.max_h, cfg.max_h
        ))
    } else {
        None
    };
    let main_idx = meta.video_stream_index;
    let cover_idx = meta.cover_stream_index;

    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-i".into(),
        input.to_string_lossy().into_owned(),
    ];
    match (&vf, cover_idx) {
        // 主视频需要滤镜 + 存在封面流：简单滤镜 `-vf` 与第二条视频流的 `copy`
        // 不能共存（ffmpeg：Filtering and streamcopy cannot be used together），
        // 且 `0:m:attached_pic?` 本身不是合法流说明符。
        // 因此主视频走 filter_complex 打标签，封面按**绝对流索引**映射后原样 copy。
        (Some(filter), Some(ci)) => {
            let src = main_idx
                .map(|i| format!("0:{i}"))
                .unwrap_or_else(|| "0:v:0".to_string());
            args.push("-filter_complex".into());
            args.push(format!("[{src}]{filter}[v]"));
            args.push("-map".into());
            args.push("[v]".into());
            args.push("-map".into());
            args.push("0:a:0?".into());
            args.push("-map".into());
            args.push(format!("0:{ci}?"));
            args.push("-c:v:0".into());
            args.push("libx265".into());
            args.push("-crf".into());
            args.push("23".into());
            args.push("-preset".into());
            args.push("medium".into());
            // 必须带 `:v:0`：不带流后缀的 `-tag:v` 会落到 mjpeg 封面流上，
            // mp4 封装直接报 `Tag hvc1 incompatible with output codec id '7'`
            args.push("-tag:v:0".into());
            args.push("hvc1".into());
            args.push("-c:v:1".into());
            args.push("copy".into());
        }
        _ => {
            if let Some(filter) = &vf {
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
                args.push("-c:v:1".into());
                args.push("copy".into());
            }
            args.push("-c:v:0".into());
            args.push(if need_downscale { "libx265" } else { "copy" }.into());
            if need_downscale {
                args.push("-crf".into());
                args.push("23".into());
                args.push("-preset".into());
                args.push("medium".into());
                args.push("-tag:v:0".into());
                args.push("hvc1".into());
            }
        }
    }
    // 音频（增益到峰值 0dBFS，MAXGAIN 封顶 24dB，TC-07 语义）
    if need_gain {
        let max_v = meta.audio_volume.max_volume_db.unwrap_or(0.0);
        let gain = (-max_v).clamp(0.0, general.max_gain_db);
        if gain > 0.1 {
            on_log(format!(
                "音量归一化：max_volume {:.1}dB → +{:.1}dB 增益",
                max_v, gain
            ));
            args.push("-af".into());
            args.push(format!("volume={:.2}dB", gain));
        }
    }
    args.push("-c:a".into());
    if need_gain {
        args.push("aac".into());
        // 音频码率跟随源，clamp 64-192k（MediaMeta::audio_bitrate_kbps，bat 同款）
        let abr = meta.audio_bitrate_kbps();
        args.push("-b:a".into());
        args.push(format!("{}k", abr));
    } else {
        args.push("copy".into());
    }
    args.push("-movflags".into());
    args.push("+faststart".into());
    args.push("-y".into());
    // 进度走 stdout、stderr 只留真错误：不加这一组时 ffmpeg 每 0.5s 往 stderr
    // 写一条统计行，libx265 重编码几十秒就够把管道缓冲写满
    args.extend(progress_tail().iter().map(|s| s.to_string()));
    args.push(out.to_string_lossy().into_owned());
    on_log(crate::exec::display_command("ffmpeg", &args));

    let mut cmd = resolver.command(Tool::Ffmpeg)?;
    cmd.args(&args);
    // 旧写法把 stderr 设成 piped 却等进程退出之后才读：ffmpeg 的 stderr 写满管道
    // 缓冲后阻塞在 write()，try_wait 永远返回 None —— 开启"画质上限/音量归一化"
    // 的后处理必然卡在"后处理中"，只有取消能解开。Monitored 在独立线程排空两条管道。
    let mut monitored = Monitored::spawn(&mut cmd)?;
    let done = match monitored.pump(cancel, |_| {}) {
        Ok(done) => done,
        Err(e) => {
            let _ = std::fs::remove_file(&out);
            return Err(e);
        }
    };
    if !done.status.success() {
        on_log(format!("后处理失败（保留原文件）：{}", done.stderr.trim()));
        let _ = std::fs::remove_file(&out);
        return Ok((input.to_path_buf(), meta.clone()));
    }
    // 产物校验：降过分辨率才要求"能读出主视频流"；只做音量增益的音频产物根本没有
    // 视频流，旧实现一律走 verify_video，于是**仅音频下载的后处理永远"校验失败"**，
    // 归一化被静默跳过（日志只留一条误导性提示）。
    let verified = if need_downscale {
        verify_video(resolver, &out)
    } else {
        std::fs::metadata(&out).map(|m| m.len() >= 1024).unwrap_or(false)
    };
    if !verified {
        on_log("后处理产物校验失败（保留原文件）".to_string());
        let _ = remove_with_retry(&out);
        return Ok((input.to_path_buf(), meta.clone()));
    }
    // 后处理产物作为**新文件**落地，原文件一个字都不动。
    //
    // 原来 rename 覆盖 input：用户点一次下载，桌面上那个 `视频.mp4` 的内容就被
    // 换成 HEVC 了 —— 这与 build_args 里特意加 `--no-overwrites`（"避免原地重编码
    // 覆盖用户既有文件"）的策略自相矛盾，也违反 §1.3"失败安全、不破坏原文件"。
    let dir = input.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".to_string());
    let ext = input
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("mp4")
        .to_ascii_lowercase();
    let final_path = match crate::paths::unique_output_path(
        &dir,
        &format!("{stem}.opt"),
        &ext,
        "auto_inc",
    ) {
        Ok(p) => p,
        Err(e) => {
            on_log(format!("后处理产物命名失败，保留中间文件：{e}"));
            return Ok((out.clone(), meta.clone()));
        }
    };
    if let Err(e) = std::fs::rename(&out, &final_path) {
        on_log(format!("后处理产物落地失败（保留原文件）：{e}"));
        let _ = remove_with_retry(&out);
        return Ok((input.to_path_buf(), meta.clone()));
    }
    on_log(format!("后处理完成，产物：{}", final_path.display()));
    Ok((final_path, meta.clone()))
}

/// 产物校验：ffprobe 能解析且存在主视频流。
fn verify_video(resolver: &ToolResolver, path: &Path) -> bool {
    let p = path.to_string_lossy().into_owned();
    let args: Vec<&str> = vec![
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-show_entries",
        "stream=codec_name",
        "-of",
        "csv=p=0",
        // 输入放 `-i` 的值位，`-` 开头的文件名才不会被当成选项
        "-i",
        p.as_str(),
    ];
    match crate::exec::run_tool_capture(resolver, Tool::Ffprobe, &args) {
        Ok(out) => !decode_text(&out.stdout).trim().is_empty(),
        Err(_) => false,
    }
}

/// 是否视频扩展名。
/// 媒体文件扩展名判定（视频 + 音频）：产物定位与目录扫描共用同一份口径，
/// 避免"拖入单个 mp3 能加、含 mp3 的目录加不进"的三处口径漂移。
pub fn is_media_file(p: &Path) -> bool {
    matches!(
        p.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .as_deref(),
        Some(
            "mp4"
                | "mkv"
                | "mov"
                | "webm"
                | "avi"
                | "flv"
                | "ts"
                | "m4v"
                | "mp3"
                | "m4a"
                | "aac"
                | "flac"
                | "opus"
                | "wav"
                | "ogg"
        )
    )
}

/// 解析产物元数据（MD-06 下载完成产物解析；与本地文件相同链路）。
/// `on_log`：ffprobe/ffmpeg 命令行回传（条目日志展示用；不关心可传 no-op）。
pub fn probe_output(
    resolver: &ToolResolver,
    path: &Path,
    on_log: impl FnMut(String),
) -> Result<MediaMeta> {
    let p = probe::probe_local(resolver, path, on_log)
        .map_err(|e| CoreError::Io(std::io::Error::other(format!("产物解析失败：{}", e))))?;
    Ok(p.meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DownloadConfig;

    fn params() -> DownloadParams {
        DownloadParams {
            format_id: None,
            audio_only: false,
            out_dir: PathBuf::from("D:/videos"),
            filename_template: "纯标题".into(),
            embed_cover: true,
            proxy: Some("socks5://127.0.0.1:10808".into()),
            cookies_file: None,
            sections: None,
            js_runtime: None,
            ffmpeg_path: None,
        }
    }

    #[test]
    fn output_template_mapping() {
        assert_eq!(output_template("纯标题", false), "%(title)s.%(ext)s");
        assert_eq!(
            output_template("标题+ID", false),
            "%(title)s [%(id)s].%(ext)s"
        );
        assert_eq!(
            output_template("UP主-标题", false),
            "%(uploader)s - %(title)s.%(ext)s"
        );
        assert_eq!(
            output_template("日期-标题", false),
            "%(upload_date)s %(title)s.%(ext)s"
        );
        assert_eq!(output_template("未知", false), "%(title)s.%(ext)s");
        assert!(output_template("纯标题", true).contains("playlist"));
    }

    #[test]
    fn default_format_includes_max_h() {
        let f = default_format(1080, 2160);
        // 长边上限 = 1080*16/9 ≈ 1920，同时限制 height 和 width（兼容竖屏）
        assert!(f.contains("height<=1920"), "{}", f);
        assert!(f.contains("width<=1920"), "{}", f);
        // 7 级回落链的关键标记
        assert!(f.contains("[vcodec*=avc]+ba[acodec*=mp4a]"), "{}", f);
        assert!(f.contains("bv*+ba/b"), "{}", f);
        let f = default_format(720, 2160);
        assert!(f.contains("height<=1280"), "{}", f); // 720*16/9=1280
    }

    #[test]
    fn build_args_defaults() {
        let args = build_args(
            "https://example.com/v",
            &params(),
            &DownloadConfig::default(),
        );
        let joined = args.join(" ");
        assert!(joined.contains("--format"));
        // 7 级格式链：H.264+AAC 直拷优先，长边上限 1920 同时限制 width/height
        assert!(joined.contains("[vcodec*=avc]+ba[acodec*=mp4a]"), "{}", joined);
        assert!(joined.contains("height<=1920"), "{}", joined);
        assert!(joined.contains("width<=1920"), "{}", joined);
        assert!(joined.contains("--merge-output-format mp4"));
        assert!(joined.contains("--no-overwrites"));
        assert!(joined.contains("-N 4"));
        // 重试三件套（download_video.bat 同款：分片/文件访问单独配额）
        assert!(joined.contains("--retries 3"));
        assert!(joined.contains("--fragment-retries 3"));
        assert!(joined.contains("--file-access-retries 3"));
        assert!(joined.contains("--embed-thumbnail"));
        assert!(joined.contains("--no-playlist"));
        assert!(joined.contains("--proxy socks5://127.0.0.1:10808"));
        assert!(joined.contains("--newline"));
        assert!(joined.ends_with("https://example.com/v"));
        // --ignore-errors 会让失败仍以 0 退出，不能加（否则失败被当成功）
        assert!(!joined.contains("--ignore-errors"), "{}", joined);
    }

    #[test]
    fn build_args_passes_js_runtime() {
        let mut p = params();
        assert!(!build_args("u", &p, &DownloadConfig::default())
            .join(" ")
            .contains("--js-runtimes"));
        p.js_runtime = Some("deno:C:\\tools\\deno.exe".into());
        let joined = build_args("u", &p, &DownloadConfig::default()).join(" ");
        assert!(
            joined.contains("--js-runtimes deno:C:\\tools\\deno.exe"),
            "{}",
            joined
        );
    }

    #[test]
    fn build_args_passes_ffmpeg_location() {
        let mut p = params();
        assert!(!build_args("u", &p, &DownloadConfig::default())
            .join(" ")
            .contains("--ffmpeg-location"));
        p.ffmpeg_path = Some("C:\\tools\\ffmpeg.exe".into());
        let joined = build_args("u", &p, &DownloadConfig::default()).join(" ");
        assert!(
            joined.contains("--ffmpeg-location C:\\tools\\ffmpeg.exe"),
            "{}",
            joined
        );
    }

    #[test]
    fn parse_merger_line_extracts_final_path() {
        let p = parse_merger_path("[Merger] Merging formats into \"D:/videos/标题 [abc].mp4\"")
            .unwrap();
        assert_eq!(p, PathBuf::from("D:/videos/标题 [abc].mp4"));
        // 非合并行不误判
        assert!(parse_merger_path("[download] Destination: a.mp4").is_none());
        assert!(parse_merger_path("[Merger] Merging formats into ").is_none());
        assert!(parse_merger_path("").is_none());
    }

    #[test]
    fn parse_already_downloaded_line() {
        let p = parse_already_downloaded_path(
            "[download] D:/videos/标题 [abc].mp4 has already been downloaded",
        )
        .unwrap();
        assert_eq!(p, PathBuf::from("D:/videos/标题 [abc].mp4"));
        assert!(parse_already_downloaded_path("[download] Destination: a.mp4").is_none());
        assert!(parse_already_downloaded_path("[Merger] Merging formats into \"a.mp4\"").is_none());
        assert!(parse_already_downloaded_path("").is_none());
        // 空路径不算命中
        assert!(parse_already_downloaded_path("[download]  has already been downloaded").is_none());
    }

    #[test]
    fn newest_media_since_only_returns_new_files() {
        let dir = tempfile::tempdir().unwrap();
        // 已存在的旧文件（模拟"输出目录里用户原有的视频"）
        let old = dir.path().join("old.mp4");
        std::fs::write(&old, b"x").unwrap();
        std::fs::write(dir.path().join("note.txt"), b"x").unwrap();
        // 保证 new 的写入时间确实晚于 old（sleep 拉开先后；since 锚定在
        // old 的实际时间戳上，不依赖 sleep 的亚秒精度）
        std::thread::sleep(std::time::Duration::from_millis(20));
        let since = file_newness(&old).unwrap()
            + std::time::Duration::from_millis(1);
        let new = dir.path().join("new.mp4");
        std::fs::write(&new, b"x").unwrap();
        // 只返回本次之后新增的最新视频，绝不返回 old.mp4
        assert_eq!(newest_media_since(dir.path(), since), Some(new.clone()));
        // 没有任何新文件时返回 None（调用方据此判失败，而不是去动既有文件）
        let since2 = file_newness(&new).unwrap()
            + std::time::Duration::from_millis(1);
        assert_eq!(newest_media_since(dir.path(), since2), None);
    }

    #[test]
    fn build_args_selected_format() {
        let mut p = params();
        p.format_id = Some("137".into());
        let args = build_args("u", &p, &DownloadConfig::default());
        let joined = args.join(" ");
        assert!(joined.contains("137+ba/b"));
    }

    #[test]
    fn build_args_audio_only() {
        let mut p = params();
        p.audio_only = true;
        p.format_id = Some("140".into());
        let args = build_args("u", &p, &DownloadConfig::default());
        let joined = args.join(" ");
        assert!(joined.contains("-x"));
        assert!(joined.contains("--audio-format mp3"));
        assert!(joined.contains("140/bestaudio/best"));
    }

    #[test]
    fn parse_progress_full_line() {
        // 以下都是 yt-dlp 2026.08.19 的**真实输出**（列对齐，数字字段带前导空格）。
        // 旧实现把正则写成 `of `（单个空格）→ 全部失配 → 进度恒为 0%。
        let p = parse_progress_line("[download]  45.2% of    2.72MiB at   1.02MiB/s ETA 00:02")
            .unwrap();
        assert!((p.percent - 45.2).abs() < 0.01);
        assert_eq!(p.speed.as_deref(), Some("1.02MiB/s"));
        assert_eq!(p.eta.as_deref(), Some("00:02"));

        // 首行：speed 与 ETA 都是 Unknown → 走无 ETA 的 PLAIN 分支
        let p = parse_progress_line("[download]   0.0% of    2.72MiB at  Unknown B/s ETA Unknown")
            .unwrap();
        assert_eq!(p.percent, 0.0);
        assert!(p.speed.is_none());

        // 完成行：百分比无小数、用 `in <耗时>` 而非 ETA
        let p = parse_progress_line("[download] 100% of    2.72MiB in 00:00:03 at 885.98KiB/s")
            .unwrap();
        assert_eq!(p.percent, 100.0);
        assert!(p.speed.is_none());

        // 并发/分片下载时总量是估算值，带 `~` 前缀
        let p =
            parse_progress_line("[download]  12.3% of ~123.45MiB at  1.23MiB/s ETA 00:12").unwrap();
        assert!((p.percent - 12.3).abs() < 0.01);
    }

    #[test]
    fn parse_progress_destination() {
        let p = parse_progress_line("[download] Destination: D:/videos/测试视频.mp4").unwrap();
        assert_eq!(p.file.as_deref(), Some("D:/videos/测试视频.mp4"));
    }

    #[test]
    fn parse_progress_plain_percent() {
        let p = parse_progress_line("[download] 100.0% of 35.6MiB").unwrap();
        assert_eq!(p.percent, 100.0);
        assert!(p.speed.is_none());
    }

    #[test]
    fn parse_progress_ignores_other_lines() {
        assert!(parse_progress_line("[youtube] abc: Downloading webpage").is_none());
        assert!(parse_progress_line("Merging formats into ...").is_none());
        assert!(parse_progress_line("").is_none());
    }

    #[test]
    fn is_media_file_detects_exts() {
        assert!(is_media_file(Path::new("a.MP4")));
        assert!(is_media_file(Path::new("a.mkv")));
        assert!(!is_media_file(Path::new("a.jpg")));
        assert!(is_media_file(Path::new("a.mp3")));
        assert!(is_media_file(Path::new("a.m4a")));
        assert!(!is_media_file(Path::new("a.jpg")));
    }

    #[test]
    fn parse_extract_audio_destination() {
        let p =
            parse_extract_audio_path("[ExtractAudio] Destination: D:/videos/测试音频.mp3").unwrap();
        assert_eq!(p, PathBuf::from("D:/videos/测试音频.mp3"));
        assert_eq!(
            parse_extract_audio_path("[ExtractAudio] Destination: "),
            None
        );
        assert_eq!(parse_extract_audio_path("[download] 1.0% of 1MiB"), None);
    }
}
