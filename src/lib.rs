pub mod agent;
pub mod client;
pub mod clock;
pub mod cnc;
pub mod context;
pub mod frame;
pub mod media;

#[cfg(test)]
mod tests {
    // Smoke test: all public modules are importable.
    use super::*;

    #[test]
    fn public_modules_accessible() {
        let _ = context::DriverContext::default();
        let _ = clock::NanoClock::new();
        let _ = frame::FRAME_HEADER_LENGTH;
    }
}
