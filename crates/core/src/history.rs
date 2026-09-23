//! history.json：统一列表/队列/历史持久化（§3.1 UL-07/UL-10、§7.2）。
//!
//! 规则：MediaItem 序列化；保留条数默认 100、上限 200（§6 统一）；
//! JSON 原子写；重启恢复未完成任务（可继续/标记失败），已完成项保留。

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::model::MediaItem;
use crate::{paths::atomic_write_json, CoreError, Result};

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

    /// 删除条目（仅终态可删，调用方保证）。返回**被删掉的条目 id**，
    /// 调用方据此清理它们在 config/cache 里的缩略图，否则缓存只增不减。
    pub fn remove(&mut self, id: &str) -> Vec<String> {
        let before = self.items.len();
        let removed: Vec<String> = self
            .items
            .iter()
            .filter(|i| i.id == id)
            .map(|i| i.id.clone())
            .collect();
        self.items.retain(|i| i.id != id);
        if self.items.len() == before {
            Vec::new()
        } else {
            removed
        }
    }

    /// 清除全部终态条目（Done/Failed/Canceled）。返回被删掉的 id（同上）。
    pub fn clear_terminal(&mut self) -> Vec<String> {
        let removed: Vec<String> = self
            .items
            .iter()
            .filter(|i| i.status.is_terminal())
            .map(|i| i.id.clone())
            .collect();
        self.items.retain(|i| !i.status.is_terminal());
        removed
    }

    /// 加载；文件缺失返回空历史；JSON 损坏备份为 `<原名>.corrupt-<ts>.json`。
    ///
    /// **逐条救回**：旧实现整份 `from_str::<History>`，只要有一条条目字段缺失
    /// 或是新版本写入的未知状态，整份 history.json 就判为损坏 → 上层回退空列表
    /// → 第一次 persist 就把用户的整个列表覆盖掉。坏一条只丢一条。
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        const MAX_HISTORY_BYTES: u64 = 32 * 1024 * 1024;
        if let Ok(md) = std::fs::metadata(path) {
            if md.len() > MAX_HISTORY_BYTES {
                return Err(CoreError::ConfigCorrupt(format!(
                    "history.json 异常过大（{} 字节），拒绝读取：{}",
                    md.len(),
                    path.display()
                )));
            }
        }
        let text = std::fs::read_to_string(path)?;
        // 剥 UTF-8 BOM：与 config 同一防御（记事本编辑后 serde 会误判损坏）
        let text = text
            .strip_prefix('\u{feff}')
            .map(str::to_string)
            .unwrap_or(text);
        let doc: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                let backup = crate::paths::corrupt_backup_path(path);
                let _ = std::fs::copy(path, &backup);
                return Err(CoreError::ConfigCorrupt(format!(
                    "history.json 损坏（已备份到 {}）：{}",
                    backup.display(),
                    e
                )));
            }
        };
        let limit = doc
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(100)
            .clamp(1, crate::config::GeneralConfig::HISTORY_LIMIT_MAX);
        let empty = Vec::new();
        let raw_items = doc.get("items").and_then(|v| v.as_array()).unwrap_or(&empty);
        let mut items = Vec::with_capacity(raw_items.len());
        let mut dropped = 0usize;
        for v in raw_items {
            match serde_json::from_value::<MediaItem>(v.clone()) {
                Ok(item) => items.push(item),
                Err(_) => dropped += 1,
            }
        }
        if dropped > 0 {
            eprintln!("history.json 有 {dropped} 条无法解析的条目已跳过（其余 {}/{} 条已保留）", items.len(), raw_items.len());
        }
        Ok(Self { items, limit })
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        atomic_write_json(path, self)
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
    fn remove_returns_removed_ids_for_cache_cleanup() {
        let mut h = History::new(10);
        let it = item(1);
        let id = it.id.clone();
        h.upsert(it);
        assert_eq!(h.remove(&id), vec![id.clone()], "调用方要靠这个 id 删缩略图缓存");
        assert!(h.remove(&id).is_empty());
        assert!(h.is_empty());
    }

    #[test]
    fn load_salvages_good_items_when_one_is_broken() {
        // 一条坏数据不能毁掉整份列表：旧实现整档 from_str 失败 → 上层回退空列表
        // → 第一次 persist 就把用户全部历史覆盖掉
        let root = tempdir().unwrap();
        let p = root.path().join("history.json");
        let mut h = History::new(100);
        let a = item(1);
        let b = item(2);
        let (id_a, id_b) = (a.id.clone(), b.id.clone());
        h.upsert(a);
        h.upsert(b);
        let mut doc: serde_json::Value = serde_json::to_value(&h).unwrap();
        // 中间塞一条新版本才有的状态 / 缺字段的条目
        doc["items"][0]["status"] = serde_json::json!("SomeFutureState");
        doc["items"].as_array_mut().unwrap().push(serde_json::json!({"id": "no-title"}));
        std::fs::write(&p, serde_json::to_string_pretty(&doc).unwrap()).unwrap();

        let back = History::load(&p).unwrap();
        assert_eq!(back.len(), 1, "坏一条只丢一条");
        assert!(back.get(&id_a).is_some() ^ back.get(&id_b).is_some());
    }

    #[test]
    fn load_clamps_absurd_limit() {
        let root = tempdir().unwrap();
        let p = root.path().join("history.json");
        std::fs::write(
            &p,
            r#"{"items": [], "limit": 4294967295}"#,
        )
        .unwrap();
        let h = History::load(&p).unwrap();
        assert_eq!(h.limit, crate::config::GeneralConfig::HISTORY_LIMIT_MAX);
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
