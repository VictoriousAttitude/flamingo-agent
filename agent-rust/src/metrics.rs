//! The two sampled metrics: current UTC time and this process's resident memory.

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

/// Resident memory of the current process in bytes.
pub fn rss_bytes() -> Result<u64, MetricsError> {
    memory_stats::memory_stats()
        .map(|m| m.physical_mem as u64)
        .ok_or(MetricsError::RssUnavailable)
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
    fn rss_is_positive_on_this_platform() {
        assert!(rss_bytes().unwrap() > 0);
    }

    #[test]
    fn collect_produces_a_recent_sample() {
        let before = Utc::now();
        let m = collect().unwrap();
        assert!(m.utc >= before);
        assert!(m.rss_bytes > 0);
        assert!(m.utc_string().ends_with('Z'));
    }
}
