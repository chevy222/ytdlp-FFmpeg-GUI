//! 外部进程与工具链定位（§5.1/§3.6 依赖 / §4 安全性-路径参数化）。
//!
//! - 工具定位：config 显式路径 → `<exe 同级>\tools\` 托管 → 系统 PATH，
//!   任一级不存在即回退下一级（首次运行时 `tools\` 是空的，不能遮住 PATH）。
//! - 进程执行：Command 参数化（不拼接 shell，防注入）；取消时终止进程树（Windows taskkill /T）。
//! - 平台：Linux 上编译/测试，Windows 上生产运行；取消用条件编译。

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};

use crate::config::DependenciesConfig;
use crate::CoreError;

/// 工具种类（§3.6 依赖：yt-dlp / ffmpeg / ffprobe / deno）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    YtDlp,
    Ffmpeg,
    Ffprobe,
    Deno,
}

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Self::YtDlp => "yt-dlp",
            Self::Ffmpeg => "ffmpeg",
            Self::Ffprobe => "ffprobe",
            Self::Deno => "deno",
        }
    }

    pub fn exe_name(self) -> &'static str {
        #[cfg(windows)]
        {
            match self {
                Self::YtDlp => "yt-dlp.exe",
                Self::Ffmpeg => "ffmpeg.exe",
                Self::Ffprobe => "ffprobe.exe",
                Self::Deno => "deno.exe",
            }
        }
        #[cfg(not(windows))]
        {
            self.name()
        }
    }
}

/// 工具来自三级回退中的哪一级（「更新」据此判断能不能就地更新）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSource {
    /// 设置里显式填写的路径
    Configured,
    /// `<exe 同级>\tools\` 托管副本
    Managed,
    /// 系统 PATH 里的（不归本程序管，更新应让用户自己动手）
    Path,
}

/// 工具解析器：按 显式路径 → 托管目录 → PATH 三级解析。
#[derive(Debug, Clone, Default)]
pub struct ToolResolver {
    /// 用户在设置页显式填写的路径（填了就必须存在，不存在报错而不是静默回退）
    yt_dlp: Option<PathBuf>,
    ffmpeg: Option<PathBuf>,
    ffprobe: Option<PathBuf>,
    deno: Option<PathBuf>,
    /// `<exe 同级>\tools\` 托管目录；目录或其中某个 exe 缺失都是正常状态
    tools_dir: Option<PathBuf>,
}

impl ToolResolver {
    /// 从配置构造；路径留空 = 走 PATH，托管目录默认未登记（由调用方 with_tools_dir 补）。
    pub fn from_config(cfg: &DependenciesConfig) -> Self {
        Self {
            yt_dlp: cfg.yt_dlp_path.as_ref().map(PathBuf::from),
            ffmpeg: cfg.ffmpeg_path.as_ref().map(PathBuf::from),
            ffprobe: cfg.ffprobe_path.as_ref().map(PathBuf::from),
            deno: cfg.deno_path.as_ref().map(PathBuf::from),
            tools_dir: None,
        }
    }

    /// 显式指定工具根目录（tools/ 托管模式，exe 同级）。
    /// 只登记为回退候选，不占用四个显式配置位——用户填写的路径仍最优先。
    pub fn with_tools_dir(mut self, tools_dir: impl Into<PathBuf>) -> Self {
        self.tools_dir = Some(tools_dir.into());
        self
    }

    /// 解析工具可执行文件路径（不关心来源）。
    pub fn resolve(&self, tool: Tool) -> crate::Result<PathBuf> {
        self.resolve_with_source(tool).map(|(p, _)| p)
    }

    /// 解析工具可执行文件路径 + 命中来源：显式路径 → 托管目录 → 系统 PATH。
    pub fn resolve_with_source(&self, tool: Tool) -> crate::Result<(PathBuf, ToolSource)> {
        let configured = match tool {
            Tool::YtDlp => self.yt_dlp.clone(),
            Tool::Ffmpeg => self.ffmpeg.clone(),
            Tool::Ffprobe => self.ffprobe.clone(),
            Tool::Deno => self.deno.clone(),
        };
        if let Some(p) = configured {
            // 空字符串视为未配置（用户清空了输入框），继续回退
            if p.as_os_str().is_empty() {
                // fall through
            } else if p.is_file() {
                return Ok((p, ToolSource::Configured));
            } else {
                // 显式指定却不存在：明确报错（用户需要知道自己填错了），不回退
                return Err(CoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("{} 指定路径不存在：{}", tool.name(), p.display()),
                )));
            }
        }
        // 托管目录可能不存在、也可能只托管了部分工具（依赖页按需下载），
        // 因此这里只作候选：文件不在就继续回退 PATH，绝不让空目录把 PATH 遮死。
        if let Some(dir) = &self.tools_dir {
            let cand = dir.join(tool.exe_name());
            if cand.is_file() {
                return Ok((cand, ToolSource::Managed));
            }
        }
        find_in_path(tool.exe_name())
            .map(|p| (p, ToolSource::Path))
            .ok_or_else(|| {
                CoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("未找到 {}（请设置路径或加入系统 PATH）", tool.name()),
                ))
            })
    }

    /// 解析并生成命令（已设好程序路径）。
    pub fn command(&self, tool: Tool) -> crate::Result<Command> {
        let mut cmd = Command::new(self.resolve(tool)?);
        hide_console(&mut cmd);
        Ok(cmd)
    }
}

/// 在系统 PATH 中查找可执行文件。
pub fn find_in_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand);
        }
        #[cfg(windows)]
        {
            // Windows 上补充 .exe/.cmd/.bat 扩展名
            for ext in ["exe", "cmd", "bat"] {
                let cand = dir.join(format!("{}.{}", name, ext));
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
    }
    None
}

/// 可取消的受控子进程：Drop 时若未退出则强制终止（防泄漏）。
pub struct ChildGuard {
    child: Option<Child>,
    /// 已取消（外部标志；取消时终止进程树）
    cancelled: bool,
}

impl ChildGuard {
    pub fn spawn(cmd: &mut Command) -> crate::Result<Self> {
        let child = cmd.spawn().map_err(|e| {
            CoreError::Io(std::io::Error::new(
                e.kind(),
                format!("启动进程失败：{}", e),
            ))
        })?;
        Ok(Self {
            child: Some(child),
            cancelled: false,
        })
    }

    /// 终止进程树（Windows taskkill /T /F；其他平台 kill 主进程）。
    pub fn kill_tree(&mut self) {
        self.cancelled = true;
        if let Some(child) = self.child.as_mut() {
            kill_tree_of(child);
        }
    }

    /// 等待退出并返回完整输出（已取消时返回 Err(Canceled)）。
    pub fn wait_with_output(mut self) -> crate::Result<Output> {
        let mut child = self
            .child
            .take()
            .ok_or_else(|| CoreError::Io(std::io::Error::other("子进程句柄已丢失")))?;
        if self.cancelled {
            let _ = child.kill();
        }
        let out = child.wait_with_output().map_err(CoreError::Io)?;
        if self.cancelled {
            return Err(CoreError::Cancelled);
        }
        Ok(out)
    }

    /// 取 stdout（调用后由调用方接管）。
    pub fn stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.as_mut().and_then(|c| c.stdout.take())
    }

    /// 取 stderr（调用后由调用方接管）。
    pub fn stderr(&mut self) -> Option<std::process::ChildStderr> {
        self.child.as_mut().and_then(|c| c.stderr.take())
    }

    /// 非阻塞检查是否退出。
    pub fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        match self.child.as_mut() {
            Some(c) => c.try_wait(),
            None => Ok(None),
        }
    }

    /// 阻塞等待退出。
    pub fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        match self.child.as_mut() {
            Some(c) => c.wait(),
            None => Err(std::io::Error::other("子进程句柄已丢失")),
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            if let Ok(Some(_)) = child.try_wait() {
                // 已退出
            } else if self.cancelled {
                let _ = child.kill();
            }
        }
    }
}

/// 终止子进程树（Windows：taskkill /PID <pid> /T /F；其余：直接 kill）。
fn kill_tree_of(child: &mut Child) {
    #[cfg(windows)]
    {
        let pid = child.id();
        // taskkill 需先不 kill 掉主进程句柄，直接用 PID 命令
        let mut tk = Command::new("taskkill");
        hide_console(&mut tk);
        let _ = tk.args(["/PID", &pid.to_string(), "/T", "/F"]).status();
    }
    #[cfg(not(windows))]
    {
        let _ = child.kill();
    }
}

/// 等待非零退出的子进程完成（输出缓冲，供版本查询等短命令）。
pub fn run_capture(mut cmd: Command) -> crate::Result<Output> {
    let out = cmd.output().map_err(CoreError::Io)?;
    if !out.status.success() {
        return Err(CoreError::ProcessFailed {
            program: cmd.get_program().to_string_lossy().into_owned(),
            code: out.status.code(),
            stderr: decode_text(&out.stderr),
        });
    }
    Ok(out)
}

/// 运行并捕获输出的便捷封装（工具版本查询等）。
pub fn run_tool_capture(
    resolver: &ToolResolver,
    tool: Tool,
    args: &[&str],
) -> crate::Result<Output> {
    let mut cmd = resolver.command(tool)?;
    cmd.args(args);
    run_capture(cmd)
}

/// 版本探测参数：ffmpeg/ffprobe 用 `-version`，其余用 `--version`。
fn version_arg(tool: Tool) -> &'static str {
    match tool {
        Tool::Ffmpeg | Tool::Ffprobe => "-version",
        _ => "--version",
    }
}

/// 从版本输出首行提取干净的版本号（状态栏直接显示，不含版权头）。
///
/// - yt-dlp：首行即版本号（`2026.08.19`）
/// - ffmpeg/ffprobe：`ffmpeg version 9.0 Copyright (c) …` → `version` 后的下一段。
///   gyan.dev 构建的版本号形如 `9.0.1-full_build-www.gyan.dev`，取首个 `-` 前的
///   版本段（`9.0.1`），才能与 `release-version` feed 直接比对；BtbN 滚动构建是
///   `N-121772-g…`（不以数字开头），原样保留。
/// - deno：`deno 2.1.4 (stable, release, …)` → `2.1.4`
///
/// 取不到版本段时原样返回首行，保证状态栏不会出现空串。
pub fn parse_version_line(tool: Tool, first_line: &str) -> String {
    let line = first_line.trim();
    let token = match tool {
        Tool::Ffmpeg | Tool::Ffprobe => line
            .split_whitespace()
            .skip_while(|t| !t.eq_ignore_ascii_case("version"))
            .nth(1)
            .map(|tok| {
                if tok.starts_with(|c: char| c.is_ascii_digit()) {
                    // 9.0.1-full_build-www.gyan.dev → 9.0.1
                    tok.split('-').next().unwrap_or(tok)
                } else {
                    tok
                }
            }),
        Tool::Deno => line
            .strip_prefix("deno")
            .map(str::trim)
            .and_then(|rest| rest.split_whitespace().next()),
        Tool::YtDlp => line.split_whitespace().next(),
    };
    token.unwrap_or(line).to_string()
}

/// 版本号等值比较：忽略大小写、忽略前缀 `v`、忽略首尾空白。
/// 远端 release tag（`v2.9.7`）与本地 `--version` 输出（`2.9.7`）用同一口径比对。
pub fn versions_equal(a: &str, b: &str) -> bool {
    fn norm(s: &str) -> String {
        s.trim().trim_start_matches(['v', 'V']).to_ascii_lowercase()
    }
    !a.trim().is_empty() && norm(a) == norm(b)
}

/// 读取指定可执行文件的版本号（「更新」要读**目标文件自己**的版本，
/// 而不是"当前解析到的那个"）。
pub fn tool_version_at(tool: Tool, path: &Path) -> Option<String> {
    let mut cmd = Command::new(path);
    hide_console(&mut cmd);
    cmd.arg(version_arg(tool));
    let out = run_capture(cmd).ok()?;
    let text = decode_text(&out.stdout);
    let first = text.lines().next()?;
    let v = parse_version_line(tool, first);
    if v.is_empty() {
        return None;
    }
    Some(v)
}

/// 解析工具版本字符串（`-version` / `--version` 输出首行提取版本号）。
pub fn tool_version(resolver: &ToolResolver, tool: Tool) -> Option<String> {
    tool_version_at(tool, &resolver.resolve(tool).ok()?)
}

/// 把程序名与参数拼成一条可读、可复制、可直接粘贴执行的单行命令。
/// 含空白/引号/空串的参数加双引号并转义内部引号，其余原样。
pub fn display_command(program: &str, args: &[String]) -> String {
    let mut s = program.to_string();
    for a in args {
        if a.is_empty() || a.chars().any(|c| c.is_whitespace() || c == '"') {
            s.push_str(&format!(" \"{}\"", a.replace('"', "\\\"")));
        } else {
            s.push(' ');
            s.push_str(a);
        }
    }
    s
}

/// Windows 下隐藏子进程控制台窗口（CREATE_NO_WINDOW），避免 GUI 程序
/// 调用 yt-dlp/ffmpeg 等控制台工具时黑窗口闪烁。
pub fn hide_console(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

/// 子进程文本输出解码：优先 UTF-8；非法序列时 Windows 走 GBK（yt-dlp
/// 在中文 Windows 下常输出 cp936），其余平台做 lossy 替换。
pub fn decode_text(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    #[cfg(windows)]
    {
        let (cow, _, _) = encoding_rs::GBK.decode(bytes);
        cow.into_owned()
    }
    #[cfg(not(windows))]
    {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// 排空子进程 stderr 并收集为字符串（trim 后返回；失败原因收集用，P2-8）。
pub fn drain_stderr(stderr: &mut std::process::ChildStderr) -> String {
    use std::io::Read;
    let mut buf = String::new();
    let _ = stderr.read_to_string(&mut buf);
    buf.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn tool_names() {
        assert_eq!(Tool::YtDlp.name(), "yt-dlp");
        #[cfg(windows)]
        {
            assert_eq!(Tool::Ffmpeg.exe_name(), "ffmpeg.exe");
            assert_eq!(Tool::Ffprobe.exe_name(), "ffprobe.exe");
        }
        #[cfg(not(windows))]
        {
            assert_eq!(Tool::Ffmpeg.exe_name(), "ffmpeg");
            assert_eq!(Tool::Ffprobe.exe_name(), "ffprobe");
        }
        assert_eq!(Tool::Deno.name(), "deno");
    }

    #[test]
    fn default_resolver_uses_path() {
        let r = ToolResolver::default();
        // PATH 里一定有 sh（unix）或 system32（windows），resolve 不应 panic；
        // 具体工具可能缺失，故只验证"未配置时走 PATH 探测"这一行为不报配置错误。
        let _ = r.resolve(Tool::YtDlp);
    }

    #[test]
    fn configured_path_must_exist() {
        let root = tempdir().unwrap();
        let cfg = DependenciesConfig {
            ffmpeg_path: Some(
                root.path()
                    .join("nonexistent-ffmpeg")
                    .to_string_lossy()
                    .into_owned(),
            ),
            ..Default::default()
        };
        let r = ToolResolver::from_config(&cfg);
        let err = r.resolve(Tool::Ffmpeg).unwrap_err();
        assert!(matches!(err, CoreError::Io(_)));
    }

    #[test]
    fn managed_dir_provides_its_tools() {
        let root = tempdir().unwrap();
        let tools = root.path().join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::write(tools.join(Tool::YtDlp.exe_name()), "x").unwrap();
        std::fs::write(tools.join(Tool::Deno.exe_name()), "x").unwrap();
        let r = ToolResolver::default().with_tools_dir(&tools);
        assert_eq!(
            r.resolve(Tool::YtDlp).unwrap(),
            tools.join(Tool::YtDlp.exe_name())
        );
        assert_eq!(
            r.resolve(Tool::Deno).unwrap(),
            tools.join(Tool::Deno.exe_name())
        );
    }

    #[test]
    fn configured_overrides_tools_dir() {
        let root = tempdir().unwrap();
        let custom = root.path().join("custom-ffmpeg");
        std::fs::write(&custom, "x").unwrap();
        let cfg = DependenciesConfig {
            ffmpeg_path: Some(custom.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let r = ToolResolver::from_config(&cfg).with_tools_dir(root.path());
        assert_eq!(r.resolve(Tool::Ffmpeg).unwrap(), custom);
    }

    #[test]
    fn missing_managed_exe_falls_back_to_path() {
        let root = tempdir().unwrap();
        let tools = root.path().join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        let r = ToolResolver::default().with_tools_dir(&tools);
        match r.resolve(Tool::Ffprobe) {
            // 托管目录里没有 → 继续往 PATH 找，不能把 tools\ffprobe.exe 当成已找到
            Ok(p) => assert!(
                !p.starts_with(&tools),
                "托管目录缺失时必须回退 PATH，而不是返回 {}",
                p.display()
            ),
            // 也不该报"指定路径不存在"（那是显式配置才有的错误）
            Err(e) => assert!(e.to_string().contains("未找到"), "应报 PATH 未找到：{e}"),
        }
    }

    #[test]
    fn resolve_with_source_reports_hit_level() {
        let root = tempdir().unwrap();
        let tools = root.path().join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::write(tools.join(Tool::Ffprobe.exe_name()), "x").unwrap();
        let managed = ToolResolver::default().with_tools_dir(&tools);
        let (_, src) = managed.resolve_with_source(Tool::Ffprobe).unwrap();
        assert_eq!(src, ToolSource::Managed);

        let custom = root.path().join("custom-ffprobe");
        std::fs::write(&custom, "x").unwrap();
        let cfg = DependenciesConfig {
            ffprobe_path: Some(custom.to_string_lossy().into_owned()),
            ..Default::default()
        };
        // 显式路径优先于托管目录
        let resolver = ToolResolver::from_config(&cfg).with_tools_dir(&tools);
        let (p, src) = resolver.resolve_with_source(Tool::Ffprobe).unwrap();
        assert_eq!(src, ToolSource::Configured);
        assert_eq!(p, custom);
    }

    #[test]
    fn path_level_is_reported_as_path() {
        // 无显式路径、无托管目录 → 只可能命中 PATH（工具不存在时跳过断言，
        // 与其他"不假设工具已安装"的测试同口径）
        let r = ToolResolver::default();
        if let Ok((p, src)) = r.resolve_with_source(Tool::Ffmpeg) {
            assert_eq!(src, ToolSource::Path);
            assert!(p.is_file());
        }
    }

    #[test]
    fn versions_equal_ignores_v_prefix_and_case() {
        assert!(versions_equal("2.9.7", "v2.9.7"));
        assert!(versions_equal(" 2026.08.19 ", "2026.08.19"));
        assert!(!versions_equal("2026.08.19", "2026.09.18"));
        // 空值不算相等（避免"查不到远端版本"被当成已是最新）
        assert!(!versions_equal("", ""));
    }

    #[test]
    fn parse_version_line_extracts_version_token() {
        assert_eq!(parse_version_line(Tool::YtDlp, "2026.08.19"), "2026.08.19");
        // ffmpeg/ffprobe 的首行带版权信息，只取 "version" 后的一段
        let ffmpeg_line = "ffmpeg version 9.0 Copyright (c) 2000-2026 the FFmpeg developers";
        assert_eq!(parse_version_line(Tool::Ffmpeg, ffmpeg_line), "9.0");
        let ffprobe_line = "ffprobe version 9.0 Copyright (c) 2000-2026 the FFmpeg developers";
        assert_eq!(parse_version_line(Tool::Ffprobe, ffprobe_line), "9.0");
        // deno 的首行是 "<名字> <版本> (构建信息)"
        assert_eq!(
            parse_version_line(Tool::Deno, "deno 2.1.4 (stable)"),
            "2.1.4"
        );
        // gyan.dev 构建：版本号带构建后缀，取首个 '-' 前的版本段（与 release-version feed 同口径）
        assert_eq!(
            parse_version_line(
                Tool::Ffmpeg,
                "ffmpeg version 9.0.1-full_build-www.gyan.dev Copyright (c) the FFmpeg developers"
            ),
            "9.0.1"
        );
        // BtbN 滚动构建（N- 开头，不以数字开头）：原样保留
        assert_eq!(
            parse_version_line(Tool::Ffmpeg, "ffmpeg version N-121772-g8f5c9a1e2a"),
            "N-121772-g8f5c9a1e2a"
        );
        // 取不到版本段时原样返回首行（不返回空串）
        assert_eq!(
            parse_version_line(Tool::Ffmpeg, "ffmpeg version"),
            "ffmpeg version"
        );
    }

    #[test]
    fn tool_version_reads_first_line() {
        let r = ToolResolver::default();
        // 不假设工具存在：可用且版本输出可解析时返回非空首行
        if let Some(v) = tool_version(&r, Tool::Ffmpeg) {
            assert!(!v.is_empty());
        }
    }

    #[test]
    fn run_capture_success_and_failure() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exit 0"]);
        assert!(run_capture(cmd).is_ok());
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo boom >&2; exit 3"]);
        let err = run_capture(cmd).unwrap_err();
        match err {
            CoreError::ProcessFailed { code, stderr, .. } => {
                assert_eq!(code, Some(3));
                assert!(stderr.contains("boom"));
            }
            other => panic!("意外错误：{:?}", other),
        }
    }
}
