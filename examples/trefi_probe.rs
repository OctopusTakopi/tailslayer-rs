#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn main() {
    eprintln!("trefi_probe is only supported on Linux x86_64");
    std::process::exit(1);
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod app {
    use std::arch::asm;
    use std::hint::black_box;
    use std::io;
    use std::ptr::{self, NonNull};
    use std::time::Duration;

    const HUGEPAGE_2M: usize = 1 << 21;
    const CALIB_PROBES: usize = 500_000;
    const MAX_SPIKES: usize = 2_000_000;
    const DEFAULT_PROBES: usize = 20_000_000;
    const DEFAULT_TREFI_US: f64 = 7.8;

    #[derive(Clone, Copy, Debug)]
    struct Spike {
        tsc: u64,
        latency: u64,
    }

    struct MappedRegion {
        ptr: NonNull<u8>,
        len: usize,
    }

    impl MappedRegion {
        fn huge_2m() -> io::Result<Self> {
            let flags = libc::MAP_PRIVATE
                | libc::MAP_ANONYMOUS
                | libc::MAP_HUGETLB
                | (21 << libc::MAP_HUGE_SHIFT);
            let ptr = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    HUGEPAGE_2M,
                    libc::PROT_READ | libc::PROT_WRITE,
                    flags,
                    -1,
                    0,
                )
            };
            if ptr == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            unsafe {
                libc::mlock(ptr, HUGEPAGE_2M);
                ptr::write_bytes(ptr.cast::<u8>(), 0x42, HUGEPAGE_2M);
            }
            Ok(Self {
                ptr: NonNull::new(ptr.cast::<u8>())
                    .expect("successful mmap must return a non-null pointer"),
                len: HUGEPAGE_2M,
            })
        }

        fn as_ptr(&self) -> *const u8 {
            self.ptr.as_ptr()
        }
    }

    impl Drop for MappedRegion {
        fn drop(&mut self) {
            unsafe {
                libc::munmap(self.ptr.as_ptr().cast::<libc::c_void>(), self.len);
            }
        }
    }

    pub fn main_impl() {
        if let Err(error) = run() {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }

    fn run() -> io::Result<()> {
        let program = std::env::args()
            .next()
            .unwrap_or_else(|| "trefi_probe".to_string());
        let args: Vec<String> = std::env::args().skip(1).collect();

        let mut probes = DEFAULT_PROBES;
        let mut threshold_override = 0_u64;
        let mut trefi_us = DEFAULT_TREFI_US;
        let mut threshold_multiplier = 2.0_f64;

        let mut idx = 0;
        while idx < args.len() {
            match args[idx].as_str() {
                "--probes" => {
                    probes = parse_usize(&args, idx + 1, "--probes")?;
                    idx += 2;
                }
                "--threshold" => {
                    threshold_override = parse_u64(&args, idx + 1, "--threshold")?;
                    idx += 2;
                }
                "--trefi-us" => {
                    trefi_us = parse_f64(&args, idx + 1, "--trefi-us")?;
                    idx += 2;
                }
                "--thresh-mult" => {
                    threshold_multiplier = parse_f64(&args, idx + 1, "--thresh-mult")?;
                    idx += 2;
                }
                "--help" => {
                    usage(&program);
                    return Ok(());
                }
                other => {
                    usage(&program);
                    return Err(io::Error::other(format!("unknown option: {other}")));
                }
            }
        }

        let tsc_ghz = calibrate_tsc_ghz();
        eprintln!("TSC: {tsc_ghz:.3} GHz");

        let expected_trefi_cycles = trefi_us * 1000.0 * tsc_ghz;
        eprintln!("Expected tREFI: {trefi_us:.1} us = {expected_trefi_cycles:.0} cycles");

        let hugepage = MappedRegion::huge_2m()?;
        let addr = hugepage.as_ptr();

        eprintln!("\n=== CALIBRATING ===");
        for _ in 0..2_000 {
            let _ = timed_probe(addr);
        }

        let mut calibration = Vec::with_capacity(CALIB_PROBES);
        for _ in 0..CALIB_PROBES {
            calibration.push(timed_probe(addr));
        }

        let mut calibration_sorted = calibration.clone();
        let calibration_percentiles = compute_percentiles(&mut calibration_sorted);
        let threshold = if threshold_override > 0 {
            threshold_override
        } else {
            (threshold_multiplier * calibration_percentiles.p50 as f64) as u64
        };

        let spikes_over_threshold = calibration
            .iter()
            .copied()
            .filter(|&latency| latency > threshold)
            .count();

        eprintln!(
            "  {} probes: median={} p90={} p99={} p99.9={} p99.99={}",
            CALIB_PROBES,
            calibration_percentiles.p50,
            calibration_percentiles.p90,
            calibration_percentiles.p99,
            calibration_percentiles.p999,
            calibration_percentiles.p9999
        );
        eprintln!("  Threshold: {threshold} ({threshold_multiplier:.1}x median)");
        eprintln!(
            "  Calibration spikes: {} ({:.3}%)",
            spikes_over_threshold,
            100.0 * spikes_over_threshold as f64 / CALIB_PROBES as f64
        );

        let mut spikes = Vec::with_capacity(MAX_SPIKES);
        eprintln!("\n=== PROBING ({probes} probes) ===");
        let tsc_start = rdtsc_lfence();

        for _ in 0..probes {
            clflush_addr(addr);
            mfence_inst();
            lfence_inst();
            let t0 = rdtsc_lfence();
            let value = unsafe { ptr::read_volatile(addr) };
            black_box(value);
            let t1 = rdtscp_lfence();
            let latency = t1 - t0;
            if latency > threshold && spikes.len() < MAX_SPIKES {
                spikes.push(Spike { tsc: t0, latency });
            }
        }

        let tsc_end = rdtscp_lfence();
        let elapsed_s = (tsc_end - tsc_start) as f64 / (tsc_ghz * 1e9);
        eprintln!("  Duration: {elapsed_s:.2} s");
        eprintln!(
            "  Spikes: {} ({:.4}%)",
            spikes.len(),
            100.0 * spikes.len() as f64 / probes as f64
        );

        println!("abs_tsc,latency_cyc");
        for spike in &spikes {
            println!("{},{}", spike.tsc, spike.latency);
        }

        eprintln!("\n=== PERIODICITY ANALYSIS ===");
        if spikes.len() < 10 {
            eprintln!("  Too few spikes ({}) for analysis", spikes.len());
            eprintln!("  VERDICT: INSUFFICIENT DATA");
            return Ok(());
        }

        let intervals: Vec<f64> = spikes
            .windows(2)
            .map(|window| (window[1].tsc - window[0].tsc) as f64)
            .collect();

        let mut count_1t = 0_usize;
        let mut count_2t = 0_usize;
        let mut count_3t = 0_usize;
        let mut count_other = 0_usize;
        for interval in &intervals {
            if *interval >= expected_trefi_cycles * 0.85
                && *interval <= expected_trefi_cycles * 1.15
            {
                count_1t += 1;
            } else if *interval >= expected_trefi_cycles * 1.85
                && *interval <= expected_trefi_cycles * 2.15
            {
                count_2t += 1;
            } else if *interval >= expected_trefi_cycles * 2.85
                && *interval <= expected_trefi_cycles * 3.15
            {
                count_3t += 1;
            } else {
                count_other += 1;
            }
        }

        let total_intervals = intervals.len() as f64;
        let harmonic_fraction = (count_1t + count_2t + count_3t) as f64 / total_intervals;
        eprintln!(
            "  Expected tREFI: {:.0} cycles ({trefi_us:.1} us)",
            expected_trefi_cycles
        );
        eprintln!("  Intervals: {} total", intervals.len());
        eprintln!(
            "  1T (±15%): {count_1t} ({:.1}%)",
            100.0 * count_1t as f64 / total_intervals
        );
        eprintln!(
            "  2T (±15%): {count_2t} ({:.1}%)",
            100.0 * count_2t as f64 / total_intervals
        );
        eprintln!(
            "  3T (±15%): {count_3t} ({:.1}%)",
            100.0 * count_3t as f64 / total_intervals
        );
        eprintln!(
            "  Other:     {count_other} ({:.1}%)",
            100.0 * count_other as f64 / total_intervals
        );
        eprintln!("  Harmonic total: {:.1}%", 100.0 * harmonic_fraction);

        let histogram_bins = 200_usize;
        let bin_lo = expected_trefi_cycles * 0.5;
        let bin_hi = expected_trefi_cycles * 1.5;
        let bin_width = (bin_hi - bin_lo) / histogram_bins as f64;
        let mut histogram = vec![0_usize; histogram_bins];

        for interval in &intervals {
            if *interval >= bin_lo && *interval < bin_hi {
                let bin = ((*interval - bin_lo) / bin_width) as usize;
                if bin < histogram_bins {
                    histogram[bin] += 1;
                }
            }
        }

        let (peak_bin, peak_count) = histogram
            .iter()
            .copied()
            .enumerate()
            .max_by_key(|&(_, count)| count)
            .unwrap_or((0, 0));
        let peak_cycles = bin_lo + (peak_bin as f64 + 0.5) * bin_width;
        let peak_us = peak_cycles / (tsc_ghz * 1000.0);

        eprintln!(
            "\n  Histogram peak: {:.0} cycles ({peak_us:.2} us), count={peak_count}",
            peak_cycles
        );
        eprintln!(
            "  Expected:       {:.0} cycles ({trefi_us:.2} us)",
            expected_trefi_cycles
        );
        eprintln!(
            "  Deviation:      {:.1}%",
            ((peak_cycles - expected_trefi_cycles).abs() / expected_trefi_cycles) * 100.0
        );

        let min_latency = spikes.iter().map(|spike| spike.latency).min().unwrap_or(0);
        let max_latency = spikes.iter().map(|spike| spike.latency).max().unwrap_or(0);
        let avg_latency =
            spikes.iter().map(|spike| spike.latency as f64).sum::<f64>() / spikes.len() as f64;
        eprintln!(
            "\n  Spike latency: min={min_latency} avg={avg_latency:.0} max={max_latency} cycles"
        );
        eprintln!(
            "  Spike latency: min={:.1} avg={:.1} max={:.1} ns",
            min_latency as f64 / tsc_ghz,
            avg_latency / tsc_ghz,
            max_latency as f64 / tsc_ghz
        );
        eprintln!();

        if harmonic_fraction > 0.30 {
            eprintln!(
                "  VERDICT: PERIODIC — {:.0}% of intervals at tREFI harmonics",
                harmonic_fraction * 100.0
            );
            eprintln!("  tREFI is visible via clflush timing on this DDR4 system");
        } else if harmonic_fraction > 0.15 {
            eprintln!(
                "  VERDICT: WEAK SIGNAL — {:.0}% at harmonics (borderline)",
                harmonic_fraction * 100.0
            );
        } else {
            eprintln!(
                "  VERDICT: NO PERIODIC SIGNAL — {:.0}% at harmonics",
                harmonic_fraction * 100.0
            );
            eprintln!("  Spikes are likely controller noise, not refresh");
        }

        Ok(())
    }

    fn timed_probe(addr: *const u8) -> u64 {
        clflush_addr(addr);
        mfence_inst();
        lfence_inst();
        let t0 = rdtsc_lfence();
        let value = unsafe { ptr::read_volatile(addr) };
        black_box(value);
        let t1 = rdtscp_lfence();
        t1 - t0
    }

    fn parse_usize(args: &[String], value_idx: usize, flag: &str) -> io::Result<usize> {
        args.get(value_idx)
            .ok_or_else(|| io::Error::other(format!("{flag} requires a value")))?
            .parse::<usize>()
            .map_err(|_| io::Error::other(format!("invalid value for {flag}")))
    }

    fn parse_u64(args: &[String], value_idx: usize, flag: &str) -> io::Result<u64> {
        args.get(value_idx)
            .ok_or_else(|| io::Error::other(format!("{flag} requires a value")))?
            .parse::<u64>()
            .map_err(|_| io::Error::other(format!("invalid value for {flag}")))
    }

    fn parse_f64(args: &[String], value_idx: usize, flag: &str) -> io::Result<f64> {
        args.get(value_idx)
            .ok_or_else(|| io::Error::other(format!("{flag} requires a value")))?
            .parse::<f64>()
            .map_err(|_| io::Error::other(format!("invalid value for {flag}")))
    }

    fn usage(program: &str) {
        eprintln!("Usage: {program} [--probes N] [--threshold N] [--trefi-us F] [--thresh-mult F]");
    }

    #[derive(Clone, Copy, Debug, Default)]
    struct Percentiles {
        p50: u64,
        p90: u64,
        p99: u64,
        p999: u64,
        p9999: u64,
    }

    fn compute_percentiles(data: &mut [u64]) -> Percentiles {
        data.sort_unstable();
        let len = data.len();
        Percentiles {
            p50: data[(len as f64 * 0.50) as usize],
            p90: data[(len as f64 * 0.90) as usize],
            p99: data[(len as f64 * 0.99) as usize],
            p999: data[usize::min(len - 1, (len as f64 * 0.999) as usize)],
            p9999: data[usize::min(len - 1, (len as f64 * 0.9999) as usize)],
        }
    }

    fn rdtsc_lfence() -> u64 {
        let lo: u32;
        let hi: u32;
        unsafe {
            asm!(
                "lfence",
                "rdtsc",
                out("eax") lo,
                out("edx") hi,
                options(nostack, nomem, preserves_flags)
            );
        }
        ((hi as u64) << 32) | lo as u64
    }

    fn rdtscp_lfence() -> u64 {
        let lo: u32;
        let hi: u32;
        let aux: u32;
        unsafe {
            asm!(
                "rdtscp",
                out("eax") lo,
                out("edx") hi,
                out("ecx") aux,
                options(nostack, nomem, preserves_flags)
            );
            asm!("lfence", options(nostack, nomem, preserves_flags));
        }
        let _ = aux;
        ((hi as u64) << 32) | lo as u64
    }

    fn clflush_addr(addr: *const u8) {
        unsafe {
            asm!("clflush [{}]", in(reg) addr, options(nostack));
        }
    }

    fn mfence_inst() {
        unsafe {
            asm!("mfence", options(nostack, nomem, preserves_flags));
        }
    }

    fn lfence_inst() {
        unsafe {
            asm!("lfence", options(nostack, nomem, preserves_flags));
        }
    }

    fn calibrate_tsc_ghz() -> f64 {
        let t0 = std::time::Instant::now();
        let c0 = rdtsc_lfence();
        std::thread::sleep(Duration::from_millis(100));
        let c1 = rdtscp_lfence();
        let elapsed_ns = t0.elapsed().as_nanos() as f64;
        (c1 - c0) as f64 / elapsed_ns
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn main() {
    app::main_impl();
}
