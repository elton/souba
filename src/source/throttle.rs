use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// 单个 host 的请求节流：保证两次请求之间至少隔 min_interval，
/// 失败后按指数退避。
///
/// 免费行情源会因连续请求静默封 IP（实测新浪约 10 次快速请求即返回空，
/// 东财直接长时间拒绝），所以这不是优化，是必需品。
pub struct Throttle {
    min_interval: Duration,
    state: Mutex<State>,
}

struct State {
    /// 下一次允许发请求的时刻
    next_allowed: Option<Instant>,
    failures: u32,
}

impl Throttle {
    pub fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            state: Mutex::new(State {
                next_allowed: None,
                failures: 0,
            }),
        }
    }

    /// 仅测试使用：退避是内部状态，生产代码不该依赖它做判断
    #[cfg(test)]
    pub fn current_backoff(&self) -> Duration {
        backoff_for(self.state.lock().expect("throttle 锁中毒").failures)
    }

    /// 等到可以发下一个请求为止。
    pub async fn acquire(&self) {
        let wait = {
            let mut st = self.state.lock().expect("throttle 锁中毒");
            let gap = self.min_interval + backoff_for(st.failures);
            let now = Instant::now();
            let wait = match st.next_allowed {
                Some(at) => at.saturating_duration_since(now),
                None => Duration::ZERO,
            };
            // 先把闸门推到未来再释放锁，避免并发调用同时通过
            st.next_allowed = Some(now + wait + gap);
            wait
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    pub fn penalize(&self) {
        let mut st = self.state.lock().expect("throttle 锁中毒");
        st.failures = st.failures.saturating_add(1);
    }

    pub fn reset(&self) {
        self.state.lock().expect("throttle 锁中毒").failures = 0;
    }
}

fn backoff_for(failures: u32) -> Duration {
    if failures == 0 {
        return Duration::ZERO;
    }
    let secs = 1u64.checked_shl(failures.min(20)).unwrap_or(u64::MAX);
    Duration::from_secs(secs).min(MAX_BACKOFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn 首次获取不等待() {
        let t = Throttle::new(Duration::from_millis(500));
        let started = Instant::now();
        t.acquire().await;
        assert!(started.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn 第二次获取要等够间隔() {
        let t = Throttle::new(Duration::from_millis(300));
        t.acquire().await;
        let started = Instant::now();
        t.acquire().await;
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(250), "只等了 {waited:?}");
    }

    #[tokio::test]
    async fn 失败后退避翻倍() {
        let t = Throttle::new(Duration::from_millis(100));
        assert_eq!(t.current_backoff(), Duration::ZERO);
        t.penalize();
        let first = t.current_backoff();
        assert!(first > Duration::ZERO);
        t.penalize();
        assert!(t.current_backoff() >= first * 2);
    }

    #[tokio::test]
    async fn 成功后退避归零() {
        let t = Throttle::new(Duration::from_millis(100));
        t.penalize();
        t.penalize();
        assert!(t.current_backoff() > Duration::ZERO);
        t.reset();
        assert_eq!(t.current_backoff(), Duration::ZERO);
    }

    #[tokio::test]
    async fn 退避有上限() {
        let t = Throttle::new(Duration::from_millis(100));
        for _ in 0..50 {
            t.penalize();
        }
        assert!(t.current_backoff() <= MAX_BACKOFF);
    }
}
