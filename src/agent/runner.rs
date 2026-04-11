// Agent runner - owns an Agent + IdleStrategy and runs the duty cycle loop.
//
// Supports both synchronous (run) and threaded (start/stop/join) modes.
// Graceful shutdown via AtomicBool with Acquire/Release ordering (not SeqCst).
// Monomorphized over Agent type (no dyn). Zero-allocation in duty cycle.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use super::idle_strategy::{IdleStrategy, IdleStrategyState, idle};
use super::{Agent, AgentError};

/// Runs an agent in a duty-cycle loop with idle strategy and shutdown control.
///
/// Generic over `A: Agent` - fully monomorphized, no trait objects.
pub struct AgentRunner<A: Agent> {
    agent: A,
    idle_strategy: IdleStrategy,
    idle_state: IdleStrategyState,
    running: Arc<AtomicBool>,
}

impl<A: Agent> AgentRunner<A> {
    /// Create a new runner. The agent is not started until `run()` or `start()`.
    pub fn new(agent: A, idle_strategy: IdleStrategy) -> Self {
        let idle_state = idle_strategy.new_state();
        Self {
            agent,
            idle_strategy,
            idle_state,
            running: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Get a handle that can be used to stop the runner from another context.
    /// Call this before `run()` if you need external shutdown control in
    /// synchronous mode.
    pub fn stop_handle(&self) -> StopHandle {
        StopHandle {
            running: Arc::clone(&self.running),
        }
    }

    /// Run the agent duty cycle synchronously (blocks the calling thread).
    ///
    /// 1. Calls `agent.on_start()`
    /// 2. Loops: `do_work() -> idle() -> check running flag`
    /// 3. Calls `agent.on_close()` on exit (even if do_work returned Err)
    ///
    /// Returns `Ok(())` on graceful shutdown, or the first `AgentError` from
    /// `do_work`. The `on_close` error is returned if do_work succeeded but
    /// on_close failed.
    pub fn run(&mut self) -> Result<(), AgentError> {
        self.agent.on_start()?;

        let mut work_err: Option<AgentError> = None;

        while self.running.load(Ordering::Acquire) {
            match self.agent.do_work() {
                Ok(work_count) => {
                    idle(&self.idle_strategy, &mut self.idle_state, work_count);
                }
                Err(e) => {
                    work_err = Some(e);
                    break;
                }
            }
        }

        // Always call on_close, even after error.
        let close_result = self.agent.on_close();

        // Propagate the first error encountered.
        if let Some(e) = work_err {
            return Err(e);
        }
        close_result
    }
}

impl<A: Agent + Send + 'static> AgentRunner<A> {
    /// Start the agent on a dedicated thread.
    ///
    /// Consumes the runner and returns an `AgentRunnerHandle` for stop/join.
    /// The thread is named after `agent.name()` (one allocation at spawn time).
    pub fn start(mut self) -> AgentRunnerHandle {
        let running = Arc::clone(&self.running);
        let name = self.agent.name().to_owned();

        let join_handle = thread::Builder::new()
            .name(name)
            .spawn(move || self.run())
            .expect("failed to spawn agent thread");

        AgentRunnerHandle {
            join_handle: Some(join_handle),
            running,
        }
    }
}

/// Handle for a running agent thread. Allows stopping and joining.
pub struct AgentRunnerHandle {
    join_handle: Option<JoinHandle<Result<(), AgentError>>>,
    running: Arc<AtomicBool>,
}

impl AgentRunnerHandle {
    /// Signal the agent to stop. Non-blocking.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
    }

    /// Signal stop and wait for the agent thread to exit.
    ///
    /// Returns the result from the agent's run loop.
    pub fn join(mut self) -> Result<(), AgentError> {
        self.stop();
        if let Some(handle) = self.join_handle.take() {
            match handle.join() {
                Ok(result) => result,
                Err(_panic) => Err(AgentError::Io(std::io::Error::other(
                    "agent thread panicked",
                ))),
            }
        } else {
            Ok(())
        }
    }
}

/// Lightweight stop-only handle. Can be cloned and sent across threads.
/// Does not own the join handle - cannot wait for the thread to exit.
#[derive(Clone)]
pub struct StopHandle {
    running: Arc<AtomicBool>,
}

impl StopHandle {
    /// Signal the agent to stop. Non-blocking.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Stub agent for testing ----

    struct CountingAgent {
        started: bool,
        closed: bool,
        iterations: i32,
        max_iterations: i32,
    }

    impl CountingAgent {
        fn new(max_iterations: i32) -> Self {
            Self {
                started: false,
                closed: false,
                iterations: 0,
                max_iterations,
            }
        }
    }

    impl Agent for CountingAgent {
        fn name(&self) -> &str {
            "counting-agent"
        }

        fn do_work(&mut self) -> Result<i32, AgentError> {
            self.iterations += 1;
            if self.iterations >= self.max_iterations {
                // Return 0 work to allow idle strategy to park.
                Ok(0)
            } else {
                Ok(1)
            }
        }

        fn on_start(&mut self) -> Result<(), AgentError> {
            self.started = true;
            Ok(())
        }

        fn on_close(&mut self) -> Result<(), AgentError> {
            self.closed = true;
            Ok(())
        }
    }

    // ---- run() tests ----

    #[test]
    fn run_calls_on_start_and_on_close() {
        let agent = CountingAgent::new(5);
        let mut runner = AgentRunner::new(agent, IdleStrategy::Noop);
        let stop = runner.stop_handle();

        // Stop after a few iterations via a separate mechanism.
        // We set running to false before run() so it exits after one check.
        stop.stop();

        let result = runner.run();
        assert!(result.is_ok());
        assert!(runner.agent.started, "on_start should have been called");
        assert!(runner.agent.closed, "on_close should have been called");
    }

    #[test]
    fn run_exits_when_running_set_to_false() {
        let agent = CountingAgent::new(i32::MAX); // never finishes on its own
        let mut runner = AgentRunner::new(agent, IdleStrategy::Noop);

        // Pre-set running to false so the loop exits immediately.
        runner.running.store(false, Ordering::Release);

        let result = runner.run();
        assert!(result.is_ok());
        // May have done 0 or 1 iterations depending on check order.
        assert!(runner.agent.iterations <= 1);
    }

    #[test]
    fn run_propagates_do_work_error() {
        struct FailingAgent;

        impl Agent for FailingAgent {
            fn name(&self) -> &str {
                "failing-agent"
            }
            fn do_work(&mut self) -> Result<i32, AgentError> {
                Err(AgentError::Io(std::io::Error::other("test error")))
            }
        }

        let mut runner = AgentRunner::new(FailingAgent, IdleStrategy::Noop);
        let result = runner.run();
        assert!(result.is_err());
    }

    // ---- start() / join() tests ----

    #[test]
    fn start_and_join_thread_lifecycle() {
        let agent = CountingAgent::new(i32::MAX);
        let runner = AgentRunner::new(agent, IdleStrategy::Noop);
        let handle = runner.start();

        // Let it run briefly.
        std::thread::sleep(std::time::Duration::from_millis(5));

        // Stop and join.
        let result = handle.join();
        assert!(result.is_ok());
    }

    #[test]
    fn stop_handle_stops_runner() {
        let agent = CountingAgent::new(i32::MAX);
        let runner = AgentRunner::new(agent, IdleStrategy::Noop);
        let stop = runner.stop_handle();
        let handle = runner.start();

        // Stop via the separate stop handle.
        stop.stop();

        let result = handle.join();
        assert!(result.is_ok());
    }

    #[test]
    fn agent_runner_handle_stop_is_idempotent() {
        let agent = CountingAgent::new(i32::MAX);
        let runner = AgentRunner::new(agent, IdleStrategy::Noop);
        let handle = runner.start();

        handle.stop();
        handle.stop(); // second call should not panic

        let result = handle.join();
        assert!(result.is_ok());
    }

    #[test]
    fn runner_with_backoff_strategy() {
        let agent = CountingAgent::new(i32::MAX);
        let runner = AgentRunner::new(agent, IdleStrategy::default_backoff());
        let handle = runner.start();

        std::thread::sleep(std::time::Duration::from_millis(10));

        let result = handle.join();
        assert!(result.is_ok());
    }
}
