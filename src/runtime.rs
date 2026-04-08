use crate::storage::ReplicatedBuffer;
use crate::sys;
use crate::{Error, Result};
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Worker placement policy.
#[derive(Clone, Debug, Default)]
pub enum CpuPinning {
    /// Do not pin worker threads.
    #[default]
    None,
    /// Pin each worker to the matching CPU id.
    Exact(Vec<usize>),
}

impl CpuPinning {
    /// Creates a pinning policy from an iterator of CPU ids.
    pub fn exact<I>(cpus: I) -> Self
    where
        I: IntoIterator<Item = usize>,
    {
        Self::Exact(cpus.into_iter().collect())
    }
}

/// Idle strategy used while workers wait for the next logical index.
#[derive(Clone, Copy, Debug, Default)]
pub enum IdleStrategy {
    /// Spin in place for the lowest latency.
    Spin,
    /// Yield to the scheduler while waiting.
    #[default]
    Yield,
    /// Sleep between polls.
    Sleep(Duration),
}

impl IdleStrategy {
    fn wait(self) {
        match self {
            Self::Spin => std::hint::spin_loop(),
            Self::Yield => thread::yield_now(),
            Self::Sleep(duration) => thread::sleep(duration),
        }
    }
}

/// Builder for `HedgedRuntime<T>`.
pub struct HedgedRuntimeBuilder<T> {
    buffer: ReplicatedBuffer<T>,
    cpu_pinning: CpuPinning,
    idle_strategy: IdleStrategy,
}

impl<T: Copy + Send + Sync + 'static> HedgedRuntimeBuilder<T> {
    /// Sets the worker pinning policy.
    pub fn cpu_pinning(mut self, cpu_pinning: CpuPinning) -> Self {
        self.cpu_pinning = cpu_pinning;
        self
    }

    /// Applies a Linux hardware spec to the worker pinning policy.
    pub fn linux_hardware_spec(mut self, spec: &crate::linux_hardware::LinuxHardwareSpec) -> Self {
        self.cpu_pinning = spec.cpu_pinning();
        self
    }

    /// Sets the waiting strategy used by workers and readers.
    pub fn idle_strategy(mut self, idle_strategy: IdleStrategy) -> Self {
        self.idle_strategy = idle_strategy;
        self
    }

    /// Spawns one worker per replica.
    pub fn build(self) -> Result<HedgedRuntime<T>> {
        HedgedRuntime::new(self.buffer, self.cpu_pinning, self.idle_strategy)
    }
}

/// Persistent worker runtime that returns the earliest completed replica read.
pub struct HedgedRuntime<T> {
    buffer: ReplicatedBuffer<T>,
    idle_strategy: IdleStrategy,
    next_request_id: AtomicU64,
    state: Arc<SharedState<T>>,
    request_lock: Mutex<()>,
    workers: Vec<JoinHandle<()>>,
}

impl<T: Copy + Send + Sync + 'static> HedgedRuntime<T> {
    /// Creates a builder from an existing replicated buffer.
    pub fn builder(buffer: ReplicatedBuffer<T>) -> HedgedRuntimeBuilder<T> {
        HedgedRuntimeBuilder {
            buffer,
            cpu_pinning: CpuPinning::None,
            idle_strategy: IdleStrategy::Yield,
        }
    }

    fn new(
        buffer: ReplicatedBuffer<T>,
        cpu_pinning: CpuPinning,
        idle_strategy: IdleStrategy,
    ) -> Result<Self> {
        let replicas = buffer.replicas();
        if matches!(&cpu_pinning, CpuPinning::Exact(cpus) if cpus.len() != replicas) {
            return Err(Error::InvalidConfig(
                "CpuPinning::Exact must provide one CPU id per replica",
            ));
        }

        let state = Arc::new(SharedState::new());
        let mut workers = Vec::with_capacity(replicas);

        for replica in 0..replicas {
            let worker_state = Arc::clone(&state);
            let replica_base = buffer.replica_base_ptr(replica)? as usize;
            let cpu = match &cpu_pinning {
                CpuPinning::None => None,
                CpuPinning::Exact(cpus) => Some(cpus[replica]),
            };
            let idle = idle_strategy;
            let layout = buffer.layout();
            let len = buffer.len();

            workers.push(thread::spawn(move || {
                if let Some(cpu_id) = cpu {
                    let _ = sys::pin_to_cpu(cpu_id);
                }

                let mut seen_request = 0_u64;
                loop {
                    if worker_state.shutdown.load(Ordering::Acquire) {
                        break;
                    }

                    let request_id = worker_state.request_id.load(Ordering::Acquire);
                    if request_id == 0 || request_id == seen_request {
                        idle.wait();
                        continue;
                    }

                    if worker_state.shutdown.load(Ordering::Acquire) {
                        break;
                    }

                    seen_request = request_id;
                    let logical_index = worker_state.request_index.load(Ordering::Relaxed);
                    if logical_index >= len {
                        continue;
                    }

                    let base = replica_base as *const T;
                    let offset = layout.element_offset(logical_index);
                    let value = unsafe { ptr::read_volatile(base.add(offset)) };

                    let pending = request_id << 2;
                    let claimed = pending | 1;
                    let ready = pending | 2;
                    if worker_state
                        .winner_state
                        .compare_exchange(pending, claimed, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        unsafe {
                            (*worker_state.value.get()).write(value);
                        }
                        worker_state.winner_state.store(ready, Ordering::Release);
                    }
                }
            }));
        }

        Ok(Self {
            buffer,
            idle_strategy,
            next_request_id: AtomicU64::new(1),
            state,
            request_lock: Mutex::new(()),
            workers,
        })
    }

    /// Returns the logical number of elements available for hedged reads.
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Returns whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Issues a logical read and returns the earliest completed replica value.
    pub fn read(&self, index: usize) -> Result<T> {
        if index >= self.buffer.len() {
            return Err(Error::OutOfBounds {
                index,
                len: self.buffer.len(),
            });
        }

        let _guard = self.request_guard()?;
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let pending = request_id << 2;
        let ready = pending | 2;

        self.state.request_index.store(index, Ordering::Relaxed);
        self.state.winner_state.store(pending, Ordering::Relaxed);
        self.state.request_id.store(request_id, Ordering::Release);

        loop {
            if self.state.shutdown.load(Ordering::Acquire) {
                return Err(Error::RuntimeClosed);
            }

            if self.state.winner_state.load(Ordering::Acquire) == ready {
                let value = unsafe { (*self.state.value.get()).assume_init_read() };
                return Ok(value);
            }

            self.idle_strategy.wait();
        }
    }

    fn request_guard(&self) -> Result<MutexGuard<'_, ()>> {
        self.request_lock.lock().map_err(|_| Error::RuntimeClosed)
    }
}

impl<T> Drop for HedgedRuntime<T> {
    fn drop(&mut self) {
        self.state.shutdown.store(true, Ordering::Release);
        self.state.request_id.fetch_add(1, Ordering::Release);
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

struct SharedState<T> {
    request_id: AtomicU64,
    request_index: AtomicUsize,
    winner_state: AtomicU64,
    shutdown: AtomicBool,
    value: UnsafeCell<MaybeUninit<T>>,
}

impl<T> SharedState<T> {
    fn new() -> Self {
        Self {
            request_id: AtomicU64::new(0),
            request_index: AtomicUsize::new(0),
            winner_state: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }
}

unsafe impl<T: Send> Send for SharedState<T> {}
unsafe impl<T: Send> Sync for SharedState<T> {}

#[cfg(test)]
mod tests {
    use super::HedgedRuntime;
    use crate::Error;
    use crate::storage::{HugePageSize, ReplicatedBuffer};

    #[test]
    fn runtime_returns_the_expected_value() {
        let mut buffer = match ReplicatedBuffer::<u8>::builder()
            .capacity(8)
            .replicas(2)
            .hugepage_size(HugePageSize::Size2MiB)
            .build()
        {
            Ok(buffer) => buffer,
            Err(Error::Io(_)) | Err(Error::Unsupported { .. }) => return,
            Err(error) => panic!("unexpected buffer construction failure: {error}"),
        };
        buffer.extend_from_slice(&[0x43, 0x44]).unwrap();

        let runtime = HedgedRuntime::builder(buffer).build().unwrap();
        assert_eq!(runtime.read(1).unwrap(), 0x44);
    }
}
