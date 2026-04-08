use crate::linux_hardware::LinuxHardwareSpec;
use crate::storage::{ChannelValidation, HugePageSize, ReplicatedBuffer};
use crate::{Error, LayoutSpec, Result, sys};
use std::marker::PhantomData;
use std::ptr;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Default worker core used for the first replica.
const CORE_MEAS_A: usize = 11;
/// Default worker core used for the second replica.
const CORE_MEAS_B: usize = 12;

/// Pins the current thread to a specific CPU.
pub fn pin_to_core(core: usize) -> Result<()> {
    sys::pin_to_cpu(core)
}

/// Builder for the Linux-only low-level reader API.
#[derive(Clone, Debug)]
pub struct LinuxHedgedReaderBuilder<T> {
    layout: LayoutSpec,
    capacity: usize,
    hugepage_size: HugePageSize,
    validation: ChannelValidation,
    worker_cores: Option<Vec<usize>>,
    _marker: PhantomData<T>,
}

impl<T> Default for LinuxHedgedReaderBuilder<T> {
    fn default() -> Self {
        Self {
            layout: LayoutSpec::default(),
            capacity: 1024,
            hugepage_size: HugePageSize::Size1GiB,
            validation: ChannelValidation::None,
            worker_cores: None,
            _marker: PhantomData,
        }
    }
}

impl<T: Copy + Send + Sync + 'static> LinuxHedgedReaderBuilder<T> {
    /// Sets the logical capacity in elements.
    pub fn capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity;
        self
    }

    /// Sets the number of replicas.
    pub fn replicas(mut self, replicas: usize) -> Self {
        self.layout.replicas = replicas;
        self
    }

    /// Sets the number of hardware channels in the layout stride.
    pub fn channels(mut self, channels: usize) -> Self {
        self.layout.channels = channels;
        self
    }

    /// Sets the byte distance between adjacent channel bases.
    pub fn channel_offset_bytes(mut self, bytes: usize) -> Self {
        self.layout.channel_offset_bytes = bytes;
        self
    }

    /// Sets the hugetlb page size used for the backing allocation.
    pub fn hugepage_size(mut self, hugepage_size: HugePageSize) -> Self {
        self.hugepage_size = hugepage_size;
        self
    }

    /// Enables or disables physical-channel validation.
    pub fn validation(mut self, validation: ChannelValidation) -> Self {
        self.validation = validation;
        self
    }

    /// Applies a Linux hardware spec to the layout, validation, and worker cores.
    pub fn linux_hardware_spec(mut self, spec: &LinuxHardwareSpec) -> Self {
        self.layout = spec.layout_spec();
        self.hugepage_size = spec.hugepage_size;
        self.validation = spec.validation();
        self.worker_cores = Some(spec.worker_cpus.clone());
        self
    }

    /// Sets the worker cores explicitly.
    pub fn worker_cores<I>(mut self, worker_cores: I) -> Self
    where
        I: IntoIterator<Item = usize>,
    {
        self.worker_cores = Some(worker_cores.into_iter().collect());
        self
    }

    /// Builds a low-level reader with original-style worker placement defaults.
    pub fn build(self) -> Result<LinuxHedgedReader<T>> {
        let buffer = ReplicatedBuffer::with_options(
            self.layout,
            self.capacity,
            self.hugepage_size,
            self.validation,
        )?;
        let replicas = buffer.replicas();
        let worker_cores = self
            .worker_cores
            .unwrap_or_else(|| default_worker_cores(replicas));
        if worker_cores.len() != replicas {
            return Err(Error::InvalidConfig(
                "worker_cores must provide one CPU id per replica",
            ));
        }

        Ok(LinuxHedgedReader {
            buffer,
            worker_cores,
            workers: Vec::with_capacity(replicas),
            started: false,
        })
    }
}

/// Linux-only low-level reader that mirrors the original callback-driven design.
///
/// Values are inserted before `start_workers()`. Once workers start, the buffer is
/// treated as immutable.
pub struct LinuxHedgedReader<T> {
    buffer: ReplicatedBuffer<T>,
    worker_cores: Vec<usize>,
    workers: Vec<JoinHandle<()>>,
    started: bool,
}

impl<T: Copy + Send + Sync + 'static> LinuxHedgedReader<T> {
    /// Creates a builder with original-style defaults.
    pub fn builder() -> LinuxHedgedReaderBuilder<T> {
        LinuxHedgedReaderBuilder::default()
    }

    /// Returns the logical number of inserted elements.
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Alias for the original C++ API.
    pub fn size(&self) -> usize {
        self.len()
    }

    /// Returns the logical capacity.
    pub fn capacity(&self) -> usize {
        self.buffer.capacity()
    }

    /// Returns whether the logical buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Returns the configured worker cores.
    pub fn worker_cores(&self) -> &[usize] {
        &self.worker_cores
    }

    /// Inserts a logical value into all replicas.
    pub fn insert(&mut self, value: T) -> Result<()> {
        self.ensure_not_started()?;
        self.buffer.push(value)
    }

    /// Inserts a slice of values into all replicas.
    pub fn extend_from_slice(&mut self, values: &[T]) -> Result<()> {
        self.ensure_not_started()?;
        self.buffer.extend_from_slice(values)
    }

    /// Spawns one worker per replica.
    ///
    /// The buffer must not be modified after workers start.
    ///
    /// This method mirrors the original C++ design but rejects out-of-bounds
    /// indices from `wait_work` instead of relying on caller-side unsafe
    /// preconditions.
    pub fn start_workers<WaitWork, FinalWork>(
        &mut self,
        wait_work: WaitWork,
        final_work: FinalWork,
    ) -> Result<()>
    where
        WaitWork: Fn() -> usize + Send + Sync + 'static,
        FinalWork: Fn(T) + Send + Sync + 'static,
    {
        self.ensure_not_started()?;
        self.started = true;

        let wait_work = Arc::new(wait_work);
        let final_work = Arc::new(final_work);
        let layout = self.buffer.layout();
        let logical_len = self.buffer.len();

        for (replica, &core) in self.worker_cores.iter().enumerate() {
            let replica_base = self.buffer.replica_base_ptr(replica)? as usize;
            let wait_work = Arc::clone(&wait_work);
            let final_work = Arc::clone(&final_work);

            self.workers.push(thread::spawn(move || {
                let _ = sys::pin_to_cpu(core);

                let logical_index = wait_work();
                assert!(
                    logical_index < logical_len,
                    "wait_work returned out-of-bounds index {logical_index} for logical length {logical_len}"
                );
                let base = replica_base as *const T;
                let offset = layout.element_offset(logical_index);
                let value = unsafe { ptr::read_volatile(base.add(offset)) };
                final_work(value);
            }));
        }

        if !self.workers.is_empty() {
            thread::sleep(Duration::from_millis(10));
        }

        Ok(())
    }

    /// Joins all worker threads started by `start_workers()`.
    pub fn join(&mut self) -> Result<()> {
        let mut panic_count = 0_usize;
        for worker in self.workers.drain(..) {
            if worker.join().is_err() {
                panic_count += 1;
            }
        }
        if panic_count == 0 {
            Ok(())
        } else {
            Err(Error::WorkerPanicked { count: panic_count })
        }
    }

    fn ensure_not_started(&self) -> Result<()> {
        if self.started {
            Err(Error::InvalidConfig(
                "cannot modify or restart LinuxHedgedReader after workers have started",
            ))
        } else {
            Ok(())
        }
    }
}

impl<T> Drop for LinuxHedgedReader<T> {
    fn drop(&mut self) {
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn default_worker_cores(replicas: usize) -> Vec<usize> {
    let mut cores = Vec::with_capacity(replicas);
    if replicas >= 1 {
        cores.push(CORE_MEAS_A);
    }
    if replicas >= 2 {
        cores.push(CORE_MEAS_B);
    }
    for replica in 2..replicas {
        cores.push(CORE_MEAS_B + replica - 1);
    }
    cores
}

#[cfg(test)]
mod tests {
    use super::LinuxHedgedReader;
    use crate::Error;
    use crate::HugePageSize;
    use std::sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    };

    #[test]
    fn low_level_reader_executes_callback_style_workers() {
        let mut reader = match LinuxHedgedReader::<u8>::builder()
            .capacity(8)
            .replicas(2)
            .hugepage_size(HugePageSize::Size2MiB)
            .build()
        {
            Ok(reader) => reader,
            Err(Error::Io(_)) | Err(Error::Unsupported { .. }) => return,
            Err(error) => panic!("unexpected low-level reader construction failure: {error}"),
        };
        reader.insert(0x43).unwrap();
        reader.insert(0x44).unwrap();

        let observed = Arc::new(AtomicU8::new(0));
        let callback_value = Arc::clone(&observed);
        reader
            .start_workers(
                || 1,
                move |value| {
                    callback_value.store(value, Ordering::Release);
                },
            )
            .unwrap();
        reader.join().unwrap();

        assert_eq!(observed.load(Ordering::Acquire), 0x44);
    }
}
