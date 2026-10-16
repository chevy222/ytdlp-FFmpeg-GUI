//! PO Token 自动生成与缓存（§3.6 依赖 / YouTube 风控验证令牌）。
//!
//! 生成由**内嵌的 deno 脚本**完成（[`SCRIPT_JS`] 经 `include_str!` 打进 exe，
//! 运行时释放到临时目录再交给 deno 执行）。脚本基于 MIT 许可的 bgutils-js 运行
//! YouTube BotGuard VM：取挑战 → snapshot 得 botguardResponse → 换 integrityToken
//! → 铸出与 visitorData 绑定的 poToken。
//!
//! 传给 yt-dlp 的正确形式是 `--extractor-args "youtube:visitor_data=..;po_token=web.gvs+.."`
//! （yt-dlp 没有 `--po-token` 参数）。GVS 场景 poToken 与 visitorData 绑定，二者必须
//! 成对下发，见 [`extractor_args`]。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::exec::{decode_text, display_command, run_capture_deadline, Tool, ToolResolver};
use crate::{CoreError, Result};

/// 内嵌的 deno 生成脚本（打包进 exe）。
pub const SCRIPT_JS: &str = include_str!("../../../scripts/potoken.js");

/// 释放到临时目录时的文件名。
pub const SCRIPT_NAME: &str = "potoken.js";

/// 生成用时上限：含首次运行时 deno 解析/下载 npm 依赖（jsdom、bgutils-js），
/// 正常几秒，留足冷启动余量。
pub const GENERATE_TIMEOUT: Duration = Duration::from_secs(120);

/// 缓存默认 TTL：官方未给出 TTL 时的保守值（与业界实现一致的 6 小时）。
pub const DEFAULT_TTL: Duration = Duration::from_secs(6 * 60 * 60);

/// 缓存 TTL 上限：即便官方给出更长的 estimatedTtlSecs 也不超过此时长。
pub const MAX_TTL: Duration = Duration::from_secs(6 * 60 * 60);

/// 自动生成结果（visitorData + poToken 成对；GVS 场景必需）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pot {
    pub visitor_data: String,
    pub po_token: String,
    /// 官方给出的 TTL（秒）；0 表示未知。
    pub ttl_secs: u64,
}

/// 带过期时间的缓存项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedPot {
    pub pot: Pot,
    expires_at: Instant,
}

impl CachedPot {
    /// 按官方 TTL（有则用，封顶 [`MAX_TTL`]）或默认 TTL 计算过期时刻。
    pub fn new(pot: Pot, now: Instant) -> Self {
        let ttl = if pot.ttl_secs > 0 {
            Duration::from_secs(pot.ttl_secs).min(MAX_TTL)
        } else {
            DEFAULT_TTL
        };
        Self {
            pot,
            expires_at: now + ttl,
        }
    }

    pub fn is_fresh(&self, now: Instant) -> bool {
        now < self.expires_at
    }

    /// 序列化为磁盘缓存结构（Instant 不能直接序列化，用 epoch 毫秒）。
    pub fn to_disk(&self) -> PotCacheDisk {
        let now = Instant::now();
        let remaining = self.expires_at.saturating_duration_since(now);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        PotCacheDisk {
            po_token: self.pot.po_token.clone(),
            visitor_data: self.pot.visitor_data.clone(),
            ttl_secs: self.pot.ttl_secs,
            expires_at_ms: now_ms + remaining.as_millis() as u64,
        }
    }
}

/// 磁盘缓存结构（可序列化，存 config/cache/potoken.json）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PotCacheDisk {
    pub po_token: String,
    pub visitor_data: String,
    pub ttl_secs: u64,
    /// 过期时刻（epoch 毫秒）；0 或已过期则丢弃。
    pub expires_at_ms: u64,
}

impl PotCacheDisk {
    /// 从磁盘缓存恢复（过期返回 None）。
    pub fn to_cached(&self) -> Option<CachedPot> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis() as u64;
        if self.expires_at_ms <= now_ms {
            return None;
        }
        let expires_in_ms = self.expires_at_ms - now_ms;
        Some(CachedPot {
            pot: Pot {
                visitor_data: self.visitor_data.clone(),
                po_token: self.po_token.clone(),
                ttl_secs: self.ttl_secs,
            },
            expires_at: Instant::now() + Duration::from_millis(expires_in_ms),
        })
    }
}

/// 解析脚本 stdout（单行 JSON：`{"visitorData":..,"poToken":..,"ttlSecs":..}`）。
///
/// 从后往前扫描：deno/依赖库可能往 stdout 混入无关行，取最后一行可解析出
/// 非空 `poToken` 的 JSON 为准。
pub fn parse_output(stdout: &str) -> Result<Pot> {
    for line in stdout.lines().rev() {
        let t = line.trim();
        if !t.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(t) else {
            continue;
        };
        let po_token = v["poToken"].as_str().unwrap_or_default().to_string();
        if po_token.is_empty() {
            continue;
        }
        let visitor_data = v["visitorData"].as_str().unwrap_or_default().to_string();
        let ttl_secs = v["ttlSecs"].as_u64().unwrap_or(0);
        return Ok(Pot {
            visitor_data,
            po_token,
            ttl_secs,
        });
    }
    Err(CoreError::Io(std::io::Error::other(
        "PO Token 生成脚本未输出有效令牌",
    )))
}

/// 是否 YouTube 系 URL（自动生成/下发 PO Token 只对 YouTube 有意义）。
///
/// 只判 **host 段**（含子域），不做全串 `contains`：`https://example.com/?u=youtube.com`
/// 这类只在路径/查询里出现关键词的 URL 不该触发 PO Token 生成链路（deno 冷启动可达分钟级）。
pub fn is_youtube_url(url: &str) -> bool {
    const HOSTS: [&str; 3] = ["youtube.com", "youtu.be", "youtube-nocookie.com"];
    // 取 host：`scheme://[userinfo@]host[:port]/...`；无 scheme 时按裸 host/path 处理
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let host_port = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host_port.rsplit('@').next().unwrap_or(""); // 去掉 userinfo
    let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase(); // 去掉端口
    HOSTS
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
}

/// 组装 yt-dlp `--extractor-args` 的值（youtube 提取器）：
/// `youtube:visitor_data=<vd>;po_token=<pt>`。两者都为空返回 None。
///
/// `po_token` 需为 yt-dlp 认可的形态（`client.context+token`，见 [`normalize_manual`]）。
pub fn extractor_args(po_token: Option<&str>, visitor_data: Option<&str>) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(vd) = visitor_data.map(str::trim).filter(|s| !s.is_empty()) {
        parts.push(format!("visitor_data={vd}"));
    }
    if let Some(pt) = po_token.map(str::trim).filter(|s| !s.is_empty()) {
        parts.push(format!("po_token={pt}"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!("youtube:{}", parts.join(";")))
    }
}

/// 归一化设置页手动填入的 PO Token：
/// - 裸 token（无 `+`）→ 补 `web.gvs+` 前缀（yt-dlp 需要 `client.context` 元信息）；
/// - 已带 `+`（完整 `client.context+token`）→ 原样保留；
/// - 指向存在的文件 → 读其内容后按上面规则处理（yt-dlp 的 po_token 配置不接受文件路径）。
pub fn normalize_manual(raw: &str) -> Option<String> {
    let v = raw.trim();
    if v.is_empty() {
        return None;
    }
    if v.contains('+') {
        return Some(v.to_string());
    }
    let p = Path::new(v);
    if p.is_file() {
        if let Ok(bytes) = std::fs::read(p) {
            return normalize_manual(decode_text(&bytes).trim());
        }
    }
    Some(format!("web.gvs+{v}"))
}

/// deno 运行生成脚本的参数（供日志展示与执行共用，保证日志=实际执行）。
pub fn deno_args(script: &Path, proxy: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "run".into(),
        "--allow-net".into(),
        "--allow-env".into(),
        script.to_string_lossy().into_owned(),
    ];
    if let Some(p) = proxy.filter(|s| !s.is_empty()) {
        args.push("--proxy".into());
        args.push(p.to_string());
    }
    args
}

/// 释放内嵌脚本到目录（内容一致则不重写），返回脚本路径。
pub fn materialize_script(dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(SCRIPT_NAME);
    let up_to_date = std::fs::read_to_string(&path)
        .map(|c| c == SCRIPT_JS)
        .unwrap_or(false);
    if !up_to_date {
        std::fs::write(&path, SCRIPT_JS)?;
    }
    Ok(path)
}

/// 国内 npm 镜像（deno 从 registry.npmjs.org 下载 jsdom/bgutils-js 等依赖时，
/// 国内直连必败；npmmirror 无需代理即可访问）。
pub const NPM_REGISTRY: &str = "https://registry.npmmirror.com";

/// 把 socks5 代理地址转成 HTTP 代理地址（同一端口）。
/// deno 的 `--proxy` 和 `HTTP_PROXY` 环境变量只支持 HTTP 代理，不支持 SOCKS5；
/// 用户通常配的是 socks5（yt-dlp 支持），同一端口的 HTTP 代理也能用于 deno
/// （Clash 混合端口、V2RayN 等默认同一端口支持两种协议）。
pub fn to_http_proxy(proxy: &str) -> String {
    if let Some(rest) = proxy.strip_prefix("socks5://") {
        format!("http://{}", rest)
    } else if let Some(rest) = proxy.strip_prefix("socks5h://") {
        format!("http://{}", rest)
    } else {
        proxy.to_string()
    }
}

/// 运行 deno 生成 PO Token（阻塞；调用方需放在工作线程/阻塞线程）。
///
/// 失败（deno 缺失、网络不可达、YouTube 端变化等）返回 Err，由调用方决定回退。
pub fn generate(
    resolver: &ToolResolver,
    script: &Path,
    proxy: Option<&str>,
    timeout: Duration,
) -> Result<Pot> {
    // deno 不支持 socks5，自动转成 http（同一端口）
    let http_proxy = proxy.map(to_http_proxy);
    let args = deno_args(script, http_proxy.as_deref());
    let mut cmd = resolver.command(Tool::Deno)?;
    cmd.args(&args);
    // npm 依赖下载走国内镜像（jsdom/bgutils-js 体积大，首次下载几十 MB），
    // 即使配了代理也优先用镜像（国内 CDN 更快）；YouTube API 请求走 --proxy。
    cmd.env("DENO_NPM_REGISTRY", NPM_REGISTRY);
    if let Some(p) = http_proxy.filter(|s| !s.is_empty()) {
        // deno 自身的模块下载/缓存也走同一代理（脚本内的 fetch 走 --proxy）
        cmd.env("HTTP_PROXY", &p).env("HTTPS_PROXY", &p);
    }
    let out = run_capture_deadline(cmd, timeout)?;
    parse_output(&decode_text(&out.stdout))
}

/// 生成器的可读命令行（日志用）。proxy 自动转 http（与实际执行一致）。
pub fn display_invocation(script: &Path, proxy: Option<&str>) -> String {
    let http_proxy = proxy.map(to_http_proxy);
    display_command("deno", &deno_args(script, http_proxy.as_deref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_output_reads_json_line() {
        let ok = r#"{"visitorData":"VD","poToken":"POT","ttlSecs":3600}"#;
        assert_eq!(
            parse_output(ok).unwrap(),
            Pot {
                visitor_data: "VD".into(),
                po_token: "POT".into(),
                ttl_secs: 3600
            }
        );
        // 混入无关输出（依赖下载日志）时取最后一行可解析的令牌
        let noisy = format!("some log line\n{ok}\ntrailing\n");
        assert_eq!(parse_output(&noisy).unwrap().po_token, "POT");
        // 无令牌 → 错误
        assert!(parse_output("{\"error\":\"x\"}").is_err());
        assert!(parse_output("").is_err());
    }

    #[test]
    fn is_youtube_url_matches_only_youtube() {
        assert!(is_youtube_url("https://www.youtube.com/watch?v=x"));
        assert!(is_youtube_url("https://youtu.be/x"));
        assert!(is_youtube_url("https://music.youtube.com/watch?v=x"));
        assert!(!is_youtube_url("https://www.bilibili.com/video/x"));
        assert!(!is_youtube_url(""));
        // 只认 host：路径/查询里出现关键词不算（避免无谓触发 deno 生成链路）
        assert!(!is_youtube_url("https://example.com/path?u=youtube.com"));
        assert!(!is_youtube_url("https://youtube.com.evil.com/watch"));
        assert!(!is_youtube_url("https://evil-youtube.com/watch"));
        // 端口 / userinfo 不影响判定；无 scheme 的裸域名也能识别
        assert!(is_youtube_url("https://www.youtube.com:443/watch?v=x"));
        assert!(is_youtube_url("youtube.com/watch?v=x"));
    }

    #[test]
    fn extractor_args_only_when_present() {
        assert_eq!(extractor_args(None, None), None);
        assert_eq!(extractor_args(Some(""), Some("  ")), None);
        assert_eq!(
            extractor_args(Some("web.gvs+TOK"), None).as_deref(),
            Some("youtube:po_token=web.gvs+TOK")
        );
        assert_eq!(
            extractor_args(None, Some("VD")).as_deref(),
            Some("youtube:visitor_data=VD")
        );
        assert_eq!(
            extractor_args(Some("web.gvs+TOK"), Some("VD")).as_deref(),
            Some("youtube:visitor_data=VD;po_token=web.gvs+TOK")
        );
    }

    #[test]
    fn normalize_manual_adds_prefix_and_reads_file() {
        assert_eq!(normalize_manual(""), None);
        assert_eq!(normalize_manual("   "), None);
        // 裸 token → 补 web.gvs+
        assert_eq!(
            normalize_manual("abc-DEF_123").as_deref(),
            Some("web.gvs+abc-DEF_123")
        );
        // 已带 client.context → 原样
        assert_eq!(
            normalize_manual("web.player+TOK").as_deref(),
            Some("web.player+TOK")
        );
        // 文件路径 → 读内容
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("token.txt");
        std::fs::write(&f, "file-TOKEN\n").unwrap();
        assert_eq!(
            normalize_manual(&f.to_string_lossy()).as_deref(),
            Some("web.gvs+file-TOKEN")
        );
    }

    #[test]
    fn materialize_script_writes_and_reuses() {
        let dir = tempfile::tempdir().unwrap();
        let p = materialize_script(dir.path()).unwrap();
        assert!(p.is_file());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), SCRIPT_JS);
        // 再调用不重写（内容相同）
        let p2 = materialize_script(dir.path()).unwrap();
        assert_eq!(p, p2);
    }

    #[test]
    fn cached_pot_expires_per_ttl() {
        let now = Instant::now();
        let pot = Pot {
            visitor_data: "VD".into(),
            po_token: "TOK".into(),
            ttl_secs: 10,
        };
        let c = CachedPot::new(pot.clone(), now);
        assert!(c.is_fresh(now));
        assert!(!c.is_fresh(now + Duration::from_secs(11)));

        // ttlSecs 超过上限时封顶
        let long = Pot {
            ttl_secs: 999_999,
            ..pot.clone()
        };
        let c = CachedPot::new(long, now);
        assert!(c.is_fresh(now + MAX_TTL - Duration::from_secs(1)));
        assert!(!c.is_fresh(now + MAX_TTL + Duration::from_secs(1)));

        // ttlSecs=0 → 默认 TTL
        let unknown = Pot {
            ttl_secs: 0,
            ..pot
        };
        let c = CachedPot::new(unknown, now);
        assert!(c.is_fresh(now + DEFAULT_TTL - Duration::from_secs(1)));
    }

    #[test]
    fn deno_args_shape() {
        let args = deno_args(Path::new("/tmp/potoken.js"), Some("socks5://127.0.0.1:1080"));
        assert_eq!(args[0], "run");
        assert!(args.contains(&"--allow-net".to_string()));
        assert!(args.contains(&"/tmp/potoken.js".to_string()));
        assert!(args.contains(&"--proxy".to_string()));
        // 无代理时不带 --proxy
        let plain = deno_args(Path::new("/tmp/potoken.js"), None);
        assert!(!plain.contains(&"--proxy".to_string()));
    }
}
