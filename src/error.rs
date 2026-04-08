use std::fmt;
use std::io;

/// Crate-wide result type.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by `tailslayer`.
#[derive(Debug)]
pub enum Error {
    /// The provided configuration is invalid.
    InvalidConfig(&'static str),
    /// The requested logical index is out of bounds.
    OutOfBounds {
        /// Requested logical index.
        index: usize,
        /// Current logical length.
        len: usize,
    },
    /// Writing more elements would exceed the configured capacity.
    CapacityExceeded {
        /// Current logical length.
        len: usize,
        /// Configured logical capacity.
        capacity: usize,
    },
    /// The current platform or host setup does not support the requested operation.
    Unsupported {
        /// The operation that failed.
        operation: &'static str,
        /// Extra context for the unsupported condition.
        details: &'static str,
    },
    /// Runtime validation failed.
    ValidationFailed(&'static str),
    /// The runtime is no longer available.
    RuntimeClosed,
    /// One or more worker threads panicked while executing user-provided code.
    WorkerPanicked {
        /// Number of worker threads that panicked.
        count: usize,
    },
    /// Wrapper around underlying I/O errors.
    Io(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(message) => write!(f, "invalid configuration: {message}"),
            Self::OutOfBounds { index, len } => {
                write!(f, "logical index {index} out of bounds for length {len}")
            }
            Self::CapacityExceeded { len, capacity } => {
                write!(f, "buffer length {len} exceeds capacity {capacity}")
            }
            Self::Unsupported { operation, details } => {
                write!(f, "{operation} is unsupported: {details}")
            }
            Self::ValidationFailed(message) => write!(f, "validation failed: {message}"),
            Self::RuntimeClosed => write!(f, "hedged runtime is closed"),
            Self::WorkerPanicked { count } => {
                write!(f, "{count} worker thread(s) panicked")
            }
            Self::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
