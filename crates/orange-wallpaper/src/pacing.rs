//! 帧率档位调度（§3 A-1：30 / 60 / 120 / 无限，双缓冲 + VSync）。
//!
//! 纯逻辑、跨平台可测：视频模式下渲染循环在消息泵之间用它决定
//! 「现在是否该解码并提交下一帧」，避免空转轮询（P4）。

use std::time::{Duration, Instant};

pub struct FramePacer {
    /// 帧间隔；`Duration::ZERO` 表示 Unlimited（仅 VSync 对齐）
    interval: Duration,
    last_present: Option<Instant>,
}

impl FramePacer {
    pub fn new(frame_interval_ms: u64) -> Self {
        Self {
            interval: Duration::from_millis(frame_interval_ms),
            last_present: None,
        }
    }

    /// 此刻是否应出帧（首次恒真；Unlimited 恒真）
    pub fn should_present(&self, now: Instant) -> bool {
        match self.last_present {
            None => true,
            Some(last) => now.duration_since(last) >= self.interval,
        }
    }

    /// 标记已完成一次提交
    pub fn mark_presented(&mut self, now: Instant) {
        self.last_present = Some(now);
    }

    /// 距下一次到期的剩余时间（消息循环 sleep 用；Unlimited / 首帧为 0）
    pub fn until_next(&self, now: Instant) -> Duration {
        match self.last_present {
            None => Duration::ZERO,
            Some(last) => self.interval.saturating_sub(now.duration_since(last)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_tier_always_due() {
        let mut pacer = FramePacer::new(0);
        let start = Instant::now();
        assert!(pacer.should_present(start));
        pacer.mark_presented(start);
        assert!(pacer.should_present(start));
        assert_eq!(pacer.until_next(start), Duration::ZERO);
    }

    #[test]
    fn tier_throttles_before_interval() {
        let mut pacer = FramePacer::new(16);
        let start = Instant::now();
        assert!(pacer.should_present(start), "first frame is due");
        pacer.mark_presented(start);

        let early = start + Duration::from_millis(8);
        assert!(!pacer.should_present(early), "8ms < 16ms interval");
        assert_eq!(pacer.until_next(early), Duration::from_millis(8));

        let due = start + Duration::from_millis(16);
        assert!(pacer.should_present(due));
        assert_eq!(pacer.until_next(due), Duration::ZERO);
    }

    #[test]
    fn overdue_frame_is_due_immediately() {
        let mut pacer = FramePacer::new(33);
        let start = Instant::now();
        pacer.mark_presented(start);
        let late = start + Duration::from_millis(100);
        assert!(pacer.should_present(late));
        assert_eq!(pacer.until_next(late), Duration::ZERO);
    }
}
