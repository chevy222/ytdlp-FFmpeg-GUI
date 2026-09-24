//! 工具链托管下载（依赖页 下载/更新）：yt-dlp / deno 从官方 GitHub Release，
//! ffmpeg/ffprobe 从 gyan.dev release 构建；下载后原子激活（tmp + rename 覆盖）。
//!
//! 安装位置由调用方给定：依赖页「下载」固定装到 `<exe 同级>\tools\`，
//! 「更新」装到该工具**当前生效**的那个文件（设置里填的路径或托管副本）。
//!
//! "有没有新版本"的判定（版本 feed）：
//! - yt-dlp / deno：跟随 `releases/latest` 重定向取 tag；
//! - ffmpeg / ffprobe：gyan.dev 的 `release-version` 纯文本 feed（内容即当前
//!   release 版本号，如 `9.0.1`），与本地 `-version` 输出比对。
//!
//! 下载源（Windows x86_64）：
//! - yt-dlp.exe：https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe
//! - ffmpeg/ffprobe：https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip
//!   （303 跳转到当版 `packages/ffmpeg-<ver>-essentials_build.zip`；两工具同包各取所需）
//! - deno.exe：https://github.com/denoland/deno/releases/latest/download/deno-x86_64-pc-windows-msvc.zip
//!
//! SHA-256：yt-dlp / deno 有 `.sha256` 旁路文件；gyan.dev 没有 → ffmpeg/ffprobe
//! 跳过产物校验，版本一致性由上面的版本 feed 比对保证。

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::exec::Tool;

/// ffmpeg/ffprobe 的下载包（gyan.dev release 别名，恒指向最新 release）。
pub const FFMPEG_ZIP_URL: &str = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip";
/// ffmpeg/ffprobe 的版本 feed：响应体即当前 release 版本号（如 `9.0.1`）。
pub const FFMPEG_VERSION_FEED: &str = "https://www.gyan.dev/ffmpeg/builds/release-version";
/// 人工查看/手动下载的构建页（依赖页"链接"按钮展示）。
pub const FFMPEG_BUILDS_PAGE: &str = "https://www.gyan.dev/ffmpeg/builds/";

/// 版本 feed 的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionFeed {
    /// GitHub `releases/latest`：跟随重定向，取最终 URL 的 tag。
    GithubLatest(&'static str),
    /// 纯文本 feed：响应体就是版本号。
    Text(&'static str),
}

/// 可托管的工具类型（依赖页四个入口）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    YtDlp,
    Ffmpeg,
    Ffprobe,
    Deno,
}

impl ToolKind {
    /// 依赖页 data-tool 键（dependencies.* 配置键名）。
    pub fn from_config_key(key: &str) -> Option<Self> {
        match key {
            "yt_dlp_path" => Some(Self::YtDlp),
            "ffmpeg_path" => Some(Self::Ffmpeg),
            "ffprobe_path" => Some(Self::Ffprobe),
            "deno_path" => Some(Self::Deno),
            _ => None,
        }
    }

    /// 激活后的文件名（Windows exe；非 Windows 环境无后缀）。
    pub fn exe_name(&self) -> &'static str {
        match self {
            Self::YtDlp => "yt-dlp.exe",
            Self::Ffmpeg => "ffmpeg.exe",
            Self::Ffprobe => "ffprobe.exe",
            Self::Deno => "deno.exe",
        }
    }

    /// 对应的 Tool（用于执行/校验）。
    pub fn tool(&self) -> Tool {
        match self {
            Self::YtDlp => Tool::YtDlp,
            Self::Ffmpeg => Tool::Ffmpeg,
            Self::Ffprobe => Tool::Ffprobe,
            Self::Deno => Tool::Deno,
        }
    }

    /// 下载 URL。
    ///
    /// ffmpeg/ffprobe 用 gyan.dev 的 release 别名包（303 跳转到当版
    /// `packages/ffmpeg-<ver>-essentials_build.zip`，ffmpeg 与 ffprobe 同包各取所需）。
    pub fn url(&self) -> &'static str {
        match self {
            Self::YtDlp => {
                "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe"
            }
            Self::Ffmpeg | Self::Ffprobe => FFMPEG_ZIP_URL,
            Self::Deno => {
                "https://github.com/denoland/deno/releases/latest/download/deno-x86_64-pc-windows-msvc.zip"
            }
        }
    }

    /// SHA-256 校验文件 URL（可能不存在，404 时跳过校验）。
    /// gyan.dev 不提供 `.sha256` 旁路文件 → ffmpeg/ffprobe 下载后不做产物校验，
    /// "有没有新版本"改由 `release-version` 文本比对保证（见 [`Self::version_feed`]）。
    pub fn sha_url(&self) -> Option<&'static str> {
        match self {
            Self::YtDlp => Some(
                "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe.sha256",
            ),
            Self::Ffmpeg | Self::Ffprobe => None,
            Self::Deno => Some(
                "https://github.com/denoland/deno/releases/latest/download/deno-x86_64-pc-windows-msvc.zip.sha256",
            ),
        }
    }

    /// zip 压缩包内目标条目的**后缀**（大小写不敏感匹配）。
    ///
    /// gyan/BtbN 的包顶层目录带版本号（如 `ffmpeg-9.0.1-essentials_build/bin/ffmpeg.exe`），
    /// 写死完整路径会随版本失效，只能按后缀选条目。
    pub fn zip_entry_suffix(&self) -> Option<&'static str> {
        match self {
            Self::Ffmpeg => Some("bin/ffmpeg.exe"),
            Self::Ffprobe => Some("bin/ffprobe.exe"),
            Self::Deno => Some("deno.exe"),
            _ => None,
        }
    }

    /// 人工查看构建/发布页的地址（依赖页展示给用户，方便手动下载）。
    pub fn check_page_url(&self) -> &'static str {
        match self {
            Self::YtDlp => "https://github.com/yt-dlp/yt-dlp/releases/latest",
            Self::Ffmpeg | Self::Ffprobe => FFMPEG_BUILDS_PAGE,
            Self::Deno => "https://github.com/denoland/deno/releases/latest",
        }
    }

    /// 版本 feed（"有没有新版本"的数据源）。
    pub fn version_feed(&self) -> Option<VersionFeed> {
        match self {
            Self::YtDlp => Some(VersionFeed::GithubLatest(
                "https://github.com/yt-dlp/yt-dlp/releases/latest",
            )),
            Self::Deno => Some(VersionFeed::GithubLatest(
                "https://github.com/denoland/deno/releases/latest",
            )),
            // gyan.dev 的版本 feed：纯文本，内容即当前 release 版本号（如 `9.0.1`）
            Self::Ffmpeg | Self::Ffprobe => Some(VersionFeed::Text(FFMPEG_VERSION_FEED)),
        }
    }
}

/// 一次安装的结果：落地路径 + 远端产物指纹（拿不到远端 `.sha256` 时为 None）。
#[derive(Debug, Clone)]
pub struct DownloadedTool {
    pub path: PathBuf,
    pub remote_sha256: Option<String>,
}

/// 单个工具的安装指纹：上一次由本程序安装到的位置 + 当时远端产物的 SHA-256。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledFingerprint {
    /// 安装目标（绝对路径）
    pub path: String,
    /// 远端产物 SHA-256：yt-dlp 是 exe 本身；ffmpeg / deno 是 zip 包
    pub sha256: String,
}

/// `tools/installed.json` 索引（key = 设置页的配置键名，如 `ffmpeg_path`）。
///
/// 只服务于「更新」的"有没有新版本"判定：文件丢失/损坏只会让下一次「更新」
/// 退化成"重新下载一次"，不影响安装，也不需要用户处理。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InstalledIndex {
    entries: std::collections::BTreeMap<String, InstalledFingerprint>,
}

impl InstalledIndex {
    /// 索引文件路径（`<exe 同级>\tools\installed.json`）。
    pub fn file_in(tools_dir: &Path) -> PathBuf {
        tools_dir.join(INSTALLED_INDEX_FILE)
    }

    /// 读取索引（不存在/损坏 → 空表）。
    pub fn load(tools_dir: &Path) -> Self {
        std::fs::read_to_string(Self::file_in(tools_dir))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn get(&self, key: &str) -> Option<&InstalledFingerprint> {
        self.entries.get(key)
    }

    /// 记录一次安装（`sha256` 为空表示远端没给可校验的指纹，此时不记录）。
    pub fn record(&mut self, key: &str, path: &Path, sha256: &str) {
        if sha256.is_empty() {
            return;
        }
        self.entries.insert(
            key.to_string(),
            InstalledFingerprint {
                path: path.to_string_lossy().into_owned(),
                sha256: sha256.to_ascii_lowercase(),
            },
        );
    }

    /// 记录一次安装（**load + record + save 原子化**）。
    ///
    /// 前端允许同时"下载/更新"不同工具（取消键按工具隔离），两个人各自
    /// load → record → save 会互相覆盖：后写入者用陈旧索引把先写入者的指纹抹掉，
    /// 表现为"明明刚更新过，下次还提示有新版本 / 又下一遍 100MB"。
    /// 索引只影响「更新」的判定，失败只告警不阻断安装。
    pub fn record_install(tools_dir: &Path, key: &str, path: &Path, sha256: &str) -> Result<(), String> {
        static LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
        let _guard = LOCK.lock();
        let mut index = Self::load(tools_dir);
        index.record(key, path, sha256);
        index.save(tools_dir)
    }

    /// 原子写回索引。
    pub fn save(&self, tools_dir: &Path) -> Result<(), String> {
        let file = Self::file_in(tools_dir);
        crate::paths::atomic_write_json(&file, self)
            .map_err(|e| format!("写入 {} 失败：{e}", file.display()))
    }
}

/// 工具安装指纹索引文件名。
pub const INSTALLED_INDEX_FILE: &str = "installed.json";

/// 「远端没变过」判定：远端产物指纹与上次安装记录一致，且目标路径没变过。
///
/// 任一条件不满足（含拿不到远端指纹）都返回 false —— 宁可多下一次，
/// 也不要把"其实有新版本"误报成"已是最新"。
pub fn installed_matches(
    rec: Option<&InstalledFingerprint>,
    target: &Path,
    remote_sha: Option<&str>,
) -> bool {
    let (Some(rec), Some(remote)) = (rec, remote_sha) else {
        return false;
    };
    !rec.sha256.is_empty()
        && rec.path == target.to_string_lossy().as_ref()
        && rec.sha256.eq_ignore_ascii_case(remote)
}

/// 从 `curl -w %{url_effective}` 的结果里取版本号：`…/releases/tag/<tag>` → `<tag>`（去前缀 `v`）。
/// 滚动标签（`latest`）或不是版本形态的 tag 一律 None，避免拿它当版本比较。
pub fn parse_release_tag(effective_url: &str) -> Option<String> {
    let (_, tag) = effective_url.split_once("/tag/")?;
    let tag = tag.trim().trim_end_matches('/').trim_start_matches('v');
    if tag.is_empty() || !tag.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some(tag.to_string())
}

/// 在 zip 条目名列表里选第一个以 `suffix` 结尾（大小写不敏感）的条目。
///
/// 包的顶层目录带版本号（gyan：`ffmpeg-9.0.1-essentials_build/bin/ffmpeg.exe`；
/// deno 官方包内是裸 `deno.exe`），写死完整路径会随版本失效，只能按后缀选。
pub fn select_zip_entry<'a, I>(names: I, suffix: &str) -> Option<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let suffix = suffix.to_ascii_lowercase();
    names
        .into_iter()
        .find(|n| n.to_ascii_lowercase().ends_with(&suffix))
        .map(str::to_string)
}

/// 工具托管下载器。
#[derive(Debug, Clone)]
pub struct ToolDownloader {
    pub tools_dir: PathBuf,
    pub temp_dir: PathBuf,
    /// 取消标志（下载中点"取消"置 true → kill curl）
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl ToolDownloader {
    /// `tools_dir`：`<exe 同级>\tools\`；`temp_dir`：`<exe 同级>\temp\tool_dl\`。
    pub fn new(tools_dir: impl Into<PathBuf>, temp_dir: impl Into<PathBuf>) -> Self {
        Self {
            tools_dir: tools_dir.into(),
            temp_dir: temp_dir.into(),
            cancel: None,
        }
    }

    /// 设置取消标志。
    pub fn with_cancel(
        mut self,
        cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Self {
        self.cancel = cancel;
        self
    }

    /// 托管副本的目标路径（`tools\<工具名>`）——「下载」的固定落点。
    pub fn target_path(&self, kind: ToolKind) -> PathBuf {
        self.tools_dir.join(kind.exe_name())
    }

    /// 该工具是否已经有托管副本。
    pub fn is_installed(&self, kind: ToolKind) -> bool {
        self.target_path(kind).is_file()
    }

    /// 远端最新版本号（按工具的版本 feed 查询）。
    /// feed 取不到 / 网络失败返回 None（调用方回退指纹或按"需要更新"处理）。
    pub fn latest_version(&self, kind: ToolKind) -> Option<String> {
        match kind.version_feed()? {
            VersionFeed::Text(url) => {
                let mut cmd = std::process::Command::new("curl");
                // --max-time：版本查询跑在"下载/更新"的前置判断里，卡住就等于整个按钮无响应
                cmd.args(["-sS", "--fail", "--max-time", "20", url]);
                crate::exec::hide_console(&mut cmd);
                let out = cmd.output().ok()?;
                if !out.status.success() {
                    return None;
                }
                let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if v.is_empty() {
                    None
                } else {
                    Some(v)
                }
            }
            VersionFeed::GithubLatest(url) => {
                std::fs::create_dir_all(&self.temp_dir).ok()?;
                // 文件名带工具名：同时点两个工具的"更新"时互不覆盖（进程 id 是同一个）
                let head = self.temp_dir.join(format!(
                    "head-{}-{}.txt",
                    kind.exe_name(),
                    std::process::id()
                ));
                let mut cmd = std::process::Command::new("curl");
                cmd.args(["-sIL", "--fail", "--max-time", "20", "-o"])
                    .arg(&head)
                    .args(["-w", "%{url_effective}"])
                    .arg(url);
                crate::exec::hide_console(&mut cmd);
                let out = cmd.output().ok();
                let _ = std::fs::remove_file(&head);
                let out = out?;
                if !out.status.success() {
                    return None;
                }
                parse_release_tag(&String::from_utf8_lossy(&out.stdout))
            }
        }
    }

    /// 远端产物 SHA-256（`.sha256` 旁路文件；404/网络失败返回 None）。
    pub fn remote_sha(&self, kind: ToolKind) -> Option<String> {
        let url = kind.sha_url()?;
        let mut cmd = std::process::Command::new("curl");
        cmd.args(["-L", "--fail", "-sS", "--max-time", "20", url]);
        crate::exec::hide_console(&mut cmd);
        let output = cmd.output().ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        // 格式："<64hex>  filename" 或 裸 64hex
        let hex = text
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if hex.len() == 64 {
            Some(hex)
        } else {
            None
        }
    }

    /// 下载并安装到 `dest`（目标文件绝对路径：`tools\<工具名>` 或设置里填的位置）。
    /// `on_progress(phase, percent)`：phase 为"下载/校验/解压/安装"等阶段名。
    pub fn download(
        &self,
        kind: ToolKind,
        dest: &Path,
        on_progress: &mut dyn FnMut(String, f32),
    ) -> Result<DownloadedTool, String> {
        // 目标目录可能是托管 `tools\`，也可能是用户自填位置 → 提前建好并尽早报错
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("创建目标目录失败：{e}"))?;
        }
        std::fs::create_dir_all(&self.temp_dir).map_err(|e| format!("创建临时目录失败：{e}"))?;

        let (raw, verify_zip) = self.download_artifact(kind, on_progress)?;

        on_progress("校验".into(), 0.0);
        let digest = sha256_hex(&raw).map_err(|e| format!("计算 SHA-256 失败：{e}"))?;
        // 期望值：有 `.sha256` 旁路文件的工具（yt-dlp / deno），取不到期望值 = 硬失败，
        // 绝不"空即放行"——否则攻击者只要让那一次 .sha256 请求失败（403 限流 / DNS /
        // 404）就能让整步校验静默跳过。ffmpeg/ffprobe 的 sha_url() 本身为 None（设计如此，
        // 由 release-version feed 兜底），不在此列。
        let want = match kind.sha_url() {
            Some(_) => self
                .remote_sha(kind)
                .ok_or_else(|| "SHA-256 校验：无法获取远端期望值（网络失败/限流），拒绝安装".to_string())?,
            None => String::new(),
        };
        if !want.is_empty() && !digest.eq_ignore_ascii_case(&want) {
            let _ = std::fs::remove_file(&raw);
            return Err(format!(
                "SHA-256 校验失败：期望 {} 实际 {}",
                &want[..16.min(want.len())],
                &digest[..16]
            ));
        }
        on_progress("校验".into(), 1.0);

        // 解压或直取
        if verify_zip {
            let suffix = kind
                .zip_entry_suffix()
                .ok_or_else(|| "内部错误：zip 型工具缺少目标条目后缀".to_string())?;
            on_progress("解压".into(), 0.0);
            let extracted = self.extract_from_zip(&raw, suffix)?;
            on_progress("解压".into(), 1.0);
            let _ = std::fs::remove_file(&raw);
            self.activate(&extracted, dest, on_progress)?;
            let _ = std::fs::remove_file(&extracted);
        } else {
            self.activate(&raw, dest, on_progress)?;
            let _ = std::fs::remove_file(&raw);
        }
        on_progress("完成".into(), 1.0);
        Ok(DownloadedTool {
            path: dest.to_path_buf(),
            remote_sha256: if want.is_empty() { None } else { Some(want) },
        })
    }

    /// 下载原始文件到 temp，返回 (路径, 是否为 zip)。
    fn download_artifact(
        &self,
        kind: ToolKind,
        on_progress: &mut dyn FnMut(String, f32),
    ) -> Result<(PathBuf, bool), String> {
        let is_zip = kind.zip_entry_suffix().is_some();
        let ext = if is_zip { "zip" } else { "bin" };
        // 文件名带 UUID：同一工具的重复下载/两条任务残留不会互相踩（pid 在同一
        // 进程内相同，不足以区分；上次崩溃留下的同名半成品也会被误当"续传"）。
        let raw = self.temp_dir.join(format!(
            "{}-{}.{}",
            kind.exe_name(),
            uuid::Uuid::new_v4(),
            ext
        ));
        let url = kind.url();
        // 先问总大小：拿不到（CDN 不给 content-length）时进度退化为阶段提示，不影响下载
        let total = self.remote_size(url);

        on_progress("连接".into(), 0.0);
        // 用系统 curl（Windows 10+ 自带 curl.exe）下载，避免 TLS 库交叉编译问题。
        // 超时/重试必须显式给：curl 默认不设总时长上限，对端半死时这条命令会一直挂着，
        // 用户只能点"取消"（而且工具下载的取消是轮询实现，依赖这里的 try_wait 循环）。
        let out_str = raw.to_string_lossy().into_owned();
        let mut cmd = std::process::Command::new("curl");
        cmd.args([
            "-L",
            "--fail",
            "-sS",
            "--connect-timeout",
            "20",
            "--retry",
            "3",
            "--retry-delay",
            "2",
            "--max-time",
            "3600",
            "-o",
            &out_str,
            url,
        ]);
        // stderr 必须 pipe：否则 wait_with_output 拿不到 curl 的报错，
        // 下载失败时用户只能看到"下载失败 <url>"而无任何原因。
        // -sS 已静默进度，出错才输出，短文本不会撑爆管道缓冲。
        cmd.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        crate::exec::hide_console(&mut cmd);
        let mut child = cmd.spawn().map_err(|e| format!("无法调用 curl：{e}"))?;
        // curl -sS 自身不输出进度，这里按已写入的字节数估算百分比上报，
        // 否则整个下载过程前端只能停在 0%（大文件动辄几分钟）。
        on_progress("下载".into(), 0.0);
        let mut last_pct = 0.0f32;
        // 轮询等待 + 检查取消标志（点"取消"则 kill）
        loop {
            if let Some(flag) = &self.cancel {
                if flag.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = std::fs::remove_file(&raw);
                    return Err("已取消".to_string());
                }
            }
            if let Some(total) = total.filter(|t| *t > 0) {
                let done = std::fs::metadata(&raw).map(|m| m.len()).unwrap_or(0);
                let pct = (done as f64 / total as f64).min(1.0) as f32;
                // 限流：每前进 1% 才上报一次，避免 150ms 一条事件打爆前端
                if pct - last_pct >= 0.01 {
                    last_pct = pct;
                    on_progress("下载".into(), pct);
                }
            }
            match child
                .try_wait()
                .map_err(|e| format!("curl 等待失败：{e}"))?
            {
                Some(_) => break,
                None => std::thread::sleep(std::time::Duration::from_millis(150)),
            }
        }
        let output = child
            .wait_with_output()
            .map_err(|e| format!("curl 读取输出失败：{e}"))?;
        if !output.status.success() {
            let _ = std::fs::remove_file(&raw);
            let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(if err.is_empty() {
                format!("下载失败 {url}")
            } else {
                format!("下载失败 {url}: {err}")
            });
        }
        let done = std::fs::metadata(&raw).map(|m| m.len()).unwrap_or(0);
        if done == 0 {
            let _ = std::fs::remove_file(&raw);
            return Err(format!("下载失败 {url}: 文件为空"));
        }
        // 与声明大小对账：curl 已能识别截断（exit 18），这里只是把不一致留个痕迹，
        // 便于事后判断"包坏了"还是"网络断了"
        if let Some(total) = total.filter(|t| *t > 0) {
            if done != total {
                crate::log::warn(format!(
                    "{url} 实收 {done} 字节，声明 {total} 字节（按已下载内容继续，SHA-256 仍会校验）"
                ));
            }
        }
        on_progress("下载".into(), 1.0);
        Ok((raw, is_zip))
    }

    /// 查询远端文件总大小（HEAD 跟随重定向）。
    /// 取不到（CDN 不给 content-length / 网络异常）返回 None，调用方退化为阶段提示。
    fn remote_size(&self, url: &str) -> Option<u64> {
        let mut cmd = std::process::Command::new("curl");
        cmd.args(["-sIL", "--fail", "--max-time", "20", url]);
        crate::exec::hide_console(&mut cmd);
        let out = cmd.output().ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        last_content_length(&text)
    }

    /// 从 zip 提取目标条目（按 `suffix` 后缀匹配、大小写不敏感）到 temp 目录。
    fn extract_from_zip(&self, zip_path: &Path, suffix: &str) -> Result<PathBuf, String> {
        let file = std::fs::File::open(zip_path).map_err(|e| format!("打开压缩包失败：{e}"))?;
        let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("解析压缩包失败：{e}"))?;
        // 先收集条目名再挑（顶层目录带版本号，如 ffmpeg-9.0.1-essentials_build/bin/ffmpeg.exe）
        let names: Vec<String> = (0..archive.len())
            .filter_map(|i| archive.by_index(i).ok().map(|f| f.name().to_string()))
            .collect();
        let matched = select_zip_entry(names.iter().map(String::as_str), suffix)
            .ok_or_else(|| format!("压缩包内未找到以 {suffix} 结尾的条目"))?;
        let mut entry = archive
            .by_name(&matched)
            .map_err(|e| format!("压缩包内打开 {matched} 失败：{e}"))?;
        let out = self.temp_dir.join(format!(
            "extract-{}-{}",
            std::process::id(),
            std::path::Path::new(&matched)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("out")
        ));
        let mut file = std::fs::File::create(&out).map_err(|e| format!("创建解压文件失败：{e}"))?;
        std::io::copy(&mut entry, &mut file).map_err(|e| format!("解压失败：{e}"))?;
        Ok(out)
    }

    /// 激活：写到目标同目录的 `<文件名>.tmp` 再 rename 覆盖
    /// （同目录才能保证 rename 是原子替换；用户自填目录不可写时在这里报错）。
    ///
    /// rename 在 Windows 上是 `MoveFileExW(REPLACE_EXISTING)`：**目标被其他进程
    /// 打开且未共享删除**就失败 —— 更新正在被转码/下载使用的 ffmpeg 时必然撞上。
    /// 这里重试几次并把失败翻译成用户看得懂的原因（旧实现只回一句"拒绝访问"）。
    fn activate(
        &self,
        src: &Path,
        dest: &Path,
        on_progress: &mut dyn FnMut(String, f32),
    ) -> Result<(), String> {
        on_progress("安装".into(), 0.5);
        let dir = dest
            .parent()
            .ok_or_else(|| format!("无效的目标路径：{}", dest.display()))?;
        let name = dest
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| format!("无效的目标文件名：{}", dest.display()))?;
        let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
        std::fs::copy(src, &tmp)
            .map_err(|e| format!("写入 {} 失败（目标目录不可写？）：{e}", tmp.display()))?;
        let mut last_err: Option<std::io::Error> = None;
        for attempt in 0..5 {
            match std::fs::rename(&tmp, dest) {
                Ok(()) => {
                    on_progress("安装".into(), 1.0);
                    return Ok(());
                }
                Err(e) => {
                    if attempt < 4 {
                        crate::log::warn(format!(
                            "激活 {} 第 {} 次失败（{}），500ms 后重试",
                            dest.display(),
                            attempt + 1,
                            e
                        ));
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                    last_err = Some(e);
                }
            }
        }
        let _ = std::fs::remove_file(&tmp);
        Err(format!(
            "激活工具失败：{}。该文件可能正被运行中的任务占用（转码/下载在用它），请结束相关任务后重试",
            last_err
                .map(|e| e.to_string())
                .unwrap_or_else(|| "未知原因".into())
        ))
    }
}

/// 从 curl `-I` 输出里取最后一个 `content-length`。
/// 带 `-L` 时输出含每一次跳转的响应头，只有最终响应的大小是真实文件大小。
fn last_content_length(headers: &str) -> Option<u64> {
    headers
        .lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            if k.trim().eq_ignore_ascii_case("content-length") {
                v.trim().parse::<u64>().ok()
            } else {
                None
            }
        })
        .next_back()
}

fn sha256_hex(path: &Path) -> Result<String, std::io::Error> {
    use sha2::Digest;
    use std::io::BufReader;
    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut hasher = sha2::Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn config_key_roundtrip() {
        assert_eq!(
            ToolKind::from_config_key("yt_dlp_path"),
            Some(ToolKind::YtDlp)
        );
        assert_eq!(ToolKind::from_config_key("deno_path"), Some(ToolKind::Deno));
        assert_eq!(ToolKind::from_config_key("nope"), None);
    }

    #[test]
    fn urls_and_entries() {
        assert!(ToolKind::YtDlp.url().ends_with("yt-dlp.exe"));
        // ffmpeg/ffprobe 走 gyan.dev 的 release 别名包，两工具同 URL 各取所需
        assert_eq!(ToolKind::Ffmpeg.url(), FFMPEG_ZIP_URL);
        assert_eq!(ToolKind::Ffprobe.url(), FFMPEG_ZIP_URL);
        // gyan.dev 没有 .sha256 旁路文件 → 产物校验跳过，版本靠 release-version 比对
        assert_eq!(ToolKind::Ffmpeg.sha_url(), None);
        assert_eq!(
            ToolKind::Ffmpeg.version_feed(),
            Some(VersionFeed::Text(FFMPEG_VERSION_FEED))
        );
        assert_eq!(ToolKind::Ffmpeg.zip_entry_suffix(), Some("bin/ffmpeg.exe"));
        assert_eq!(
            ToolKind::Ffprobe.zip_entry_suffix(),
            Some("bin/ffprobe.exe")
        );
        assert_eq!(ToolKind::Deno.zip_entry_suffix(), Some("deno.exe"));
        assert_eq!(ToolKind::YtDlp.exe_name(), "yt-dlp.exe");
    }

    #[test]
    fn select_zip_entry_matches_suffix_case_insensitive() {
        let names = [
            "ffmpeg-9.0.1-essentials_build/",
            "ffmpeg-9.0.1-essentials_build/bin/",
            "ffmpeg-9.0.1-essentials_build/BIN/FFMPEG.EXE",
            "ffmpeg-9.0.1-essentials_build/doc/ffmpeg.txt",
        ];
        // 大小写不敏感；同名目录不应误配（目录名以 / 结尾不会命中 .exe 后缀）
        assert_eq!(
            select_zip_entry(names, "bin/ffmpeg.exe").as_deref(),
            Some("ffmpeg-9.0.1-essentials_build/BIN/FFMPEG.EXE")
        );
        assert_eq!(
            select_zip_entry(["deno.exe", "LICENSE"], "deno.exe").as_deref(),
            Some("deno.exe")
        );
        assert_eq!(select_zip_entry(["a/ffprobe.exe"], "bin/ffmpeg.exe"), None);
    }

    #[test]
    fn downloader_target_path() {
        let dl = ToolDownloader::new("/x/tools", "/x/temp");
        assert_eq!(
            dl.target_path(ToolKind::Deno),
            PathBuf::from("/x/tools/deno.exe")
        );
    }

    #[test]
    fn last_content_length_takes_final_hop() {
        // GitHub → S3 的两次跳转：302 只有 content-length: 0，最终响应才是文件大小
        let head = concat!(
            "HTTP/2 302\r\ncontent-length: 0\r\nlocation: https://example/x\r\n\r\n",
            "HTTP/2 200\r\nContent-Length: 172693744\r\n",
        );
        assert_eq!(last_content_length(head), Some(172693744));
        // 没有该头（分块传输 / HEAD 被拒）→ None，进度退化为阶段提示
        assert_eq!(
            last_content_length("HTTP/2 200\r\ntransfer-encoding: chunked\r\n"),
            None
        );
    }

    #[test]
    fn parse_release_tag_reads_version_only() {
        let yt_dlp = "https://github.com/yt-dlp/yt-dlp/releases/tag/2026.08.19\n";
        assert_eq!(parse_release_tag(yt_dlp).as_deref(), Some("2026.08.19"));
        // deno 的 tag 带 v 前缀
        let deno = "https://github.com/denoland/deno/releases/tag/v2.9.7";
        assert_eq!(parse_release_tag(deno).as_deref(), Some("2.9.7"));
        // BtbN 是滚动发布，tag 恒为 latest → 没有版本可比
        let btb = "https://github.com/BtbN/FFmpeg-Builds/releases/tag/latest";
        assert_eq!(parse_release_tag(btb), None);
        assert_eq!(parse_release_tag("https://github.com/yt-dlp/yt-dlp"), None);
    }

    #[test]
    fn installed_index_roundtrip() {
        let root = tempdir().unwrap();
        let tools = root.path().join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        let mut idx = InstalledIndex::load(&tools);
        assert!(idx.get("ffmpeg_path").is_none());

        let target = tools.join("ffmpeg.exe");
        idx.record("ffmpeg_path", &target, "ABCDEF01");
        // 空指纹（远端没给 .sha256）不记录
        idx.record("deno_path", &tools.join("deno.exe"), "");
        idx.save(&tools).unwrap();

        let back = InstalledIndex::load(&tools);
        let rec = back.get("ffmpeg_path").unwrap();
        assert_eq!(PathBuf::from(&rec.path), target);
        assert_eq!(rec.sha256, "abcdef01");
        assert!(back.get("deno_path").is_none());
        // 索引缺失/损坏时退化为空表（不影响安装，只是下次更新会重新比一次）
        std::fs::write(InstalledIndex::file_in(&tools), "{ not json").unwrap();
        assert!(InstalledIndex::load(&tools).get("ffmpeg_path").is_none());
    }

    #[test]
    fn installed_matches_requires_same_path_and_sha() {
        let target = PathBuf::from("/x/tools/ffmpeg.exe");
        let rec = InstalledFingerprint {
            path: "/x/tools/ffmpeg.exe".to_string(),
            sha256: "abc123".to_string(),
        };
        assert!(installed_matches(Some(&rec), &target, Some("abc123")));
        assert!(installed_matches(Some(&rec), &target, Some("ABC123")));
        // 远端有新构建 → 需要更新
        assert!(!installed_matches(Some(&rec), &target, Some("def456")));
        // 用户改过设置、目标换到别处 → 记录失效，需要更新
        assert!(!installed_matches(
            Some(&rec),
            Path::new("/y/ffmpeg.exe"),
            Some("abc123")
        ));
        // 没有记录 / 拿不到远端指纹 → 一律按"需要更新"处理（宁可多下一次）
        assert!(!installed_matches(None, &target, Some("abc123")));
        assert!(!installed_matches(Some(&rec), &target, None));
    }
}
