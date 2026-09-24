//! config.json 用户设置（§7.2 / §3.6）：原子写，损坏回退默认。

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::cookies::host_from_url;
use crate::paths::{atomic_write_json, corrupt_backup_path};
use crate::{CoreError, Result};

/// 下载段（§3.6 下载分组 + §7.2）。
///
/// `Default` 返回规范默认值（1080/4/3/true），配合容器级 `#[serde(default)]`：
/// 段内**缺失**字段取 Default 的同名值；用户显式写 `0`/`false` 属于本人选择，
/// 予以保留。字段级 `serde(default)` 补的是类型零值，做不到这一点。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DownloadConfig {
    /// 画质上限（短边），超限自动降分辨率转码（DL-03/DL-04）
    pub max_h: u32,
    /// 下载高度硬上限（MAX_DL_H 语义）
    pub max_dl_h: u32,
    /// 并发分片数（默认 4）
    pub fragments: u32,
    /// 重试次数
    pub retries: u32,
    /// 仅音频默认（新 URL 条目的初始 audio_only）
    pub audio_only: bool,
    /// 播放列表默认（默认关）
    pub playlist: bool,
    /// 嵌入封面/元数据
    pub embed_cover: bool,
    /// 文件名模板（纯标题/标题+ID/UP主-标题/日期-标题）
    pub filename_template: String,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            max_h: 1080,
            max_dl_h: 2160,
            fragments: 4,
            retries: 3,
            audio_only: false,
            playlist: false,
            embed_cover: true,
            filename_template: "纯标题".into(),
        }
    }
}

/// 转码段（§3.6 转码分组 + §7.2；x265 CRF 固定 23 不落配置项）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TranscodeConfig {
    pub max_w: u32,
    pub max_h: u32,
    /// 码率封顶 kbps
    pub brcap_kbps: Option<u32>,
    /// 兜底码率 kbps：封顶留空时的 maxrate 兜底（0 = 不兜底）
    pub br_default_kbps: u32,
    /// 编码器模式：auto | libx265 | nvenc | amf
    pub force_encoder_mode: String,
    /// QSV low_power
    pub low_power: bool,
    /// 保留封面
    pub keep_cover: bool,
}

impl Default for TranscodeConfig {
    fn default() -> Self {
        Self {
            max_w: 1920,
            max_h: 1080,
            brcap_kbps: Some(5000),
            br_default_kbps: 8000,
            force_encoder_mode: "auto".into(),
            low_power: true,
            keep_cover: true,
        }
    }
}

/// 通用段（§3.6 通用分组：输出/音量/并发/检查更新/历史上限）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    /// 默认输出目录（默认桌面）
    pub default_output_dir: Option<String>,
    /// 碰撞命名策略：auto_inc | skip
    pub collision_policy: String,
    /// 音量归一化（下载后处理与转码共用）
    pub normalize_audio: bool,
    /// 音量增益上限 dB（默认 24；负值无意义，加载时归一为 0）
    pub max_gain_db: f32,
    /// 并发任务数（全局：下载/转码/合并共享，默认 3）
    pub concurrency: u32,
    /// 启动时检查更新（默认开启）
    pub check_update: bool,
    /// 历史上限（默认 100，上限 200，§6 统一）
    pub history_limit: usize,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            default_output_dir: None,
            collision_policy: "auto_inc".into(),
            normalize_audio: true,
            max_gain_db: 24.0,
            concurrency: 3,
            check_update: true,
            history_limit: 100,
        }
    }
}

impl GeneralConfig {
    pub const HISTORY_LIMIT_MAX: usize = 200;
}

/// 依赖段（§3.6 依赖分组：路径输入框留空＝PATH，PO-Token 默认启用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DependenciesConfig {
    pub yt_dlp_path: Option<String>,
    pub ffmpeg_path: Option<String>,
    pub ffprobe_path: Option<String>,
    pub deno_path: Option<String>,
    /// PO-Token 服务（YouTube 风控验证令牌，deno 跑 potoken 生成器），默认启用
    pub potoken_enabled: bool,
}

impl Default for DependenciesConfig {
    fn default() -> Self {
        Self {
            yt_dlp_path: None,
            ffmpeg_path: None,
            ffprobe_path: None,
            deno_path: None,
            potoken_enabled: true,
        }
    }
}

/// 网络段（§3.6 网络分组：代理地址 + 站点分流）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkConfig {
    pub proxy_url: String,
    /// 站点分流：站点 -> 是否走代理（其余直连）
    pub site_proxy: std::collections::HashMap<String, bool>,
}

impl NetworkConfig {
    /// 站点分流解析：返回该 URL 应使用的代理地址。
    /// 白名单规则：仅分流中勾选（=走代理）的站点使用代理，其余一律直连；
    /// 代理地址为空时全部直连。多条站点规则命中同一 host 时取**最长标签**
    /// （最具体优先），避免 HashMap 随机迭代导致同 URL 结果不定。
    pub fn resolve_proxy(&self, url: &str) -> Option<String> {
        if self.proxy_url.is_empty() {
            return None;
        }
        let host = host_from_url(url)?;
        let best = self
            .site_proxy
            .iter()
            .filter(|(site, _)| site_matches(site, &host))
            .max_by_key(|(site, _)| site.trim().trim_start_matches('.').len());
        match best {
            Some((_, true)) => Some(self.proxy_url.clone()),
            _ => None,
        }
    }
}

/// 站点匹配：精确相等或子域后缀（bilibili.com 命中 www.bilibili.com）。
fn site_matches(site: &str, host: &str) -> bool {
    let site = site.trim().trim_start_matches('.').to_lowercase();
    if site.is_empty() {
        return false;
    }
    host == site || host.ends_with(&format!(".{site}"))
}

/// 读取并剥掉 UTF-8 BOM（Windows 记事本编辑过的配置带 BOM，
/// serde_json 对 BOM 报"expected value"会被误判为损坏）。
fn read_text_stripping_bom(path: &Path) -> Result<String> {
    let text = std::fs::read_to_string(path)?;
    Ok(text
        .strip_prefix('\u{feff}')
        .map(str::to_string)
        .unwrap_or(text))
}

/// 根配置（§7.2）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub download: DownloadConfig,
    pub transcode: TranscodeConfig,
    pub general: GeneralConfig,
    pub dependencies: DependenciesConfig,
    pub network: NetworkConfig,
}

impl AppConfig {
    /// 加载；文件缺失返回默认；损坏则备份（`<原名>.corrupt-<ts>.json`）后回退默认（§3.7 规则）。
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = read_text_stripping_bom(path)?;
        let mut cfg: AppConfig = match serde_json::from_str(&text) {
            Ok(c) => c,
            Err(e) => {
                let backup = corrupt_backup_path(path);
                let copied = std::fs::copy(path, &backup).is_ok();
                let detail = if copied {
                    format!("（已备份到 {}）", backup.display())
                } else {
                    format!("（备份到 {} 失败）", backup.display())
                };
                return Err(CoreError::ConfigCorrupt(format!("{}{}", e, detail)));
            }
        };
        cfg.sanitize();
        Ok(cfg)
    }

    /// 取值归一化：把越界/无意义的配置收进合法区间。
    ///
    /// **`load` 与 `save_config` 都必须调用**：前端只有 UI 层限制（number 输入的
    /// `min`/`max` 属性拦不住手输与粘贴），后端不校验就会出现
    /// 负的 `max_gain_db`（`f32::clamp(0.0, 负值)` 直接 panic，panic 又发生在任务
    /// 线程里 → 条目卡死 + 并发额度永久泄漏）、0/999 并发、超大 history_limit。
    ///
    /// 本函数自身必须 panic-free（它的存在意义就是防 panic），因此对非有限浮点
    /// 也做了兜底，不用 `clamp` 直接吃输入。
    pub fn sanitize(&mut self) {
        // 增幅上限：非有限值回退默认（24dB），负值收敛到 0
        let gain = self.general.max_gain_db;
        self.general.max_gain_db = if gain.is_finite() {
            gain.clamp(0.0, MAX_GAIN_DB)
        } else {
            24.0
        };
        // 并发上限：前端输入框是 1..16，后端按同一口径收口（避免 config.json 被手改）
        self.general.concurrency = self.general.concurrency.clamp(1, MAX_CONCURRENCY);
        self.general.history_limit = self
            .general.history_limit
            .clamp(1, GeneralConfig::HISTORY_LIMIT_MAX);
        if self.general.collision_policy != "skip" {
            self.general.collision_policy = "auto_inc".into();
        }

        // 分辨率上限保留 0 = "不限制"（UI 允许 0、消费端 download.rs:635 也以
        // `cfg.max_h > 0` 判定是否需要降采样）。用 .min 而非 .clamp(1,…)：
        // 否则用户设 0 存盘变 1 → need_downscale 恒真，毁掉全部下载。
        // transcode.max_w/max_h 同语义字段即用 .min(MAX_EDGE)，这里保持一致。
        self.download.max_h = self.download.max_h.min(MAX_EDGE);
        self.download.max_dl_h = self.download.max_dl_h.min(MAX_EDGE);
        self.download.fragments = self.download.fragments.clamp(1, 16);
        self.download.retries = self.download.retries.clamp(0, 10);

        self.transcode.max_w = self.transcode.max_w.min(MAX_EDGE);
        self.transcode.max_h = self.transcode.max_h.min(MAX_EDGE);
        self.transcode.brcap_kbps = self
            .transcode
            .brcap_kbps
            .map(|k| k.clamp(1, MAX_BITRATE_KBPS));
        self.transcode.br_default_kbps = self.transcode.br_default_kbps.min(MAX_BITRATE_KBPS);
        match self.transcode.force_encoder_mode.as_str() {
            "auto" | "libx265" | "nvenc" | "amf" => {}
            _ => self.transcode.force_encoder_mode = "auto".into(),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        atomic_write_json(path, self)
    }
}

/// 增益上限的硬上限（dB，与设置页输入框上限一致）。
pub const MAX_GAIN_DB: f32 = 48.0;
/// 全局并发任务数上限（与设置页输入框上限一致）。
pub const MAX_CONCURRENCY: u32 = 16;
/// 分辨率上限（像素，超出必是误填）。
const MAX_EDGE: u32 = 8192;
/// 码率上限（kbps，超出必是误填）。
const MAX_BITRATE_KBPS: u32 = 200_000;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn defaults_match_spec() {
        let c = AppConfig::default();
        assert_eq!(c.download.max_h, 1080);
        assert_eq!(c.download.fragments, 4);
        assert!(!c.download.playlist);
        assert!(c.download.embed_cover);
        assert_eq!(c.transcode.force_encoder_mode, "auto");
        assert!(c.transcode.low_power);
        assert_eq!(c.general.concurrency, 3);
        assert_eq!(c.general.max_gain_db, 24.0);
        assert!(c.general.check_update);
        assert_eq!(c.general.history_limit, 100);
        assert!(c.dependencies.potoken_enabled);
        assert!(c.dependencies.yt_dlp_path.is_none());
        assert_eq!(c.network.proxy_url, "");
    }

    #[test]
    fn history_limit_bounded() {
        assert!(GeneralConfig::HISTORY_LIMIT_MAX >= GeneralConfig::default().history_limit);
    }

    #[test]
    fn load_missing_file_returns_default() {
        let root = tempdir().unwrap();
        let c = AppConfig::load(&root.path().join("nope.json")).unwrap();
        assert_eq!(c.general.concurrency, 3);
        assert!(c.download.embed_cover);
    }

    #[test]
    fn save_then_load_roundtrip() {
        let root = tempdir().unwrap();
        let p = root.path().join("config.json");
        let mut c = AppConfig::default();
        c.general.concurrency = 5;
        c.network.site_proxy.insert("youtube.com".into(), true);
        c.save(&p).unwrap();
        let back = AppConfig::load(&p).unwrap();
        assert_eq!(back.general.concurrency, 5);
        assert_eq!(back.network.site_proxy.get("youtube.com"), Some(&true));
    }

    #[test]
    fn resolve_proxy_whitelist_only() {
        // 未配置任何分流站点：即使代理地址非空也一律直连
        let n = NetworkConfig {
            proxy_url: "socks5://127.0.0.1:10808".into(),
            ..Default::default()
        };
        assert_eq!(
            n.resolve_proxy("https://www.bilibili.com/video/BV1xx"),
            None
        );
        assert_eq!(n.resolve_proxy("https://www.youtube.com/watch?v=abc"), None);
        // 代理地址为空：全部直连
        assert_eq!(
            NetworkConfig::default().resolve_proxy("https://www.bilibili.com/video/BV1xx"),
            None
        );
    }

    #[test]
    fn resolve_proxy_site_split() {
        let mut n = NetworkConfig {
            proxy_url: "socks5://127.0.0.1:10808".into(),
            ..Default::default()
        };
        n.site_proxy.insert("bilibili.com".into(), false); // 勾选=走代理，此处为直连
        assert_eq!(
            n.resolve_proxy("https://www.bilibili.com/video/BV1xx"),
            None
        );
        n.site_proxy.insert("youtube.com".into(), true);
        assert_eq!(
            n.resolve_proxy("https://www.youtube.com/watch?v=abc"),
            Some("socks5://127.0.0.1:10808".to_string())
        );
        assert_eq!(n.resolve_proxy("https://vimeo.com/1"), None); // 未配置站点一律直连
        assert_eq!(n.resolve_proxy("https://api.bilibili.com/x"), None); // 子域命中 false
    }

    #[test]
    fn resolve_proxy_overlapping_rules_most_specific_wins() {
        // 重叠规则：bilibili.com=false 与 www.bilibili.com=true 同时存在时，
        // 结果必须是确定的（最具体=最长标签优先），不随 HashMap 迭代序漂移
        let mut n = NetworkConfig {
            proxy_url: "socks5://127.0.0.1:10808".into(),
            ..Default::default()
        };
        n.site_proxy.insert("bilibili.com".into(), false);
        n.site_proxy.insert("www.bilibili.com".into(), true);
        for _ in 0..20 {
            assert_eq!(
                n.resolve_proxy("https://www.bilibili.com/video/BV1xx"),
                Some("socks5://127.0.0.1:10808".to_string())
            );
            assert_eq!(n.resolve_proxy("https://api.bilibili.com/x"), None);
        }
    }

    #[test]
    fn corrupt_config_backed_up_and_errors() {
        let root = tempdir().unwrap();
        let p = root.path().join("config.json");
        std::fs::write(&p, "{ not json !").unwrap();
        let err = AppConfig::load(&p).unwrap_err();
        assert!(matches!(err, CoreError::ConfigCorrupt(_)));
        let backups: Vec<_> = std::fs::read_dir(root.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("corrupt"))
            .collect();
        assert_eq!(backups.len(), 1, "损坏配置应备份后回退");
    }

    #[test]
    fn bom_config_loads() {
        let root = tempdir().unwrap();
        let p = root.path().join("config.json");
        std::fs::write(&p, "\u{feff}{\"general\":{\"concurrency\":6}}").unwrap();
        let c = AppConfig::load(&p).unwrap();
        assert_eq!(c.general.concurrency, 6);
    }

    #[test]
    fn negative_gain_db_normalized() {
        let root = tempdir().unwrap();
        let p = root.path().join("config.json");
        std::fs::write(&p, r#"{"general":{"max_gain_db":-5.0}}"#).unwrap();
        let c = AppConfig::load(&p).unwrap();
        assert_eq!(c.general.max_gain_db, 0.0);
    }

    #[test]
    fn sanitize_clamps_out_of_range_values() {
        let mut c = AppConfig::default();
        c.general.max_gain_db = -5.0;
        c.general.concurrency = 999;
        c.general.history_limit = 100_000;
        c.general.collision_policy = "whatever".into();
        c.download.fragments = 0;
        c.download.max_h = 0;
        c.transcode.brcap_kbps = Some(0);
        c.transcode.force_encoder_mode = "h265".into();
        c.sanitize();
        assert_eq!(c.general.max_gain_db, 0.0);
        assert_eq!(c.general.concurrency, MAX_CONCURRENCY);
        assert_eq!(c.general.history_limit, GeneralConfig::HISTORY_LIMIT_MAX);
        assert_eq!(c.general.collision_policy, "auto_inc");
        assert_eq!(c.download.fragments, 1);
        // max_h 保留 0 = "不限制"（见 sanitize 注释；clamp 到 1 会把 0 吃成 1，
        // 使"不限制"变成恒降采样）
        assert_eq!(c.download.max_h, 0);
        assert_eq!(c.transcode.brcap_kbps, Some(1));
        assert_eq!(c.transcode.force_encoder_mode, "auto");

        // 非有限值也不能让 sanitize 自己 panic（它的存在意义就是防 panic）
        c.general.max_gain_db = f32::NAN;
        c.sanitize();
        assert_eq!(c.general.max_gain_db, 24.0);

        // 合法取值原样保留（含用户显式选择的 skip 与 5 并发）
        let mut c = AppConfig::default();
        c.general.concurrency = 5;
        c.general.collision_policy = "skip".into();
        c.general.max_gain_db = 12.0;
        c.sanitize();
        assert_eq!(c.general.concurrency, 5);
        assert_eq!(c.general.collision_policy, "skip");
        assert_eq!(c.general.max_gain_db, 12.0);
    }

    #[test]
    fn partial_config_fills_missing_with_defaults() {
        // 旧版本/字段缺失：容器级 serde(default) 取 Default 的规范值（非零值）
        let json = r#"{"general":{"concurrency":6},"download":{}}"#;
        let c: AppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(c.general.concurrency, 6);
        assert_eq!(c.download.fragments, 4);
        assert_eq!(c.download.filename_template, "纯标题");
        // 用户显式写 0 属于本人选择，予以保留
        let json = r#"{"download":{"fragments":0}}"#;
        let c: AppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(c.download.fragments, 0);
    }

    #[test]
    fn missing_section_fills_spec_defaults() {
        // 整段缺失 → 补齐规范默认值
        let root = tempdir().unwrap();
        let p = root.path().join("config.json");
        std::fs::write(&p, r#"{"general":{"concurrency":6}}"#).unwrap();
        let c = AppConfig::load(&p).unwrap();
        assert_eq!(c.general.concurrency, 6);
        assert_eq!(c.download.fragments, 4);
        assert!(c.download.embed_cover);
        assert!(c.dependencies.potoken_enabled);
        assert_eq!(c.transcode.max_w, 1920);
    }
}
