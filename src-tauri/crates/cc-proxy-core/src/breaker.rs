//! 按上游计数的熔断器（设计文档 §5.5）。
//!
//! 语义与桌面端现有代理一致：
//! - `closed`：连续失败达到阈值 → `open`；成功清零。
//! - `open`：经过 `open_duration` → `half_open`。
//! - `half_open`：只放行一个探测请求；成功 → `closed`，失败 → `open`。
//!
//! 时间由调用方传入（`now`），不依赖异步运行时，便于测试。

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::BreakerConfig;

/// 对外展示的状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    Closed,
    Open,
    HalfOpen,
}

/// 放行许可。拿到许可的请求结束后必须调用 `record_success` / `record_failure` / `release`
/// 之一，否则半开状态下的探测名额不会归还。
#[must_use = "请求结束后必须把许可交回熔断器"]
#[derive(Debug, PartialEq, Eq)]
pub enum Permit {
    /// 正常放行
    Normal,
    /// 半开状态下的唯一探测请求
    Probe,
}

#[derive(Debug)]
struct Inner {
    state: BreakerState,
    consecutive_failures: u32,
    opened_at: Option<Instant>,
    probe_in_flight: bool,
}

#[derive(Debug)]
pub struct CircuitBreaker {
    failure_threshold: u32,
    open_duration: Duration,
    inner: Mutex<Inner>,
}

impl CircuitBreaker {
    pub fn new(failure_threshold: u32, open_duration: Duration) -> Self {
        Self {
            failure_threshold: failure_threshold.max(1),
            open_duration,
            inner: Mutex::new(Inner {
                state: BreakerState::Closed,
                consecutive_failures: 0,
                opened_at: None,
                probe_in_flight: false,
            }),
        }
    }

    pub fn from_config(config: &BreakerConfig) -> Self {
        Self::new(
            config.failure_threshold,
            Duration::from_secs(config.open_secs),
        )
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // 状态机本身不会 panic；即使锁被污染，内部数据仍然一致，继续使用
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 当前状态；`open` 已到期时报告为 `half_open`。
    pub fn state(&self, now: Instant) -> BreakerState {
        let mut inner = self.lock();
        self.refresh(&mut inner, now);
        inner.state
    }

    /// 此刻 `try_acquire` 是否会放行（不占用许可，只用于判断是否还有后续上游可试）。
    pub fn is_available(&self, now: Instant) -> bool {
        let mut inner = self.lock();
        self.refresh(&mut inner, now);
        match inner.state {
            BreakerState::Closed => true,
            BreakerState::HalfOpen => !inner.probe_in_flight,
            BreakerState::Open => false,
        }
    }

    /// 请求放行：`closed` 总是放行；`half_open` 只放行一个探测；`open` 拒绝。
    pub fn try_acquire(&self, now: Instant) -> Option<Permit> {
        let mut inner = self.lock();
        self.refresh(&mut inner, now);
        match inner.state {
            BreakerState::Closed => Some(Permit::Normal),
            BreakerState::HalfOpen if !inner.probe_in_flight => {
                inner.probe_in_flight = true;
                Some(Permit::Probe)
            }
            BreakerState::HalfOpen | BreakerState::Open => None,
        }
    }

    pub fn record_success(&self, permit: Permit) {
        let mut inner = self.lock();
        match permit {
            Permit::Probe => {
                inner.state = BreakerState::Closed;
                inner.consecutive_failures = 0;
                inner.opened_at = None;
                inner.probe_in_flight = false;
            }
            Permit::Normal if inner.state == BreakerState::Closed => {
                inner.consecutive_failures = 0;
            }
            // 放行后熔断器已被其它请求打开：这次成功不改变状态，等待探测结果
            Permit::Normal => {}
        }
    }

    pub fn record_failure(&self, permit: Permit, now: Instant) {
        let mut inner = self.lock();
        match permit {
            Permit::Probe => {
                inner.state = BreakerState::Open;
                inner.opened_at = Some(now);
                inner.probe_in_flight = false;
            }
            Permit::Normal if inner.state == BreakerState::Closed => {
                inner.consecutive_failures = inner.consecutive_failures.saturating_add(1);
                if inner.consecutive_failures >= self.failure_threshold {
                    inner.state = BreakerState::Open;
                    inner.opened_at = Some(now);
                }
            }
            // 已经打开：不重复计数，也不延长打开时间
            Permit::Normal => {}
        }
    }

    /// 请求没有产生可归因的结果（例如客户端提前断开）时归还许可，不计成功也不计失败。
    pub fn release(&self, permit: Permit) {
        if permit == Permit::Probe {
            self.lock().probe_in_flight = false;
        }
    }

    fn refresh(&self, inner: &mut Inner, now: Instant) {
        if inner.state == BreakerState::Open {
            let expired = inner
                .opened_at
                .is_none_or(|at| now.saturating_duration_since(at) >= self.open_duration);
            if expired {
                inner.state = BreakerState::HalfOpen;
                inner.probe_in_flight = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPEN: Duration = Duration::from_secs(60);

    fn breaker() -> CircuitBreaker {
        CircuitBreaker::new(3, OPEN)
    }

    fn fail(b: &CircuitBreaker, now: Instant) {
        let permit = b.try_acquire(now).expect("should be allowed");
        b.record_failure(permit, now);
    }

    #[test]
    fn opens_after_consecutive_failures() {
        let b = breaker();
        let t0 = Instant::now();
        fail(&b, t0);
        fail(&b, t0);
        assert_eq!(b.state(t0), BreakerState::Closed);
        fail(&b, t0);
        assert_eq!(b.state(t0), BreakerState::Open);
        assert_eq!(b.try_acquire(t0), None);
    }

    #[test]
    fn success_resets_failure_count() {
        let b = breaker();
        let t0 = Instant::now();
        fail(&b, t0);
        fail(&b, t0);
        let permit = b.try_acquire(t0).unwrap();
        b.record_success(permit);
        fail(&b, t0);
        fail(&b, t0);
        assert_eq!(b.state(t0), BreakerState::Closed);
    }

    #[test]
    fn half_open_allows_a_single_probe() {
        let b = breaker();
        let t0 = Instant::now();
        for _ in 0..3 {
            fail(&b, t0);
        }
        let later = t0 + OPEN;
        assert_eq!(b.state(later), BreakerState::HalfOpen);
        let probe = b.try_acquire(later).unwrap();
        assert_eq!(probe, Permit::Probe);
        assert_eq!(
            b.try_acquire(later),
            None,
            "second concurrent probe must wait"
        );

        b.record_success(probe);
        assert_eq!(b.state(later), BreakerState::Closed);
        assert_eq!(b.try_acquire(later), Some(Permit::Normal));
    }

    #[test]
    fn failed_probe_reopens_for_a_full_period() {
        let b = breaker();
        let t0 = Instant::now();
        for _ in 0..3 {
            fail(&b, t0);
        }
        let t1 = t0 + OPEN;
        let probe = b.try_acquire(t1).unwrap();
        b.record_failure(probe, t1);
        assert_eq!(
            b.state(t1 + OPEN - Duration::from_secs(1)),
            BreakerState::Open
        );
        assert_eq!(b.state(t1 + OPEN), BreakerState::HalfOpen);
    }

    #[test]
    fn released_probe_frees_the_slot() {
        let b = breaker();
        let t0 = Instant::now();
        for _ in 0..3 {
            fail(&b, t0);
        }
        let t1 = t0 + OPEN;
        let probe = b.try_acquire(t1).unwrap();
        b.release(probe);
        assert_eq!(b.state(t1), BreakerState::HalfOpen);
        assert_eq!(b.try_acquire(t1), Some(Permit::Probe));
    }

    #[test]
    fn late_results_of_normal_permits_do_not_change_open_state() {
        let b = breaker();
        let t0 = Instant::now();
        let late_ok = b.try_acquire(t0).unwrap();
        let late_fail = b.try_acquire(t0).unwrap();
        for _ in 0..3 {
            fail(&b, t0);
        }
        b.record_success(late_ok);
        assert_eq!(b.state(t0), BreakerState::Open);
        b.record_failure(late_fail, t0 + Duration::from_secs(30));
        // 打开时间没有被延长
        assert_eq!(b.state(t0 + OPEN), BreakerState::HalfOpen);
    }

    #[test]
    fn concurrent_acquire_in_half_open_grants_exactly_one_probe() {
        use std::sync::Arc;

        let b = Arc::new(breaker());
        let t0 = Instant::now();
        for _ in 0..3 {
            fail(&b, t0);
        }
        let t1 = t0 + OPEN;
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let b = Arc::clone(&b);
                std::thread::spawn(move || b.try_acquire(t1))
            })
            .collect();
        let granted: Vec<Permit> = handles
            .into_iter()
            .filter_map(|h| h.join().unwrap())
            .collect();
        assert_eq!(granted, vec![Permit::Probe]);
    }

    #[test]
    fn availability_matches_acquire_without_taking_a_permit() {
        let b = breaker();
        let t0 = Instant::now();
        assert!(b.is_available(t0));
        for _ in 0..3 {
            fail(&b, t0);
        }
        assert!(!b.is_available(t0));
        let t1 = t0 + OPEN;
        assert!(b.is_available(t1));
        assert!(b.is_available(t1), "checking must not consume the probe");
        let probe = b.try_acquire(t1).unwrap();
        assert!(!b.is_available(t1));
        b.release(probe);
    }

    #[test]
    fn zero_threshold_is_treated_as_one() {
        let b = CircuitBreaker::new(0, OPEN);
        let t0 = Instant::now();
        fail(&b, t0);
        assert_eq!(b.state(t0), BreakerState::Open);
    }
}
