//! 应用全局状态：路径、配置、历史列表、并发队列、取消注册表。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use ytdlp_core::config::AppConfig;
use ytdlp_core::exec::ToolResolver;
use ytdlp_core::history::History;
use ytdlp_core::paths::Paths;
use ytdlp_core::worker::TaskQueue;

/// 应用全局状态（Tauri State）。
/// 本次调用级覆盖（CLI 传入，不写 config.json，§UL-09）。
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    pub dir: Option<String>,
    pub cookies: Option<String>,
    pub yt_dlp_path: Option<String>,
    pub deno_path: Option<String>,
}

impl From<ytdlp_core::cli::CliArgs> for CliOverrides {
    fn from(c: ytdlp_core::cli::CliArgs) -> Self {
        Self {
            dir: c.dir,
            cookies: c.cookies,
            yt_dlp_path: c.yt_dlp_path,
            deno_path: c.deno_path,
        }
    }
}

pub struct AppState {
    pub paths: Paths,
    pub history: Mutex<History>,
    pub config: Mutex<AppConfig>,
    pub queue: Mutex<TaskQueue>,
    /// 任务取消标志注册表（id -> flag）
    pub cancels: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// 合并面板参数（入队等待时保存；低频操作，仅面板内配置，不落 config）
    pub merge_jobs: Mutex<HashMap<String, crate::commands::MergeJob>>,
    /// CLI 本次调用级覆盖（--dir/--cookies/--yt-dlp-path/--deno-path）
    pub cli: Mutex<CliOverrides>,
    /// 历史写盘串行锁（同一时刻只有一个写入者）
    persist_lock: Mutex<()>,
    /// persist 请求序号（每调用一次 +1）
    persist_req: AtomicU64,
    /// 已完成写盘的请求序号（写盘时取"当时最高的请求序号"）
    persist_done: AtomicU64,
}

impl AppState {
    pub fn new() -> Self {
        let paths = Paths::from_exe();
        // 目录创建在 lib.rs setup 里做（那里会报告错误）；这里不重复
        let config = match AppConfig::load(&paths.config_file()) {
            Ok(c) => c,
            Err(e) => {
                // 损坏文件已在 load 内备份为 .corrupt-<ts>.json；回退默认并告警
                ytdlp_core::log::error(format!("config 加载失败（回退默认设置）：{e}"));
                AppConfig::default()
            }
        };
        let mut history = History::load(&paths.history_file()).unwrap_or_else(|e| {
            ytdlp_core::log::error(format!("history 加载失败（回退空列表）：{e}"));
            History::default()
        });
        // 设置页的"历史上限"要真正生效（此前只存在于配置里，UI 只能标注"暂未生效"）
        history.set_limit(config.general.history_limit);
        let concurrency = config.general.concurrency as usize;
        Self {
            paths,
            history: Mutex::new(history),
            config: Mutex::new(config),
            queue: Mutex::new(TaskQueue::new(concurrency)),
            cancels: Mutex::new(HashMap::new()),
            merge_jobs: Mutex::new(HashMap::new()),
            cli: Mutex::new(CliOverrides::default()),
            persist_lock: Mutex::new(()),
            persist_req: AtomicU64::new(0),
            persist_done: AtomicU64::new(0),
        }
    }

    /// 当前工具解析器（按 config 依赖段构造；CLI 覆盖优先）。
    pub fn resolver(&self) -> ToolResolver {
        let mut cfg = self.config.lock().clone();
        {
            let cli = self.cli.lock();
            if let Some(p) = &cli.yt_dlp_path {
                cfg.dependencies.yt_dlp_path = Some(p.clone());
            }
            if let Some(p) = &cli.deno_path {
                cfg.dependencies.deno_path = Some(p.clone());
            }
        }
        let mut r = ToolResolver::from_config(&cfg.dependencies);
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                r = r.with_tools_dir(dir.join("tools"));
            }
        }
        r
    }

    /// 注册取消标志并返回。
    pub fn register_cancel(&self, id: &str) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        self.cancels
            .lock()
            .insert(id.to_string(), flag.clone());
        flag
    }

    pub fn cancel_flag(&self, id: &str) -> Option<Arc<AtomicBool>> {
        self.cancels.lock().get(id).cloned()
    }

    /// 持久化历史（变更即原子写，写失败降级为内存态并告警）。
    ///
    /// §11.15 锁纪律：锁内只取快照，序列化 + 写盘在锁外——
    /// 持锁写盘（满载 100 条 × 300 行日志时毫秒到百毫秒级）会阻塞所有 update_item。
    ///
    /// 但"锁外写"意味着多个任务线程可能同时写（每个任务收尾都 persist）：
    /// - 临时文件名已按 `pid + 序号` 隔离，互相不会截断（见 `paths::atomic_write_json`）；
    /// - 这里再把写入者串行化，并做**写合并**：同时刻多个请求只需最后那一次写盘
    ///   （写盘者取的是"当时最高请求号"对应的最新快照，被覆盖的请求直接返回）。
    ///   批量任务（播放列表展开、批量转码）收尾时的 N 次多 MB 写盘会收敛成 1 次。
    pub fn persist(&self) {
        let my_seq = self.persist_req.fetch_add(1, Ordering::SeqCst) + 1;
        let _writer = self.persist_lock.lock();
        let highest = self.persist_req.load(Ordering::SeqCst);
        if self.persist_done.load(Ordering::SeqCst) >= my_seq {
            // 已有写入者用不早于我这轮的快照写过盘了
            return;
        }
        let snapshot = self.history.lock().clone();
        match snapshot.save(&self.paths.history_file()) {
            Ok(()) => self.persist_done.store(highest, Ordering::SeqCst),
            Err(e) => ytdlp_core::log::error(format!("history 持久化失败（保持内存态）：{e}")),
        }
    }
}
