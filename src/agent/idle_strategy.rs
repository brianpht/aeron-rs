// Idle strategy for agent duty cycle - enum dispatch (no dyn).
//
// Three-phase backoff: spin -> yield -> park.
// Driven by work_count from Agent::do_work():
//   work_count > 0 -> reset to spin phase
//   work_count == 0 -> advance through phases
//
// Zero-allocation in steady state. No SeqCst.

use std::thread;
use std::time::Duration;

/// Idle strategy configuration. Clone + Copy - holds config only, no mutable state.
/// Enum dispatch (no dyn) per coding rules.
#[derive(Debug, Clone, Copy)]
pub enum IdleStrategy {
    /// Tight spin loop with spin_loop() hint. Lowest latency, 100% CPU.
    BusySpin,
    /// Do nothing between iterations (for testing).
    Noop,
    /// Always sleep for a fixed duration.
    Sleeping { period_ns: u64 },
    /// Standard three-phase backoff: spin -> yield -> park.
    Backoff {
        max_spins: u64,
        max_yields: u64,
        min_park_ns: u64,
        max_park_ns: u64,
    },
}

impl IdleStrategy {
    /// Default backoff with Aeron-like parameters.
    pub fn default_backoff() -> Self {
        IdleStrategy::Backoff {
            max_spins: 10,
            max_yields: 5,
            min_park_ns: 1_000,       // 1 us
            max_park_ns: 1_000_000,   // 1 ms
        }
    }

    /// Create a new IdleStrategyState for use with this strategy.
    pub fn new_state(&self) -> IdleStrategyState {
        let initial_park = match self {
            IdleStrategy::Backoff { min_park_ns, .. } => *min_park_ns,
            _ => 0,
        };
        IdleStrategyState {
            spin_count: 0,
            yield_count: 0,
            park_ns: initial_park,
        }
    }
}

/// Mutable state for the idle strategy. Owned by AgentRunner, not by IdleStrategy.
/// Separated from config so IdleStrategy can be Copy/shared.
pub struct IdleStrategyState {
    spin_count: u64,
    yield_count: u64,
    park_ns: u64,
}

/// Perform one idle step based on the strategy and current state.
///
/// If work_count > 0, resets state (back to spin phase).
/// If work_count == 0, advances through spin -> yield -> park phases.
///
/// Zero-allocation, O(1). Called once per duty cycle iteration.
#[inline]
pub fn idle(strategy: &IdleStrategy, state: &mut IdleStrategyState, work_count: i32) {
    match strategy {
        IdleStrategy::Noop => {}
        IdleStrategy::BusySpin => {
            if work_count == 0 {
                std::hint::spin_loop();
            }
        }
        IdleStrategy::Sleeping { period_ns } => {
            if work_count == 0 {
                thread::park_timeout(Duration::from_nanos(*period_ns));
            }
        }
        IdleStrategy::Backoff {
            max_spins,
            max_yields,
            min_park_ns,
            max_park_ns,
        } => {
            if work_count > 0 {
                // Reset to spin phase.
                state.spin_count = 0;
                state.yield_count = 0;
                state.park_ns = *min_park_ns;
                return;
            }

            // Phase 1: Spin.
            if state.spin_count < *max_spins {
                state.spin_count += 1;
                std::hint::spin_loop();
                return;
            }

            // Phase 2: Yield.
            if state.yield_count < *max_yields {
                state.yield_count += 1;
                thread::yield_now();
                return;
            }

            // Phase 3: Park with linear ramp.
            thread::park_timeout(Duration::from_nanos(state.park_ns));
            // Ramp toward max_park_ns (saturating add, capped).
            let next = state.park_ns.saturating_add(*min_park_ns);
            state.park_ns = if next > *max_park_ns { *max_park_ns } else { next };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- IdleStrategy::default_backoff ----

    #[test]
    fn default_backoff_values() {
        match IdleStrategy::default_backoff() {
            IdleStrategy::Backoff {
                max_spins,
                max_yields,
                min_park_ns,
                max_park_ns,
            } => {
                assert_eq!(max_spins, 10);
                assert_eq!(max_yields, 5);
                assert_eq!(min_park_ns, 1_000);
                assert_eq!(max_park_ns, 1_000_000);
            }
            _ => panic!("expected Backoff variant"),
        }
    }

    // ---- Phase transitions for Backoff ----

    #[test]
    fn backoff_spins_then_yields_then_parks() {
        let strategy = IdleStrategy::Backoff {
            max_spins: 3,
            max_yields: 2,
            min_park_ns: 100,
            max_park_ns: 1000,
        };
        let mut state = strategy.new_state();

        // 3 spins.
        for i in 0..3 {
            idle(&strategy, &mut state, 0);
            assert_eq!(state.spin_count, i + 1, "spin phase iteration {i}");
            assert_eq!(state.yield_count, 0);
        }

        // 2 yields.
        for i in 0..2 {
            idle(&strategy, &mut state, 0);
            assert_eq!(state.spin_count, 3, "spin count stable in yield phase");
            assert_eq!(state.yield_count, i + 1, "yield phase iteration {i}");
        }

        // Park phase - park_ns ramps.
        idle(&strategy, &mut state, 0);
        assert_eq!(state.park_ns, 200); // 100 + 100
        idle(&strategy, &mut state, 0);
        assert_eq!(state.park_ns, 300); // 200 + 100
    }

    #[test]
    fn backoff_park_ramp_capped_at_max() {
        let strategy = IdleStrategy::Backoff {
            max_spins: 0,
            max_yields: 0,
            min_park_ns: 500,
            max_park_ns: 1000,
        };
        let mut state = strategy.new_state();

        // First park: starts at 500, ramps to 1000.
        idle(&strategy, &mut state, 0);
        assert_eq!(state.park_ns, 1000); // 500 + 500

        // Second park: capped at 1000.
        idle(&strategy, &mut state, 0);
        assert_eq!(state.park_ns, 1000);
    }

    #[test]
    fn backoff_reset_on_positive_work_count() {
        let strategy = IdleStrategy::Backoff {
            max_spins: 2,
            max_yields: 2,
            min_park_ns: 100,
            max_park_ns: 1000,
        };
        let mut state = strategy.new_state();

        // Advance into yield phase.
        for _ in 0..3 {
            idle(&strategy, &mut state, 0);
        }
        assert!(state.spin_count > 0);
        assert!(state.yield_count > 0);

        // Positive work resets.
        idle(&strategy, &mut state, 1);
        assert_eq!(state.spin_count, 0);
        assert_eq!(state.yield_count, 0);
        assert_eq!(state.park_ns, 100);
    }

    // ---- BusySpin ----

    #[test]
    fn busy_spin_does_not_modify_state() {
        let strategy = IdleStrategy::BusySpin;
        let mut state = strategy.new_state();
        // Should not panic or change state.
        idle(&strategy, &mut state, 0);
        idle(&strategy, &mut state, 1);
    }

    // ---- Noop ----

    #[test]
    fn noop_does_nothing() {
        let strategy = IdleStrategy::Noop;
        let mut state = strategy.new_state();
        idle(&strategy, &mut state, 0);
        idle(&strategy, &mut state, 1);
    }

    // ---- Sleeping ----

    #[test]
    fn sleeping_does_not_crash() {
        let strategy = IdleStrategy::Sleeping { period_ns: 1 };
        let mut state = strategy.new_state();
        // Just verify it doesn't panic (actual sleep is ~0).
        idle(&strategy, &mut state, 0);
        // No sleep when work was done.
        idle(&strategy, &mut state, 1);
    }

    // ---- Skip phases ----

    #[test]
    fn backoff_zero_spins_skips_to_yield() {
        let strategy = IdleStrategy::Backoff {
            max_spins: 0,
            max_yields: 2,
            min_park_ns: 100,
            max_park_ns: 1000,
        };
        let mut state = strategy.new_state();

        // First idle should go directly to yield.
        idle(&strategy, &mut state, 0);
        assert_eq!(state.spin_count, 0);
        assert_eq!(state.yield_count, 1);
    }

    #[test]
    fn backoff_zero_spins_zero_yields_skips_to_park() {
        let strategy = IdleStrategy::Backoff {
            max_spins: 0,
            max_yields: 0,
            min_park_ns: 100,
            max_park_ns: 1000,
        };
        let mut state = strategy.new_state();

        // First idle should go directly to park.
        idle(&strategy, &mut state, 0);
        assert_eq!(state.spin_count, 0);
        assert_eq!(state.yield_count, 0);
        // park_ns should have ramped.
        assert_eq!(state.park_ns, 200);
    }

    // ---- new_state ----

    #[test]
    fn new_state_initializes_park_ns_from_config() {
        let strategy = IdleStrategy::Backoff {
            max_spins: 0,
            max_yields: 0,
            min_park_ns: 42,
            max_park_ns: 100,
        };
        let state = strategy.new_state();
        assert_eq!(state.park_ns, 42);

        let noop_state = IdleStrategy::Noop.new_state();
        assert_eq!(noop_state.park_ns, 0);
    }
}

