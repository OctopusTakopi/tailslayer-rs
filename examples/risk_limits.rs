use tailslayer::{HedgedRuntime, HugePageSize, ReplicatedBuffer};

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct RiskLimit {
    max_position: i32,
    max_order_size: i32,
    price_band_bps: u32,
    _reserved: u32,
}

fn main() -> tailslayer::Result<()> {
    // Logical index = instrument id in a precomputed table.
    let limits = [
        RiskLimit {
            max_position: 10,
            max_order_size: 2,
            price_band_bps: 25,
            _reserved: 0,
        },
        RiskLimit {
            max_position: 20,
            max_order_size: 5,
            price_band_bps: 20,
            _reserved: 0,
        },
        RiskLimit {
            max_position: 50,
            max_order_size: 10,
            price_band_bps: 15,
            _reserved: 0,
        },
        RiskLimit {
            max_position: 100,
            max_order_size: 25,
            price_band_bps: 10,
            _reserved: 0,
        },
    ];

    let mut buffer = ReplicatedBuffer::<RiskLimit>::builder()
        .capacity(limits.len())
        .replicas(2)
        .hugepage_size(HugePageSize::Size2MiB)
        .build()?;
    buffer.extend_from_slice(&limits)?;

    let runtime = HedgedRuntime::builder(buffer).build()?;

    let instrument_id = 2;
    let limit = runtime.read(instrument_id)?;

    println!(
        "instrument={instrument_id} max_position={} max_order_size={} price_band_bps={}",
        limit.max_position, limit.max_order_size, limit.price_band_bps
    );

    Ok(())
}
