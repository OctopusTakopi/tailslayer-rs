#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]
#![doc = include_str!("../README.md")]

mod error;
mod layout;
#[cfg(target_os = "linux")]
mod linux_low_level;
mod runtime;
mod storage;
mod sys;

pub use error::{Error, Result};
pub use layout::{LayoutPlan, LayoutSpec};
#[cfg(target_os = "linux")]
pub use linux_low_level::{
    CORE_MAIN, CORE_MEAS_A, CORE_MEAS_B, LinuxHedgedReader, LinuxHedgedReaderBuilder, pin_to_core,
};
pub use runtime::{CpuPinning, HedgedRuntime, HedgedRuntimeBuilder, IdleStrategy};
pub use storage::{ChannelValidation, HugePageSize, ReplicatedBuffer, ReplicatedBufferBuilder};
