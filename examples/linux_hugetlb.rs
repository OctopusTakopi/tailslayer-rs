use tailslayer::{HedgedRuntime, HugePageSize, IdleStrategy, LinuxHardwareSpec, ReplicatedBuffer};

fn main() -> tailslayer::Result<()> {
    // Adjust these host-specific settings to match your machine.
    let hardware = LinuxHardwareSpec {
        channel_bit: Some(8),
        hugepage_size: HugePageSize::Size1GiB,
        ..LinuxHardwareSpec::new([11, 12])
    };

    let mut buffer = ReplicatedBuffer::<u8>::builder()
        .capacity(1024)
        .linux_hardware_spec(&hardware)
        .build()?;

    buffer.extend_from_slice(&[0x43, 0x44])?;

    let runtime = HedgedRuntime::builder(buffer)
        .linux_hardware_spec(&hardware)
        .idle_strategy(IdleStrategy::Spin)
        .build()?;

    let value = runtime.read(1)?;
    println!("value={value:#x}");
    Ok(())
}
