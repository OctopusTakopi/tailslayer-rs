use tailslayer::{HedgedRuntime, HugePageSize, ReplicatedBuffer};

fn main() -> tailslayer::Result<()> {
    let mut buffer = ReplicatedBuffer::<u8>::builder()
        .capacity(16)
        .replicas(2)
        .hugepage_size(HugePageSize::Size2MiB)
        .build()?;

    buffer.extend_from_slice(&[0x43, 0x44])?;

    let runtime = HedgedRuntime::builder(buffer).build()?;
    let value = runtime.read(1)?;

    println!("value={value:#x}");
    Ok(())
}
