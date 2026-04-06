pub mod sender;
pub mod receiver;

/// Error returned from agent duty-cycle work.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Single-threaded agent duty-cycle interface.
///
/// Mirrors `aeron_agent_t` - called in a tight loop by the agent runner.
/// Implementations must be zero-allocation in steady state.
pub trait Agent {
    /// Human-readable name for logging / diagnostics.
    fn name(&self) -> &str;

    /// Perform one unit of work. Returns a work count:
    /// - 0 → idle (runner may park / yield)
    /// - >0 → did useful work (runner spins again immediately)
    fn do_work(&mut self) -> Result<i32, AgentError>;

    /// Called once before the agent loop starts (optional).
    fn on_start(&mut self) -> Result<(), AgentError> {
        Ok(())
    }

    /// Called once after the agent loop exits (optional).
    fn on_close(&mut self) -> Result<(), AgentError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubAgent {
        work_count: i32,
    }

    impl Agent for StubAgent {
        fn name(&self) -> &str {
            "stub-agent"
        }

        fn do_work(&mut self) -> Result<i32, AgentError> {
            self.work_count += 1;
            Ok(self.work_count)
        }
    }

    #[test]
    fn default_on_start_returns_ok() {
        let mut agent = StubAgent { work_count: 0 };
        assert!(agent.on_start().is_ok());
    }

    #[test]
    fn default_on_close_returns_ok() {
        let mut agent = StubAgent { work_count: 0 };
        assert!(agent.on_close().is_ok());
    }

    #[test]
    fn do_work_increments() {
        let mut agent = StubAgent { work_count: 0 };
        assert_eq!(agent.do_work().unwrap(), 1);
        assert_eq!(agent.do_work().unwrap(), 2);
    }

    #[test]
    fn agent_name() {
        let agent = StubAgent { work_count: 0 };
        assert_eq!(agent.name(), "stub-agent");
    }
}

