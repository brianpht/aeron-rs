// Client library - user-facing API for connecting to a running media driver,
// adding publications/subscriptions, and sending/receiving data.
//
// For v1, the client works in-process (same process as the driver).
// Publications use ConcurrentPublication (Arc-shared log buffer) with a
// lock-free bridge to deliver SenderPublication handles to the sender agent.
//
// No Mutex in steady state. No allocation after setup.

pub mod aeron;
pub mod bridge;
pub mod media_driver;
pub mod publication;
pub mod sub_bridge;
pub mod subscription;

pub use aeron::Aeron;
pub use media_driver::MediaDriver;
pub use publication::Publication;
pub use subscription::Subscription;

/// Stack-only error for client operations. No heap allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientError {
    /// Driver is not running or CnC file not ready.
    DriverNotRunning,
    /// Timed out waiting for driver response.
    Timeout,
    /// Driver returned an error for this operation.
    RegistrationFailed,
    /// Channel URI is invalid or cannot be resolved.
    ChannelInvalid,
    /// Maximum number of in-flight publication transfers reached.
    MaxPublicationsReached,
    /// Maximum number of in-flight subscription endpoint transfers reached.
    MaxSubscriptionsReached,
    /// Invalid parameters (term_length, mtu, etc).
    InvalidParams,
    /// CnC mapping or version error.
    CncError,
    /// Transport (socket) open failed.
    TransportError,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DriverNotRunning => f.write_str("driver not running"),
            Self::Timeout => f.write_str("timed out waiting for driver response"),
            Self::RegistrationFailed => f.write_str("registration failed"),
            Self::ChannelInvalid => f.write_str("invalid channel URI"),
            Self::MaxPublicationsReached => f.write_str("max publications reached"),
            Self::MaxSubscriptionsReached => f.write_str("max subscriptions reached"),
            Self::InvalidParams => f.write_str("invalid parameters"),
            Self::CncError => f.write_str("CnC mapping error"),
            Self::TransportError => f.write_str("transport open failed"),
        }
    }
}

