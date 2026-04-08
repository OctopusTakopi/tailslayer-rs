#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("original_style is only supported on Linux");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn main() -> tailslayer::Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tailslayer::{CORE_MAIN, HugePageSize, LinuxHedgedReader, pin_to_core};

    pin_to_core(CORE_MAIN)?;

    let mut reader = LinuxHedgedReader::<u8>::builder()
        .capacity(16)
        .replicas(2)
        .hugepage_size(HugePageSize::Size2MiB)
        .build()?;
    reader.insert(0x43)?;
    reader.insert(0x44)?;

    let signal_index = Arc::new(AtomicUsize::new(1));
    let wait_signal = Arc::clone(&signal_index);

    reader.start_workers(
        move || wait_signal.load(Ordering::Relaxed),
        |value| {
            println!("value={value:#x}");
        },
    )?;

    reader.join()?;
    Ok(())
}
