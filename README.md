# tailslayer-rs

`tailslayer-rs` is a Rust crate for hedged reads over hugepage-backed replicated lookup tables.

It stores the same `Copy` values in multiple channel-spaced replicas and lets one
worker per replica race the load. `HedgedRuntime::read()` returns the first
completed value for a logical index.

This repository is derived from the original C++ `tailslayer` project: https://github.com/LaurieWired/tailslayer

This crate is a redesign, not a direct API translation.

## Core Types

- `ReplicatedBuffer<T>` owns a logically indexed table replicated across hugetlb-backed placements.
- `HedgedRuntime<T>` spawns one worker per replica and serves one in-flight read at a time.

`ReplicatedBuffer` requires hugetlb-backed memory. The builder defaults to 1 GiB
pages to match the original implementation, and `HugePageSize::Size2MiB` is
available for smaller local setups.

## When It Fits

Use `tailslayer-rs` when all of these are true:

- the data is read-mostly or initialized once
- the lookup key is already an integer index
- tail latency matters more than memory efficiency
- replicating the table is acceptable

Reasonable examples:

- precomputed risk-limit tables keyed by instrument id
- routing or policy tables in a low-latency gateway
- dense lookup tables used in a hot scoring or inference path

It is not a good fit for general key/value storage, frequently mutated data, or
large non-`Copy` payloads.

## Example

```rust,no_run
use tailslayer::{HedgedRuntime, HugePageSize, ReplicatedBuffer};

let mut buffer = ReplicatedBuffer::<u8>::builder()
    .capacity(16)
    .replicas(2)
    .hugepage_size(HugePageSize::Size2MiB)
    .build()?;

buffer.extend_from_slice(&[0x43, 0x44])?;

let runtime = HedgedRuntime::builder(buffer).build()?;
let value = runtime.read(1)?;

assert_eq!(value, 0x44);
# Ok::<(), tailslayer::Error>(())
```

## Linux Requirements

The replicated buffer path depends on Linux hugetlb support. The crate also supports:

- worker CPU pinning
- `/proc/self/pagemap`-based channel validation

Buffer construction returns an error when the host does not support hugetlb
allocation or when the requested hugepages are not configured.

For host-specific CPU placement and channel assumptions, define a
`LinuxHardwareSpec` and apply it to the existing builders instead of relying on
crate-level hardcoded core ids.

For a lower-level Linux-only API closer to the original C++ design, use
`LinuxHedgedReader<T>`. It spawns one pinned worker per replica and runs
user-provided `wait_work` / `final_work` callbacks directly on those threads.
See [`examples/original_style.rs`](./examples/original_style.rs).

## Linux Hugepage Example

The repository includes a hugetlb-backed example at
[`examples/linux_hugetlb.rs`](./examples/linux_hugetlb.rs).

Typical setup for one 1 GiB hugepage:

```bash
sudo sh -c 'echo 1 > /sys/kernel/mm/hugepages/hugepages-1048576kB/nr_hugepages'
grep -E 'Huge|Hugetlb' /proc/meminfo
cargo run --release --example linux_hugetlb
```

Notes:

- the default builder uses `HugePageSize::Size1GiB`
- `linux_hugetlb` uses `HugePageSize::Size1GiB`
- `/proc/self/pagemap` validation may require elevated privileges on your host
- for smaller local runs, use `HugePageSize::Size2MiB` in your code

## Analysis Examples

The repository also includes Rust ports of the original DRAM analysis tools.

Hedged read benchmark:

```bash
cargo run --release --example hedged_read -- --all --channel-bit 8 --skip-phys-check
```

Use `--skip-phys-check` for unprivileged runs. Omit it when you want
`/proc/self/pagemap` validation and have the required privileges.

tREFI spike probe:

```bash
cargo run --release --example trefi_probe -- --probes 100000
```

## Development

```bash
cargo test
cargo bench --bench layout
```
