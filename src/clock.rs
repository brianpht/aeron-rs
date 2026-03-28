use std::time::Instant;

/// Monotonic nanosecond clock. Equivalent to aeron_nano_clock().
#[derive(Clone)]
pub struct NanoClock {
    epoch: Instant,
}

impl NanoClock {
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
        }
    }

    /// Current time in nanoseconds since clock creation.
    #[inline]
    pub fn nano_time(&self) -> i64 {
        self.epoch.elapsed().as_nanos() as i64
    }
}

impl Default for NanoClock {
    fn default() -> Self {
        Self::new()
    }
}

/// Cached clock — caches the last read for tight loops where
/// you poll time once per duty cycle iteration.
pub struct CachedNanoClock {
    inner: NanoClock,
    cached: i64,
}

impl CachedNanoClock {
    pub fn new() -> Self {
        let inner = NanoClock::new();
        let cached = inner.nano_time();
        Self { inner, cached }
    }

    /// Update the cached value and return it.
    #[inline]
    pub fn update(&mut self) -> i64 {
        self.cached = self.inner.nano_time();
        self.cached
    }

    /// Return the last cached value without a syscall.
    #[inline]
    pub fn cached(&self) -> i64 {
        self.cached
    }
}

impl Default for CachedNanoClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nano_clock_monotonic() {
        let clock = NanoClock::new();
        let t1 = clock.nano_time();
        let t2 = clock.nano_time();
        assert!(t2 >= t1, "nano_time must be monotonically non-decreasing");
    }

    #[test]
    fn nano_clock_default_trait() {
        let clock = NanoClock::default();
        let t = clock.nano_time();
        assert!(t >= 0);
    }

    #[test]
    fn cached_clock_returns_same_between_updates() {
        let mut clock = CachedNanoClock::new();
        let t1 = clock.update();
        let c1 = clock.cached();
        let c2 = clock.cached();
        assert_eq!(c1, c2, "cached() must return the same value between update() calls");
        assert_eq!(c1, t1, "cached() must equal the last update() return");
    }

    #[test]
    fn cached_clock_update_advances() {
        let mut clock = CachedNanoClock::new();
        let t1 = clock.update();
        // Spin briefly so time actually advances.
        std::hint::spin_loop();
        let t2 = clock.update();
        assert!(t2 >= t1, "update() must be monotonically non-decreasing");
    }

    #[test]
    fn cached_clock_default_trait() {
        let mut clock = CachedNanoClock::default();
        let t = clock.update();
        assert!(t >= 0);
    }
}

