use tailslayer::{
    ChannelValidation, CpuPinning, HedgedRuntime, HugePageSize, IdleStrategy, ReplicatedBuffer,
};

fn main() -> tailslayer::Result<()> {
    let mut buffer = ReplicatedBuffer::<u8>::builder()
        .capacity(1024)
        .replicas(2)
        .channels(2)
        .channel_offset_bytes(256)
        .hugepage_size(HugePageSize::Size1GiB)
        .validation(ChannelValidation::Pagemap { channel_bit: 8 })
        .build()?;

    buffer.extend_from_slice(&[0x43, 0x44])?;

    let runtime = HedgedRuntime::builder(buffer)
        .cpu_pinning(CpuPinning::exact([11, 12]))
        .idle_strategy(IdleStrategy::Spin)
        .build()?;

    let value = runtime.read(1)?;
    println!("value={value:#x}");
    Ok(())
}
