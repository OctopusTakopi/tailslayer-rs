#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]
#![doc = include_str!("../README.md")]

mod error;
mod layout;
mod linux_hardware;
#[cfg(target_os = "linux")]
mod linux_low_level;
mod runtime;
mod storage;
mod sys;

pub use error::{Error, Result};
pub use layout::{LayoutPlan, LayoutSpec};
pub use linux_hardware::LinuxHardwareSpec;
#[cfg(target_os = "linux")]
pub use linux_low_level::{LinuxHedgedReader, LinuxHedgedReaderBuilder, pin_to_core};
pub use runtime::{CpuPinning, HedgedRuntime, HedgedRuntimeBuilder, IdleStrategy};
pub use storage::{ChannelValidation, HugePageSize, ReplicatedBuffer, ReplicatedBufferBuilder};
