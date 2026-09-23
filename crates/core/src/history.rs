//! history.json：统一列表/队列/历史持久化（§3.1 UL-07/UL-10、§7.2）。
//!
//! 规则：MediaItem 序列化；保留条数默认 100、上限 200（§6 统一）；
//! JSON 原子写；重启恢复未完成任务（可继续/标记失败），已完成项保留。

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::model::MediaItem;
use crate::paths::atomic_write_json_with;
use crate::{CoreError, Result};

/// 历史仓库：条目有序列表 + 当前上限。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct History {
    #[serde(default)]
    pub items: Vec<MediaItem>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    100
}

impl Default for History {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            limit: 100,
        }
    }
}

impl History {
    pub fn new(limit: usize) -> Self {
        Self {
            items: Vec::new(),
            limit: limit.clamp(1, crate::config::GeneralConfig::HISTORY_LIMIT_MAX),
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 应用设置里的历史上限（§6 统一：默认 100、上限 200）。
    /// 设置页可改，改完立即生效（超出部分在下次 upsert 时按"最旧终态优先"裁剪）。
    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit.clamp(1, crate::config::GeneralConfig::HISTORY_LIMIT_MAX);
    }

    /// 按 id 查找。
    pub fn get(&self, id: &str) -> Option<&MediaItem> {
        self.items.iter().find(|i| i.id == id)
    }

    /// 按 id 可变查找（高频进度/日志原地更新用，避免 clone 整个条目再 upsert）。
    pub fn get_mut(&mut self, id: &str) -> Option<&mut MediaItem> {
        self.items.iter_mut().find(|i| i.id == id)
    }

    /// 追加或按 id 更新；超过上限时优先裁剪**最旧的终态条目**
    /// （Done/Failed/Canceled），避免把仍在处理中的活跃任务裁掉
    /// （任务完成 upsert 会"复活"被裁条目，造成列表闪烁）。
    pub fn upsert(&mut self, item: MediaItem) {
        if let Some(existing) = self.items.iter_mut().find(|i| i.id == item.id) {
            *existing = item;
        } else {
            self.items.push(item);
        }
        while self.items.len() > self.limit {
            if let Some(pos) = self.items.iter().position(|i| i.status.is_terminal()) {
                self.items.remove(pos);
            } else {
                self.items.remove(0);
            }
        }
    }

    /// 删除条目（仅终态可删，调用方保证）。
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.items.len();
        self.items.retain(|i| i.id != id);
        self.items.len() != before
    }

    /// 清除全部终态条目（Done/Failed/Canceled）。
    pub fn clear_terminal(&mut self) {
        self.items.retain(|i| !i.status.is_terminal());
    }

    /// 加载；文件缺失返回空历史；JSON 损坏备份为 `<原名>.corrupt-<ts>.json`。
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)?;
        // 剥 UTF-8 BOM：与 config 同一防御（记事本编辑后 serde 会误判损坏）
        let text = text
            .strip_prefix('\u{feff}')
            .map(str::to_string)
            .unwrap_or(text);
        match serde_json::from_str::<History>(&text) {
            Ok(mut h) => {
                // 文件里的 limit 不可信（手改/旧版本/损坏）：不夹的话列表会无界增长，
                // persist 的克隆与序列化成本随之爆炸
                h.limit = h
                    .limit
                    .clamp(1, crate::config::GeneralConfig::HISTORY_LIMIT_MAX);
                Ok(h)
            }
            Err(e) => {
                let backup = crate::paths::corrupt_backup_path(path);
                // 备份成败要如实说：文案写"已备份"而实际没备份，会把用户引向不存在的文件
                let note = match std::fs::copy(path, &backup) {
                    Ok(_) => format!("已备份到 {}", backup.display()),
                    Err(err) => format!("备份失败（{err}）：损坏内容仍留在原文件"),
                };
                Err(CoreError::ConfigCorrupt(format!(
                    "history.json 损坏（{}）：{}",
                    note, e
                )))
            }
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        // 紧凑格式：history.json 是机器读写的大文件（100 条 × 最多 300 行日志），
        // 缩进格式的体积与序列化耗时约为紧凑的 3 倍；config.json 保持缩进便于手改。
        atomic_write_json_with(path, self, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ItemKind, Status};
    use tempfile::tempdir;

    fn item(n: usize) -> MediaItem {
        MediaItem::new(ItemKind::LocalFile, format!("文件{}.mp4", n))
    }

    #[test]
    fn default_limit_100() {
        assert_eq!(History::default().limit, 100);
    }

    #[test]
    fn new_clamps_limit_to_200() {
        assert_eq!(History::new(999).limit, 200);
        assert_eq!(History::new(0).limit, 1);
        assert_eq!(History::new(150).limit, 150);
    }

    #[test]
    fn upsert_trims_oldest_terminal_first() {
        let mut h = History::new(3);
        let mut active = item(0);
        active.status = Status::Downloading;
        h.upsert(active);
        // 其余条目须为终态（MediaItem::new 默认 Probing 非终态），
        // 否则裁剪回退到"删最旧"会把活跃条目删掉
        for n in 1..=3 {
            let mut done = item(n);
            done.status = Status::Done;
            h.upsert(done);
        }
        // 超限：裁掉最旧的终态（文件1），保留活跃条目
        assert_eq!(h.len(), 3);
        assert!(h.items.iter().any(|i| i.status == Status::Downloading));
        assert!(h.items.iter().all(|i| i.title != "文件1.mp4"));
    }

    #[test]
    fn upsert_trims_oldest_when_all_active() {
        let mut h = History::new(2);
        for i in 0..3 {
            let mut it = item(i);
            it.status = Status::Downloading;
            h.upsert(it);
        }
        assert_eq!(h.len(), 2);
        assert!(h.items.iter().all(|i| i.status == Status::Downloading));
    }

    #[test]
    fn upsert_updates_by_id() {
        let mut h = History::new(10);
        let it = item(1);
        let id = it.id.clone();
        h.upsert(it);
        let mut it2 = item(2);
        it2.id = id.clone();
        it2.status = Status::Done;
        h.upsert(it2);
        assert_eq!(h.len(), 1);
        assert_eq!(h.get(&id).unwrap().status, Status::Done);
    }

    #[test]
    fn remove_returns_bool() {
        let mut h = History::new(10);
        let it = item(1);
        let id = it.id.clone();
        h.upsert(it);
        assert!(h.remove(&id));
        assert!(!h.remove(&id));
        assert!(h.is_empty());
    }

    #[test]
    fn clear_done_keeps_active() {
        let mut h = History::new(10);
        let mut done = item(1);
        done.status = Status::Done;
        let mut failed = item(2);
        failed.status = Status::Failed;
        let mut active = item(3);
        active.status = Status::Downloading;
        h.upsert(done);
        h.upsert(failed);
        h.upsert(active);
        h.clear_terminal();
        assert_eq!(h.len(), 1);
        assert_eq!(h.items[0].status, Status::Downloading);
    }

    #[test]
    fn load_missing_returns_empty() {
        let root = tempdir().unwrap();
        let h = History::load(&root.path().join("nope.json")).unwrap();
        assert!(h.is_empty());
    }

    #[test]
    fn save_load_roundtrip() {
        let root = tempdir().unwrap();
        let p = root.path().join("history.json");
        let mut h = History::new(100);
        h.upsert(item(1));
        h.save(&p).unwrap();
        let back = History::load(&p).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back.items[0].title, "文件1.mp4");
        // 紧凑格式（机器读写的大文件不必缩进）
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(!raw.contains("\n  "), "history.json 应为紧凑 JSON");
    }

    #[test]
    fn load_clamps_limit_from_file() {
        let root = tempdir().unwrap();
        let p = root.path().join("history.json");
        // 手改文件把 limit 拉到天文数字：加载时必须夹回上限，否则列表无界增长
        std::fs::write(&p, r#"{"items":[],"limit":100000000}"#).unwrap();
        let h = History::load(&p).unwrap();
        assert_eq!(h.limit, crate::config::GeneralConfig::HISTORY_LIMIT_MAX);
        std::fs::write(&p, r#"{"items":[],"limit":0}"#).unwrap();
        let h = History::load(&p).unwrap();
        assert_eq!(h.limit, 1);
    }

    #[test]
    fn concurrent_saves_never_produce_partial_json() {
        // P0 回归：并发 persist（多个任务线程各自收尾）不得互相截断临时文件。
        // 用多个线程同时写同一路径，最终文件必须是完整可解析的 JSON。
        let root = tempdir().unwrap();
        let p = root.path().join("history.json");
        let mut handles = Vec::new();
        for n in 0..8 {
            let p = p.clone();
            handles.push(std::thread::spawn(move || {
                let mut h = History::new(100);
                for i in 0..20 {
                    h.upsert(item(n * 100 + i));
                }
                h.save(&p).unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let back = History::load(&p).expect("并发写后文件必须仍是合法 JSON");
        assert_eq!(back.len(), 20);
    }

    #[test]
    fn corrupt_history_backed_up() {
        let root = tempdir().unwrap();
        let p = root.path().join("history.json");
        std::fs::write(&p, "boom").unwrap();
        assert!(History::load(&p).is_err());
        // 备份名统一为 <原名>.corrupt-<ts>.json（paths::corrupt_backup_path）
        let backed = std::fs::read_dir(root.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.starts_with("history.json.corrupt-") && n.ends_with(".json")
            });
        assert!(backed, "损坏历史应备份为 .corrupt-<ts>.json");
    }
}
