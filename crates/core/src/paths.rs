//! 目录与存储约定（§3.7）：所有产生文件均在 exe 同级，不写注册表、不依赖 %APPDATA%。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// exe 同级目录约定（绿色便携，§3.7）。
pub const DIR_CONFIG: &str = "config";
pub const DIR_TEMP: &str = "temp";
pub const DIR_TOOLS: &str = "tools";
pub const DIR_COOKIES: &str = "cookies";
pub const DIR_CACHE: &str = "cache";
/// 运行日志（GUI 无控制台，异常现场只能靠落盘日志，见 core::log）
pub const DIR_LOGS: &str = "logs";

/// 路径解析器：以 exe 所在目录为根（测试中可替换为任意根）。
#[derive(Debug, Clone)]
pub struct Paths {
    root: PathBuf,
}

impl Paths {
    /// 使用 exe 所在目录作为根。
    /// 定位失败（极罕见）时退回当前工作目录，而不是 panic —— 核心层不该有 panic 点。
    pub fn from_exe() -> Self {
        let root = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."));
        Self { root }
    }

    /// 显式指定根（测试/开发用）。
    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config_dir(&self) -> PathBuf {
        self.root.join(DIR_CONFIG)
    }

    pub fn temp_dir(&self) -> PathBuf {
        self.root.join(DIR_TEMP)
    }

    pub fn tools_dir(&self) -> PathBuf {
        self.root.join(DIR_TOOLS)
    }

    pub fn cookies_dir(&self) -> PathBuf {
        self.config_dir().join(DIR_COOKIES)
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.config_dir().join(DIR_CACHE)
    }

    /// 运行日志目录（exe 同级 `logs/`，见 core::log）。
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join(DIR_LOGS)
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir().join("config.json")
    }

    pub fn history_file(&self) -> PathBuf {
        self.config_dir().join("history.json")
    }

    /// 创建全部运行时目录（幂等）。
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        for d in [
            self.config_dir(),
            self.temp_dir(),
            self.tools_dir(),
            self.cookies_dir(),
            self.cache_dir(),
            self.logs_dir(),
        ] {
            std::fs::create_dir_all(d)?;
        }
        Ok(())
    }

    /// 任务私有临时目录（temp/<task_id>/，任务结束清理，§3.7/UL-08）。
    pub fn task_temp_dir(&self, task_id: &str) -> PathBuf {
        self.temp_dir().join(task_id)
    }
}

/// 损坏备份路径：`<原名>.corrupt-<epoch秒>.json`（config/history 统一口径，
/// 多次损坏不互相覆盖）。
///
/// 用 `OsString` 拼名而不是 `format!("{}", path.display())`：后者对非 UTF-8 路径
/// 会把非法字节替换成 U+FFFD 再落盘，备份文件凭空改名。
pub fn corrupt_backup_path(path: &Path) -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(format!(".corrupt-{ts}.json"));
    path.with_file_name(name)
}

/// 原子写的临时文件名序列（同进程内单调递增，保证并发写入者互不撞名）。
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// JSON 原子写：先写临时文件（含 fsync）再 rename，避免中途损坏（§3.7 规则）。
///
/// 临时名**必须**带 pid + 序号：多个任务线程会并发调用写盘（每个任务收尾都
/// `persist()`），共用一个 `history.json.tmp` 时，第二个写入者会在
/// `File::create` 处截断第一个的半成品，最终把混合内容 rename 成正式文件 ——
/// 一份非法 JSON 就等于下次启动列表全丢（History::load 只能备份 + 回退空列表）。
pub fn atomic_write_json<T: serde::Serialize>(path: &Path, value: &T) -> crate::Result<()> {
    atomic_write_json_with(path, value, true)
}

/// 同 [`atomic_write_json`]，`pretty=false` 时输出紧凑 JSON（体积/耗时约 1/3，
/// 用于 history.json 这类机器读写的文件；config.json 保留缩进便于用户手改）。
pub fn atomic_write_json_with<T: serde::Serialize>(
    path: &Path,
    value: &T,
    pretty: bool,
) -> crate::Result<()> {
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let stem = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "data".to_string());
    let tmp = path.with_file_name(format!("{stem}.{}.{seq}.tmp", std::process::id()));
    let bytes = if pretty {
        serde_json::to_vec_pretty(value)?
    } else {
        serde_json::to_vec(value)?
    };
    // 写入 + fsync：掉电时保证 rename 之前数据页已落盘（只 rename 不 fsync
    // 可能出现"元数据已提交、数据未提交"的损坏文件）。
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    // 直接 rename 覆盖目标：std::fs::rename 在 Windows 上走 MoveFileExW
    // （MOVEFILE_REPLACE_EXISTING），在类 Unix 上是原子替换。
    // 不要"先 remove 再 rename"——那会在两步之间制造文件缺失窗口，崩溃即丢整份配置。
    if let Err(e) = std::fs::rename(&tmp, path) {
        // rename 失败（如目标被其他程序占用）时清理 tmp，避免累积 *.tmp 垃圾
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// 容器名 → 输出扩展名（mkv → mkv，其余 → mp4；转码/合并共用，C2）。
pub fn container_extension(container: &str) -> &'static str {
    match container {
        "mkv" => "mkv",
        _ => "mp4",
    }
}

/// 输出文件名碰撞处理（TC-11）：`skip` 且已存在 → 报错；`auto_inc` →
/// 依次尝试 `base (1).ext`、`base (2).ext` …（上限 1000）。转码与合并共用（C2）。
///
/// 命中候选名后用 `create_new` **占位**再返回：仅靠 `exists()` 判断存在
/// TOCTOU —— 两个并发任务（默认并发 3）会同时看到"这个名字空闲"，抢到同一个
/// 输出路径，后完成的那个静默覆盖前一个的产物。占位文件是 0 字节，调用方
/// 随后用 ffmpeg `-y` 覆写；任务失败时调用方会删除该路径（残留不会留成垃圾）。
pub fn unique_output_path(
    out_dir: &Path,
    base: &str,
    ext: &str,
    policy: &str,
) -> crate::Result<PathBuf> {
    let candidate = out_dir.join(format!("{base}.{ext}"));
    if !candidate.exists() {
        if try_reserve(&candidate) {
            return Ok(candidate);
        }
    } else if policy == "skip" {
        return Err(crate::CoreError::Io(std::io::Error::other(format!(
            "输出已存在，按策略跳过：{}",
            candidate.display()
        ))));
    }
    for i in 1..1000 {
        let p = out_dir.join(format!("{base} ({i}).{ext}"));
        if try_reserve(&p) {
            return Ok(p);
        }
        if p.exists() && policy == "skip" {
            return Err(crate::CoreError::Io(std::io::Error::other(format!(
                "输出已存在，按策略跳过：{}",
                p.display()
            ))));
        }
    }
    Err(crate::CoreError::Io(std::io::Error::other(
        "无法生成不冲突的输出名",
    )))
}

/// 抢占输出名（`create_new` 是原子操作：只有一方能成功）。
/// 目录不可写时返回 false，由调用方继续尝试或报错。
fn try_reserve(path: &Path) -> bool {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn paths_resolve_exe_relative_dirs() {
        let root = tempdir().unwrap();
        let p = Paths::from_root(root.path());
        assert_eq!(p.config_dir(), root.path().join("config"));
        assert_eq!(p.temp_dir(), root.path().join("temp"));
        assert_eq!(p.tools_dir(), root.path().join("tools"));
        assert_eq!(p.cookies_dir(), root.path().join("config/cookies"));
        assert_eq!(p.cache_dir(), root.path().join("config/cache"));
        assert_eq!(p.config_file(), root.path().join("config/config.json"));
        assert_eq!(p.history_file(), root.path().join("config/history.json"));
    }

    #[test]
    fn ensure_dirs_creates_all() {
        let root = tempdir().unwrap();
        let p = Paths::from_root(root.path());
        p.ensure_dirs().unwrap();
        for d in [DIR_CONFIG, DIR_TEMP, DIR_TOOLS] {
            assert!(root.path().join(d).is_dir(), "缺失目录 {}", d);
        }
        assert!(root.path().join("config/cookies").is_dir());
        assert!(root.path().join("config/cache").is_dir());
    }

    #[test]
    fn task_temp_dir_isolated_by_id() {
        let root = tempdir().unwrap();
        let p = Paths::from_root(root.path());
        assert_eq!(p.task_temp_dir("abc"), root.path().join("temp/abc"));
    }

    #[test]
    fn atomic_write_then_read_back() {
        let root = tempdir().unwrap();
        let p = Paths::from_root(root.path());
        p.ensure_dirs().unwrap();
        let f = p.config_file();
        atomic_write_json(&f, &serde_json::json!({"a": 1})).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        assert_eq!(v["a"], 1);
        // 覆盖写（不再"先删后改名"，必须直接覆盖成功）
        atomic_write_json(&f, &serde_json::json!({"a": 2})).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        assert_eq!(v["a"], 2);
        // 临时文件不残留
        assert!(!root.path().join("config/config.json.tmp").exists());
    }

    #[test]
    fn unique_output_path_fresh_and_auto_inc() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let p = unique_output_path(dir, "视频", "mp4", "auto_inc").unwrap();
        assert_eq!(p, dir.join("视频.mp4"));
        std::fs::File::create(&p).unwrap();
        // 已存在 → auto_inc 生成 (1)
        let p2 = unique_output_path(dir, "视频", "mp4", "auto_inc").unwrap();
        assert_eq!(p2, dir.join("视频 (1).mp4"));
    }

    #[test]
    fn unique_output_path_skip_errors() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        std::fs::write(dir.join("a.mp4"), "x").unwrap();
        assert!(unique_output_path(dir, "a", "mp4", "skip").is_err());
        // 不同扩展名不冲突
        assert!(unique_output_path(dir, "a", "mkv", "skip").is_ok());
    }
}
