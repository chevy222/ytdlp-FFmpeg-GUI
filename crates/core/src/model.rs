//! 统一列表条目 MediaItem 与状态机（需求文档 §7.1 / §3.1）。

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

/// 条目状态（与 UI"已就绪"对应的内部枚举名为 `Ready`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Status {
    /// 解析中（URL 取信息 / 本地 ffprobe 探测）
    Probing,
    /// 已就绪（已解析完成、可执行下载/转码/合并动作；UI 术语"已就绪"）
    Ready,
    /// 下载中
    Downloading,
    /// 后处理中（下载产物画质上限/封面/元数据/音量归一化）
    PostProcessing,
    /// 转码中
    Transcoding,
    /// 合并中
    Merging,
    /// 已完成（含下载完成）
    Done,
    /// 失败（含解析失败/下载失败/转码失败）
    Failed,
    /// 已取消（取消时清理 temp 与输出目录残留）
    Canceled,
    /// 需要登录（可恢复：WebView2 登录后续传）
    NeedLogin,
}

impl Status {
    /// 是否为终态（不可再执行动作，仅可删除/重试）。
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Canceled)
    }

    /// 是否为处理中（下载/后处理/转码/合并，取消按钮出现的状态）。
    pub fn is_processing(self) -> bool {
        matches!(
            self,
            Self::Downloading | Self::PostProcessing | Self::Transcoding | Self::Merging
        )
    }

    /// UI 文案（状态用颜色 + 文字双表达，见 §6.3）。
    pub fn label(self) -> &'static str {
        match self {
            Self::Probing => "解析中",
            Self::Ready => "已就绪",
            Self::Downloading => "下载中",
            Self::PostProcessing => "后处理中",
            Self::Transcoding => "转码中",
            Self::Merging => "合并中",
            Self::Done => "已完成",
            Self::Failed => "失败",
            Self::Canceled => "已取消",
            Self::NeedLogin => "需要登录",
        }
    }
}

/// 旋转角度（0°/90°/180°/270°，封面旋转箭头指定，随条目保存，转码生效）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RotAngle(u16);

impl RotAngle {
    pub const ZERO: Self = Self(0);

    pub fn from_degrees(deg: u16) -> Self {
        Self((deg / 90) % 4 * 90)
    }

    pub fn degrees(self) -> u16 {
        self.0
    }

    /// 顺时针旋转 90°。
    pub fn rotate_cw(self) -> Self {
        Self::from_degrees(self.0 + 90)
    }

    /// 逆时针旋转 90°。
    pub fn rotate_ccw(self) -> Self {
        Self::from_degrees(self.0 + 270)
    }
}

impl Default for RotAngle {
    fn default() -> Self {
        Self::ZERO
    }
}

/// 条目来源类型（统一列表：URL 任务 / 本地文件 / 转码产物 / 合并产物）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ItemKind {
    /// URL 下载任务
    UrlTask,
    /// 本地文件/目录（添加后先解析）
    LocalFile,
    /// 转码产物（回到列表）
    TranscodeOut,
    /// 合并产物（回到列表）
    MergeOut,
}

/// 音频音量探测结果（volumedetect，供转码增益决策，§MD-02/MD-06/TC-07）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AudioVolume {
    pub mean_volume_db: Option<f32>,
    pub max_volume_db: Option<f32>,
}

/// 解析元数据（ffprobe / yt-dlp -j 结果，画质/格式列 11 项字段，§6.2）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MediaMeta {
    pub title: Option<String>,
    pub duration_secs: Option<f64>,
    /// 容器格式，如 MP4 / MKV
    pub container: Option<String>,
    /// 分辨率（短边），如 2160/1080/720
    pub height: Option<u32>,
    /// 视频编码器，如 HEVC / H.264 / AV1
    pub vcodec: Option<String>,
    /// 视频码率 kbps
    pub vbitrate_kbps: Option<u32>,
    /// 帧率
    pub fps: Option<f64>,
    /// 音频编码器
    pub acodec: Option<String>,
    /// 音频码率 kbps
    pub abitrate_kbps: Option<u32>,
    /// 音轨数
    pub audio_tracks: Option<u32>,
    /// 声道数（首个音频流 channels）
    pub audio_channels: Option<u32>,
    /// 最大音量（转码增益依据）
    pub audio_volume: AudioVolume,
    /// 文件大小字节
    pub size_bytes: Option<u64>,
    /// 是否含封面
    pub has_cover: bool,
    /// 视频流 extradata（SPS/PPS 等，hex）——合并直拼硬性条件（MG-02）
    #[serde(default)]
    pub extradata: Option<String>,
    /// 首音频流采样率 Hz
    #[serde(default)]
    pub sample_rate: Option<u32>,
    /// 主视频流绝对索引（ffprobe `index`）。
    /// 封面流（attached_pic）可能排在主视频之前，映射必须用绝对索引，
    /// 否则 `0:v:0` 会选到封面（§TC-08 / MD-02）。
    #[serde(default)]
    pub video_stream_index: Option<u32>,
    /// 封面流（attached_pic）绝对索引；`-map` / `-filter_complex` 按此映射。
    /// 注意：ffmpeg 的 `0:t?` 在 MP4 上**选不中** attached_pic（实测），必须用绝对索引。
    #[serde(default)]
    pub cover_stream_index: Option<u32>,
    /// 容器内旋转标记（stream tags `rotate`，MD-02 采集）。
    /// 仅记录源文件标记；实际转码采用条目 `rot_angle`（TC-04 手动旋转）。
    #[serde(default)]
    pub rotate_tag: Option<i32>,
    /// 源编码宽度（竖屏源与 rotate_tag 配合判定"短边"分辨率，MD-02/§8）。
    #[serde(default)]
    pub width: Option<u32>,
    /// 结构化下载格式列表（DL-02 格式选择；含 format_id 供下载使用）
    #[serde(default)]
    pub download_formats: Vec<DownloadFormat>,
}

/// 下载格式（DL-02：清晰度/编码/大小/帧率/码率，格式弹窗展示项）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DownloadFormat {
    pub format_id: String,
    /// 可读标签（如 `1080P · MP4 · H.264 · 5.2MB · 30fps`）
    pub label: String,
    pub height: Option<u32>,
    pub ext: Option<String>,
    pub vcodec: Option<String>,
    pub acodec: Option<String>,
    pub filesize_bytes: Option<u64>,
    pub fps: Option<f64>,
    pub tbr_kbps: Option<u32>,
    /// 附加说明（如"需登录"、"AV1"）
    pub note: Option<String>,
    /// 是否仅音频格式
    #[serde(default)]
    pub audio_only: bool,
}

impl DownloadFormat {
    /// 生成可读标签（清晰度 · 容器 · 编码 · 大小 · 帧率）。
    pub fn make_label(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(h) = self.height {
            parts.push(format!("{}P", h));
        } else if self.audio_only {
            parts.push("仅音频".into());
        } else {
            parts.push("自适应".into());
        }
        if let Some(e) = &self.ext {
            parts.push(e.to_uppercase());
        }
        if let Some(c) = &self.vcodec {
            let cl = c.to_lowercase();
            // yt-dlp 的 vcodec 常带 profile 后缀（vp09.00.50.08 / av01.0.08M.08），
            // 必须前缀匹配而非精确匹配，否则整串塞进标签。
            let short = if cl.starts_with("av01") {
                "AV1"
            } else if cl.starts_with("avc1") || cl.starts_with("h264") || cl.starts_with("h.264") {
                "H.264"
            } else if cl.starts_with("hevc") || cl.starts_with("h265") || cl.starts_with("h.265") {
                "H.265"
            } else if cl.starts_with("vp9") || cl.starts_with("vp09") {
                "VP9"
            } else {
                c.as_str()
            };
            parts.push(short.to_string());
        }
        if let Some(a) = &self.acodec {
            let al = a.to_lowercase();
            let short = if al.starts_with("mp4a") || al.starts_with("aac") {
                "AAC"
            } else if al.starts_with("opus") {
                "Opus"
            } else {
                a.as_str()
            };
            parts.push(short.to_string());
        }
        if let Some(f) = self.filesize_bytes {
            parts.push(human_size(f));
        }
        if let Some(f) = self.fps {
            parts.push(format!("{:.0}fps", f));
        }
        parts.join(" · ")
    }
}

impl MediaMeta {
    /// 分辨率短边：无论横竖屏，都取 min(宽,高)。
    /// 文档 §8 口径"分辨率(4K/2K/xP)"按短边计（§6.2）。
    pub fn short_edge(&self) -> Option<u32> {
        let h = self.height?;
        match self.width {
            Some(w) => Some(w.min(h)),
            None => Some(h),
        }
    }

    /// 是否需要音量增益：normalize 开启 + 峰值有效（-100~-0.5dB）+ **单音轨**。
    /// 多音轨跳过增益：volumedetect 峰值只测了第一轨，一个增益应用到所有轨
    /// 可能削波或不足（download_video.bat PROBE_AUDIO 同款保护）。
    pub fn needs_audio_gain(&self, normalize: bool) -> bool {
        normalize
            && self.audio_tracks.unwrap_or(1) <= 1
            && self
                .audio_volume
                .max_volume_db
                .map(|v| v < -0.5 && v > -100.0)
                .unwrap_or(false)
    }

    /// 音频重编码码率 kbps：跟随源，clamp 64-192（convert_h265.bat 同款：
    /// 低码率源不膨胀、高码率源不浪费），源不可探测时 128k 兜底。
    pub fn audio_bitrate_kbps(&self) -> u32 {
        self.abitrate_kbps.unwrap_or(128).clamp(64, 192)
    }
}

/// 人类可读大小（KB/MB/GB）。
pub fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1}GB", b / GB)
    } else if b >= MB {
        format!("{:.1}MB", b / MB)
    } else if b >= KB {
        format!("{:.1}KB", b / KB)
    } else {
        format!("{}B", bytes)
    }
}

/// 统一列表条目（§7.1）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaItem {
    pub id: String,
    pub kind: ItemKind,
    pub title: String,
    pub path: Option<String>,
    pub url: Option<String>,
    pub site: Option<String>,
    pub host: Option<String>,
    pub status: Status,
    pub percent: f32,
    pub error: Option<String>,
    /// 下载进度附加信息（§7.1 speed/eta/file）
    #[serde(default)]
    pub speed: Option<String>,
    #[serde(default)]
    pub eta: Option<String>,
    #[serde(default)]
    pub file: Option<String>,
    /// 最近日志行（≤300 行，按条目查看，§3.8/§6.2）
    #[serde(default)]
    pub log: VecDeque<String>,
    #[serde(default)]
    pub meta: MediaMeta,
    /// 封面缩略图本地路径（config/cache/thumbs/<id>.jpg）
    #[serde(default)]
    pub thumb: Option<String>,
    pub rot_angle: RotAngle,
    /// 下载任务专属：选中格式
    #[serde(default)]
    pub format_id: Option<String>,
    #[serde(default)]
    pub audio_only: bool,
    #[serde(default)]
    pub updated_at: String,
    /// 时间范围下载（DL-12）：起止 "HH:MM:SS"（yt-dlp --download-sections）
    #[serde(default)]
    pub sections: Option<(String, String)>,
}

impl MediaItem {
    pub fn new(kind: ItemKind, title: String) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            kind,
            title,
            path: None,
            url: None,
            site: None,
            host: None,
            status: Status::Probing,
            percent: 0.0,
            sections: None,
            error: None,
            speed: None,
            eta: None,
            file: None,
            log: VecDeque::new(),
            meta: MediaMeta::default(),
            thumb: None,
            rot_angle: RotAngle::ZERO,
            format_id: None,
            audio_only: false,
            // 创建即打时间戳：前端列表按 updated_at 降序（最新的在上），
            // 空串会被排到末尾（历史 bug：普通条目从未赋值，新条目永远沉底）
            updated_at: crate::timefmt::datetime_str(
                crate::timefmt::now_secs(),
                crate::timefmt::local_offset_secs(),
            ),
        }
    }

    pub fn from_url(url: String) -> Self {
        let mut it = Self::new(ItemKind::UrlTask, url.clone());
        it.url = Some(url);
        it
    }

    pub fn from_path(path: String) -> Self {
        let title = std::path::Path::new(&path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.clone());
        let mut it = Self::new(ItemKind::LocalFile, title);
        it.path = Some(path);
        it
    }

    /// 日志追加，保留最近 `MAX_LOG_LINES` 行（300）。
    pub fn push_log(&mut self, line: impl Into<String>) {
        const MAX_LOG_LINES: usize = 300;
        self.log.push_back(line.into());
        while self.log.len() > MAX_LOG_LINES {
            self.log.pop_front();
        }
    }
}

/// 状态机：合法迁移校验（§3.1 UL-06）。
///
/// 允许的迁移：
/// ```text
/// Probing -> Ready | Failed | Canceled | NeedLogin
/// Ready   -> Downloading | Transcoding | Merging | Canceled
/// Downloading -> PostProcessing | Failed | Canceled | NeedLogin
/// PostProcessing -> Done | Failed | Canceled
/// Transcoding -> Done | Failed | Canceled | Ready(任务结束恢复)
/// Merging -> Done | Failed | Canceled | Ready(任务结束恢复)
/// Done/Failed/Canceled -> Probing (重试/重新解析)
/// Failed -> NeedLogin (登录后转解析)
/// NeedLogin -> Probing (登录完成自动重新解析)
/// ```
// 白名单迁移表用显式 match 保持可读，禁用 matches! 风格提示。
#[allow(clippy::match_like_matches_macro)]
pub fn transition(from: Status, to: Status) -> Result<Status, crate::CoreError> {
    let ok = match (from, to) {
        (Status::Probing, Status::Ready)
        | (Status::Probing, Status::Failed)
        | (Status::Probing, Status::Canceled)
        | (Status::Probing, Status::NeedLogin)
        | (Status::Ready, Status::Downloading)
        | (Status::Ready, Status::Transcoding)
        | (Status::Ready, Status::Merging)
        | (Status::Ready, Status::Canceled)
        | (Status::Downloading, Status::PostProcessing)
        | (Status::Downloading, Status::Failed)
        | (Status::Downloading, Status::Canceled)
        | (Status::Downloading, Status::NeedLogin)
        | (Status::Downloading, Status::Done)
        | (Status::PostProcessing, Status::Done)
        | (Status::PostProcessing, Status::Failed)
        | (Status::PostProcessing, Status::Canceled)
        | (Status::Done, Status::Transcoding)
        | (Status::Done, Status::Merging)
        | (Status::Done, Status::Probing)
        | (Status::Transcoding, Status::Done)
        | (Status::Transcoding, Status::Failed)
        | (Status::Transcoding, Status::Canceled)
        // 任务结束恢复：转码/合并的参与条目回到"已就绪"（本地/转码产物）
        | (Status::Transcoding, Status::Ready)
        | (Status::Merging, Status::Done)
        | (Status::Merging, Status::Failed)
        | (Status::Merging, Status::Canceled)
        // 任务结束恢复：合并的参与条目回到"已就绪"（本地/转码产物）
        | (Status::Merging, Status::Ready)
        | (Status::Failed, Status::Probing)
        | (Status::Failed, Status::NeedLogin)
        | (Status::Canceled, Status::Probing)
        | (Status::NeedLogin, Status::Probing) => true,
        _ => false,
    };
    if ok {
        Ok(to)
    } else {
        Err(crate::CoreError::InvalidTransition { from, to })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item() -> MediaItem {
        MediaItem::new(ItemKind::LocalFile, "测试.mp4".into())
    }

    #[test]
    fn new_item_starts_probing() {
        assert_eq!(item().status, Status::Probing);
    }

    #[test]
    fn from_url_sets_kind_and_url() {
        let it = MediaItem::from_url("https://www.bilibili.com/video/BV1xx".into());
        assert_eq!(it.kind, ItemKind::UrlTask);
        assert_eq!(
            it.url.as_deref(),
            Some("https://www.bilibili.com/video/BV1xx")
        );
        assert!(it.status == Status::Probing);
    }

    #[test]
    fn from_path_uses_filename_as_title() {
        let it = MediaItem::from_path("D:/Videos/手机录像/VID_1.mp4".into());
        assert_eq!(it.kind, ItemKind::LocalFile);
        assert_eq!(it.title, "VID_1.mp4");
        assert_eq!(it.path.as_deref(), Some("D:/Videos/手机录像/VID_1.mp4"));
    }

    #[test]
    fn log_keeps_last_300_lines() {
        let mut it = item();
        for i in 0..320 {
            it.push_log(format!("line {}", i));
        }
        assert_eq!(it.log.len(), 300);
        assert_eq!(it.log.front().map(String::as_str), Some("line 20"));
        assert_eq!(it.log.back().map(String::as_str), Some("line 319"));
    }

    #[test]
    fn rot_angle_cw_cycles() {
        let mut a = RotAngle::ZERO;
        a = a.rotate_cw();
        assert_eq!(a.degrees(), 90);
        a = a.rotate_cw();
        assert_eq!(a.degrees(), 180);
        a = a.rotate_cw();
        assert_eq!(a.degrees(), 270);
        a = a.rotate_cw();
        assert_eq!(a.degrees(), 0);
    }

    #[test]
    fn rot_angle_ccw() {
        assert_eq!(RotAngle::ZERO.rotate_ccw().degrees(), 270);
    }

    #[test]
    fn rot_angle_from_degrees_normalizes() {
        assert_eq!(RotAngle::from_degrees(540).degrees(), 180);
        assert_eq!(RotAngle::from_degrees(135).degrees(), 90);
    }

    #[test]
    fn terminal_statuses() {
        assert!(Status::Done.is_terminal());
        assert!(Status::Failed.is_terminal());
        assert!(Status::Canceled.is_terminal());
        assert!(!Status::Ready.is_terminal());
        assert!(!Status::Downloading.is_terminal());
    }

    #[test]
    fn processing_statuses() {
        assert!(Status::Downloading.is_processing());
        assert!(Status::PostProcessing.is_processing());
        assert!(Status::Transcoding.is_processing());
        assert!(Status::Merging.is_processing());
        assert!(!Status::Ready.is_processing());
        assert!(!Status::Probing.is_processing());
    }

    #[test]
    fn status_labels_chinese() {
        assert_eq!(Status::Ready.label(), "已就绪");
        assert_eq!(Status::NeedLogin.label(), "需要登录");
        assert_eq!(Status::Probing.label(), "解析中");
    }

    #[test]
    fn transition_probing_to_ready_ok() {
        assert_eq!(
            transition(Status::Probing, Status::Ready).unwrap(),
            Status::Ready
        );
    }

    #[test]
    fn transition_download_to_postprocess_ok() {
        assert_eq!(
            transition(Status::Downloading, Status::PostProcessing).unwrap(),
            Status::PostProcessing
        );
    }

    #[test]
    fn transition_transcode_to_done_ok() {
        assert_eq!(
            transition(Status::Transcoding, Status::Done).unwrap(),
            Status::Done
        );
    }

    #[test]
    fn transition_merge_to_done_ok() {
        assert_eq!(
            transition(Status::Merging, Status::Done).unwrap(),
            Status::Done
        );
    }

    #[test]
    fn transition_needlogin_to_probing_ok() {
        assert_eq!(
            transition(Status::NeedLogin, Status::Probing).unwrap(),
            Status::Probing
        );
    }

    #[test]
    fn transition_failed_to_needlogin_ok() {
        assert_eq!(
            transition(Status::Failed, Status::NeedLogin).unwrap(),
            Status::NeedLogin
        );
    }

    #[test]
    fn transition_terminal_to_probing_for_retry() {
        assert_eq!(
            transition(Status::Failed, Status::Probing).unwrap(),
            Status::Probing
        );
        assert_eq!(
            transition(Status::Canceled, Status::Probing).unwrap(),
            Status::Probing
        );
        assert_eq!(
            transition(Status::Done, Status::Probing).unwrap(),
            Status::Probing
        );
    }

    #[test]
    fn transition_restore_to_ready_allowed() {
        // 任务结束恢复：转码/合并参与条目回到"已就绪"
        assert_eq!(
            transition(Status::Transcoding, Status::Ready).unwrap(),
            Status::Ready
        );
        assert_eq!(
            transition(Status::Merging, Status::Ready).unwrap(),
            Status::Ready
        );
    }

    #[test]
    fn transition_done_to_transcoding_allowed() {
        // TC 语义：下载完成（Done）的产物可直接进入转码
        assert_eq!(
            transition(Status::Done, Status::Transcoding).unwrap(),
            Status::Transcoding
        );
    }

    #[test]
    fn transition_ready_to_probing_rejected() {
        assert!(transition(Status::Ready, Status::Probing).is_err());
    }

    #[test]
    fn transition_probing_to_done_rejected() {
        assert!(transition(Status::Probing, Status::Done).is_err());
    }

    #[test]
    fn short_edge_respects_rotation() {
        // 竖屏源 1080x1920：短边 1080，不管 rotate_tag
        let mut m = MediaMeta {
            height: Some(1920),
            width: Some(1080),
            rotate_tag: Some(90),
            ..Default::default()
        };
        assert_eq!(m.short_edge(), Some(1080));
        m.rotate_tag = None;
        assert_eq!(m.short_edge(), Some(1080));
        // width 缺失时退回 height
        m.width = None;
        m.rotate_tag = Some(90);
        assert_eq!(m.short_edge(), Some(1920));
    }

    #[test]
    fn needs_audio_gain_skips_multitrack() {
        // 单音轨 + 音量有效 → 增益
        let mut m = MediaMeta {
            audio_tracks: Some(1),
            audio_volume: AudioVolume {
                max_volume_db: Some(-8.2),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(m.needs_audio_gain(true));
        // 多音轨 → 跳过（峰值只测了第一轨，bat PROBE_AUDIO 同款）
        m.audio_tracks = Some(2);
        assert!(!m.needs_audio_gain(true));
        // normalize 关 → 不增益
        m.audio_tracks = Some(1);
        assert!(!m.needs_audio_gain(false));
        // 接近满度/无音量 → 不增益
        m.audio_volume.max_volume_db = Some(-0.2);
        assert!(!m.needs_audio_gain(true));
        m.audio_volume.max_volume_db = None;
        assert!(!m.needs_audio_gain(true));
    }

    #[test]
    fn audio_bitrate_clamped_64_192() {
        let mut m = MediaMeta::default();
        assert_eq!(m.audio_bitrate_kbps(), 128); // 不可探测兜底 128
        m.abitrate_kbps = Some(48);
        assert_eq!(m.audio_bitrate_kbps(), 64); // 下限 64
        m.abitrate_kbps = Some(320);
        assert_eq!(m.audio_bitrate_kbps(), 192); // 上限 192
        m.abitrate_kbps = Some(96);
        assert_eq!(m.audio_bitrate_kbps(), 96); // 区间内跟随源
    }

    #[test]
    fn codec_label_prefix_match() {
        let f = DownloadFormat {
            vcodec: Some("vp09.00.50.08".into()),
            acodec: Some("mp4a.40.2".into()),
            ..Default::default()
        };
        let label = f.make_label();
        assert!(label.contains("VP9"));
        assert!(label.contains("AAC"));
    }

    #[test]
    fn human_size_formats() {
        assert_eq!(human_size(1024), "1.0KB");
        assert_eq!(human_size(1024 * 1024), "1.0MB");
        assert_eq!(human_size(35_651_584), "34.0MB");
        assert_eq!(human_size(2 * 1024 * 1024 * 1024), "2.0GB");
        assert_eq!(human_size(512), "512B");
    }

    #[test]
    fn meta_serde_roundtrip() {
        let meta = MediaMeta {
            height: Some(1080),
            width: Some(1920),
            download_formats: vec![DownloadFormat {
                format_id: "bv120".into(),
                label: "1080P".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: MediaMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.height, Some(1080));
        assert_eq!(back.width, Some(1920));
        assert_eq!(back.download_formats.len(), 1);
    }

    #[test]
    fn item_serde_roundtrip_preserves_log_and_rot() {
        let mut it = item();
        it.status = Status::Ready;
        it.rot_angle = RotAngle::from_degrees(90);
        it.push_log("解析完成");
        it.meta.audio_volume.max_volume_db = Some(-8.2);
        let json = serde_json::to_string(&it).unwrap();
        let back: MediaItem = serde_json::from_str(&json).unwrap();
        assert_eq!(back.status, Status::Ready);
        assert_eq!(back.rot_angle.degrees(), 90);
        assert_eq!(back.log.front().map(String::as_str), Some("解析完成"));
        assert_eq!(back.meta.audio_volume.max_volume_db, Some(-8.2));
    }

    #[test]
    fn new_stamps_updated_at() {
        // 前端列表按 updated_at 降序排"最新的在上"：创建即必须有时间戳，
        // 空串在降序排序里永远沉底（历史 bug：普通条目从未赋值）
        let it = MediaItem::from_url("https://example.com/a".into());
        assert_eq!(it.updated_at.len(), 19); // YYYY-MM-DD HH:MM:SS
        assert_eq!(&it.updated_at[4..5], "-");
        assert_eq!(&it.updated_at[10..11], " ");
    }
}
