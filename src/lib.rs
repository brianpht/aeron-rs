pub mod frame;
pub mod clock;
pub mod context;
pub mod media;
pub mod agent;
pub mod cnc;

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
