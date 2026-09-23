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
        Self {
            paths,
            history: Mutex::new(history),
            config: Mutex::new(config),
            queue: Mutex::new(TaskQueue::new(concurrency)),
            cancels: Mutex::new(HashMap::new()),
            merge_jobs: Mutex::new(HashMap::new()),
            cli: Mutex::new(CliOverrides::default()),
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
    pub fn persist(&self) {
        // §11.15 锁纪律：锁内只取快照，序列化 + 写盘在锁外——
        // 持锁写盘（满载 100 条 × 300 行日志时毫秒到百毫秒级）会阻塞所有 update_item
        let snapshot = self.history.lock().clone();
        if let Err(e) = snapshot.save(&self.paths.history_file()) {
            eprintln!("history 持久化失败（保持内存态）：{}", e);
        }
    }
}
