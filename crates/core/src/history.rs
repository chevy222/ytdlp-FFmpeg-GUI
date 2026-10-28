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
    #[serde(skip, default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    100
}

/// 逐条容错的恢复结果。
struct PartialRecovery {
    history: History,
    kept: usize,
    skipped: usize,
}

/// 从"整份反序列化失败"的 history.json 文本里逐条恢复可解析的条目。
///
/// 外层结构解析为 JSON Value，`items` 若存在则逐条 `from_value::<MediaItem>()`；
/// 好条目保留，坏条目跳过（不因单条坏字段丢弃整份列表）。结构与
/// `History` 不一致（如 items 缺失/非数组）时返回 None，由调用方走备份分支。
fn recover_partial_history(text: &str) -> Option<PartialRecovery> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let items = v.get("items")?.as_array()?;
    let mut kept = Vec::new();
    let mut skipped = 0usize;
    for entry in items {
        match serde_json::from_value::<MediaItem>(entry.clone()) {
            Ok(it) => kept.push(it),
            Err(_) => skipped += 1,
        }
    }
    let kept_count = kept.len();
    Some(PartialRecovery {
        history: History {
            items: kept,
            limit: default_limit(),
        },
        kept: kept_count,
        skipped,
    })
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
    /// （Done/Failed/Canceled）；若没有终态，则退而裁剪其他**非活跃**条目
    /// （Ready/NeedLogin/Unknown，不占用子进程、裁剪安全）；仅当全部都是
    /// 活跃任务（解析中/下载中/转码中/合并中）时才临时超限，
    /// 避免把仍在运行的条目裁掉（任务完成 upsert 会"复活"被裁条目，造成列表闪烁）。
    pub fn upsert(&mut self, item: MediaItem) {
        if let Some(existing) = self.items.iter_mut().find(|i| i.id == item.id) {
            *existing = item;
        } else {
            self.items.push(item);
        }
        while self.items.len() > self.limit {
            match self.oldest_trimmable() {
                Some(pos) => {
                    self.items.remove(pos);
                }
                None => {
                    // 全部都是活跃任务（运行中）：**宁可临时超限也不裁**。
                    // 否则正在下载/转码/合并的条目会被挤出列表 → update_item 返回
                    // None 静默 no-op → 产物路径永久丢失、取消按钮消失，而子进程
                    // 还在写盘。等有任务结束变非活跃后下次 upsert 自然裁剪。
                    crate::log::warn(format!(
                        "历史上限 {} 触发时全部条目均为活跃任务，临时保留 {} 条（不裁剪，避免丢失运行中任务）",
                        self.limit, self.items.len()
                    ));
                    break;
                }
            }
        }
    }

    /// 最旧的可裁剪条目下标：先在所有**终态**里取 `updated_at` 最小者；
    /// 没有终态时，在所有**非活跃**条目（Ready/NeedLogin/Unknown）里取最小的。
    /// 活跃任务（Probing/处理中）永不裁剪。
    ///
    /// 用 `updated_at` 而非插入顺序：长驻条目（早期插入、最近才完成）位置仍在队首，
    /// 按位置裁剪会优先删掉"刚完成的长任务"；按时间戳则删真正最旧的。
    /// 空 `updated_at`（旧版本条目）字典序最小，视为最旧优先裁。
    fn oldest_trimmable(&self) -> Option<usize> {
        let oldest = |only_terminal: bool| -> Option<usize> {
            self.items
                .iter()
                .enumerate()
                .filter(|(_, i)| {
                    if only_terminal {
                        i.status.is_terminal()
                    } else {
                        !i.status.is_active()
                    }
                })
                .min_by(|(_, a), (_, b)| a.updated_at.cmp(&b.updated_at))
                .map(|(idx, _)| idx)
        };
        oldest(true).or_else(|| oldest(false))
    }

    /// 删除条目（仅终态可删，调用方保证）。
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.items.len();
        self.items.retain(|i| i.id != id);
        self.items.len() != before
    }

    /// 清除全部非活跃条目（Ready/Done/Failed/Canceled/NeedLogin），
    /// 保留解析中和处理中的（Probing/Downloading/PostProcessing/Transcoding/Merging）。
    /// "清空列表"按钮用这个（只清终态会留下本地添加的 Ready 条目，与用户预期不符）。
    pub fn clear_idle(&mut self) {
        self.items.retain(|i| i.status.is_active());
    }

    /// 加载；文件缺失返回空历史；JSON 损坏备份为 `<原名>.corrupt-<ts>.json`。
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        // 字节读 + BOM/UTF-8/GBK 解码。`read_to_string` 遇到 GBK/UTF-16 会整个失败，
        // 上层回退空列表后启动落盘会覆盖原文件 —— 这类文件必须进入逐条恢复流程。
        let text = match crate::paths::read_text_file(path) {
            Ok(t) => t,
            Err(e) => {
                let backup = crate::paths::corrupt_backup_path(path);
                let note = match std::fs::copy(path, &backup) {
                    Ok(_) => format!("已备份到 {}", backup.display()),
                    Err(err) => format!("备份失败（{err}）：损坏内容仍留在原文件"),
                };
                return Err(CoreError::ConfigCorrupt(format!(
                    "history.json 无法读取（{e}）：{note}"
                )));
            }
        };
        match serde_json::from_str::<History>(&text) {
            Ok(mut h) => {
                // 文件里的 limit 不可信（手改/旧版本/损坏）：不夹的话列表会无界增长，
                // persist 的克隆与序列化成本随之爆炸
                h.limit = h
                    .limit
                    .clamp(1, crate::config::GeneralConfig::HISTORY_LIMIT_MAX);
                Ok(h)
            }
            Err(_) => {
                // D2：**不要一遇到单条坏字段就回退空列表**。history.json 是唯一持久层，
                // 任何一条条目坏（如 percent 被手改成 null、未来新增字段、某条 NaN）
                // 都让整份 JSON 反序列化失败 → 回退空 → 下一次 persist 覆盖真文件，
                // 用户全部历史与运行中任务状态消失。这里降级为逐条容错：
                // 坏条目跳过并计数，好条目保留。
                let recovered = recover_partial_history(&text);
                // 仅当**确实救回至少一条**时才按"部分恢复"返回；若 kept==0（每条都坏），
                // 不当作加载成功（否则 UI 直接空表却以为正常），落到下面的损坏分支。
                if let Some(partial) = recovered.filter(|p| p.kept > 0) {
                    let mut h = partial.history;
                    h.limit = h
                        .limit
                        .clamp(1, crate::config::GeneralConfig::HISTORY_LIMIT_MAX);
                    // 备份原文件，让用户能找回被跳过条目的数据。备份失败时不能
                    // 返回部分列表——上层启动落盘会用部分内容覆盖原文件，坏条目
                    // 就永久丢了；宁可本次不加载，交给启动保护逻辑保留原文件。
                    let backup = crate::paths::corrupt_backup_path(path);
                    if let Err(err) = std::fs::copy(path, &backup) {
                        return Err(CoreError::ConfigCorrupt(format!(
                            "history.json 存在无法解析的条目，且备份到 {} 失败（{err}）：为避免覆盖原文件，本次不加载",
                            backup.display()
                        )));
                    }
                    crate::log::warn(format!(
                        "history.json 存在无法解析的条目，已跳过 {} 条、保留 {} 条；原文件已备份到 {}",
                        partial.skipped,
                        partial.kept,
                        backup.display()
                    ));
                    return Ok(h);
                }
                let backup = crate::paths::corrupt_backup_path(path);
                // 备份成败要如实说：文案写"已备份"而实际没备份，会把用户引向不存在的文件
                let note = match std::fs::copy(path, &backup) {
                    Ok(_) => format!("已备份到 {}", backup.display()),
                    Err(err) => format!("备份失败（{err}）：损坏内容仍留在原文件"),
                };
                Err(CoreError::ConfigCorrupt(format!(
                    "history.json 损坏（{}）：{}",
                    note, "无法恢复任何条目"
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
    fn upsert_keeps_active_when_all_nonterminal() {
        // 全部是运行中/待命（非终态）时：宁可临时超限也不裁 —— 裁掉运行中条目
        // 会让 update_item 返回 None、产物路径永久丢失、取消按钮消失，而子进程
        // 还在写盘。等有任务变终态后自然裁剪。
        let mut h = History::new(2);
        for i in 0..3 {
            let mut it = item(i);
            it.status = Status::Downloading;
            h.upsert(it);
        }
        assert_eq!(h.len(), 3);
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
    fn clear_idle_keeps_active() {
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
        h.clear_idle();
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
    fn legacy_limit_is_not_a_second_config_source() {
        let root = tempdir().unwrap();
        let p = root.path().join("history.json");
        std::fs::write(&p, r#"{"items":[],"limit":100000000}"#).unwrap();
        assert_eq!(History::load(&p).unwrap().limit, default_limit());
        let h = History::new(42);
        h.save(&p).unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(!raw.contains("\"limit\""));
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
    fn gbk_history_loads() {
        let root = tempdir().unwrap();
        let p = root.path().join("history.json");
        let text = r#"{"items":[{"id":"x","kind":"LocalFile","title":"测试视频","status":"Ready"}],"limit":100}"#;
        let (gbk, _, _) = encoding_rs::GBK.encode(text);
        std::fs::write(&p, gbk.as_ref()).unwrap();
        let h = History::load(&p).unwrap();
        assert_eq!(h.items[0].title, "测试视频");
    }

    #[test]
    fn partial_history_keeps_good_items_and_backs_up() {
        let root = tempdir().unwrap();
        let p = root.path().join("history.json");
        // 第二条缺 status：逐条恢复应保留第一条，并备份原文件
        std::fs::write(
            &p,
            r#"{"items":[{"id":"ok","kind":"LocalFile","title":"好条目","status":"Ready"},{"id":"bad","kind":"LocalFile","title":"坏条目"}],"limit":100}"#,
        )
        .unwrap();
        let h = History::load(&p).unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h.items[0].title, "好条目");
        assert!(std::fs::read_dir(root.path())
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains("corrupt")));
    }

    #[test]
    fn invalid_bytes_history_backed_up_before_error() {
        let root = tempdir().unwrap();
        let p = root.path().join("history.json");
        std::fs::write(&p, [0x81u8]).unwrap();
        assert!(History::load(&p).is_err());
        let backups: Vec<_> = std::fs::read_dir(root.path())
            .unwrap()
            .flatten()
            .filter(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.starts_with("history.json.corrupt-")
            })
            .collect();
        assert_eq!(backups.len(), 1, "读取/解码失败也必须先备份");
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
