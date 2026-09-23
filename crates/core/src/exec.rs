//! 外部进程与工具链定位（§5.1/§3.6 依赖 / §4 安全性-路径参数化）。
//!
//! - 工具定位：config 显式路径 → `<exe 同级>\tools\` 托管 → 系统 PATH，
//!   任一级不存在即回退下一级（首次运行时 `tools\` 是空的，不能遮住 PATH）。
//! - 进程执行：Command 参数化（不拼接 shell，防注入）；取消时终止进程树（Windows taskkill /T）。
//! - 平台：Linux 上编译/测试，Windows 上生产运行；取消用条件编译。

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

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
        // 出生即入 Job：本进程退出时内核负责收掉整棵进程树（见 assign_to_job）
        #[cfg(windows)]
        assign_to_job(&child);
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
    #[cfg(windows)]
    {
        let pid = child.id();
        // taskkill 需先不 kill 掉主进程句柄，直接用 PID 命令
        let mut tk = Command::new(system_tool("taskkill.exe"));
        hide_console(&mut tk);
        let _ = tk.args(["/PID", &pid.to_string(), "/T", "/F"]).status();
        // taskkill 可能失败（不存在 / 被拦 / PID 竞态）且无从知晓：
        // 再对直句柄补一刀（TerminateProcess 不依赖外部程序），否则调用方
        // 紧接着的 wait() 会替一个没死成的进程无限等下去。
        let _ = child.kill();
    }
    #[cfg(not(windows))]
    {
        let _ = child.kill();
    }
}

/// 把子进程挂进一个"本进程退出即全部终止"的 Job Object（Windows）。
///
/// 关窗时 Windows **不会**随父进程杀掉子进程：正在写桌面的 ffmpeg 会变成孤儿，
/// 继续占网络/CPU 并持有输出文件句柄 —— 用户看到的"文件被占用删不掉"就是这个。
/// `taskkill /T` 靠快照枚举，进程正在创建子进程时会漏；入 Job 后 yt-dlp 自己
/// spawn 的 ffmpeg/deno 也自动继承同一 Job，由内核保证一起死。
///
/// 任何一步失败都静默忽略：Job 是兜底，原有 taskkill 链路仍然保留。
/// Job 句柄**故意不关**——关闭即触发 KILL_ON_JOB_CLOSE，必须留到进程退出。
#[cfg(windows)]
fn assign_to_job(child: &Child) {
    use std::os::windows::process::ChildExt;
    use windows::Win32::Foundation::{HANDLE, PCWSTR};
    use windows::Win32::System::Threading::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_BASIC_LIMIT_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    static JOB: std::sync::OnceLock<isize> = std::sync::OnceLock::new();
    let hjob = *JOB.get_or_init(|| match unsafe { CreateJobObjectW(None, PCWSTR::null()) } {
        Ok(handle) => {
            let info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
                BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                    LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                    ..Default::default()
                },
                ..Default::default()
            };
            let ok = unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok.as_bool() {
                handle.0
            } else {
                0
            }
        }
        Err(_) => 0,
    });
    if hjob == 0 {
        return;
    }
    let hproc = HANDLE(child.process().as_raw_handle() as isize);
    let _ = unsafe { AssignProcessToJobObject(HANDLE(hjob), hproc) };
}

/// 一次受监控执行的结局。
pub struct Completed {
    pub status: std::process::ExitStatus,
    /// 子进程 stderr（已按 UTF-8→GBK 口径解码；超上限时只保留尾部）
    pub stderr: String,
}

/// stderr 尾部保留上限：真错误总在末尾，超出从头部丢弃，防无界增长。
const STDERR_TAIL_CAP: usize = 256 * 1024;

/// 长任务子进程的统一入口：**两条输出管道都在独立线程上持续排空**。
///
/// 为什么必须是这一个入口：匿名管道缓冲有限，ffmpeg/yt-dlp 往 stderr 写得足够多
/// 而无人读取时会阻塞在 write()，于是 stdout 不再产出，父线程的 read 与子进程的
/// write 互等成**永久死锁**；更糟的是"取消"通常检查在"收到下一行之后"，
/// 连取消都会失效。手写这个循环必然出错，所以把排空做成不可遗忘的内建行为。
pub struct Monitored {
    guard: ChildGuard,
    /// stdout 逐行（后台读线程投递）
    lines: std::sync::mpsc::Receiver<String>,
    /// stderr 原始字节尾部（后台读线程追加）
    err_tail: Arc<Mutex<Vec<u8>>>,
    /// stderr 读线程结束信号（用它给"读完"一个有界等待，而非无限 join）
    err_done: std::sync::mpsc::Receiver<()>,
}

impl Monitored {
    /// 启动子进程并接管两条管道（stdin 一律置空：ffmpeg 会读 stdin，
    /// 继承来的句柄可能让它与父进程互相等）。
    pub fn spawn(cmd: &mut Command) -> crate::Result<Self> {
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        let mut guard = ChildGuard::spawn(cmd)?;
        let stdout = guard
            .stdout()
            .ok_or_else(|| CoreError::Io(std::io::Error::other("无法读取子进程 stdout")))?;
        let stderr = guard
            .stderr()
            .ok_or_else(|| CoreError::Io(std::io::Error::other("无法读取子进程 stderr")))?;

        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                match line {
                    // 接收端已丢弃：pump 已退出，没必要再读
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let err_tail = Arc::new(Mutex::new(Vec::new()));
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        {
            let tail = err_tail.clone();
            std::thread::spawn(move || {
                use std::io::Read;
                let mut buf = [0u8; 8 * 1024];
                let mut reader = std::io::BufReader::new(stderr);
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let mut g = tail.lock();
                            g.extend_from_slice(&buf[..n]);
                            if g.len() > STDERR_TAIL_CAP {
                                let cut = g.len() - STDERR_TAIL_CAP;
                                g.drain(..cut);
                            }
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                let _ = done_tx.send(());
            });
        }

        Ok(Self {
            guard,
            lines: rx,
            err_tail,
            err_done: done_rx,
        })
    }

    /// 逐行消费 stdout，期间持续检查取消标志；取消则杀进程树并返回 `Err(Cancelled)`。
    ///
    /// 用带超时的接收而不是 `for line in lines()`：子进程沉默时（慢 seek、GPU 驱动卡住、
    /// 或干脆已经死锁）这里**照样会醒**，取消按钮始终有效。
    pub fn pump(
        &mut self,
        cancel: &AtomicBool,
        mut on_line: impl FnMut(&str),
    ) -> crate::Result<Completed> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                self.guard.kill_tree();
                return Err(CoreError::Cancelled);
            }
            match self.lines.recv_timeout(Duration::from_millis(250)) {
                Ok(line) => on_line(&line),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                // stdout 已关闭：子进程退出，或读线程因句柄错误结束
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let status = self.reap()?;
        // 有界等待 stderr 读完（正常退出时立刻到）；子孙进程占着管道句柄时
        // 宁可拿到略短的 stderr，也不无限等下去。
        let _ = self.err_done.recv_timeout(Duration::from_millis(500));
        let tail = self.err_tail.lock();
        Ok(Completed {
            status,
            stderr: decode_text(&tail),
        })
    }

    /// 等子进程退出。读取线程已结束而进程仍在（管道被打满之类）时，
    /// 宽限 2 秒后强杀再回收——绝不无限等待。
    fn reap(&mut self) -> std::io::Result<std::process::ExitStatus> {
        for _ in 0..40 {
            if let Some(status) = self.guard.try_wait()? {
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.guard.kill_tree();
        self.guard.wait()
    }

    /// 非阻塞推进一次（保留给需要自己控制节奏的调用方）。
    pub fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.guard.try_wait()
    }
}

/// ffmpeg `-progress` 模式的标准尾巴：机器可读进度走 stdout，
/// stderr 只剩真错误（不加 `-nostats -loglevel error` 时 ffmpeg 每 0.5s
/// 往 stderr 写一条统计，足以打满管道缓冲）。
pub fn progress_tail() -> [&'static str; 5] {
    [
        "-progress",
        "pipe:1",
        "-nostats",
        "-loglevel",
        "error",
    ]
}

/// 只接受 http(s)。把外部可控字符串当 URL 交给 yt-dlp 之前必须过这一关：
/// 播放列表条目地址来自站点 JSON，`-` 开头会被解析成 `--exec` 等选项。
pub fn is_http_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// 追加待处理 URL，并用 `--` 终结选项解析（optparse 识别）。
///
/// `--` 之后的内容永远是位置参数，恶意/畸形地址（如 `--exec=calc.exe`）
/// 就只能是一条下不去的链接，而不是一条命令。
pub fn push_url_arg(args: &mut Vec<String>, url: &str) {
    args.push("--".to_string());
    args.push(url.to_string());
}

/// 把选项插在 `--`（由 [`push_url_arg`] 追加）之前，保证 URL 仍是唯一的尾部位置参数。
pub fn insert_before_url(args: &mut Vec<String>, opts: impl IntoIterator<Item = String>) {
    let at = args
        .iter()
        .rposition(|a| a == "--")
        .unwrap_or_else(|| args.len());
    args.splice(at..at, opts);
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

/// 抹掉参数里 URL 的 userinfo（`user:pass@`），保留协议/主机/端口便于排障。
///
/// 命令行会经 `on_log` 进条目日志、落盘 history.json、并能在 UI 里一键复制；
/// `--proxy http://user:pass@host:7890` 原样写进去等于把代理凭据存成明文并展示。
fn redact_arg(a: &str) -> String {
    let Some(scheme_end) = a.find("://") else {
        return a.to_string();
    };
    let rest = &a[scheme_end + 3..];
    // userinfo 只可能出现在下一个 `/`、`?`、`#` 之前
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let Some(at) = authority.rfind('@') else {
        return a.to_string();
    };
    let mut out = String::with_capacity(a.len() + 8);
    out.push_str(&a[..scheme_end + 3]);
    out.push_str("***:***@");
    out.push_str(&rest[at + 1..]);
    out
}

/// 把程序名与参数拼成一条可读、可复制、可直接粘贴执行的单行命令（凭据已脱敏）。
/// 含空白/引号/空串的参数加双引号并转义内部引号，其余原样。
pub fn display_command(program: &str, args: &[String]) -> String {
    let mut s = program.to_string();
    for a in args {
        let a = redact_arg(a);
        if a.is_empty() || a.chars().any(|c| c.is_whitespace() || c == '"') {
            s.push_str(&format!(" \"{}\"", a.replace('"', "\\\"")));
        } else {
            s.push(' ');
            s.push_str(&a);
        }
    }
    s
}

/// 删除文件并短暂重试。
///
/// Windows 上刚被 kill 的子进程，其文件对象可能还要占几毫秒；杀软也会抢先打开
/// 新出现的大视频。一次 `remove_file` 返回 SharingViolation 是常态而不是例外，
/// 旧代码用 `let _ =` 吞掉它，于是日志写着"已清理残留"、桌上留着几 GB 半成品。
pub fn remove_with_retry(path: &Path) -> std::io::Result<()> {
    let mut last = None;
    for _ in 0..20 {
        match std::fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    Err(last.unwrap_or_else(|| {
        std::io::Error::other(format!("删除失败：{}", path.display()))
    }))
}

/// 删除目录树并短暂重试（合并任务的私有临时目录）。
pub fn remove_dir_with_retry(path: &Path) -> std::io::Result<()> {
    let mut last = None;
    for _ in 0..20 {
        match std::fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    Err(last.unwrap_or_else(|| {
        std::io::Error::other(format!("删除目录失败：{}", path.display()))
    }))
}

/// 是否为 Win32 保留设备名（CON/PRN/AUX/NUL 与 COM1-9/LPT1-9，与扩展名无关）。
///
/// 必须精确到"COM+单个非零数字"：按"以 COM 开头"粗判会把 `COM-1.mp4`、
/// `COMPUTER.mp4` 这类正常名字一起误伤。
fn is_reserved_device(stem_upper: &str) -> bool {
    if matches!(stem_upper, "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    let tail = stem_upper
        .strip_prefix("COM")
        .or_else(|| stem_upper.strip_prefix("LPT"));
    match tail {
        Some(d) => d.len() == 1 && {
            let b = d.as_bytes()[0];
            b.is_ascii_digit() && b != b'0'
        },
        None => false,
    }
}

/// 长任务开工前的廉价校验：保留设备名与过长路径。
///
/// 两类都必须早报：`NUL.mp4` 会让 ffmpeg 落进设备命名空间、>260 字符的路径
/// 只有 std::fs 能过（ffmpeg 不加 `\\?\` 会报 no such file），而这两种报错都
/// 发生在一整轮编码之后，用户完全看不出真实原因。
pub fn check_output_path(path: &Path) -> crate::Result<()> {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
        .to_ascii_uppercase();
    if is_reserved_device(&stem) {
        return Err(CoreError::InvalidInput(format!(
            "输出名是 Windows 保留设备名：{}",
            path.display()
        )));
    }
    if path.as_os_str().len() > 240 {
        return Err(CoreError::InvalidInput(format!(
            "输出路径过长（{} 字符），请换更浅的输出目录：{}",
            path.as_os_str().len(),
            path.display()
        )));
    }
    Ok(())
}

/// 系统工具的绝对路径（`curl.exe` / `taskkill.exe` / `explorer.exe`）。
///
/// `Command::new("curl")` 走 PATH 搜索：绿色便携版可能放在任何目录，PATH 里
/// 被人抢先放一个同名 curl.exe，就等于把"下载可执行文件"这件事交给了攻击者。
pub fn system_tool(name: &str) -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(root) = std::env::var_os("SystemRoot").or_else(|| std::env::var_os("WINDIR")) {
            let p = PathBuf::from(root).join("System32").join(name);
            if p.is_file() {
                return p;
            }
        }
    }
    PathBuf::from(name)
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

    /// 管道死锁回归：子进程先往 stderr 灌 100KB（远超管道缓冲）**才**写 stdout。
    /// 不后台排空 stderr 的实现会永远卡在 read(stdout) 上，本测试会挂死而不是失败。
    #[test]
    fn monitored_drains_stderr_so_a_chatty_child_cannot_deadlock() {
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            "i=0; while [ $i -lt 2000 ]; do echo 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx' >&2; i=$((i+1)); done; echo done",
        ]);
        let cancel = AtomicBool::new(false);
        let mut m = Monitored::spawn(&mut cmd).unwrap();
        let mut last = None;
        let done = m
            .pump(&cancel, |l| last = Some(l.to_string()))
            .unwrap();
        assert!(done.status.success(), "子进程应正常退出");
        assert_eq!(last.as_deref(), Some("done"), "stdout 行必须逐行送达");
        assert!(
            done.stderr.len() >= 100_000,
            "stderr 必须被完整排空，实际只拿到 {} 字节",
            done.stderr.len()
        );
    }

    #[test]
    fn stderr_tail_is_capped_at_the_end() {
        // 真错误总在末尾：上限之外从头部丢弃，且不得超过上限太多
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            "i=0; while [ $i -lt 9000 ]; do echo 'yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy' >&2; i=$((i+1)); done",
        ]);
        let cancel = AtomicBool::new(false);
        let mut m = Monitored::spawn(&mut cmd).unwrap();
        let done = m.pump(&cancel, |_| {}).unwrap();
        assert!(
            done.stderr.len() <= STDERR_TAIL_CAP + 128,
            "stderr 未按上限截断：{}",
            done.stderr.len()
        );
        assert!(done.stderr.contains('y'));
    }

    #[test]
    fn url_argument_is_isolated_by_the_options_terminator() {
        let mut args = vec!["-o".to_string(), "x".to_string()];
        push_url_arg(&mut args, "https://example.com/v");
        assert_eq!(args.join(" "), "-o x -- https://example.com/v");
        // 后补的选项必须落在 `--` 之前，否则会被当成第二条 URL 而不是选项
        insert_before_url(&mut args, vec!["--newline".to_string(), "2".to_string()]);
        assert_eq!(args.join(" "), "-o x --newline 2 -- https://example.com/v");
        assert!(is_http_url("http://a.test"));
        assert!(is_http_url("https://a.test"));
        // 站点 JSON 里回来的字符串不能直接当 URL 用
        assert!(!is_http_url("--exec=calc.exe"));
        assert!(!is_http_url("file:///C:/Windows/win.ini"));
    }

    #[test]
    fn progress_tail_sends_machine_progress_to_stdout() {
        assert_eq!(
            progress_tail().join(" "),
            "-progress pipe:1 -nostats -loglevel error"
        );
    }

    #[test]
    fn display_command_redacts_proxy_credentials() {
        let args = vec![
            "--proxy".to_string(),
            "http://user:pass@127.0.0.1:7890".to_string(),
            "--cookies".to_string(),
            "C:\\config\\cookies\\www.youtube.com.txt".to_string(),
            "https://example.com/watch?v=1".to_string(),
        ];
        let shown = display_command("yt-dlp", &args);
        assert!(!shown.contains("user:pass"), "凭据泄漏到日志：{shown}");
        assert!(shown.contains("***:***@127.0.0.1:7890"), "{shown}");
        // 主机与端口要留着便于排障；无凭据的参数一律原样
        assert!(shown.contains("www.youtube.com.txt"), "{shown}");
        assert!(shown.contains("https://example.com/watch?v=1"), "{shown}");
    }

    #[test]
    fn check_output_path_rejects_reserved_and_long() {
        let root = tempdir().unwrap();
        for bad in ["NUL", "con", "COM1", "com9", "LPT3", "PRN"] {
            let p = root.path().join(format!("{bad}.mp4"));
            assert!(
                check_output_path(&p).is_err(),
                "保留设备名未被拦下：{}",
                p.display()
            );
        }
        // 正常中文名与 NULL/COM-1/COMPUTER/COM0 这类名字不能误伤
        for ok in ["标题 [abc]", "NULL", "1080p", "COM-1", "COMPUTER", "COM0", "LPTA"] {
            let p = root.path().join(format!("{ok}.mp4"));
            assert!(check_output_path(&p).is_ok(), "误伤正常文件名：{ok}");
        }
        let long = root.path().join(format!("{}.mp4", "a".repeat(300)));
        assert!(check_output_path(&long).is_err());
    }

    #[test]
    fn remove_with_retry_treats_missing_as_success() {
        let root = tempdir().unwrap();
        let gone = root.path().join("not-there.mp4");
        assert!(remove_with_retry(&gone).is_ok());
        std::fs::write(&gone, b"x").unwrap();
        assert!(remove_with_retry(&gone).is_ok());
        assert!(!gone.exists());
    }
}
