//! 外部进程与工具链定位（§5.1/§3.6 依赖 / §4 安全性-路径参数化）。
//!
//! - 工具定位：config 显式路径 → `<exe 同级>\tools\` 托管 → 系统 PATH，
//!   任一级不存在即回退下一级（首次运行时 `tools\` 是空的，不能遮住 PATH）。
//! - 进程执行：Command 参数化（不拼接 shell，防注入）；取消时终止进程树（Windows taskkill /T）。
//! - 平台：Linux 上编译/测试，Windows 上生产运行；取消用条件编译。

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

    /// 子进程 PID（看门狗线程按 PID 终止进程树用；句柄被 `wait_with_output`
    /// 接管后为 None）。
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // std::process::Child 在 Rust 中 drop 不会自动 kill：异常路径（`?` 提前
        // 返回、panic unwinding、进度解析中途出错）若不兜底，ffmpeg/yt-dlp 会成为
        // 孤儿进程继续跑（占网络/CPU/写文件）。因此只要进程未确认退出，一律杀。
        if let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(Some(_)) => { /* 已退出，无需处理 */ }
                _ => {
                    // 仍在运行（或 try_wait 出错）：杀整棵进程树并回收，防僵尸/孤儿
                    kill_tree_of(child);
                    let _ = child.wait();
                }
            }
        }
    }
}

/// 终止子进程树（Windows：taskkill /PID <pid> /T /F；其余：直接 kill）。
fn kill_tree_of(child: &mut Child) {
    // 已退出（含刚被 kill 掉）就不必再 taskkill：否则会打出一堆
    // "错误: 没有找到进程" 的噪音，掩盖真正的失败
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    #[cfg(windows)]
    {
        let pid = child.id();
        let mut tk = Command::new("taskkill");
        hide_console(&mut tk);
        let ok = match tk.args(["/PID", &pid.to_string(), "/T", "/F"]).status() {
            Ok(s) => s.success(),
            Err(_) => false,
        };
        if !ok {
            // taskkill 不可用/失败（PATH 异常、安全软件拦截、权限不足）：
            // 至少把直接子进程杀掉，不能因为外部命令失败就整棵进程树都不动
            crate::log::warn(format!(
                "taskkill 未能终止进程树（pid {pid}），回退 kill 主进程；其子进程可能残留"
            ));
            let _ = child.kill();
        }
    }
    #[cfg(not(windows))]
    {
        let _ = child.kill();
    }
}

/// 按 PID 终止进程树（Windows taskkill /T /F）。
///
/// 供**看门狗线程**使用：任务线程正常时阻塞在管道读取上，只有旁路线程才能
/// 在"子进程不再输出"时把取消/超时落实成真正的终止。
pub fn kill_tree_by_pid(pid: u32) {
    #[cfg(windows)]
    {
        let mut tk = Command::new("taskkill");
        hide_console(&mut tk);
        let ok = match tk.args(["/PID", &pid.to_string(), "/T", "/F"]).status() {
            Ok(s) => s.success(),
            Err(_) => false,
        };
        if !ok {
            crate::log::warn(format!("看门狗 taskkill 未能终止进程树（pid {pid}）"));
        }
    }
    #[cfg(not(windows))]
    {
        // 非 Windows（开发/测试机）：没有等价内建命令，尽量按 PID 发终止信号
        let _ = Command::new("kill").args(["-TERM", &pid.to_string()]).status();
    }
}

/// 流式读取子进程输出的结果（[`stream_lines`]）。
pub struct StreamOutcome {
    pub status: std::process::ExitStatus,
    /// 是否因 cancel / deadline 被终止
    pub killed: bool,
    /// stderr 全文（已解码；失败诊断信息都在这里）
    pub stderr: String,
}

/// 逐行读取子进程 stdout 并回调，同时保证 stderr 被持续排空、取消/超时真正生效。
///
/// **为什么不能"先读 stdout、等进程退出再读 stderr"**：管道缓冲区有限
/// （Windows 约 64KB），父子任何一端停读都会让另一端阻塞在 `write` 上。
/// yt-dlp 的错误行、ffmpeg 的报错都走 stderr，写满即双向死锁：子进程不再产出
/// stdout，父进程阻塞在读 stdout 上，连"取消"标志都没机会检查。
///
/// **为什么不能用 `BufRead::lines()`**：它对非 UTF-8 字节返回 `Err`。yt-dlp 在
/// 中文 Windows 下会输出 cp936（见 [`decode_text`] 的说明），一旦某行非法，
/// 读取循环被中断，父进程停止消费 stdout，同样把子进程卡死。这里一律
/// `read_until(b'\n')` + [`decode_text`]。
///
/// - `cancel`：置位后终止整棵进程树（由看门狗线程执行，任务线程无需先收到输出）；
/// - `deadline`：到点仍无进展也终止（给"探测类短任务"用的兜底，长任务传 None）。
pub fn stream_lines(
    guard: &mut ChildGuard,
    cancel: &Arc<AtomicBool>,
    deadline: Option<Instant>,
    mut on_line: impl FnMut(&str),
) -> crate::Result<StreamOutcome> {
    let stdout = guard
        .stdout()
        .ok_or_else(|| CoreError::Io(std::io::Error::other("无法读取子进程标准输出")))?;
    let stderr = guard
        .stderr()
        .ok_or_else(|| CoreError::Io(std::io::Error::other("无法读取子进程错误输出")))?;

    // stderr：独立线程排空，逐块解码（GBK 安全）
    let err_buf = Arc::new(parking_lot::Mutex::new(String::new()));
    let err_thread = {
        let err_buf = err_buf.clone();
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stderr);
            let mut chunk: Vec<u8> = Vec::new();
            loop {
                chunk.clear();
                match std::io::BufRead::read_until(&mut reader, b'\n', &mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => err_buf.lock().push_str(&decode_text(&chunk)),
                }
            }
        })
    };

    // 看门狗：任务线程会阻塞在 read_until 上，cancel/deadline 只能在旁路线程执行
    let done = Arc::new(AtomicBool::new(false));
    let killed = Arc::new(AtomicBool::new(false));
    let watchdog = guard.pid().map(|pid| {
        let done = done.clone();
        let killed = killed.clone();
        let cancel = cancel.clone();
        std::thread::spawn(move || loop {
            if done.load(Ordering::Relaxed) {
                return;
            }
            let timeout = deadline.map(|d| Instant::now() >= d).unwrap_or(false);
            if cancel.load(Ordering::Relaxed) || timeout {
                killed.store(true, Ordering::Relaxed);
                kill_tree_by_pid(pid);
                return;
            }
            std::thread::sleep(Duration::from_millis(150));
        })
    });

    let mut reader = std::io::BufReader::new(stdout);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        match std::io::BufRead::read_until(&mut reader, b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        on_line(decode_text(&buf).trim_end_matches(['\r', '\n']));
        if cancel.load(Ordering::Relaxed) {
            killed.store(true, Ordering::Relaxed);
            guard.kill_tree();
            break;
        }
    }

    let status = guard.wait()?;
    done.store(true, Ordering::Relaxed);
    let _ = err_thread.join();
    if let Some(w) = watchdog {
        let _ = w.join();
    }
    let stderr = err_buf.lock().clone();
    Ok(StreamOutcome {
        status,
        killed: killed.load(Ordering::Relaxed),
        stderr,
    })
}

/// 带截止时间的输出捕获（版本查询等短命令）。
///
/// 到点即终止进程树：`yt-dlp --version` 这类看着"一定很快"的命令，在
/// 杀软扫描/网络盘/损坏 exe 下可能永久挂起，主线程或被占用的线程不能陪着等。
/// 用完依赖 std 的 `wait_with_output`（内部并发排空两条管道，不会死锁）。
pub fn run_capture_deadline(mut cmd: Command, timeout: Duration) -> crate::Result<Output> {
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // 不需要 mut：pid() 走 &self，wait_with_output() 按值接管（其内部自会取 mut）
    let guard = ChildGuard::spawn(&mut cmd)?;
    let pid = guard.pid();
    let done = Arc::new(AtomicBool::new(false));
    let watchdog = pid.map(|pid| {
        let done = done.clone();
        std::thread::spawn(move || {
            let deadline = Instant::now() + timeout;
            while !done.load(Ordering::Relaxed) {
                if Instant::now() >= deadline {
                    crate::log::warn(format!("命令超时（{:?}），终止进程树 pid {pid}", timeout));
                    kill_tree_by_pid(pid);
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        })
    });
    let out = guard.wait_with_output();
    done.store(true, Ordering::Relaxed);
    if let Some(w) = watchdog {
        let _ = w.join();
    }
    let out = out?;
    if !out.status.success() {
        return Err(CoreError::ProcessFailed {
            program: cmd.get_program().to_string_lossy().into_owned(),
            code: out.status.code(),
            stderr: decode_text(&out.stderr),
        });
    }
    Ok(out)
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
/// 而不是"当前解析到的那个"）。带 20 秒截止时间：损坏的 exe / 挂在网络盘的
/// 文件会让"查个版本号"永久阻塞。
pub fn tool_version_at(tool: Tool, path: &Path) -> Option<String> {
    let mut cmd = Command::new(path);
    hide_console(&mut cmd);
    cmd.arg(version_arg(tool));
    let out = run_capture_deadline(cmd, Duration::from_secs(20)).ok()?;
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
///
/// 用 `read_to_end` + [`decode_text`] 而不是 `read_to_string`：后者遇非 UTF-8
/// （中文 Windows 下 yt-dlp/ffmpeg 可能输出 cp936）会整段失败，报错信息连同
/// 已读到的部分一起丢掉，"失败却不知道为什么"。
/// 前置条件：进程已退出（否则会一直阻塞到 EOF）。
pub fn drain_stderr(stderr: &mut std::process::ChildStderr) -> String {
    use std::io::Read;
    let mut bytes = Vec::new();
    let _ = stderr.read_to_end(&mut bytes);
    decode_text(&bytes).trim().to_string()
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

    /// 管道读取的回归防线：以下用例对应真实踩过的坑，改动流式读取逻辑时必须全绿。
    #[test]
    fn stream_lines_survives_stderr_flood() {
        // stderr 远超管道缓冲区（Windows 约 64KB）时，stdout 的最后一行仍必须读到；
        // 旧实现（stderr 全程不读、退出后才 drain）会在这里双向死锁
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(
            "i=0; while [ $i -lt 20000 ]; do \
               echo 'err-line-padding-padding-padding-padding' >&2; i=$((i+1)); \
             done; echo '[download] 100% of 1MiB'; exit 0",
        );
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut guard = ChildGuard::spawn(&mut cmd).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut lines: Vec<String> = Vec::new();
        let out = stream_lines(&mut guard, &cancel, None, |l| lines.push(l.to_string())).unwrap();
        assert!(out.status.success(), "退出状态：{:?}", out.status);
        assert!(
            lines.iter().any(|l| l.contains("100% of 1MiB")),
            "stderr 灌满后 stdout 仍要读完，实际读到：{:?}",
            lines
        );
        assert!(
            out.stderr.len() > 64 * 1024,
            "stderr 应被完整收集，实际 {} 字节",
            out.stderr.len()
        );
    }

    #[test]
    fn stream_lines_keeps_reading_after_invalid_utf8() {
        // 非 UTF-8 行（GBK 的"你"= C4 E3）不能让读取循环中断：
        // `lines()` 遇非法字节返回 Err，旧实现 break 后子进程会卡死在写 stdout 上
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("printf '\\304\\343\\n'; echo '[download]  12.3% of 1MiB at 1MiB/s ETA 00:01'; exit 0");
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut guard = ChildGuard::spawn(&mut cmd).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut lines: Vec<String> = Vec::new();
        let out = stream_lines(&mut guard, &cancel, None, |l| lines.push(l.to_string())).unwrap();
        assert!(out.status.success());
        assert!(
            lines.iter().any(|l| l.contains("12.3%")),
            "非法 UTF-8 行之后的行必须继续解析，实际读到：{:?}",
            lines
        );
    }

    #[test]
    fn stream_lines_cancel_works_without_any_output() {
        // 子进程零输出时任务线程阻塞在 read_until 上，取消只能靠看门狗线程落实。
        // `exec sleep` 让 sh 自身变成唯一进程，避免退出后残留孤儿持有管道。
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("exec sleep 30");
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut guard = ChildGuard::spawn(&mut cmd).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let c2 = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            c2.store(true, Ordering::Relaxed);
        });
        let t0 = Instant::now();
        let out = stream_lines(&mut guard, &cancel, None, |_| {}).unwrap();
        let spent = t0.elapsed();
        assert!(out.killed, "取消应被看门狗记录");
        assert!(
            spent < Duration::from_secs(4),
            "取消应在看门狗周期内生效，实际耗时 {spent:?}"
        );
    }

    #[test]
    fn run_capture_deadline_kills_at_timeout() {
        // 超时必须真的把进程杀掉，而不是让调用方无限等
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("exec sleep 30");
        let t0 = Instant::now();
        let r = run_capture_deadline(cmd, Duration::from_millis(300));
        assert!(r.is_err(), "超时应返回错误");
        assert!(
            t0.elapsed() < Duration::from_secs(4),
            "超时应及时返回，实际 {:?}",
            t0.elapsed()
        );
    }
}
