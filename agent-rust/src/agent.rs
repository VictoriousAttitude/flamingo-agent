//! The tick loop (design §5.1): fixed period, delayed missed ticks, at most one cycle in
//! flight, and each cycle isolated in its own task so neither an error nor a panic stops the
//! loop.

use std::future::Future;
use std::time::Duration;

use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

/// Run `cycle` every `period` until `cancel` fires. The first cycle runs immediately.
///
/// Cancellation is observed **between** cycles: while a cycle is in flight the loop awaits
/// it and does not poll `cancel`, so a long or hung cycle delays shutdown until it returns.
/// A cycle that can block (for example on a child process) must capture a clone of the same
/// token and stop itself when it is cancelled if prompt cancellation during a cycle is
/// required.
///
/// At most one cycle is ever in flight. Each cycle runs as its own task, so a cycle that
/// returns `Err` or panics is logged and the next tick still runs.
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

    /// Paused time makes this exact: tokio auto-advances the clock to the next timer
    /// whenever the runtime is idle, so the interval fires at 0, 50, 100, 150, 200 and
    /// 250 ms and the cancellation at 275 ms lands before the seventh tick.
    #[tokio::test(start_paused = true)]
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
        assert_eq!(count.load(Ordering::SeqCst), 6);
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
