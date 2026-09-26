//! 应用全局状态：路径、配置、历史列表、并发队列、取消注册表。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;

use ytdlp_core::config::AppConfig;
use ytdlp_core::exec::ToolResolver;
use ytdlp_core::history::History;
use ytdlp_core::paths::Paths;
use ytdlp_core::worker::TaskQueue;

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

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
    /// 上次写盘的 epoch 毫秒（用于去抖窗口判断）
    last_persist_ms: AtomicU64,
    /// 已有一个延迟 flush 任务在等（防止重复 spawn）
    pending_flush: AtomicBool,
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
            last_persist_ms: AtomicU64::new(0),
            pending_flush: AtomicBool::new(false),
        }
    }

    /// 当前工具解析器（按 config 依赖段构造；CLI 覆盖优先）。
    ///
    /// 只 clone `dependencies` 段（四个路径 + 一个开关）而不是整份 AppConfig：
    /// 本函数在任务收尾等路径上会被反复调用，而整份 AppConfig 还带着
    /// `network.site_proxy` 的 HashMap，白付一次堆分配。exe 同级 `tools/` 目录
    /// 直接取自 `Paths`（与 `current_exe().parent()` 同源），省掉一次系统调用。
    pub fn resolver(&self) -> ToolResolver {
        let mut deps = self.config.lock().dependencies.clone();
        {
            let cli = self.cli.lock();
            if let Some(p) = &cli.yt_dlp_path {
                deps.yt_dlp_path = Some(p.clone());
            }
            if let Some(p) = &cli.deno_path {
                deps.deno_path = Some(p.clone());
            }
        }
        ToolResolver::from_config(&deps).with_tools_dir(self.paths.tools_dir())
    }

    /// 注册取消标志并返回。
    ///
    /// 用 `entry().or_insert_with` 而非无条件 insert：cancel_item 可能在任务线程
    /// register_cancel **之前** 就被调用（"已排队/刚启动"窗口），若这里无条件新建
    /// flag 会覆盖掉取消意图；复用已存在的 flag 则取消请求不会因注册顺序蒸发。
    pub fn register_cancel(&self, id: &str) -> Arc<AtomicBool> {
        self.cancels
            .lock()
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone()
    }

    pub fn cancel_flag(&self, id: &str) -> Option<Arc<AtomicBool>> {
        self.cancels.lock().get(id).cloned()
    }

    /// 持久化历史（立即写盘 + 写合并，写失败降级为内存态并告警）。
    ///
    /// §11.15 锁纪律：锁内只取快照，序列化 + 写盘在锁外——
    /// 持锁写盘（满载 100 条 × 300 行日志时毫秒到百毫秒级）会阻塞所有 update_item。
    ///
    /// 写合并（并发到达）：同时刻多个请求只需最后那一次写盘。
    ///
    /// P1-5 去抖在 [`crate::commands::persist`] 层做：500ms 内的多次调用合并为
    /// 一次 `persist()`。本函数只负责真正写盘，并更新 `last_persist_ms`。
    pub fn persist(&self) {
        let my_seq = self.persist_req.fetch_add(1, Ordering::SeqCst) + 1;
        let _writer = self.persist_lock.lock();
        let highest = self.persist_req.load(Ordering::SeqCst);
        if self.persist_done.load(Ordering::SeqCst) >= my_seq {
            return;
        }
        let snapshot = self.history.lock().clone();
        match snapshot.save(&self.paths.history_file()) {
            Ok(()) => {
                self.persist_done.store(highest, Ordering::SeqCst);
                self.last_persist_ms.store(epoch_ms(), Ordering::SeqCst);
            }
            Err(e) => ytdlp_core::log::error(format!("history 持久化失败（保持内存态）：{e}")),
        }
    }

    /// 距上次写盘的毫秒数（去抖判断用）。
    pub(crate) fn ms_since_last_persist(&self) -> u64 {
        epoch_ms().saturating_sub(self.last_persist_ms.load(Ordering::SeqCst))
    }

    /// 是否已有延迟 flush 任务在等（防止重复 spawn）。
    pub(crate) fn pending_flush(&self) -> &AtomicBool {
        &self.pending_flush
    }
}
