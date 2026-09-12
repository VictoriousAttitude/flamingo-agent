//! The tick loop (design §5.1): fixed period, delayed missed ticks, at most one cycle in
//! flight, and each cycle isolated in its own task so neither an error nor a panic stops the
//! loop.

use std::future::Future;
use std::time::Duration;

use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

/// Run `cycle` every `period` until `cancel` fires. The first cycle runs immediately.
pub async fn run_loop<F, Fut>(period: Duration, cancel: CancellationToken, mut cycle: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let mut ticker = interval(period);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    info!(period_secs = period.as_secs_f64(), "agent loop started");
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            _ = ticker.tick() => match tokio::spawn(cycle()).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => error!(error = format!("{err:#}"), "cycle failed"),
                Err(join) => error!(error = %join, "cycle panicked; continuing"),
            },
        }
    }
    info!("agent loop stopped");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Instant;

    fn counter() -> Arc<AtomicUsize> {
        Arc::new(AtomicUsize::new(0))
    }

    #[tokio::test]
    async fn runs_once_per_period_starting_immediately() {
        let count = counter();
        let cancel = CancellationToken::new();
        let c = count.clone();
        let stop = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(275)).await;
            stop.cancel();
        });
        run_loop(Duration::from_millis(50), cancel, move || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        })
        .await;
        let n = count.load(Ordering::SeqCst);
        assert!((5..=7).contains(&n), "expected about 6 cycles, got {n}");
    }

    #[tokio::test]
    async fn a_failing_cycle_does_not_stop_the_loop() {
        let count = counter();
        let cancel = CancellationToken::new();
        let c = count.clone();
        let stop = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            stop.cancel();
        });
        run_loop(Duration::from_millis(50), cancel, move || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("simulated failure")
            }
        })
        .await;
        assert!(count.load(Ordering::SeqCst) >= 3);
    }

    #[tokio::test]
    async fn a_panicking_cycle_does_not_stop_the_loop() {
        let count = counter();
        let cancel = CancellationToken::new();
        let c = count.clone();
        let stop = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            stop.cancel();
        });
        run_loop(Duration::from_millis(50), cancel, move || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                panic!("simulated panic");
            }
        })
        .await;
        assert!(count.load(Ordering::SeqCst) >= 3);
    }

    #[tokio::test]
    async fn cancellation_stops_promptly_even_with_a_long_period() {
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            stop.cancel();
        });
        let started = Instant::now();
        run_loop(Duration::from_secs(10), cancel, || async { Ok(()) }).await;
        assert!(started.elapsed() < Duration::from_millis(500));
    }
}
