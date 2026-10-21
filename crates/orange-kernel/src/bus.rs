//! 生命周期事件总线（P4：事件驱动纪律全局化，禁止高频轮询）。
//!
//! 单生产者多订阅中的最简形态：平台事件源与内核自身向总线注入事件，
//! supervisor 的单一分发线程消费并同步推给各模块，保证 <10ms 级响应
//! 且模块看到的事件顺序全局一致。

use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

use orange_module_api::KernelEvent;

/// 事件发布端：平台事件源 / 演示注入器 / 模块间解耦的事件入口
#[derive(Clone)]
pub struct EventPublisher {
    tx: Sender<KernelEvent>,
}

impl EventPublisher {
    /// 向总线注入事件。总线已关闭（进程退出中）时静默丢弃并记录。
    pub fn publish(&self, event: KernelEvent) {
        if let Err(err) = self.tx.send(event.clone()) {
            log::debug!("event bus closed, dropping event {event:?}: {err}");
        }
    }
}

/// 生命周期事件总线。
pub struct EventBus {
    tx: Sender<KernelEvent>,
    rx: Receiver<KernelEvent>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        Self { tx, rx }
    }

    /// 创建新的发布端（可克隆，分发给事件源线程）
    pub fn publisher(&self) -> EventPublisher {
        EventPublisher {
            tx: self.tx.clone(),
        }
    }

    /// 阻塞等待下一个事件。
    /// 返回 `None` 表示本轮超时（supervisor 借此周期检查停机标志）。
    pub fn recv_timeout(&self, timeout: Duration) -> Option<KernelEvent> {
        self.rx.recv_timeout(timeout).ok()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_flow_from_publisher_to_bus() {
        let bus = EventBus::new();
        let publisher = bus.publisher();
        publisher.publish(KernelEvent::ShellRestarted);
        let received = bus.recv_timeout(Duration::from_millis(100));
        assert_eq!(received, Some(KernelEvent::ShellRestarted));
    }

    #[test]
    fn recv_timeout_returns_none_when_idle() {
        let bus = EventBus::new();
        assert!(bus.recv_timeout(Duration::from_millis(10)).is_none());
    }

    #[test]
    fn multiple_publishers_share_one_bus() {
        let bus = EventBus::new();
        let a = bus.publisher();
        let b = bus.publisher();
        a.publish(KernelEvent::LockScreenChanged { locked: true });
        b.publish(KernelEvent::LockScreenChanged { locked: false });
        assert_eq!(
            bus.recv_timeout(Duration::from_millis(50)),
            Some(KernelEvent::LockScreenChanged { locked: true })
        );
        assert_eq!(
            bus.recv_timeout(Duration::from_millis(50)),
            Some(KernelEvent::LockScreenChanged { locked: false })
        );
    }
}
