use crate::layout::{LayoutPlan, LayoutSpec};
use crate::sys;
use crate::{Error, Result};
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::ptr::{self, NonNull};

/// Supported hugetlb sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HugePageSize {
    /// A 2 MiB hugepage.
    Size2MiB,
    /// A 1 GiB hugepage.
    Size1GiB,
}

impl HugePageSize {
    /// Returns the size in bytes.
    pub const fn bytes(self) -> usize {
        match self {
            Self::Size2MiB => 1 << 21,
            Self::Size1GiB => 1 << 30,
        }
    }
}

/// Optional validation step for Linux channel placement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChannelValidation {
    /// Skip validation.
    #[default]
    None,
    /// Validate that all replica bases resolve to distinct channels via `/proc/self/pagemap`.
    Pagemap {
        /// Physical-address bit used to derive the channel id.
        channel_bit: usize,
    },
}

/// Builder for `ReplicatedBuffer<T>`.
#[derive(Clone, Debug)]
pub struct ReplicatedBufferBuilder<T> {
    layout: LayoutSpec,
    capacity: usize,
    hugepage_size: HugePageSize,
    validation: ChannelValidation,
    _marker: PhantomData<T>,
}

impl<T> Default for ReplicatedBufferBuilder<T> {
    fn default() -> Self {
        Self {
            layout: LayoutSpec::default(),
            capacity: 1024,
            hugepage_size: HugePageSize::Size1GiB,
            validation: ChannelValidation::None,
            _marker: PhantomData,
        }
    }
}

impl<T: Copy> ReplicatedBufferBuilder<T> {
    /// Sets the logical capacity in elements.
    ///
    /// A capacity of zero is allowed and produces an empty buffer that rejects pushes.
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

    /// Builds an empty replicated buffer.
    pub fn build(self) -> Result<ReplicatedBuffer<T>> {
        ReplicatedBuffer::with_options(
            self.layout,
            self.capacity,
            self.hugepage_size,
            self.validation,
        )
    }
}

/// Owning replicated storage for `Copy` values.
pub struct ReplicatedBuffer<T> {
    layout: LayoutPlan,
    capacity: usize,
    len: usize,
    storage: Storage<T>,
}

impl<T: Copy> ReplicatedBuffer<T> {
    /// Creates a builder with production-oriented defaults.
    pub fn builder() -> ReplicatedBufferBuilder<T> {
        ReplicatedBufferBuilder::default()
    }

    /// Creates a replicated buffer directly from a slice using hugetlb-backed allocation.
    pub fn from_slice(values: &[T]) -> Result<Self> {
        let mut buffer = Self::builder().capacity(values.len()).build()?;
        buffer.extend_from_slice(values)?;
        Ok(buffer)
    }

    pub(crate) fn with_options(
        spec: LayoutSpec,
        capacity: usize,
        hugepage_size: HugePageSize,
        validation: ChannelValidation,
    ) -> Result<Self> {
        let layout = LayoutPlan::for_type::<T>(spec)?;
        let storage = Storage::new(&layout, capacity, hugepage_size)?;

        let buffer = Self {
            layout,
            capacity,
            len: 0,
            storage,
        };

        if let ChannelValidation::Pagemap { channel_bit } = validation {
            buffer.validate_channels(channel_bit)?;
        }

        Ok(buffer)
    }

    /// Returns the logical number of elements stored in the buffer.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns the logical capacity of the buffer.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the number of replicas in the layout.
    pub fn replicas(&self) -> usize {
        self.layout.spec().replicas
    }

    /// Returns the layout used by the buffer.
    pub fn layout(&self) -> LayoutPlan {
        self.layout
    }

    /// Appends one logical value to all replicas.
    pub fn push(&mut self, value: T) -> Result<()> {
        if self.len >= self.capacity {
            return Err(Error::CapacityExceeded {
                len: self.len,
                capacity: self.capacity,
            });
        }

        for replica in 0..self.layout.spec().replicas {
            let ptr = self.replica_slot_ptr(replica, self.len)?;
            unsafe {
                ptr::write(ptr, value);
            }
        }

        self.len += 1;
        Ok(())
    }

    /// Appends a slice of values.
    pub fn extend_from_slice(&mut self, values: &[T]) -> Result<()> {
        for &value in values {
            self.push(value)?;
        }
        Ok(())
    }

    /// Returns the value at the given logical index from the first replica.
    pub fn get(&self, index: usize) -> Option<T> {
        self.replica_value(0, index).ok()
    }

    /// Returns the value for a specific replica and logical index.
    pub fn replica_value(&self, replica: usize, index: usize) -> Result<T> {
        let ptr = self.replica_ptr(replica, index)?;
        Ok(unsafe { ptr::read(ptr) })
    }

    pub(crate) fn replica_ptr(&self, replica: usize, index: usize) -> Result<*mut T> {
        if index >= self.len {
            return Err(Error::OutOfBounds {
                index,
                len: self.len,
            });
        }

        self.replica_slot_ptr(replica, index)
    }

    pub(crate) fn replica_base_ptr(&self, replica: usize) -> Result<*mut T> {
        if replica >= self.layout.spec().replicas {
            return Err(Error::OutOfBounds {
                index: replica,
                len: self.layout.spec().replicas,
            });
        }

        let offset = replica * self.layout.channel_offset_elements();
        Ok(unsafe { self.storage.base_ptr().as_ptr().add(offset).cast::<T>() })
    }

    fn replica_slot_ptr(&self, replica: usize, index: usize) -> Result<*mut T> {
        if index >= self.capacity {
            return Err(Error::OutOfBounds {
                index,
                len: self.capacity,
            });
        }

        let offset = self.layout.replica_element_offset(replica, index)?;
        Ok(unsafe { self.storage.base_ptr().as_ptr().add(offset).cast::<T>() })
    }

    fn validate_channels(&self, channel_bit: usize) -> Result<()> {
        let mut channels = Vec::with_capacity(self.layout.spec().replicas);
        for replica in 0..self.layout.spec().replicas {
            let base = unsafe {
                self.storage
                    .base_ptr()
                    .as_ptr()
                    .add(replica * self.layout.channel_offset_elements())
                    .cast::<u8>()
            };
            let phys = sys::virt_to_phys(base.cast_const())?;
            let channel = sys::compute_channel(phys, channel_bit);
            channels.push(channel);
        }

        for left in 0..channels.len() {
            for right in (left + 1)..channels.len() {
                if channels[left] == channels[right] {
                    return Err(Error::ValidationFailed(
                        "two replica bases resolved to the same channel",
                    ));
                }
            }
        }

        Ok(())
    }
}

struct Storage<T> {
    ptr: NonNull<MaybeUninit<T>>,
    bytes: usize,
}

impl<T> Storage<T> {
    fn new(layout: &LayoutPlan, capacity: usize, hugepage_size: HugePageSize) -> Result<Self> {
        let bytes = layout.allocation_bytes(capacity);
        let rounded = round_up(bytes, hugepage_size.bytes());
        let ptr = sys::map_hugetlb(rounded, hugepage_size.bytes())?.cast::<MaybeUninit<T>>();
        unsafe {
            ptr::write_bytes(ptr.as_ptr().cast::<u8>(), 0, rounded);
        }
        Ok(Self {
            ptr,
            bytes: rounded,
        })
    }

    fn base_ptr(&self) -> NonNull<MaybeUninit<T>> {
        self.ptr
    }
}

impl<T> Drop for Storage<T> {
    fn drop(&mut self) {
        unsafe {
            sys::unmap_hugetlb(self.ptr.cast::<u8>(), self.bytes);
        }
    }
}

fn round_up(value: usize, alignment: usize) -> usize {
    if value == 0 {
        alignment
    } else {
        value.next_multiple_of(alignment)
    }
}

#[cfg(test)]
mod tests {
    use super::{HugePageSize, ReplicatedBuffer};
    use crate::Error;

    fn build_test_buffer(capacity: usize, replicas: usize) -> Option<ReplicatedBuffer<u8>> {
        match ReplicatedBuffer::<u8>::builder()
            .capacity(capacity)
            .replicas(replicas)
            .hugepage_size(HugePageSize::Size2MiB)
            .build()
        {
            Ok(buffer) => Some(buffer),
            Err(Error::Io(_)) | Err(Error::Unsupported { .. }) => None,
            Err(error) => panic!("unexpected buffer construction failure: {error}"),
        }
    }

    #[test]
    fn push_and_get_round_trip() {
        let Some(mut buffer) = build_test_buffer(8, 2) else {
            return;
        };
        buffer.extend_from_slice(&[1, 2, 3]).unwrap();

        assert_eq!(buffer.len(), 3);
        assert_eq!(buffer.get(1), Some(2));
        assert_eq!(buffer.replica_value(1, 2).unwrap(), 3);
    }

    #[test]
    fn zero_capacity_buffer_is_empty_and_rejects_pushes() {
        let Some(mut buffer) = build_test_buffer(0, 2) else {
            return;
        };

        assert!(buffer.is_empty());
        assert_eq!(buffer.capacity(), 0);
        assert!(matches!(
            buffer.push(1),
            Err(Error::CapacityExceeded {
                len: 0,
                capacity: 0
            })
        ));
    }
}
