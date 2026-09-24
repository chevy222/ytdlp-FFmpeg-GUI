//! 任务并发调度（UL-06：并发 worker，全局并行默认 3，下载/转码/合并共享）。
//!
//! 纯逻辑：`submit` 返回 StartNow（有 slot 空闲）或 Queued；`finish` 释放 slot 并
//! 弹出下一个等待任务。执行体（子进程）由应用层驱动，本模块只管调度账本，可单测。

use std::collections::{HashSet, VecDeque};

/// 提交结果：立即启动或进入等待。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// 有并发 slot，调用方应立刻启动任务
    StartNow,
    /// 已排队，等待 slot 释放
    Queued,
}

/// 并发任务队列（按 id 管理；同一任务重复提交不重复排队）。
#[derive(Debug, Clone)]
pub struct TaskQueue {
    concurrency: usize,
    running: HashSet<String>,
    waiting: VecDeque<String>,
}

impl TaskQueue {
    pub fn new(concurrency: usize) -> Self {
        Self {
            concurrency: concurrency.max(1),
            running: HashSet::new(),
            waiting: VecDeque::new(),
        }
    }

    /// 更新并发上限。返回本次提升后应**立即启动**的等待任务（A4：调高并发时，
    /// 若已有任务排队，必须马上放行，而不是等某个任务自然结束才弹队）。
    pub fn set_concurrency(&mut self, c: usize) -> Vec<String> {
        let new = c.max(1);
        let grown = new > self.concurrency;
        self.concurrency = new;
        if !grown {
            return Vec::new();
        }
        let mut to_launch = Vec::new();
        while self.running.len() < self.concurrency {
            let Some(next) = self.waiting.pop_front() else {
                break;
            };
            self.running.insert(next.clone());
            to_launch.push(next);
        }
        to_launch
    }

    /// 提交任务。返回 StartNow 时该任务已占用 slot。
    pub fn submit(&mut self, id: &str) -> SubmitOutcome {
        if self.running.contains(id) || self.waiting.iter().any(|w| w == id) {
            return SubmitOutcome::Queued;
        }
        if self.running.len() < self.concurrency {
            self.running.insert(id.to_string());
            SubmitOutcome::StartNow
        } else {
            self.waiting.push_back(id.to_string());
            SubmitOutcome::Queued
        }
    }

    /// 任务结束（无论成败）：释放 slot，返回下一个应启动的等待任务（若有）。
    pub fn finish(&mut self, id: &str) -> Option<String> {
        self.running.remove(id);
        if let Some(next) = self.waiting.pop_front() {
            if self.running.len() < self.concurrency {
                self.running.insert(next.clone());
                return Some(next);
            }
            // 理论不可达（slot 刚释放），保险放回队首
            self.waiting.push_front(next);
        }
        None
    }

    /// 取消：仅从等待队列移除（运行中的由调用方 kill 进程）。
    pub fn cancel_waiting(&mut self, id: &str) -> bool {
        let before = self.waiting.len();
        self.waiting.retain(|w| w != id);
        self.waiting.len() != before
    }

    pub fn running_count(&self) -> usize {
        self.running.len()
    }

    /// 当前并发上限（UI 展示"运行中 x/y"用）。
    pub fn concurrency(&self) -> usize {
        self.concurrency
    }

    pub fn waiting_count(&self) -> usize {
        self.waiting.len()
    }

    pub fn is_running(&self, id: &str) -> bool {
        self.running.contains(id)
    }

    pub fn is_queued(&self, id: &str) -> bool {
        self.waiting.iter().any(|w| w == id)
    }

    /// 当前占用总和（running + waiting）。
    pub fn total(&self) -> usize {
        self.running.len() + self.waiting.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_uses_slots() {
        let mut q = TaskQueue::new(2);
        assert_eq!(q.submit("a"), SubmitOutcome::StartNow);
        assert_eq!(q.submit("b"), SubmitOutcome::StartNow);
        assert_eq!(q.submit("c"), SubmitOutcome::Queued);
        assert_eq!(q.running_count(), 2);
        assert_eq!(q.waiting_count(), 1);
    }

    #[test]
    fn finish_releases_slot_to_next() {
        let mut q = TaskQueue::new(2);
        q.submit("a");
        q.submit("b");
        q.submit("c");
        let next = q.finish("a");
        assert_eq!(next.as_deref(), Some("c"));
        assert!(q.is_running("c"));
        assert_eq!(q.running_count(), 2);
        assert_eq!(q.waiting_count(), 0);
    }

    #[test]
    fn duplicate_submit_no_op() {
        let mut q = TaskQueue::new(1);
        assert_eq!(q.submit("a"), SubmitOutcome::StartNow);
        assert_eq!(q.submit("a"), SubmitOutcome::Queued);
        assert_eq!(q.running_count(), 1);
    }

    #[test]
    fn cancel_waiting_removes() {
        let mut q = TaskQueue::new(1);
        q.submit("a");
        q.submit("b");
        assert!(q.cancel_waiting("b"));
        assert!(!q.cancel_waiting("b"));
        assert_eq!(q.waiting_count(), 0);
    }

    #[test]
    fn concurrency_update() {
        let mut q = TaskQueue::new(1);
        q.submit("a");
        q.submit("b");
        q.submit("c");
        // 调到 2：应放行 1 个等待任务（b），c 继续排队
        let launched = q.set_concurrency(2);
        assert_eq!(launched, vec!["b".to_string()]);
        assert_eq!(q.running_count(), 2);
        assert_eq!(q.waiting_count(), 1);
        assert!(q.is_running("b"));
        // 再调到 3：放行 c
        let launched = q.set_concurrency(3);
        assert_eq!(launched, vec!["c".to_string()]);
        assert_eq!(q.running_count(), 3);
        assert_eq!(q.waiting_count(), 0);
    }

    #[test]
    fn concurrency_shrink_no_launch() {
        let mut q = TaskQueue::new(4);
        q.submit("a");
        q.submit("b");
        // 调低/持平：不放行任何任务
        assert!(q.set_concurrency(2).is_empty());
        assert!(q.set_concurrency(3).is_empty());
        assert_eq!(q.running_count(), 2);
    }

    #[test]
    fn concurrency_clamps_to_one() {
        let q = TaskQueue::new(0);
        assert_eq!(q.running_count() + q.waiting_count(), 0);
        // 内部 clamp 到 1，不影响对外计数
        let mut q = TaskQueue::new(0);
        assert_eq!(q.submit("x"), SubmitOutcome::StartNow);
    }
}
