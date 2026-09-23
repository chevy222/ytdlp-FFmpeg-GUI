//! 应用全局状态：路径、配置、历史列表、并发队列、取消注册表。

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
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

/// 解析队列容量：满时调用方退化为一次性线程，绝不静默丢任务。
const PROBE_QUEUE_CAP: usize = 512;

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
    /// 解析任务投递口（固定线程池消费）
    pub probe_tx: std::sync::mpsc::SyncSender<String>,
    probe_rx: Arc<Mutex<std::sync::mpsc::Receiver<String>>>,
    /// 串行化"快照 → 写盘"全程
    persist_lock: Mutex<()>,
}

impl AppState {
    pub fn new() -> Self {
        let paths = Paths::from_exe();
        // 目录创建在 lib.rs setup 里做（那里会报告错误）；这里不重复
        let config = match AppConfig::load(&paths.config_file()) {
            Ok(c) => c,
            Err(e) => {
                // 损坏文件已在 load 内备份为 .corrupt-<ts>.json；回退默认并告警
                eprintln!("config 加载失败（回退默认设置）：{}", e);
                AppConfig::default()
            }
        };
        let history = History::load(&paths.history_file()).unwrap_or_else(|e| {
            eprintln!("history 加载失败（回退空列表）：{}", e);
            History::default()
        });
        let concurrency = config.general.concurrency as usize;
        let (probe_tx, probe_rx) = std::sync::mpsc::sync_channel::<String>(PROBE_QUEUE_CAP);
        Self {
            paths,
            history: Mutex::new(history),
            config: Mutex::new(config),
            queue: Mutex::new(TaskQueue::new(concurrency)),
            cancels: Mutex::new(HashMap::new()),
            merge_jobs: Mutex::new(HashMap::new()),
            cli: Mutex::new(CliOverrides::default()),
            probe_tx,
            probe_rx: Arc::new(Mutex::new(probe_rx)),
            persist_lock: Mutex::new(()),
        }
    }

    /// 解析队列接收端（供 lib.rs 启动固定数量的解析线程）。
    pub fn probe_receiver(&self) -> Arc<Mutex<std::sync::mpsc::Receiver<String>>> {
        self.probe_rx.clone()
    }

    /// 投递一个解析任务。返回 Err 表示队列已满，调用方需自行退化处理。
    pub fn submit_probe(&self, id: &str) -> Result<(), String> {
        self.probe_tx
            .try_send(id.to_string())
            .map_err(|e| match e {
                std::sync::mpsc::TrySendError::Full(_) => "解析队列已满".to_string(),
                std::sync::mpsc::TrySendError::Disconnected(_) => "解析池已关闭".to_string(),
            })
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
    pub fn persist(&self) {
        // §11.15 锁纪律：锁内只取快照，序列化 + 写盘在锁外——
        // 持锁写盘（满载 100 条 × 300 行日志时毫秒到百毫秒级）会阻塞所有 update_item
        //
        // 但"快照"也必须在这把锁之内取：只串行化写盘的话，A 先快照、B 后快照，
        // B 先落盘、A 后落盘，磁盘上留下的仍是**较旧**的那份（丢更新）。
        let _w = self.persist_lock.lock();
        let snapshot = self.history.lock().clone();
        if let Err(e) = snapshot.save(&self.paths.history_file()) {
            eprintln!("history 持久化失败（保持内存态）：{}", e);
        }
    }
}
