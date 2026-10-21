//! EDF 调度器（§3 B-1：内核调度器按最早截止时间统一编排唤醒）。
//!
//! 纯逻辑、跨平台可测。核心语义：
//! - 每组件独立周期（`insert` 时声明），到期批次 [`pop_due`] 返回并**立即重排**
//!   下次截止——组件 update 完成即排下次，天然无漂移累积
//! - 遮挡 / 全局暂停期间调度器不被驱动（宿主不调用 `pop_due`）；
//!   恢复时错过的截止一次性全部到期——立即刷新一轮数据后回到正常节奏
//!   （满足「遮挡结束立刻拿到最新值」）
//! - 错过多个周期不会补跑多次（每组件最多一个待办截止）

use std::collections::BTreeMap;

/// 单组件的调度条目
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduleEntry {
    pub interval_ms: u64,
    pub next_deadline_ms: u64,
}

/// EDF 周期任务调度器（以毫秒单调时钟为时间轴）
#[derive(Debug, Default, PartialEq)]
pub struct WidgetScheduler {
    entries: BTreeMap<u64, ScheduleEntry>,
    next_seq: u64,
}

impl WidgetScheduler {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册 / 重设组件周期（首个截止 = now + interval）
    pub fn insert(&mut self, interval_ms: u64, now_ms: u64) -> u64 {
        let id = self.next_seq;
        self.next_seq += 1;
        self.entries.insert(
            id,
            ScheduleEntry {
                interval_ms: interval_ms.max(1),
                next_deadline_ms: now_ms.saturating_add(interval_ms.max(1)),
            },
        );
        id
    }

    /// 移除组件（崩溃隔离时使用）
    pub fn remove(&mut self, id: u64) -> bool {
        self.entries.remove(&id).is_some()
    }

    /// 组件数
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 最近的截止时刻（宿主线程 sleep 计算用）
    pub fn next_deadline_ms(&self) -> Option<u64> {
        self.entries.values().map(|e| e.next_deadline_ms).min()
    }

    /// 取出所有已到期组件并重排它们的下次截止。
    /// 返回值按截止时间升序（EDF 语义：最早截止先执行）。
    ///
    /// 重排规则：`deadline + interval`；若仍落后于当前时刻（暂停 / 遮挡
    /// 恢复后的追赶），则直接跳到 `now + interval`——每组件每轮最多
    /// 唤醒一次，追赶一两轮后回到正常节奏，不做无意义的连续补跑。
    pub fn pop_due(&mut self, now_ms: u64) -> Vec<u64> {
        let mut due: Vec<(u64, u64)> = self
            .entries
            .iter()
            .filter(|(_, e)| e.next_deadline_ms <= now_ms)
            .map(|(id, e)| (*id, e.next_deadline_ms))
            .collect();
        // 按 (deadline, id) 升序 = 最早截止优先
        due.sort_by_key(|(id, deadline)| (*deadline, *id));
        let ids: Vec<u64> = due.iter().map(|(id, _)| *id).collect();
        for (id, deadline) in due {
            if let Some(entry) = self.entries.get_mut(&id) {
                let mut next = deadline.saturating_add(entry.interval_ms);
                if next <= now_ms {
                    next = now_ms.saturating_add(entry.interval_ms);
                }
                entry.next_deadline_ms = next;
            }
        }
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_intervals_fire_independently() {
        // 文档 B-1 示例频率：时钟 1s、系统监控 2s——禁止全局统一节拍
        let mut sched = WidgetScheduler::new();
        let clock = sched.insert(1000, 0);
        let sysmon = sched.insert(2000, 0);
        assert_eq!(sched.next_deadline_ms(), Some(1000));

        // t=1000：只有时钟到期
        assert_eq!(sched.pop_due(1000), vec![clock]);
        // 时钟重排到 2000，与 sysmon 同刻
        assert_eq!(sched.next_deadline_ms(), Some(2000));
        // t=2000：两者都到期，按截止+id 升序
        let batch = sched.pop_due(2000);
        assert_eq!(batch.len(), 2);
        assert!(batch.contains(&clock) && batch.contains(&sysmon));
        // t=3000：时钟第 3 次，sysmon 未到
        assert_eq!(sched.pop_due(3000), vec![clock]);
    }

    #[test]
    fn missed_deadlines_catch_up_once_not_many() {
        let mut sched = WidgetScheduler::new();
        let clock = sched.insert(1000, 0);
        assert_eq!(sched.pop_due(1000), vec![clock]);
        // 暂停 5.5 秒后恢复（t=6500）：只补一次（不补跑 5 次），
        // 且追赶快照后跳回当前节奏（下次 = now + interval = 7500）
        assert_eq!(sched.pop_due(6500), vec![clock]);
        assert_eq!(sched.next_deadline_ms(), Some(7500));
    }

    #[test]
    fn remove_stops_scheduling() {
        let mut sched = WidgetScheduler::new();
        let id = sched.insert(100, 0);
        assert!(sched.remove(id));
        assert!(!sched.remove(id));
        assert_eq!(sched.pop_due(1000), Vec::<u64>::new());
        assert_eq!(sched.next_deadline_ms(), None);
    }

    #[test]
    fn edf_orders_by_earliest_deadline() {
        let mut sched = WidgetScheduler::new();
        let slow = sched.insert(10_000, 0);
        let fast = sched.insert(1000, 500); // deadline 1500
                                            // t=1500：fast 到期；slow（deadline 10000）未到
        assert_eq!(sched.pop_due(1500), vec![fast]);
        // t=10000：fast 的 deadline 已滚到 2500（早于 slow 的 10000）——
        // EDF 批内按截止升序：fast 在前
        assert_eq!(sched.pop_due(10_000), vec![fast, slow]);
    }

    #[test]
    fn interval_clamped_to_minimum_one_ms() {
        let mut sched = WidgetScheduler::new();
        sched.insert(0, 0);
        assert_eq!(sched.next_deadline_ms(), Some(1));
    }
}
