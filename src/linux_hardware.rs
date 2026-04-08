use crate::{ChannelValidation, CpuPinning, HugePageSize, LayoutSpec};

/// Host-specific Linux placement settings for replicated buffers and worker threads.
///
/// This type is intentionally explicit. CPU ids and DRAM-channel assumptions are
/// operator-supplied values, not topology derived by the crate at runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinuxHardwareSpec {
    /// CPU ids used for one worker per replica.
    ///
    /// The number of worker CPUs defines the replica count used by [`LayoutSpec`].
    pub worker_cpus: Vec<usize>,
    /// Number of hardware channels represented by the layout stride.
    pub channels: usize,
    /// Byte distance between adjacent channel bases.
    pub channel_offset_bytes: usize,
    /// Optional physical-address bit used for `/proc/self/pagemap` channel validation.
    pub channel_bit: Option<usize>,
    /// Hugetlb page size used for the backing allocation.
    pub hugepage_size: HugePageSize,
}

impl LinuxHardwareSpec {
    /// Creates a Linux hardware spec from the worker CPU ids.
    ///
    /// The worker CPU count defines the replica count. By default, the channel
    /// count matches the replica count, the channel offset is 256 bytes, channel
    /// validation is disabled, and the hugepage size is 1 GiB.
    pub fn new<I>(worker_cpus: I) -> Self
    where
        I: IntoIterator<Item = usize>,
    {
        let worker_cpus: Vec<usize> = worker_cpus.into_iter().collect();
        let channels = worker_cpus.len();
        Self {
            worker_cpus,
            channels,
            channel_offset_bytes: 256,
            channel_bit: None,
            hugepage_size: HugePageSize::Size1GiB,
        }
    }

    /// Returns the replica count implied by the worker CPU list.
    pub fn replicas(&self) -> usize {
        self.worker_cpus.len()
    }

    /// Returns the replicated buffer layout implied by this spec.
    pub fn layout_spec(&self) -> LayoutSpec {
        LayoutSpec {
            replicas: self.replicas(),
            channels: self.channels,
            channel_offset_bytes: self.channel_offset_bytes,
        }
    }

    /// Returns the optional channel-validation policy implied by this spec.
    pub fn validation(&self) -> ChannelValidation {
        match self.channel_bit {
            Some(channel_bit) => ChannelValidation::Pagemap { channel_bit },
            None => ChannelValidation::None,
        }
    }

    /// Returns an exact worker pinning policy for the configured worker CPUs.
    pub fn cpu_pinning(&self) -> CpuPinning {
        CpuPinning::exact(self.worker_cpus.iter().copied())
    }
}

#[cfg(test)]
mod tests {
    use super::LinuxHardwareSpec;
    use crate::{ChannelValidation, CpuPinning, HugePageSize, LayoutSpec};

    #[test]
    fn linux_hardware_spec_adapts_into_existing_primitives() {
        let spec = LinuxHardwareSpec {
            channels: 4,
            channel_offset_bytes: 512,
            channel_bit: Some(9),
            hugepage_size: HugePageSize::Size2MiB,
            ..LinuxHardwareSpec::new([3, 5])
        };

        assert_eq!(spec.replicas(), 2);
        assert_eq!(
            spec.layout_spec(),
            LayoutSpec {
                replicas: 2,
                channels: 4,
                channel_offset_bytes: 512,
            }
        );
        assert_eq!(
            spec.validation(),
            ChannelValidation::Pagemap { channel_bit: 9 }
        );
        assert!(matches!(
            spec.cpu_pinning(),
            CpuPinning::Exact(cpus) if cpus == vec![3, 5]
        ));
    }
}
