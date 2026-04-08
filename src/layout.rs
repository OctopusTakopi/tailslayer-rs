use crate::{Error, Result};

/// Logical layout for replicated storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutSpec {
    /// Number of replicas that will be read redundantly.
    pub replicas: usize,
    /// Number of hardware channels represented by the layout stride.
    pub channels: usize,
    /// Byte distance between adjacent channel bases.
    pub channel_offset_bytes: usize,
}

impl Default for LayoutSpec {
    fn default() -> Self {
        Self {
            replicas: 2,
            channels: 2,
            channel_offset_bytes: 256,
        }
    }
}

/// Precomputed addressing plan for a concrete element type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutPlan {
    spec: LayoutSpec,
    element_size: usize,
    channel_offset_elements: usize,
    elements_per_chunk: usize,
    stride_in_elements: usize,
}

impl LayoutPlan {
    /// Builds a layout plan for `T`.
    pub fn for_type<T>(spec: LayoutSpec) -> Result<Self> {
        let element_size = std::mem::size_of::<T>();
        if element_size == 0 {
            return Err(Error::InvalidConfig("zero-sized types are not supported"));
        }
        if spec.replicas == 0 {
            return Err(Error::InvalidConfig("replicas must be greater than zero"));
        }
        if spec.channels == 0 {
            return Err(Error::InvalidConfig("channels must be greater than zero"));
        }
        if spec.replicas > spec.channels {
            return Err(Error::InvalidConfig(
                "replicas cannot exceed the configured channel count",
            ));
        }
        if spec.channel_offset_bytes == 0 {
            return Err(Error::InvalidConfig(
                "channel_offset_bytes must be non-zero",
            ));
        }
        if spec.channel_offset_bytes % element_size != 0 {
            return Err(Error::InvalidConfig(
                "channel_offset_bytes must be a multiple of the element size",
            ));
        }

        let channel_offset_elements = spec.channel_offset_bytes / element_size;
        let elements_per_chunk = channel_offset_elements;
        let stride_in_elements = spec.channels * channel_offset_elements;

        Ok(Self {
            spec,
            element_size,
            channel_offset_elements,
            elements_per_chunk,
            stride_in_elements,
        })
    }

    /// Returns the underlying layout specification.
    pub fn spec(&self) -> LayoutSpec {
        self.spec
    }

    /// Returns the size of each element in bytes.
    pub fn element_size(&self) -> usize {
        self.element_size
    }

    /// Returns the channel offset in elements.
    pub fn channel_offset_elements(&self) -> usize {
        self.channel_offset_elements
    }

    /// Returns the replica stride in elements.
    pub fn stride_in_elements(&self) -> usize {
        self.stride_in_elements
    }

    /// Returns the physical element offset for a logical index within one replica base.
    pub fn element_offset(&self, logical_index: usize) -> usize {
        let chunk_index = logical_index / self.elements_per_chunk;
        let offset_in_chunk = logical_index % self.elements_per_chunk;
        (chunk_index * self.stride_in_elements) + offset_in_chunk
    }

    /// Returns the element offset for a specific replica and logical index.
    pub fn replica_element_offset(&self, replica: usize, logical_index: usize) -> Result<usize> {
        if replica >= self.spec.replicas {
            return Err(Error::OutOfBounds {
                index: replica,
                len: self.spec.replicas,
            });
        }
        Ok((replica * self.channel_offset_elements) + self.element_offset(logical_index))
    }

    /// Returns the total number of elements required to hold a logical capacity.
    pub fn allocation_len(&self, logical_capacity: usize) -> usize {
        if logical_capacity == 0 {
            return self.spec.replicas * self.channel_offset_elements;
        }

        let last_logical_index = logical_capacity - 1;
        let last_replica_base = (self.spec.replicas - 1) * self.channel_offset_elements;
        last_replica_base + self.element_offset(last_logical_index) + 1
    }

    /// Returns the total number of bytes required to hold a logical capacity.
    pub fn allocation_bytes(&self, logical_capacity: usize) -> usize {
        self.allocation_len(logical_capacity) * self.element_size
    }
}

#[cfg(test)]
mod tests {
    use super::{LayoutPlan, LayoutSpec};

    #[test]
    fn plan_matches_chunked_layout() {
        let plan = LayoutPlan::for_type::<u8>(LayoutSpec::default()).unwrap();
        assert_eq!(plan.element_offset(0), 0);
        assert_eq!(plan.element_offset(255), 255);
        assert_eq!(plan.element_offset(256), 512);
        assert_eq!(plan.replica_element_offset(1, 256).unwrap(), 768);
    }

    #[test]
    fn allocation_len_includes_replica_bases() {
        let plan = LayoutPlan::for_type::<u16>(LayoutSpec::default()).unwrap();
        assert!(plan.allocation_len(2) >= 129);
    }
}
