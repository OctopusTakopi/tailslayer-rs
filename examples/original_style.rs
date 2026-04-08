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
    use tailslayer::{HugePageSize, LinuxHardwareSpec, LinuxHedgedReader};

    // Adjust these host-specific settings to match your machine.
    let hardware = LinuxHardwareSpec {
        hugepage_size: HugePageSize::Size2MiB,
        ..LinuxHardwareSpec::new([11, 12])
    };

    let mut reader = LinuxHedgedReader::<u8>::builder()
        .linux_hardware_spec(&hardware)
        .capacity(16)
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
