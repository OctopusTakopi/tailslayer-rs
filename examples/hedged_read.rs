#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn main() {
    eprintln!("hedged_read is only supported on Linux x86_64");
    std::process::exit(1);
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod app {
    use clap::{Parser, ValueEnum};
    use std::arch::asm;
    use std::fs::File;
    use std::hint::{black_box, spin_loop};
    use std::io::{self, BufWriter, ErrorKind, Write};
    use std::mem;
    use std::os::unix::fs::FileExt;
    use std::path::{Path, PathBuf};
    use std::ptr::{self, NonNull};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::thread;
    use std::time::Duration;

    const SUPERPAGE_SIZE: usize = 1 << 30;
    const DEFAULT_CHANNEL_BIT: usize = 8;
    const DEFAULT_NUM_CHANNELS: usize = 2;
    const DEFAULT_SAMPLES: usize = 5_000_000;
    const DEFAULT_STRESS_THREADS: usize = 4;
    const WARMUP_ITERS: usize = 5_000;
    const MAX_PAIR_GAP: u64 = 400;
    const MAX_STRESS_THREADS: usize = 16;
    const DEFAULT_CORE_A: usize = 11;
    const DEFAULT_CORE_B: usize = 12;
    const DEFAULT_CORE_MAIN: usize = 14;
    const STRESS_CORES: [usize; 10] = [8, 9, 10, 13, 15, 24, 25, 26, 29, 31];

    #[derive(Clone, Copy, Debug, ValueEnum)]
    enum Arm {
        #[value(alias = "dual_quiet")]
        HedgedQuiet,
        SingleQuiet,
        #[value(alias = "dual_stress")]
        HedgedStress,
        SingleStress,
    }

    impl Arm {
        fn label(self) -> &'static str {
            match self {
                Self::SingleQuiet => "single_quiet",
                Self::HedgedQuiet => "hedged_quiet",
                Self::SingleStress => "single_stress",
                Self::HedgedStress => "hedged_stress",
            }
        }

        fn needs_stress(self) -> bool {
            matches!(self, Self::SingleStress | Self::HedgedStress)
        }
    }

    #[derive(Debug, Parser)]
    #[command(name = "hedged_read", about = "Channel-hedged DRAM read benchmark")]
    struct Cli {
        #[arg(long)]
        all: bool,
        #[arg(long, value_enum)]
        arm: Vec<Arm>,
        #[arg(long, default_value_t = DEFAULT_SAMPLES)]
        samples: usize,
        #[arg(long = "stress-threads", default_value_t = DEFAULT_STRESS_THREADS)]
        stress_threads: usize,
        #[arg(long = "core-a", default_value_t = DEFAULT_CORE_A)]
        core_a: usize,
        #[arg(long = "core-b", default_value_t = DEFAULT_CORE_B)]
        core_b: usize,
        #[arg(long = "core-main", default_value_t = DEFAULT_CORE_MAIN)]
        core_main: usize,
        #[arg(long = "channel-bit", default_value_t = DEFAULT_CHANNEL_BIT)]
        channel_bit: usize,
        #[arg(long = "channel-offset")]
        channel_offset: Option<usize>,
        #[arg(long = "channels", default_value_t = DEFAULT_NUM_CHANNELS)]
        channels: usize,
        #[arg(long = "raw-prefix")]
        raw_prefix: Option<PathBuf>,
        #[arg(long = "skip-phys-check")]
        skip_phys_check: bool,
    }

    #[derive(Clone, Debug)]
    struct Config {
        arms: Vec<Arm>,
        samples: usize,
        stress_threads: usize,
        core_a: usize,
        core_b: usize,
        core_main: usize,
        channel_bit: usize,
        channel_offset: usize,
        channels: usize,
        raw_prefix: Option<PathBuf>,
        skip_phys_check: bool,
    }

    impl Config {
        fn from_cli(cli: Cli) -> io::Result<Self> {
            let arms = if cli.all {
                vec![
                    Arm::SingleQuiet,
                    Arm::HedgedQuiet,
                    Arm::SingleStress,
                    Arm::HedgedStress,
                ]
            } else {
                cli.arm
            };

            if arms.is_empty() {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "select at least one arm or pass --all",
                ));
            }
            if cli.channels == 0 {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "--channels must be greater than zero",
                ));
            }
            let channel_offset = cli
                .channel_offset
                .unwrap_or_else(|| 1_usize << cli.channel_bit);
            if channel_offset == 0 {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "channel offset must be greater than zero",
                ));
            }

            Ok(Self {
                arms,
                samples: cli.samples,
                stress_threads: usize::min(cli.stress_threads, MAX_STRESS_THREADS),
                core_a: cli.core_a,
                core_b: cli.core_b,
                core_main: cli.core_main,
                channel_bit: cli.channel_bit,
                channel_offset,
                channels: cli.channels,
                raw_prefix: cli.raw_prefix,
                skip_phys_check: cli.skip_phys_check,
            })
        }

        fn requires_stress_page(&self) -> bool {
            self.arms.iter().copied().any(Arm::needs_stress)
        }

        fn replica_cores(&self) -> Vec<usize> {
            let mut cores = Vec::with_capacity(self.channels);
            cores.push(self.core_a);
            if self.channels > 1 {
                cores.push(self.core_b);
            }
            for replica in 2..self.channels {
                cores.push(self.core_b + replica - 1);
            }
            cores
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct Sample {
        timestamp: u64,
        latency: u64,
    }

    #[derive(Clone, Copy, Debug, Default)]
    struct Percentiles {
        min: u64,
        p50: u64,
        p90: u64,
        p95: u64,
        p99: u64,
        p999: u64,
        p9999: u64,
        max: u64,
        mean: f64,
    }

    struct Stats<'a> {
        tsc_ghz: f64,
        raw_prefix: Option<&'a Path>,
    }

    impl<'a> Stats<'a> {
        fn new(tsc_ghz: f64, raw_prefix: Option<&'a Path>) -> Self {
            Self {
                tsc_ghz,
                raw_prefix,
            }
        }

        fn print_percentiles(&self, label: &str, n: usize, p: Percentiles) {
            eprintln!("\n=== ARM: {label} (n={n}) ===");
            eprintln!(
                "  min={} ({:.1}ns)  p50={} ({:.1}ns)  p90={} ({:.1}ns)  p95={} ({:.1}ns)",
                p.min,
                p.min as f64 / self.tsc_ghz,
                p.p50,
                p.p50 as f64 / self.tsc_ghz,
                p.p90,
                p.p90 as f64 / self.tsc_ghz,
                p.p95,
                p.p95 as f64 / self.tsc_ghz
            );
            eprintln!(
                "  p99={} ({:.1}ns)  p99.9={} ({:.1}ns)  p99.99={} ({:.1}ns)  max={} ({:.1}ns)",
                p.p99,
                p.p99 as f64 / self.tsc_ghz,
                p.p999,
                p.p999 as f64 / self.tsc_ghz,
                p.p9999,
                p.p9999 as f64 / self.tsc_ghz,
                p.max,
                p.max as f64 / self.tsc_ghz
            );
            eprintln!("  mean={:.1} ({:.1}ns)", p.mean, p.mean / self.tsc_ghz);
        }

        fn emit_csv_row(&self, arm: &str, n_samples: usize, n_paired: usize, p: Percentiles) {
            println!(
                "{arm},{n_samples},{n_paired},{:.3},{},{},{},{},{},{},{},{},{:.1}",
                self.tsc_ghz, p.min, p.p50, p.p90, p.p95, p.p99, p.p999, p.p9999, p.max, p.mean
            );
        }

        fn dump_raw_latencies(&self, label: &str, latencies: &[u64]) -> io::Result<()> {
            let Some(prefix) = self.raw_prefix else {
                return Ok(());
            };
            let file_name = format!("{}_{}.csv", prefix.to_string_lossy(), label);
            let file = File::create(file_name)?;
            let mut writer = BufWriter::new(file);
            writeln!(writer, "latency_cyc")?;
            for latency in latencies {
                writeln!(writer, "{latency}")?;
            }
            writer.flush()
        }

        fn report_stride(&self, label: &str, samples: &[Sample]) {
            if samples.len() < 2 {
                return;
            }

            let count = usize::min(samples.len() - 1, 10_000);
            let mut strides = Vec::with_capacity(count);
            for pair in samples.windows(2).take(count) {
                strides.push(pair[1].timestamp - pair[0].timestamp);
            }
            strides.sort_unstable();
            eprintln!(
                "  {label} stride: min={} p50={} p99={} max={} (first {} samples)",
                strides[0],
                strides[count / 2],
                strides[usize::min(count - 1, (count as f64 * 0.99) as usize)],
                strides[count - 1],
                count
            );
        }
    }

    struct MappedRegion {
        ptr: NonNull<u8>,
        len: usize,
    }

    impl MappedRegion {
        fn huge_1g() -> io::Result<Self> {
            let flags = libc::MAP_PRIVATE
                | libc::MAP_ANONYMOUS
                | libc::MAP_HUGETLB
                | (30 << libc::MAP_HUGE_SHIFT);
            let ptr = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    SUPERPAGE_SIZE,
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
                libc::mlock(ptr, SUPERPAGE_SIZE);
            }
            Ok(Self {
                ptr: NonNull::new(ptr.cast::<u8>())
                    .expect("successful mmap must return a non-null pointer"),
                len: SUPERPAGE_SIZE,
            })
        }

        fn fill(&mut self, value: u8) {
            unsafe {
                ptr::write_bytes(self.ptr.as_ptr(), value, self.len);
            }
        }

        fn as_ptr(&self) -> *mut u8 {
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

    struct MemorySetup {
        replicas: Vec<usize>,
        replica_page: MappedRegion,
        stress_page: Option<MappedRegion>,
    }

    impl MemorySetup {
        fn new(config: &Config) -> io::Result<Self> {
            let mut replica_page = MappedRegion::huge_1g()?;
            replica_page.fill(0x42);

            let base = replica_page.as_ptr();
            let mut replicas = Vec::with_capacity(config.channels);
            replicas.push(base as usize);
            for replica in 1..config.channels {
                let replica_ptr = unsafe { base.add(replica * config.channel_offset) };
                unsafe {
                    ptr::copy_nonoverlapping(base, replica_ptr, 64);
                }
                replicas.push(replica_ptr as usize);
            }

            if config.skip_phys_check {
                eprintln!("Skipping physical address and channel validation");
            } else {
                let mut channels = Vec::with_capacity(replicas.len());
                for (replica, addr) in replicas.iter().copied().enumerate() {
                    let phys = virt_to_phys(addr as *const u8)?;
                    let channel = compute_channel(phys, config.channel_bit);
                    eprintln!(
                        "replica_{replica}: virt={:p} phys=0x{phys:x} channel={channel}",
                        addr as *const u8
                    );
                    channels.push(channel);
                }

                for left in 0..channels.len() {
                    for right in (left + 1)..channels.len() {
                        if channels[left] == channels[right] {
                            return Err(io::Error::new(
                                ErrorKind::InvalidData,
                                format!(
                                    "replicas {left} and {right} resolved to the same channel ({})",
                                    channels[left]
                                ),
                            ));
                        }
                    }
                }
            }

            let stress_page = if config.requires_stress_page() {
                let mut region = MappedRegion::huge_1g()?;
                region.fill(0xab);
                Some(region)
            } else {
                None
            };

            Ok(Self {
                replicas,
                replica_page,
                stress_page,
            })
        }
    }

    struct BenchmarkRunner<'a> {
        config: &'a Config,
        tsc_ghz: f64,
        measure_signal: Arc<AtomicBool>,
    }

    impl<'a> BenchmarkRunner<'a> {
        fn new(config: &'a Config, tsc_ghz: f64) -> Self {
            Self {
                config,
                tsc_ghz,
                measure_signal: Arc::new(AtomicBool::new(false)),
            }
        }

        fn reset(&self) {
            self.measure_signal.store(false, Ordering::Release);
        }

        fn run_arm(
            &self,
            arm: Arm,
            addrs: &[usize],
            cores: &[usize],
            stress_region: Option<usize>,
        ) -> io::Result<()> {
            eprintln!("\n--- Starting arm: {} ---", arm.label());
            let n_channels = addrs.len();
            let stress_group = StressGroup::start(self.config.stress_threads, stress_region)?;

            let mut handles = Vec::with_capacity(n_channels);
            for (&addr, &core_id) in addrs.iter().zip(cores.iter()) {
                let signal = Arc::clone(&self.measure_signal);
                let samples = self.config.samples;
                handles.push(thread::spawn(move || {
                    measurement_thread(addr, core_id, samples, signal)
                }));
            }

            self.measure_signal.store(true, Ordering::Release);

            let mut results = Vec::with_capacity(n_channels);
            for handle in handles {
                let samples = handle
                    .join()
                    .map_err(|_| io::Error::other("measurement thread panicked"))??;
                results.push(samples);
            }

            stress_group.stop()?;
            self.process_and_write(arm.label(), &results)
        }

        fn process_and_write(&self, name: &str, channel_samples: &[Vec<Sample>]) -> io::Result<()> {
            let stats = Stats::new(self.tsc_ghz, self.config.raw_prefix.as_deref());
            let n_channels = channel_samples.len();

            for (channel, samples) in channel_samples.iter().enumerate() {
                let label = if n_channels == 1 {
                    name.to_string()
                } else {
                    let label = format!("{name}_ch{channel}");
                    stats.report_stride(&label, samples);
                    label
                };

                let mut latencies: Vec<u64> = samples.iter().map(|sample| sample.latency).collect();
                let percentiles = compute_percentiles(&mut latencies);
                stats.print_percentiles(&label, samples.len(), percentiles);
                stats.emit_csv_row(
                    &label,
                    self.config.samples,
                    self.config.samples,
                    percentiles,
                );
                stats.dump_raw_latencies(&label, &latencies)?;
            }

            if n_channels > 1 {
                let mut effective = Vec::with_capacity(self.config.samples);
                let paired = pair_samples_n(channel_samples, self.config.samples, &mut effective);
                eprintln!(
                    "  Pairing: {paired}/{} samples paired across {n_channels} channels ({:.1}%)",
                    self.config.samples,
                    100.0 * paired as f64 / self.config.samples as f64
                );

                let percentiles = compute_percentiles(&mut effective);
                stats.print_percentiles(name, paired, percentiles);
                stats.emit_csv_row(name, self.config.samples, paired, percentiles);
                stats.dump_raw_latencies(name, &effective)?;
            }

            Ok(())
        }
    }

    struct StressGroup {
        stop: Arc<AtomicBool>,
        handles: Vec<thread::JoinHandle<io::Result<()>>>,
    }

    impl StressGroup {
        fn start(count: usize, region: Option<usize>) -> io::Result<Self> {
            let Some(region) = region else {
                return Ok(Self {
                    stop: Arc::new(AtomicBool::new(false)),
                    handles: Vec::new(),
                });
            };

            let go = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let mut handles = Vec::with_capacity(count);

            for stress_thread_index in 0..count {
                let go_signal = Arc::clone(&go);
                let stop_signal = Arc::clone(&stop);
                let core = STRESS_CORES[stress_thread_index % STRESS_CORES.len()];
                handles.push(thread::spawn(move || {
                    stress_thread(region, SUPERPAGE_SIZE, core, go_signal, stop_signal)
                }));
            }

            go.store(true, Ordering::Release);
            thread::sleep(Duration::from_millis(50));

            Ok(Self { stop, handles })
        }

        fn stop(self) -> io::Result<()> {
            self.stop.store(true, Ordering::Release);
            for handle in self.handles {
                handle
                    .join()
                    .map_err(|_| io::Error::other("stress thread panicked"))??;
            }
            Ok(())
        }
    }

    fn measurement_thread(
        addr: usize,
        core_id: usize,
        n_samples: usize,
        signal: Arc<AtomicBool>,
    ) -> io::Result<Vec<Sample>> {
        pin_to_core(core_id)?;
        while !signal.load(Ordering::Acquire) {
            spin_loop();
        }

        let addr = addr as *const u8;
        for _ in 0..WARMUP_ITERS {
            clflush_addr(addr);
            mfence_inst();
            lfence_inst();
            let _ = rdtsc_lfence();
            let value = unsafe { ptr::read_volatile(addr) };
            black_box(value);
            let _ = rdtscp_lfence();
        }

        let mut samples = Vec::with_capacity(n_samples);
        for _ in 0..n_samples {
            clflush_addr(addr);
            mfence_inst();
            lfence_inst();
            let t0 = rdtsc_lfence();
            let value = unsafe { ptr::read_volatile(addr) };
            black_box(value);
            let t1 = rdtscp_lfence();
            samples.push(Sample {
                timestamp: t0,
                latency: t1 - t0,
            });
        }
        Ok(samples)
    }

    fn stress_thread(
        region: usize,
        region_size: usize,
        core_id: usize,
        go: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
    ) -> io::Result<()> {
        pin_to_core(core_id)?;
        while !go.load(Ordering::Acquire) {
            spin_loop();
        }

        let region = region as *const u8;
        let mask = (region_size - 1) & !63_usize;
        let mut state = 0xdead_beef_1234_5678_u64 ^ region as u64;

        while !stop.load(Ordering::Acquire) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;

            let offset = (state as usize) & mask;
            let target = unsafe { region.add(offset) };
            clflush_addr(target);
            mfence_inst();
            let value = unsafe { ptr::read_volatile(target) };
            black_box(value);
            mfence_inst();
        }

        Ok(())
    }

    fn compute_percentiles(data: &mut [u64]) -> Percentiles {
        if data.is_empty() {
            return Percentiles::default();
        }

        data.sort_unstable();
        let len = data.len();
        let sum: u128 = data.iter().map(|&value| value as u128).sum();

        Percentiles {
            min: data[0],
            p50: data[((len as f64) * 0.50) as usize],
            p90: data[((len as f64) * 0.90) as usize],
            p95: data[((len as f64) * 0.95) as usize],
            p99: data[((len as f64) * 0.99) as usize],
            p999: data[usize::min(len - 1, ((len as f64) * 0.999) as usize)],
            p9999: data[usize::min(len - 1, ((len as f64) * 0.9999) as usize)],
            max: data[len - 1],
            mean: sum as f64 / len as f64,
        }
    }

    fn pair_samples_n(
        all_samples: &[Vec<Sample>],
        num_samples: usize,
        out_effective: &mut Vec<u64>,
    ) -> usize {
        if all_samples.is_empty() {
            return 0;
        }

        let mut indices = vec![0_usize; all_samples.len()];
        loop {
            let mut min_timestamp = u64::MAX;
            let mut max_timestamp = 0_u64;
            let mut min_channel = None;
            let mut min_latency = u64::MAX;

            for (channel, samples) in all_samples.iter().enumerate() {
                if indices[channel] >= num_samples {
                    return out_effective.len();
                }

                let sample = samples[indices[channel]];
                if sample.timestamp < min_timestamp {
                    min_timestamp = sample.timestamp;
                    min_channel = Some(channel);
                }
                if sample.timestamp > max_timestamp {
                    max_timestamp = sample.timestamp;
                }
                if sample.latency < min_latency {
                    min_latency = sample.latency;
                }
            }

            if max_timestamp - min_timestamp < MAX_PAIR_GAP {
                out_effective.push(min_latency);
                for index in &mut indices {
                    *index += 1;
                }
            } else if let Some(channel) = min_channel {
                indices[channel] += 1;
            }
        }
    }

    fn pin_to_core(core_id: usize) -> io::Result<()> {
        let mut cpuset: libc::cpu_set_t = unsafe { mem::zeroed() };
        unsafe {
            libc::CPU_ZERO(&mut cpuset);
            libc::CPU_SET(core_id, &mut cpuset);
        }
        let rc = unsafe { libc::sched_setaffinity(0, mem::size_of::<libc::cpu_set_t>(), &cpuset) };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn virt_to_phys(addr: *const u8) -> io::Result<u64> {
        let pagemap = File::open("/proc/self/pagemap")?;
        let offset = (addr as u64 / 4096) * 8;
        let mut entry = [0_u8; 8];
        let read = pagemap.read_at(&mut entry, offset)?;
        if read != entry.len() {
            return Err(io::Error::new(
                ErrorKind::UnexpectedEof,
                "short read from /proc/self/pagemap",
            ));
        }

        let entry = u64::from_ne_bytes(entry);
        if entry & (1_u64 << 63) == 0 {
            return Err(io::Error::new(ErrorKind::InvalidData, "page not present"));
        }

        let pfn = entry & ((1_u64 << 55) - 1);
        if pfn == 0 {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                "PFN unavailable from /proc/self/pagemap; run with elevated privileges",
            ));
        }

        Ok((pfn * 4096) | (addr as u64 & 0xfff))
    }

    fn compute_channel(phys: u64, channel_bit: usize) -> usize {
        ((phys >> channel_bit) & 1) as usize
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
        thread::sleep(Duration::from_millis(100));
        let c1 = rdtscp_lfence();
        let elapsed_ns = t0.elapsed().as_nanos() as f64;
        (c1 - c0) as f64 / elapsed_ns
    }

    fn run() -> io::Result<()> {
        let config = Config::from_cli(Cli::parse())?;
        pin_to_core(config.core_main)?;
        eprintln!("Main thread pinned to core {}", config.core_main);

        let tsc_ghz = calibrate_tsc_ghz();
        eprintln!("TSC frequency: {tsc_ghz:.3} GHz");

        let memory = MemorySetup::new(&config)?;
        let stress = memory
            .stress_page
            .as_ref()
            .map(|region| region.as_ptr() as usize);
        let _replica_page = memory.replica_page.as_ptr();

        println!(
            "arm,n_samples,n_paired,tsc_ghz,min_cyc,p50_cyc,p90_cyc,p95_cyc,p99_cyc,p999_cyc,p9999_cyc,max_cyc,mean_cyc"
        );

        let runner = BenchmarkRunner::new(&config, tsc_ghz);
        let cores = config.replica_cores();
        for arm in config.arms.iter().copied() {
            match arm {
                Arm::SingleQuiet => {
                    runner.run_arm(arm, &memory.replicas[0..1], &cores[0..1], None)?
                }
                Arm::HedgedQuiet => runner.run_arm(arm, &memory.replicas, &cores, None)?,
                Arm::SingleStress => {
                    runner.run_arm(arm, &memory.replicas[0..1], &cores[0..1], stress)?
                }
                Arm::HedgedStress => runner.run_arm(arm, &memory.replicas, &cores, stress)?,
            }
            runner.reset();
        }

        Ok(())
    }

    pub fn main_impl() {
        if let Err(error) = run() {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn main() {
    app::main_impl();
}
