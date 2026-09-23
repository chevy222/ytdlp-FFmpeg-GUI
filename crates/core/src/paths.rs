//! 目录与存储约定（§3.7）：所有产生文件均在 exe 同级，不写注册表、不依赖 %APPDATA%。

use std::path::{Path, PathBuf};

/// exe 同级目录约定（绿色便携，§3.7）。
pub const DIR_CONFIG: &str = "config";
pub const DIR_TEMP: &str = "temp";
pub const DIR_TOOLS: &str = "tools";
pub const DIR_COOKIES: &str = "cookies";
pub const DIR_CACHE: &str = "cache";

/// 路径解析器：以 exe 所在目录为根（测试中可替换为任意根）。
#[derive(Debug, Clone)]
pub struct Paths {
    root: PathBuf,
}

impl Paths {
    /// 使用 exe 所在目录作为根。
    pub fn from_exe() -> Self {
        let exe = std::env::current_exe().expect("无法定位当前可执行文件");
        Self {
            root: exe
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from(".")),
        }
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

/// 损坏备份路径：`<原名>.corrupt-<epoch秒>-<序号>.json`（config/history 统一口径，
/// 多次损坏既不互相覆盖、也不因同秒两次损坏而丢一份）。
pub fn corrupt_backup_path(path: &Path) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    PathBuf::from(format!("{}.corrupt-{}-{}.json", path.display(), ts, seq))
}

/// JSON 原子写：先写唯一临时文件（含 fsync）再 rename，避免中途损坏（§3.7 规则）。
///
/// 临时文件名必须**唯一**：固定名（`x.json.tmp`）在两个线程同时落盘时会互相
/// 截断对方的写入，rename 失败分支还会删掉别人刚写好的那份 —— 结果是把"原子写"
/// 变成"随机撕裂 history.json"。
pub fn atomic_write_json<T: serde::Serialize>(path: &Path, value: &T) -> crate::Result<()> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "data.json".to_string());
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_file_name(format!("{}.{}.{}.tmp", name, std::process::id(), seq));
    let bytes = serde_json::to_vec_pretty(value)?;
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
        // rename 失败（如目标被其他程序占用）时清理自己的 tmp，不累积垃圾
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
/// 候选名用 `create_new` **原子占住**，不是"先 exists() 再交给 ffmpeg"：
/// 后者的检查与实际创建之间隔着几秒到几十分钟，并发任务会拿到同一个名字，
/// 而 ffmpeg 带 `-y`，后写者静默毁掉前者的产物。
pub fn unique_output_path(
    out_dir: &Path,
    base: &str,
    ext: &str,
    policy: &str,
) -> crate::Result<PathBuf> {
    let candidate = out_dir.join(format!("{base}.{ext}"));
    if candidate.exists() && policy == "skip" {
        return Err(crate::CoreError::AlreadyExists(candidate.display().to_string()));
    }
    let attempts: Vec<String> = if candidate.exists() {
        (1..1000).map(|i| format!("{base} ({i}).{ext}")).collect()
    } else {
        std::iter::once(format!("{base}.{ext}"))
            .chain((1..1000).map(|i| format!("{base} ({i}).{ext}")))
            .collect()
    };
    for name in attempts {
        let p = out_dir.join(&name);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&p) {
            // 名字已独占；空文件由 ffmpeg 带 -y 覆盖
            Ok(_claimed) => {
                crate::exec::check_output_path(&p)?;
                return Ok(p);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(crate::CoreError::InvalidInput(
        "无法生成不冲突的输出名".into(),
    ))
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
        // 临时文件不残留（名字带 pid+序号，必须按后缀扫而不是盯死一个名字）
        let residue: Vec<_> = std::fs::read_dir(p.config_dir())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(residue.is_empty(), "残留临时文件：{:?}", residue);
    }

    #[test]
    fn concurrent_writers_do_not_corrupt_the_target() {
        // 固定 tmp 名时这里会撕裂：两个线程互相截断对方刚写的临时文件
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("hist.json");
        let mut handles = Vec::new();
        for n in 0..8u64 {
            let p = path.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..25u64 {
                    atomic_write_json(&p, &serde_json::json!({ "n": n, "i": i })).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(v["n"].is_number() && v["i"].is_number(), "最终文件不是完整 JSON：{v}");
        let residue: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(residue.is_empty(), "残留临时文件：{:?}", residue);
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

    #[test]
    fn concurrent_claims_never_hand_out_the_same_name() {
        // 旧实现是 exists() 后创建：并发两个任务能拿到同一个输出名，
        // 而 ffmpeg 带 -y，后写者会静默毁掉前者的产物
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().to_path_buf();
        let names: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let mut hs = Vec::new();
        for _ in 0..8 {
            let d = dir.clone();
            let n = names.clone();
            hs.push(std::thread::spawn(move || {
                let p = unique_output_path(&d, "同名", "mp4", "auto_inc").unwrap();
                n.lock().push(p.file_name().unwrap().to_string_lossy().into_owned());
            }));
        }
        for h in hs {
            h.join().unwrap();
        }
        let got = names.lock().clone();
        let uniq: std::collections::HashSet<&String> = got.iter().collect();
        assert_eq!(uniq.len(), got.len(), "发出了重复的名字：{:?}", got);
    }
}
