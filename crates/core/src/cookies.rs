//! 站点 Cookie 管理（§3.6 Cookie / §3.7 / DL-06）。
//!
//! - 存储：`config/cookies/<host>.txt`（Netscape 格式，yt-dlp 直接可用）。
//! - 匹配：精确 HOST → 站点级回退（父域/常见子域）→ X↔twitter 姊妹域名互退。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Result;

/// 单个 Cookie（WebView2 CookieManager 全字段，§3.7）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CookieEntry {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub expires: Option<f64>,
    pub http_only: bool,
    pub secure: bool,
    #[serde(default)]
    pub same_site: String,
}

/// 站点 Cookie 存储（按 HOST 分文件，Netscape 格式）。
#[derive(Debug, Clone)]
pub struct CookieStore {
    dir: PathBuf,
}

/// 导出到 Netscape 格式时的大小上限（防畸形 cookie 撑爆文件）。
const MAX_COOKIE_VALUE_LEN: usize = 4096;

impl CookieStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn host_file(&self, host: &str) -> PathBuf {
        self.dir.join(format!("{}.txt", sanitize_host(host)))
    }

    /// 保存/覆盖某站点 cookie（Netscape 格式，原子写）。
    /// 临时名带 UUID：并发保存（多个站点同时登录）不会互踩同一个 .tmp 文件。
    /// 形态与 `paths::atomic_write_json` 保持同口径（`<正式名>.<随机>.tmp`）：
    /// 不加隐藏名前缀（Windows 上无意义），残留文件一眼能认出归属。
    pub fn save_host(&self, host: &str, cookies: Vec<CookieEntry>) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let f = self.host_file(host);
        let tmp = f.with_file_name(format!(
            "{}.{}.tmp",
            sanitize_host(host),
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&tmp, netscape_format(&cookies))?;
        std::fs::rename(&tmp, &f)?;
        Ok(())
    }

    /// 读取某站点 cookie（Netscape 格式）。
    /// 用字节读 + [`decode_cookie_file`]：`read_to_string` 遇 GBK 会整段失败，
    /// cookie 明明在却报"没有 cookie"（下载侧表现为"需要登录"）；而直接走
    /// `exec::decode_text` 又会把"恰好是合法 UTF-8 的 GBK 字节"静默解成乱码。
    pub fn load_host(&self, host: &str) -> Result<Vec<CookieEntry>> {
        let f = self.host_file(host);
        if !f.exists() {
            return Ok(Vec::new());
        }
        let bytes = std::fs::read(f)?;
        Ok(netscape_parse(&decode_cookie_file(&bytes)))
    }

    /// 已保存站点列表（按文件名排序，不含扩展名）。
    pub fn list_hosts(&self) -> Result<Vec<String>> {
        if !self.dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut hosts = Vec::new();
        for e in std::fs::read_dir(&self.dir)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(host) = name.strip_suffix(".txt") {
                hosts.push(host.to_string());
            }
        }
        hosts.sort();
        Ok(hosts)
    }

    /// 删除某站点 cookie。
    pub fn delete_host(&self, host: &str) -> Result<()> {
        let f = self.host_file(host);
        if f.exists() {
            std::fs::remove_file(f)?;
        }
        Ok(())
    }

    /// 合并匹配候选域的 cookie（DL-06：精确 host → 父域/常见子域 → 姊妹域名互退）。
    ///
    /// 读失败**向上返回**，不当成"该站点没有 cookie"：静默降级会把
    /// "cookie 明明在、下载却报需要登录"变成无法排查的悬案。
    pub fn merged_entries(&self, host: &str) -> Result<Vec<CookieEntry>> {
        let mut entries: Vec<CookieEntry> = Vec::new();
        for h in cookie_candidates(host) {
            for c in self.load_host(&h)? {
                if !entries
                    .iter()
                    .any(|e| e.name == c.name && e.domain == c.domain)
                {
                    entries.push(c);
                }
            }
        }
        Ok(entries)
    }

    /// 为**某次任务**导出 cookie 文件（供 yt-dlp `--cookies`）。
    ///
    /// 这是纯读接口：库文件（`config/cookies/<host>.txt`）只读不改，合并结果写到
    /// 调用方给的任务私有路径。
    ///
    /// 旧实现把合并结果直接写回 `config/cookies/<host>.txt`，由此产生两个真实故障：
    /// 1) "读"带上了写副作用 —— 父域/姊妹域 cookie（如 `bilibili.com`、`twitter.com`
    ///    的）被永久合并进 `www.x.com.txt`，用户删过的 cookie 会"复活"；
    /// 2) 同一站点的第二个任务启动时截断第一个 yt-dlp 正在读的文件
    ///    （`fs::write` 先清空），表现为间歇性"需要登录"。
    pub fn export_for_task(&self, host: &str, dest: &Path) -> Result<Option<PathBuf>> {
        let entries = self.merged_entries(host)?;
        if entries.is_empty() {
            return Ok(None);
        }
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(dest, netscape_format(&entries))?;
        Ok(Some(dest.to_path_buf()))
    }
}

/// 把 cookie 列表格式化成 Netscape 文本。
fn netscape_format(cookies: &[CookieEntry]) -> String {
    let mut out = String::from("# Netscape HTTP Cookie File\n");
    for c in cookies {
        if c.value.len() > MAX_COOKIE_VALUE_LEN {
            // 静默丢弃会让"登录了但还是需要登录"变得不可解释：至少要留下痕迹
            crate::log::warn(format!(
                "cookie {}（域 {}）超过 {} 字节上限，已跳过导出",
                c.name, c.domain, MAX_COOKIE_VALUE_LEN
            ));
            continue;
        }
        let (domain, include_sub) = if c.domain.starts_with('.') {
            (c.domain.as_str(), "TRUE")
        } else {
            (c.domain.as_str(), "FALSE")
        };
        let secure = if c.secure { "TRUE" } else { "FALSE" };
        let expires = match c.expires {
            Some(e) => format!("{}", e as i64),
            None => String::new(),
        };
        let value = c.value.replace(['\t', '\n'], " ");
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            domain, include_sub, c.path, secure, expires, c.name, value
        ));
    }
    out
}

/// 解码用户手工保存的 cookie 文件（记事本"ANSI" = 中文 Windows 的 GBK）。
///
/// [`crate::exec::decode_text`] 让 UTF-8 优先，这对子进程输出是对的，对这类文件
/// 却会静默翻车：汉字的 GBK 双字节里有一批同时是合法 UTF-8 二字节序列
/// （`值` = `D6 B5` → U+05B5），文件"读成功了"，cookie 值却是乱的，表现成
/// "明明登录了还是提示需要登录"，且日志里没有任何异常。
///
/// 判据用**文件形态**而不是通用字符集猜测：Netscape 格式正文按规范只有 ASCII，
/// 非 ASCII 只可能出现在 domain/path/name/value 四处，而中文站点的值就是中文。
/// 于是"所有非 ASCII 字符都落在 UTF-8 二字节区间，且按 GBK 解能出中日韩文字"
/// 就是被误读的 GBK 文件的特征，据此改判。
///
/// 代价：值里只有纯拉丁/西里尔/希伯来文字且其 UTF-8 字节恰好组成合法 GBK 汉字对
/// 时会误判。实践中 cookie 值是会话令牌（ASCII），这个组合不会出现；反过来
/// 若把它做成无条件 GBK 优先，UTF-8 保存的中文 cookie 就会坏掉（两边都有实测
/// 用例，见 tests）。
fn decode_cookie_file(bytes: &[u8]) -> String {
    let as_text = crate::exec::decode_text(bytes);
    if !only_two_byte_band(&as_text) {
        return as_text;
    }
    let (gbk, _, had_errors) = encoding_rs::GBK.decode(bytes);
    if !had_errors && has_cjk(&gbk) {
        return gbk.into_owned();
    }
    as_text
}

/// 非 ASCII 字符是否**全部**落在 U+0080..U+07FF（UTF-8 二字节序列的取值区间）。
/// 出现三/四字节字符（汉字本身、emoji）就说明这份文本不可能是"GBK 被误读"的产物。
fn only_two_byte_band(s: &str) -> bool {
    let mut saw_non_ascii = false;
    for c in s.chars() {
        let u = c as u32;
        if u < 0x80 {
            continue;
        }
        if u > 0x7FF {
            return false;
        }
        saw_non_ascii = true;
    }
    saw_non_ascii
}

/// 是否含中日韩文字：基本汉字 U+4E00..U+9FFF、扩展 A U+3400..U+4DBF、
/// 兼容汉字 U+F900..U+FAFF、假名 U+3040..U+30FF、CJK 标点 U+3000..U+303F、
/// 全角形式 U+FF00..U+FFEF。GBK 正确解出中文时必定命中其中之一。
fn has_cjk(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(
            c as u32,
            0x3000..=0x30FF
                | 0x3400..=0x4DBF
                | 0x4E00..=0x9FFF
                | 0xF900..=0xFAFF
                | 0xFF00..=0xFFEF
        )
    })
}

/// 从 Netscape 文本解析 cookie 列表。
fn netscape_parse(s: &str) -> Vec<CookieEntry> {
    let mut out = Vec::new();
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 7 {
            continue;
        }
        let domain = parts[0].to_string();
        let include_sub = parts[1] == "TRUE";
        let path = parts[2].to_string();
        let secure = parts[3] == "TRUE";
        let expires = if parts[4].is_empty() {
            None
        } else {
            parts[4].parse::<f64>().ok()
        };
        let name = parts[5].to_string();
        let value = parts[6].to_string();
        let http_only = false; // Netscape 格式不区分 HttpOnly
        let same_site = String::new();
        out.push(CookieEntry {
            name,
            value,
            domain: if include_sub && !domain.starts_with('.') {
                format!(".{}", domain)
            } else {
                domain
            },
            path,
            expires,
            http_only,
            secure,
            same_site,
        });
    }
    out
}

/// 从 URL 中提取 HOST（小写，去端口）。
pub fn host_from_url(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    Some(u.host_str()?.to_lowercase())
}

/// 文件名安全化（host 中不允许的字符替换）。
pub fn sanitize_host(host: &str) -> String {
    host.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Cookie 匹配候选列表（DL-06）：精确 host → 站点级回退 → X↔twitter 互退。
pub fn cookie_candidates(host: &str) -> Vec<String> {
    let h = host.to_lowercase();
    let mut out = vec![h.clone()];
    let push_unique = |out: &mut Vec<String>, s: String| {
        if !out.contains(&s) {
            out.push(s);
        }
    };
    // 站点级回退：去掉常见子域前缀取父域
    let parent = h
        .rsplit_once('.')
        .and_then(|(sub, tld)| {
            if sub.contains('.') {
                Some(format!(
                    "{}.{}",
                    sub.rsplit_once('.').map(|(_, s)| s).unwrap_or(sub),
                    tld
                ))
            } else {
                None // 已是最短二级域
            }
        })
        .unwrap_or_else(|| h.clone());
    if parent != h {
        push_unique(&mut out, parent.clone());
        // 补充常见子域（www）
        if !parent.starts_with("www.") {
            push_unique(&mut out, format!("www.{}", parent));
        }
    }
    // X ↔ twitter 姊妹域名互退
    match h.as_str() {
        "x.com" | "www.x.com" => push_unique(&mut out, "twitter.com".into()),
        "twitter.com" | "www.twitter.com" => push_unique(&mut out, "x.com".into()),
        _ => {}
    }
    // YouTube ↔ youtu.be 姊妹域名互退
    match h.as_str() {
        "youtube.com" | "www.youtube.com" => push_unique(&mut out, "youtu.be".into()),
        "youtu.be" => {
            push_unique(&mut out, "youtube.com".into());
            push_unique(&mut out, "www.youtube.com".into());
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn ck(name: &str, value: &str, domain: &str) -> CookieEntry {
        CookieEntry {
            name: name.into(),
            value: value.into(),
            domain: domain.into(),
            path: "/".into(),
            expires: None,
            http_only: true,
            secure: true,
            same_site: "lax".into(),
        }
    }

    #[test]
    fn host_from_url_works() {
        assert_eq!(
            host_from_url("https://www.BiLiBiLi.com/video/BV1xx?a=1").as_deref(),
            Some("www.bilibili.com")
        );
        assert_eq!(
            host_from_url("https://x.com/status/1").as_deref(),
            Some("x.com")
        );
        assert!(host_from_url("not a url").is_none());
    }

    #[test]
    fn candidates_exact_then_parent_then_www() {
        let c = cookie_candidates("www.bilibili.com");
        assert_eq!(c[0], "www.bilibili.com");
        assert!(c.contains(&"bilibili.com".to_string()));
        // 去重：不应重复出现 www.bilibili.com
        assert_eq!(
            c.len(),
            c.iter().collect::<std::collections::HashSet<_>>().len()
        );
    }

    #[test]
    fn candidates_x_twitter_cross() {
        let c = cookie_candidates("x.com");
        assert!(c.contains(&"twitter.com".to_string()));
        let c2 = cookie_candidates("twitter.com");
        assert!(c2.contains(&"x.com".to_string()));
    }

    #[test]
    fn save_load_delete_roundtrip() {
        let root = tempdir().unwrap();
        let store = CookieStore::new(root.path().join("cookies"));
        store
            .save_host("youtube.com", vec![ck("SID", "abc", ".youtube.com")])
            .unwrap();
        assert_eq!(store.list_hosts().unwrap(), vec!["youtube.com".to_string()]);
        let loaded = store.load_host("youtube.com").unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "SID");
        store.delete_host("youtube.com").unwrap();
        assert!(store.list_hosts().unwrap().is_empty());
    }

    #[test]
    fn export_netscape_merges_and_formats() {
        let root = tempdir().unwrap();
        let store = CookieStore::new(root.path().join("cookies"));
        store
            .save_host("youtube.com", vec![ck("SID", "v1", ".youtube.com")])
            .unwrap();
        // host-only cookie（无前导点）应输出 FALSE
        store
            .save_host("www.youtube.com", vec![ck("VISITOR", "v2", "youtube.com")])
            .unwrap();
        let dest = root.path().join("temp/task/cookies.txt");
        let out = store.export_for_task("www.youtube.com", &dest).unwrap();
        assert!(out.is_some());
        let text = std::fs::read_to_string(out.unwrap()).unwrap();
        // 带点 domain 保留前导点 + TRUE（跨子域）
        assert!(text.contains(".youtube.com\tTRUE\t/\tTRUE\t\tSID\tv1"));
        // 无点 domain + FALSE（host-only）
        assert!(text.contains("youtube.com\tFALSE\t/\tTRUE\t\tVISITOR\tv2"));
        // 纯读接口：导出不得改写站点库文件（父域 cookie 不能被并进 www 的库文件）
        let lib = std::fs::read_to_string(root.path().join("cookies/www.youtube.com.txt")).unwrap();
        assert!(!lib.contains("SID"), "导出不应改写库文件：{lib}");
    }

    #[test]
    fn export_netscape_session_cookie_empty_expires() {
        let root = tempdir().unwrap();
        let store = CookieStore::new(root.path().join("cookies"));
        // expires=None（会话 cookie）：expires 字段必须为空，不能是 0
        store
            .save_host("example.com", vec![ck("SESS", "v", ".example.com")])
            .unwrap();
        let dest = root.path().join("temp/task/cookies.txt");
        let out = store.export_for_task("example.com", &dest).unwrap();
        let text = std::fs::read_to_string(out.unwrap()).unwrap();
        assert!(text.contains("\tTRUE\t/\tTRUE\t\tSESS\tv"), "got: {text}");
        assert!(!text.contains("TRUE\t0\tSESS"));
    }

    #[test]
    fn export_netscape_none_when_empty() {
        let root = tempdir().unwrap();
        let store = CookieStore::new(root.path().join("cookies"));
        let dest = root.path().join("temp/task/cookies.txt");
        assert!(store.export_for_task("nope.com", &dest).unwrap().is_none());
        // 没有任何 cookie 时不应创建空文件
        assert!(!dest.exists());
    }

    #[test]
    fn gbk_cookie_file_still_readable() {
        // 用户在记事本里另存为 ANSI（中文 Windows 即 GBK）后仍要能读出 cookie
        let root = tempdir().unwrap();
        let dir = root.path().join("cookies");
        std::fs::create_dir_all(&dir).unwrap();
        let line = "# Netscape HTTP Cookie File\n.example.com\tTRUE\t/\tTRUE\t\tSESS\t值\n";
        let (gbk, _, _) = encoding_rs::GBK.encode(line);
        std::fs::write(dir.join("example.com.txt"), gbk.as_ref()).unwrap();
        let store = CookieStore::new(&dir);
        let loaded = store.load_host("example.com").unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "SESS");
        assert_eq!(loaded[0].value, "值");
    }

    /// 只换编码、其余一律与上面那条测试同形的 cookie 行（保证两边可比）。
    fn sess_line(value: &str) -> String {
        format!("# Netscape HTTP Cookie File\n.example.com\tTRUE\t/\tTRUE\t\tSESS\t{value}\n")
    }

    fn write_cookie_file(dir: &Path, bytes: &[u8]) {
        std::fs::write(dir.join("example.com.txt"), bytes).unwrap();
    }

    fn load_value(dir: &Path) -> Vec<CookieEntry> {
        CookieStore::new(dir).load_host("example.com").unwrap()
    }

    #[test]
    fn utf8_cookie_file_is_not_rewritten_as_gbk() {
        // 与 GBK 那条互为反向：改判必须有依据，不能一律偏向 GBK，
        // 否则"记事本另存为 UTF-8"这条路径反而坏掉
        let root = tempdir().unwrap();
        let dir = root.path().join("cookies");
        std::fs::create_dir_all(&dir).unwrap();
        write_cookie_file(&dir, sess_line("值").as_bytes());
        let loaded = load_value(&dir);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].value, "值");
    }

    #[test]
    fn multi_char_gbk_cookie_file_readable() {
        // 多个汉字时字节流通常不再是合法 UTF-8，走的是 decode_text 的 GBK 分支；
        // 与单字（歧义分支）两条路径都要通
        let root = tempdir().unwrap();
        let dir = root.path().join("cookies");
        std::fs::create_dir_all(&dir).unwrap();
        let line = sess_line("学习近值");
        let gbk = encoding_rs::GBK.encode(line.as_str()).0.to_vec();
        write_cookie_file(&dir, &gbk);
        assert_eq!(load_value(&dir)[0].value, "学习近值");
    }

    #[test]
    fn utf16_and_bom_cookie_files_readable() {
        let root = tempdir().unwrap();
        let dir = root.path().join("cookies");
        std::fs::create_dir_all(&dir).unwrap();
        let line = sess_line("值");

        // UTF-8 BOM：记事本"UTF-8"另存在 Win10 1903+ 默认带 BOM
        let mut bom8 = vec![0xEF, 0xBB, 0xBF];
        bom8.extend_from_slice(line.as_bytes());
        write_cookie_file(&dir, &bom8);
        assert_eq!(load_value(&dir)[0].value, "值");

        // UTF-16LE：PowerShell 的 `>` 重定向默认产物
        let mut le: Vec<u8> = vec![0xFF, 0xFE];
        le.extend(line.encode_utf16().flat_map(|u| u.to_le_bytes()));
        write_cookie_file(&dir, &le);
        assert_eq!(load_value(&dir)[0].value, "值");

        // UTF-16BE
        let mut be: Vec<u8> = vec![0xFE, 0xFF];
        be.extend(line.encode_utf16().flat_map(|u| u.to_be_bytes()));
        write_cookie_file(&dir, &be);
        assert_eq!(load_value(&dir)[0].value, "值");
    }

    #[test]
    fn ascii_cookie_file_is_untouched() {
        // 绝大多数真实 cookie 全是 ASCII：这条守住了改判逻辑不去动正常文件
        let root = tempdir().unwrap();
        let dir = root.path().join("cookies");
        std::fs::create_dir_all(&dir).unwrap();
        write_cookie_file(&dir, sess_line("CAISxgJ1q6Ft5B2yfSjIr5bK").as_bytes());
        let loaded = load_value(&dir);
        assert_eq!(loaded[0].value, "CAISxgJ1q6Ft5B2yfSjIr5bK");
        assert_eq!(loaded[0].domain, ".example.com");
    }

    #[test]
    fn gbk_misread_signature_is_detected() {
        // 歧义的来源：值 的 GBK 编码 D6 B5 同时是合法 UTF-8 的 U+05B5
        assert_eq!(std::str::from_utf8(&[0xD6, 0xB5]).unwrap(), "\u{5b5}");
        assert!(only_two_byte_band("\u{5b5}"));
        assert!(!only_two_byte_band("值")); // 三字节汉字不是这个特征
        assert!(!only_two_byte_band("pure ascii"));
        assert!(has_cjk("值"));
        assert!(!has_cjk("\u{5b5}"));
    }
}
