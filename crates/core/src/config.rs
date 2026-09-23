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
///
/// 尺寸闸门：配置文件本该是几十 KB 量级。读一个 200MB 的"config.json"再整份
/// 解析，既可能是磁盘坏档也可能是被人塞了垃圾 —— 两种情况都不该继续往下走。
fn read_text_stripping_bom(path: &Path) -> Result<String> {
    const MAX_CONFIG_BYTES: u64 = 8 * 1024 * 1024;
    if let Ok(md) = std::fs::metadata(path) {
        if md.len() > MAX_CONFIG_BYTES {
            return Err(CoreError::ConfigCorrupt(format!(
                "配置文件异常过大（{} 字节），拒绝读取：{}",
                md.len(),
                path.display()
            )));
        }
    }
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
    /// 把越界/拼错的值归一到可执行范围。
    ///
    /// 这些值最终都会拼进外部工具的命令行，越界值的报错用户完全读不懂：
    /// `fragments: 0` → `yt-dlp -N 0`；`max_h: 0` → `bv*[height<=0]` → "no formats"；
    /// `collision_policy` 拼错会被 `paths.rs` 的 `_` 分支静默当 auto_inc。
    /// **加载与保存两条路都要过**，否则设置页可以直接写入任意值。
    pub fn normalize(&mut self) {
        let g = &mut self.general;
        g.max_gain_db = if g.max_gain_db.is_finite() {
            g.max_gain_db.clamp(0.0, 24.0)
        } else {
            24.0
        };
        g.concurrency = g.concurrency.clamp(1, 16);
        g.history_limit = g.history_limit.clamp(1, GeneralConfig::HISTORY_LIMIT_MAX);
        if !matches!(g.collision_policy.as_str(), "auto_inc" | "skip") {
            g.collision_policy = "auto_inc".into();
        }
        if let Some(dir) = &g.default_output_dir {
            if dir.trim().is_empty() {
                g.default_output_dir = None;
            }
        }

        let d = &mut self.download;
        d.fragments = d.fragments.clamp(1, 32);
        d.retries = d.retries.clamp(0, 100);
        if d.max_h == 0 {
            d.max_h = 1080;
        }
        if d.max_dl_h == 0 {
            d.max_dl_h = 2160;
        }
        if !matches!(
            d.filename_template.as_str(),
            "纯标题" | "标题+ID" | "UP主-标题" | "日期-标题"
        ) {
            d.filename_template = "纯标题".into();
        }

        let t = &mut self.transcode;
        if t.max_w == 0 {
            t.max_w = 1920;
        }
        if t.max_h == 0 {
            t.max_h = 1080;
        }
        if !matches!(
            t.force_encoder_mode.as_str(),
            "auto" | "libx265" | "nvenc" | "amf"
        ) {
            t.force_encoder_mode = "auto".into();
        }
        if let Some(cap) = t.brcap_kbps {
            if cap == 0 {
                t.brcap_kbps = None;
            }
        }

        // 依赖路径：空串一律视作"未配置"（走 PATH），否则三级回退会被空路径遮住
        let dep = &mut self.dependencies;
        for slot in [
            &mut dep.yt_dlp_path,
            &mut dep.ffmpeg_path,
            &mut dep.ffprobe_path,
            &mut dep.deno_path,
        ] {
            if let Some(p) = slot {
                if p.trim().is_empty() {
                    *slot = None;
                }
            }
        }
    }

    /// 归一化后的副本（保存前用，保证落盘的值一定可用）。
    pub fn sanitized(mut self) -> Self {
        self.normalize();
        self
    }

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
        cfg.normalize();
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        atomic_write_json(path, &self.clone().sanitized())
    }
}

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
    fn normalize_rescues_out_of_range_values() {
        let root = tempdir().unwrap();
        let p = root.path().join("config.json");
        // 手工写一份"每个字段都越界"的配置（设置页可以直接写任意值）
        std::fs::write(
            &p,
            r#"{
              "download": {"max_h": 0, "max_dl_h": 0, "fragments": 0, "retries": 99999,
                            "filename_template": "拼错了"},
              "transcode": {"force_encoder_mode": "qsv??", "max_w": 0, "max_h": 0, "brcap_kbps": 0},
              "general": {"concurrency": 0, "history_limit": 4294967295, "max_gain_db": -50,
                           "collision_policy": "overwrite"},
              "dependencies": {"yt_dlp_path": "   "}
            }"#,
        )
        .unwrap();
        let c = AppConfig::load(&p).unwrap();
        assert_eq!(c.download.max_h, 1080);
        assert_eq!(c.download.max_dl_h, 2160);
        assert_eq!(c.download.fragments, 1, "-N 0 会让 yt-dlp 直接报错");
        assert_eq!(c.download.retries, 100);
        assert_eq!(c.download.filename_template, "纯标题");
        assert_eq!(c.transcode.force_encoder_mode, "auto");
        assert_eq!(c.transcode.max_w, 1920);
        assert_eq!(c.transcode.brcap_kbps, None);
        assert_eq!(c.general.concurrency, 1);
        assert_eq!(c.general.history_limit, GeneralConfig::HISTORY_LIMIT_MAX);
        assert_eq!(c.general.max_gain_db, 0.0, "负增益上限会触发 clamp panic");
        assert_eq!(c.general.collision_policy, "auto_inc");
        assert!(c.dependencies.yt_dlp_path.is_none(), "空白路径必须视作未配置");
    }

    #[test]
    fn save_sanitizes_before_writing() {
        let root = tempdir().unwrap();
        let p = root.path().join("config.json");
        let mut c = AppConfig::default();
        c.general.concurrency = 9999;
        c.save(&p).unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(raw.contains("\"concurrency\":16"), "落盘的值未归一：{raw}");
    }

    #[test]
    fn load_rejects_absurdly_large_file() {
        let root = tempdir().unwrap();
        let p = root.path().join("config.json");
        std::fs::write(&p, vec![b' '; 9 * 1024 * 1024]).unwrap();
        let e = AppConfig::load(&p).unwrap_err();
        assert!(e.to_string().contains("异常过大"), "意外错误：{e}");
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
