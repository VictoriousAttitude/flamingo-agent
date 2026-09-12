//! The two sampled metrics: current UTC time and this process's resident memory.

use std::sync::Once;

use chrono::{DateTime, SecondsFormat, Utc};

/// One sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metrics {
    /// Sample time.
    pub utc: DateTime<Utc>,
    /// Resident set size (Working Set on Windows) of this process, in bytes.
    pub rss_bytes: u64,
}

/// Metric collection failures.
#[derive(Debug, thiserror::Error)]
pub enum MetricsError {
    /// The OS did not return memory statistics for this process.
    #[error("process memory statistics are unavailable")]
    RssUnavailable,
}

/// RFC 3339 with millisecond precision and a `Z` suffix, e.g. `2026-09-11T20:35:00.123Z`.
pub fn format_utc(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// `memory-stats` initialises its Linux statics on first use without ordering them, so a
/// concurrent first call can read a half-initialised state and report zero. Funnelling the
/// first call through a `Once` guarantees the initialisation completes before any other
/// thread queries memory. (Production calls are already sequential; this matters for tests.)
static WARM_UP: Once = Once::new();

fn memory_stats_serialized() -> Option<memory_stats::MemoryStats> {
    WARM_UP.call_once(|| {
        let _ = memory_stats::memory_stats();
    });
    memory_stats::memory_stats()
}

/// Classify one raw reading from the OS.
///
/// A zero is treated as "unavailable": a live process never has zero resident memory. The
/// `Once` in `memory_stats_serialized` already closes memory-stats 1.2.0's Linux first-call
/// race (the `SMAPS_CHECKED` CAS is not ordered against the `SMAPS_EXIST`/`PAGE_SIZE`
/// stores), so this filter is defence in depth rather than the primary safeguard.
pub(crate) fn rss_bytes_from(physical_mem: usize) -> Result<u64, MetricsError> {
    match physical_mem as u64 {
        0 => Err(MetricsError::RssUnavailable),
        bytes => Ok(bytes),
    }
}

/// Resident memory of the current process in bytes.
pub fn rss_bytes() -> Result<u64, MetricsError> {
    match memory_stats_serialized() {
        Some(stats) => rss_bytes_from(stats.physical_mem),
        None => Err(MetricsError::RssUnavailable),
    }
}

/// Take one sample.
pub fn collect() -> Result<Metrics, MetricsError> {
    Ok(Metrics {
        utc: Utc::now(),
        rss_bytes: rss_bytes()?,
    })
}

impl Metrics {
    /// The sample time formatted for the child's `--utc` argument.
    pub fn utc_string(&self) -> String {
        format_utc(self.utc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn utc_formats_with_millis_and_z() {
        let t =
            Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap() + chrono::Duration::milliseconds(6);
        assert_eq!(format_utc(t), "2026-01-02T03:04:05.006Z");
    }

    #[test]
    fn zero_rss_is_reported_unavailable() {
        assert!(matches!(
            rss_bytes_from(0),
            Err(MetricsError::RssUnavailable)
        ));
        assert_eq!(rss_bytes_from(4096).unwrap(), 4096);
    }

    // Both RSS assertions live in one test on purpose: `memory_stats()` initialises its
    // statics on the first call without ordering them, so two tests calling it from
    // different threads can race and observe a zero reading.
    #[test]
    fn rss_is_positive_and_collect_produces_a_recent_sample() {
        assert!(rss_bytes().unwrap() > 0);

        let before = Utc::now();
        let m = collect().unwrap();
        assert!(m.utc >= before);
        assert!(m.rss_bytes > 0);
        assert!(m.utc_string().ends_with('Z'));
    }
}
