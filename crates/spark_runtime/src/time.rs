//! `spark.time` runtime implementations.
//!
//! Provides `extern "C"` functions for the `spark.time` standard library module.

/// Sleeps the current thread for the given number of milliseconds.
///
/// A value of 0 or negative is a no-op (returns immediately).
#[no_mangle]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub extern "C" fn __spark_time_sleep(ms: i64) {
    if ms <= 0 {
        return;
    }
    std::thread::sleep(std::time::Duration::from_millis(ms as u64));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sleep_zero_is_noop() {
        // Should return immediately without error.
        __spark_time_sleep(0);
    }

    #[test]
    fn sleep_negative_is_noop() {
        __spark_time_sleep(-100);
    }

    #[test]
    fn sleep_short_duration() {
        let start = std::time::Instant::now();
        __spark_time_sleep(50);
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() >= 40,
            "expected at least 40ms, got {}ms",
            elapsed.as_millis()
        );
    }
}
