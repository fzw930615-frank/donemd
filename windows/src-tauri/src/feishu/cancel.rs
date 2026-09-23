//! 推送 / 拉取管线的协作式取消信号 — `FeishuSyncCancellation.swift` 的移植。
//!
//! Swift 侧是 `NSLock` + bool 的 `final class`(引用语义,协调器与 UI
//! 共享同一实例;协调器在每个安全检查点轮询,UI 按钮写一次)。Rust 用
//! `Arc<AtomicBool>` 直接对等:`Clone` 共享同一标志位,无需锁,语义
//! 完全一致且更轻。
//!
//! 为什么不用 `tokio::select!` + `Notify`(AI 流式取消走的那条路):
//! 那条适合「一个长 await 被打断」,而同步管线要的是「在若干个安全
//! 检查点主动询问」—— 拉取要在 API 调用前、转换前、图片阶段结束后
//! 各判一次,轮询式标志位才是合适形状。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// 协作式取消标志。`Clone` 出来的副本与原件共享同一状态。
#[derive(Debug, Clone, Default)]
pub struct CancellationSignal {
    flag: Arc<AtomicBool>,
}

impl CancellationSignal {
    pub fn new() -> Self {
        Self::default()
    }

    /// 是否已被取消。协调器在每个安全检查点调用。
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// 置取消。幂等 —— UI 可以绑在用户能连点的按钮上。
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_uncancelled() {
        assert!(!CancellationSignal::new().is_cancelled());
        assert!(!CancellationSignal::default().is_cancelled());
    }

    #[test]
    fn cancel_is_idempotent() {
        let signal = CancellationSignal::new();
        signal.cancel();
        assert!(signal.is_cancelled());
        // 连点两次仍是已取消,不 panic 也不翻转。
        signal.cancel();
        assert!(signal.is_cancelled());
    }

    /// 克隆共享状态 —— UI 持一份、协调器持一份,取消必须互见
    /// (Swift 那边是 class 的引用语义,这里靠 Arc 复现)。
    #[test]
    fn clone_shares_state() {
        let ui_side = CancellationSignal::new();
        let coordinator_side = ui_side.clone();
        assert!(!coordinator_side.is_cancelled());
        ui_side.cancel();
        assert!(coordinator_side.is_cancelled(), "克隆体必须看见取消");
    }
}
