//! Runtime 使用的系统时钟与 Tokio timer adapter。

use std::time::{Instant, SystemTime};

use crate::dns::Deadline;
use crate::ports::PortFuture;
use crate::ports::effects::Clock;

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl SystemClock {
    pub const fn new() -> Self {
        Self
    }
}

impl Clock for SystemClock {
    fn monotonic_now(&self) -> Instant {
        Instant::now()
    }

    fn utc_now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn sleep_until(&self, deadline: Deadline) -> PortFuture<'_, ()> {
        Box::pin(async move {
            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline.at())).await;
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crate::dns::{Cancellation, Deadline};
    use crate::ports::effects::Clock;

    use super::SystemClock;

    #[tokio::test]
    async fn sleeps_until_a_deadline_without_exposing_tokio_types() {
        let clock = SystemClock::new();
        let before = Instant::now();
        clock
            .sleep_until(Deadline::new(before + Duration::from_millis(1)))
            .await;
        assert!(clock.monotonic_now() >= before);
        let _ = Cancellation::new();
    }
}
