//! ytdlp-core：平台无关核心层（可单元测试，不依赖 GUI / 外部进程）。
//!
//! 对应需求文档 §7 数据模型与 §3.7 目录存储约定：
//! - `model`：统一列表条目 MediaItem 与状态机
//! - `config`：config.json（用户设置，原子写，损坏回退默认）
//! - `history`：history.json（列表/队列/历史持久化）
//! - `paths`：exe 同级目录约定（config/ temp/ tools/ logs/）
//!
//! 设计原则（§1.3）：核心逻辑与界面分离；失败安全（不破坏原文件）；
//! JSON 原子写（临时文件 + rename）；列表即工作台。

pub mod cli;
pub mod config;
pub mod cookies;
pub mod download;
pub mod exec;
pub mod history;
pub mod merge;
pub mod model;
pub mod paths;
pub mod probe;
pub mod thumbs;
pub mod timefmt;
pub mod tool_download;
pub mod transcode;
pub mod worker;

pub use model::{transition, ItemKind, MediaItem, RotAngle, Status};

/// 核心层统一错误类型。
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("I/O 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON 序列化/反序列化错误: {0}")]
    Json(#[from] serde_json::Error),
    #[error("非法状态迁移: {from:?} -> {to:?}")]
    InvalidTransition { from: Status, to: Status },
    #[error("条目不存在: {0}")]
    NotFound(String),
    #[error("配置损坏: {0}")]
    ConfigCorrupt(String),
    #[error("输入不合法: {0}")]
    InvalidInput(String),
    #[error("任务已取消")]
    Cancelled,
    #[error("输出已存在，按策略跳过: {0}")]
    AlreadyExists(String),
    #[error("进程执行失败 ({program}): code={code:?} stderr={stderr}")]
    ProcessFailed {
        program: String,
        code: Option<i32>,
        stderr: String,
    },
}

pub type Result<T> = std::result::Result<T, CoreError>;
