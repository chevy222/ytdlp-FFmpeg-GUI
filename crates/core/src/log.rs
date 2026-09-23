//! 进程级落盘日志（无外部依赖，仅标准库）。
//!
//! GUI 程序（`windows_subsystem = "windows"`）没有控制台，`eprintln!` 等于黑洞：
//! 任务线程 panic、子进程清理失败、配置写盘失败这类"只在异常时才有输出"的信息
//! 全部看不见，现场只剩"功能没反应"。这里提供 exe 同级 `logs/app-<日期>.log`，
//! 按天滚动、保留最近 [`KEEP_DAYS`] 天。
//!
//! 取舍：不引入 tracing/log 生态（本项目不需要结构化日志与多后端），只用标准库。
//! 日志系统自身**绝不 panic**：目录不可写、写盘失败一律降级为 `eprintln!`。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use parking_lot::Mutex;

/// 保留的日志天数（超出的按文件名日期从旧到新清理）。
const KEEP_DAYS: usize = 7;

struct Logger {
    dir: PathBuf,
    /// 当前打开的文件 + 其日期戳（跨天时重开）
    cur: Mutex<Option<(String, std::fs::File)>>,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

/// 初始化日志目录（进程启动时调用一次；重复调用忽略）。
pub fn init(dir: impl Into<PathBuf>) {
    let dir = dir.into();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    prune(&dir);
    let _ = LOGGER.set(Logger {
        dir,
        cur: Mutex::new(None),
    });
}

/// 写一行日志（未初始化或写盘失败时降级到 stderr）。
fn write_line(level: &str, msg: &str) {
    let Some(logger) = LOGGER.get() else {
        eprintln!("[{level}] {msg}");
        return;
    };
    let today = date_stamp();
    let mut cur = logger.cur.lock();
    let need_open = !matches!(cur.as_ref(), Some((d, _)) if *d == today);
    if need_open {
        let path = logger.dir.join(format!("app-{today}.log"));
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => *cur = Some((today, f)),
            Err(_) => {
                drop(cur);
                eprintln!("[{level}] {msg}");
                return;
            }
        }
    }
    if let Some((_, f)) = cur.as_mut() {
        // 每行 flush：日志的用途是"崩溃后还能读到现场"，缓冲会白写
        let _ = writeln!(f, "{} [{level}] {}", datetime_stamp(), msg);
        let _ = f.flush();
    }
}

pub fn info(msg: impl AsRef<str>) {
    write_line("INFO", msg.as_ref());
}

pub fn warn(msg: impl AsRef<str>) {
    write_line("WARN", msg.as_ref());
}

pub fn error(msg: impl AsRef<str>) {
    write_line("ERROR", msg.as_ref());
}

/// 安装 panic hook：把 panic 位置、线程名与回溯写进日志。
///
/// 任务线程 panic（例如某个 `unwrap`/`clamp` 断言）在 GUI 下原本是完全静默的，
/// 表现为"任务永远卡在某个状态"；有了这条日志才能当场定位。
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "未知位置".to_string());
        let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "非字符串 panic 载荷".to_string()
        };
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("未命名线程");
        let bt = std::backtrace::Backtrace::force_capture();
        write_line("PANIC", &format!("线程 {name} 在 {loc} panic：{msg}\n{bt}"));
    }));
}

fn date_stamp() -> String {
    crate::timefmt::date_stamp(
        crate::timefmt::now_secs(),
        crate::timefmt::local_offset_secs(),
    )
}

fn datetime_stamp() -> String {
    crate::timefmt::datetime_str(
        crate::timefmt::now_secs(),
        crate::timefmt::local_offset_secs(),
    )
}

/// 清理超出保留天数的旧日志（文件名带 ISO 日期，字典序即时间序）。
fn prune(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("app-") && name.ends_with(".log") {
            files.push((name, e.path()));
        }
    }
    files.sort();
    if files.len() > KEEP_DAYS {
        for (_, p) in files.iter().take(files.len() - KEEP_DAYS) {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn prune_keeps_recent_files() {
        let dir = tempdir().unwrap();
        for d in 1..=10 {
            std::fs::write(
                dir.path().join(format!("app-2026-09-{:02}.log", d)),
                b"x",
            )
            .unwrap();
        }
        std::fs::write(dir.path().join("other.log"), b"x").unwrap();
        prune(dir.path());
        let left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        // 只留最近 KEEP_DAYS 个 app-*.log，非本模块命名的文件不动
        assert_eq!(left.iter().filter(|n| n.starts_with("app-")).count(), KEEP_DAYS);
        assert!(left.iter().any(|n| n == "other.log"));
        assert!(left.iter().any(|n| n == "app-2026-09-10.log"));
        assert!(!left.iter().any(|n| n == "app-2026-09-01.log"));
    }

    #[test]
    fn uninitialized_log_does_not_panic() {
        // 未 init（核心层被直接调用/单测）：降级 stderr，不 panic
        info("测试信息");
        warn("测试告警");
        error("测试错误");
    }
}
