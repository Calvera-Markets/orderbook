//! Replay a Databento MBO tape against calvera-books.
//!
//! Mirrors `ordertruques/orderbook`'s `orderbook_throughput.cpp` and
//! `orderbook_latency.cpp`: decode A/C/M events, warm the first 100k ops,
//! drain the book (we have no `reset()`), then replay the whole tape.
//!
//! Throughput is wall-clock of the replay. Latency is a **per-op** sample
//! (not a criterion average) so we can print P50 / P99 / P99.9.
//!
//! Identity: the tape's venue `order_id` is mapped to an engine `OrderHandle`
//! in a HashMap that sits **inside** the timed region — same work an OMS
//! does, and the same lookup their C++ `OrderMap` does inside `cancelOrder`.
//!
//! Modify: we have no modify API. Default is cancel+add (matches their
//! price-change / size-up path; overstates their in-place size-down).
//! `--modify skip` drops `M` events instead.
//!
//! Usage:
//!   cargo run -p calvera-books --release --example tape_replay -- \
//!     --tape path/to/xnas-itch-20251110.mbo.dbn.zst
//!   cargo run -p calvera-books --release --example tape_replay -- \
//!     --synthetic 200000 --impl both
//!
//! The `.dbn.zst` is not in git. Path via `--tape` or `TAPE`.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use calvera_books::api::OrderBookApi;
use calvera_books::orderbooks::orderbook_2::{
    Fill as Fill2, FillConsumer as FillConsumer2, OrderBook as OB2,
};
use calvera_books::orderbooks::orderbook_legacy::{
    Fill as Fill1, FillConsumer as FillConsumer1, OrderBook as OB1,
};
use calvera_books::types::{Price, Side, SlabAllocator};

// ---------------------------------------------------------------------------
// Null fill sink — tape is rest/cancel dominated; don't grow a Vec.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct NullConsumer;

impl FillConsumer1 for NullConsumer {
    #[inline(always)]
    fn on_fill(&mut self, _: Fill1) {}
}

impl FillConsumer2 for NullConsumer {
    #[inline(always)]
    fn on_fill(&mut self, _: Fill2) {}
}

// ---------------------------------------------------------------------------
// Tape
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Action {
    Add,
    Cancel,
    Modify,
}

#[derive(Clone, Copy)]
struct TapeOp {
    action: Action,
    id: u64,
    side: Side,
    price: Price,
    qty: u64,
}

struct TapeStats {
    adds: usize,
    cancels: usize,
    modifies: usize,
    skipped_side: usize,
    skipped_price: usize,
    skipped_other: usize,
}

impl TapeStats {
    fn kept(&self) -> usize {
        self.adds + self.cancels + self.modifies
    }
}

fn load_dbn(path: &Path, modify: ModifyMode, limit: Option<usize>) -> (Vec<TapeOp>, TapeStats) {
    use dbn::decode::{DbnDecoder, DecodeRecordRef};
    use dbn::MboMsg;

    let mut decoder = DbnDecoder::from_zstd_file(path).unwrap_or_else(|e| {
        eprintln!("failed to open {}: {e}", path.display());
        std::process::exit(2);
    });

    let mut ops = Vec::with_capacity(limit.unwrap_or(20_000_000));
    let mut stats = TapeStats {
        adds: 0,
        cancels: 0,
        modifies: 0,
        skipped_side: 0,
        skipped_price: 0,
        skipped_other: 0,
    };

    while let Some(rec) = decoder.decode_record_ref().unwrap_or_else(|e| {
        eprintln!("dbn decode error: {e}");
        std::process::exit(2);
    }) {
        let Some(msg) = rec.get::<MboMsg>() else {
            stats.skipped_other += 1;
            continue;
        };

        let action = match msg.action as u8 {
            b'A' => Action::Add,
            b'C' => Action::Cancel,
            b'M' => {
                if matches!(modify, ModifyMode::Skip) {
                    stats.skipped_other += 1;
                    continue;
                }
                Action::Modify
            }
            _ => {
                stats.skipped_other += 1;
                continue;
            }
        };

        let side = match msg.side as u8 {
            b'B' => Side::Bid,
            b'A' => Side::Ask,
            _ => {
                stats.skipped_side += 1;
                continue;
            }
        };

        // Databento prices are int64 with 1e-9 units. C++ passes them through
        // as `int64_t`. We only accept non-negative values (`Price` is u64).
        if msg.price < 0 || msg.price == dbn::UNDEF_PRICE {
            stats.skipped_price += 1;
            continue;
        }

        match action {
            Action::Add => stats.adds += 1,
            Action::Cancel => stats.cancels += 1,
            Action::Modify => stats.modifies += 1,
        }

        ops.push(TapeOp {
            action,
            id: msg.order_id,
            side,
            price: Price(msg.price as u64),
            qty: msg.size as u64,
        });

        if let Some(n) = limit {
            if ops.len() >= n {
                break;
            }
        }
    }

    (ops, stats)
}

/// Stand-in when the Databento file is not on disk.
///
/// Not the old 1-live-order add/cancel flicker: seed a few thousand resting
/// orders across 50 ticks, then mix add / cancel / modify while holding
/// occupancy around that depth — closer to an MBO day than a micro-bench.
fn synthetic_tape(n: usize) -> (Vec<TapeOp>, TapeStats) {
    const MID: u64 = 10_000;
    const SPREAD: u64 = 50;
    const TARGET_LIVE: usize = 4_000;

    struct Live {
        id: u64,
        side: Side,
        price: Price,
        qty: u64,
    }

    let mut ops = Vec::with_capacity(n);
    let mut live: Vec<Live> = Vec::with_capacity(TARGET_LIVE * 2);
    let mut next_id = 1u64;
    let mut rng = 0xC0FFEE_u64;
    let mut adds = 0usize;
    let mut cancels = 0usize;
    let mut modifies = 0usize;

    let mut next_u64 = || {
        // xorshift64
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };

    let mut push_add = |ops: &mut Vec<TapeOp>, live: &mut Vec<Live>, next_id: &mut u64, side: Side, price: Price, qty: u64| {
        let id = *next_id;
        *next_id += 1;
        ops.push(TapeOp { action: Action::Add, id, side, price, qty });
        live.push(Live { id, side, price, qty });
        adds += 1;
    };

    // Seed a two-sided book before the mixed stream.
    let seed = TARGET_LIVE.min(n);
    for i in 0..seed {
        let side = if i % 2 == 0 { Side::Bid } else { Side::Ask };
        let offset = (i as u64 / 2) % SPREAD + 1;
        let price = match side {
            Side::Bid => Price(MID - offset),
            Side::Ask => Price(MID + offset),
        };
        push_add(&mut ops, &mut live, &mut next_id, side, price, 1);
    }

    while ops.len() < n {
        let roll = next_u64() % 100;
        let occupancy = live.len();
        let force_add = occupancy < TARGET_LIVE / 2;
        let force_cancel = occupancy > TARGET_LIVE * 2 && !live.is_empty();

        if force_cancel || (!force_add && !live.is_empty() && roll < 45) {
            let idx = (next_u64() as usize) % live.len();
            let gone = live.swap_remove(idx);
            ops.push(TapeOp {
                action: Action::Cancel,
                id: gone.id,
                side: gone.side,
                price: gone.price,
                qty: gone.qty,
            });
            cancels += 1;
        } else if !force_add && !live.is_empty() && roll < 55 {
            let idx = (next_u64() as usize) % live.len();
            let qty = (next_u64() % 4) + 1;
            let offset = (next_u64() % SPREAD) + 1;
            let side = live[idx].side;
            let price = match side {
                Side::Bid => Price(MID - offset),
                Side::Ask => Price(MID + offset),
            };
            let id = live[idx].id;
            live[idx].price = price;
            live[idx].qty = qty;
            ops.push(TapeOp { action: Action::Modify, id, side, price, qty });
            modifies += 1;
        } else {
            let side = if next_u64() % 2 == 0 { Side::Bid } else { Side::Ask };
            let offset = (next_u64() % SPREAD) + 1;
            let price = match side {
                Side::Bid => Price(MID - offset),
                Side::Ask => Price(MID + offset),
            };
            let qty = (next_u64() % 4) + 1;
            push_add(&mut ops, &mut live, &mut next_id, side, price, qty);
        }
    }

    (
        ops,
        TapeStats {
            adds,
            cancels,
            modifies,
            skipped_side: 0,
            skipped_price: 0,
            skipped_other: 0,
        },
    )
}

// ---------------------------------------------------------------------------
// Replay
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum ModifyMode {
    Emulate,
    Skip,
}

struct ReplayStats {
    adds: usize,
    cancels: usize,
    modifies: usize,
    cancel_miss: usize,
    add_dup: usize,
    slab_full: usize,
    live: usize,
}

struct Replayer<B: OrderBookApi> {
    book: B,
    handles: HashMap<u64, B::Handle>,
}

impl<B: OrderBookApi> Replayer<B> {
    fn new(slab_cap: usize, alloc: SlabAllocator) -> Self {
        let book = B::new_with_alloc(slab_cap, alloc).unwrap_or_else(|e| {
            eprintln!("failed to construct book ({e:?})");
            std::process::exit(2);
        });
        Self {
            book,
            handles: HashMap::with_capacity(1 << 20),
        }
    }

    /// Apply one tape op. Timed region includes the HashMap lookup.
    #[inline(always)]
    fn apply(&mut self, op: &TapeOp, stats: &mut ReplayStats) {
        match op.action {
            Action::Add => {
                if self.handles.contains_key(&op.id) {
                    stats.add_dup += 1;
                    return;
                }
                match self.book.add_limit(op.side, op.price, op.qty) {
                    Ok(Some(h)) => {
                        self.handles.insert(op.id, h);
                        stats.adds += 1;
                    }
                    Ok(None) => {
                        // Fully consumed on entry. Unusual on a reconstructed
                        // MBO tape (adds are already-resting); still a valid op.
                        stats.adds += 1;
                    }
                    Err(_) => stats.slab_full += 1,
                }
            }
            Action::Cancel => match self.handles.remove(&op.id) {
                Some(h) => {
                    let _ = self.book.cancel(h);
                    stats.cancels += 1;
                }
                None => stats.cancel_miss += 1,
            },
            Action::Modify => {
                if let Some(h) = self.handles.remove(&op.id) {
                    let _ = self.book.cancel(h);
                } else {
                    stats.cancel_miss += 1;
                }
                match self.book.add_limit(op.side, op.price, op.qty) {
                    Ok(Some(h)) => {
                        self.handles.insert(op.id, h);
                        stats.modifies += 1;
                    }
                    Ok(None) => stats.modifies += 1,
                    Err(_) => stats.slab_full += 1,
                }
            }
        }
    }

    fn drain(&mut self) -> usize {
        let n = self.handles.len();
        for h in self.handles.drain().map(|(_, h)| h) {
            let _ = self.book.cancel(h);
        }
        n
    }
}

// ---------------------------------------------------------------------------
// Clocks
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum ClockKind {
    Instant,
    #[cfg(target_arch = "x86_64")]
    Rdtsc,
}

struct Clock {
    kind: ClockKind,
    /// Cycles per nanosecond when `kind == Rdtsc`. 1.0 for Instant (already ns).
    cycles_per_ns: f64,
}

impl Clock {
    fn detect() -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            return Self {
                kind: ClockKind::Rdtsc,
                cycles_per_ns: calibrate_rdtsc(),
            };
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            Self {
                kind: ClockKind::Instant,
                cycles_per_ns: 1.0,
            }
        }
    }

    fn name(&self) -> &'static str {
        match self.kind {
            ClockKind::Instant => "Instant",
            #[cfg(target_arch = "x86_64")]
            ClockKind::Rdtsc => "rdtsc+lfence",
        }
    }

    fn to_ns(&self, delta: u64) -> f64 {
        delta as f64 / self.cycles_per_ns
    }
}

/// `Instant`-based clock reads an epoch, so we snapshot a start once and
/// subtract. For Instant we store that start in the `Clock` itself via a
/// companion. Simpler: Instant path records `as_nanos` of a dedicated
/// `Instant` held next to the clock. See `ReplayClock`.
struct ReplayClock {
    inner: Clock,
    origin: Instant,
}

impl ReplayClock {
    fn new(inner: Clock) -> Self {
        Self {
            inner,
            origin: Instant::now(),
        }
    }

    #[inline(always)]
    fn read(&self) -> u64 {
        match self.inner.kind {
            ClockKind::Instant => self.origin.elapsed().as_nanos() as u64,
            #[cfg(target_arch = "x86_64")]
            ClockKind::Rdtsc => rdtsc_serialized(),
        }
    }

    fn to_ns(&self, delta: u64) -> f64 {
        self.inner.to_ns(delta)
    }

    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn ghz(&self) -> Option<f64> {
        match self.inner.kind {
            ClockKind::Instant => None,
            #[cfg(target_arch = "x86_64")]
            ClockKind::Rdtsc => Some(self.inner.cycles_per_ns),
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn rdtsc_serialized() -> u64 {
    // Match ordertruques: lfence around the counter so the timed region
    // cannot be reordered around the sample. They use RDPMC; rdtsc is the
    // always-available stand-in (no /sys/devices/cpu/rdpmc needed).
    unsafe {
        std::arch::x86_64::_mm_lfence();
        let t = std::arch::x86_64::_rdtsc();
        std::arch::x86_64::_mm_lfence();
        t
    }
}

#[cfg(target_arch = "x86_64")]
fn calibrate_rdtsc() -> f64 {
    let wall = Instant::now();
    let c0 = rdtsc_serialized();
    std::thread::sleep(Duration::from_millis(200));
    let c1 = rdtsc_serialized();
    let ns = wall.elapsed().as_nanos().max(1) as f64;
    (c1.saturating_sub(c0)) as f64 / ns
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum ImplChoice {
    V1,
    V2,
    Both,
}

struct Config {
    tape: Option<PathBuf>,
    synthetic: Option<usize>,
    which: ImplChoice,
    slab: usize,
    alloc: SlabAllocator,
    warmup: usize,
    modify: ModifyMode,
    limit: Option<usize>,
    csv: Option<PathBuf>,
    latency: bool,
    pin: Option<usize>,
}

fn print_usage() {
    eprintln!(
        "\
Replay a Databento MBO tape (or a synthetic stand-in) against calvera-books.

Usage:
  tape_replay --tape <file.dbn.zst> [options]
  tape_replay --synthetic <n> [options]

Options:
  --tape <path>         Databento `.dbn.zst` (or env TAPE)
  --synthetic <n>       Generate n add/cancel ops instead of decoding a file
  --impl v1|v2|both     Engine variant (default: both)
  --slab <n>            Slab capacity (default: 20M for a tape, 2N for --synthetic)
  --alloc system|madvise|hugetlb
  --warmup <n>          Untimed prefix, then drain (default: 100000)
  --modify emulate|skip How to handle M (default: emulate = cancel+add)
  --limit <n>           Decode / generate at most n kept ops
  --csv <path>          Write per-op latency_ns (one column)
  --no-latency          Throughput only (skip the per-op histogram)
  --pin <cpu>           Linux: pin this thread to a core
  -h, --help
"
    );
}

fn parse_args() -> Config {
    let mut tape = std::env::var_os("TAPE").map(PathBuf::from);
    let mut synthetic = None;
    let mut which = ImplChoice::Both;
    let mut slab = None;
    let mut alloc = SlabAllocator::System;
    let mut warmup = 100_000usize;
    let mut modify = ModifyMode::Emulate;
    let mut limit = None;
    let mut csv = None;
    let mut latency = true;
    let mut pin = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let take = |args: &mut std::iter::Skip<std::env::Args>, flag: &str| -> String {
            args.next().unwrap_or_else(|| {
                eprintln!("{flag} requires a value");
                std::process::exit(2);
            })
        };
        match arg.as_str() {
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            "--tape" => tape = Some(PathBuf::from(take(&mut args, "--tape"))),
            "--synthetic" => {
                synthetic = Some(parse_usize(&take(&mut args, "--synthetic"), "--synthetic"));
            }
            "--impl" => {
                which = match take(&mut args, "--impl").as_str() {
                    "v1" => ImplChoice::V1,
                    "v2" => ImplChoice::V2,
                    "both" => ImplChoice::Both,
                    other => {
                        eprintln!("unknown --impl {other}");
                        std::process::exit(2);
                    }
                };
            }
            "--slab" => slab = Some(parse_usize(&take(&mut args, "--slab"), "--slab")),
            "--alloc" => {
                let s = take(&mut args, "--alloc");
                alloc = SlabAllocator::parse(&s).unwrap_or_else(|| {
                    eprintln!("unknown --alloc {s}");
                    std::process::exit(2);
                });
            }
            "--warmup" => warmup = parse_usize(&take(&mut args, "--warmup"), "--warmup"),
            "--modify" => {
                modify = match take(&mut args, "--modify").as_str() {
                    "emulate" => ModifyMode::Emulate,
                    "skip" => ModifyMode::Skip,
                    other => {
                        eprintln!("unknown --modify {other}");
                        std::process::exit(2);
                    }
                };
            }
            "--limit" => limit = Some(parse_usize(&take(&mut args, "--limit"), "--limit")),
            "--csv" => csv = Some(PathBuf::from(take(&mut args, "--csv"))),
            "--no-latency" => latency = false,
            "--pin" => pin = Some(parse_usize(&take(&mut args, "--pin"), "--pin")),
            other => {
                eprintln!("unknown argument: {other}");
                print_usage();
                std::process::exit(2);
            }
        }
    }

    if tape.is_none() && synthetic.is_none() {
        eprintln!("provide --tape <file> or --synthetic <n>");
        print_usage();
        std::process::exit(2);
    }

    // C++ uses a 20 M-slot pool so a real tape never hits SlabFull. A
    // synthetic smoke does not need that; size from N so we don't fault 640 MB.
    let slab = slab.unwrap_or(match synthetic {
        Some(n) => n.saturating_mul(2).max(1 << 10),
        None => 20_000_000,
    });

    Config {
        tape,
        synthetic,
        which,
        slab,
        alloc,
        warmup,
        modify,
        limit,
        csv,
        latency,
        pin,
    }
}

fn parse_usize(s: &str, flag: &str) -> usize {
    s.replace('_', "").parse().unwrap_or_else(|_| {
        eprintln!("{flag}: not an integer: {s}");
        std::process::exit(2);
    })
}

// ---------------------------------------------------------------------------
// Run one impl
// ---------------------------------------------------------------------------

fn run_impl<B: OrderBookApi>(
    label: &str,
    ops: &[TapeOp],
    cfg: &Config,
    clock: &ReplayClock,
) {
    println!("\n=== {label} ===");

    let mut replay = Replayer::<B>::new(cfg.slab, cfg.alloc);
    let warmup_n = cfg.warmup.min(ops.len());

    if warmup_n > 0 {
        let mut wstats = empty_replay_stats();
        for op in &ops[..warmup_n] {
            replay.apply(op, &mut wstats);
        }
        let drained = replay.drain();
        println!("warmup: {warmup_n} ops, drained {drained} live orders");
    }

    let mut stats = empty_replay_stats();
    let mut samples: Vec<u32> = if cfg.latency {
        Vec::with_capacity(ops.len())
    } else {
        Vec::new()
    };

    let wall0 = Instant::now();
    if cfg.latency {
        for op in ops {
            let t0 = clock.read();
            replay.apply(op, &mut stats);
            let t1 = clock.read();
            if t1 > t0 {
                let ns = clock.to_ns(t1 - t0);
                // Saturate at u32::MAX ns (~4.3 s) — plenty for a single op.
                samples.push(ns.min(u32::MAX as f64) as u32);
            }
        }
    } else {
        for op in ops {
            replay.apply(op, &mut stats);
        }
    }
    let wall = wall0.elapsed();

    stats.live = replay.handles.len();
    print_results(ops.len(), wall, &stats, &samples, clock, cfg.csv.as_deref());
}

fn empty_replay_stats() -> ReplayStats {
    ReplayStats {
        adds: 0,
        cancels: 0,
        modifies: 0,
        cancel_miss: 0,
        add_dup: 0,
        slab_full: 0,
        live: 0,
    }
}

fn print_results(
    n_ops: usize,
    wall: Duration,
    stats: &ReplayStats,
    samples: &[u32],
    clock: &ReplayClock,
    csv: Option<&Path>,
) {
    let secs = wall.as_secs_f64().max(1e-12);
    let mops = (n_ops as f64 / secs) / 1_000_000.0;
    let ns_per_op = (secs * 1e9) / n_ops.max(1) as f64;

    println!("operations:   {n_ops}");
    println!("wall:         {secs:.4} s");
    println!("throughput:   {mops:.2} M ops/s");
    println!("avg (wall):   {ns_per_op:.2} ns/op");
    println!(
        "applied:      add={}  cancel={}  modify(emulated)={}",
        stats.adds, stats.cancels, stats.modifies
    );
    println!(
        "anomalies:    cancel_miss={}  add_dup={}  slab_full={}",
        stats.cancel_miss, stats.add_dup, stats.slab_full
    );
    println!("live at end:  {}", stats.live);

    if samples.is_empty() {
        return;
    }

    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    let pct = |p: f64| -> u32 { sorted[((n as f64 * p) as usize).min(n - 1)] };
    let sum: u64 = sorted.iter().map(|&x| x as u64).sum();
    let avg = sum as f64 / n as f64;

    println!();
    print!("latency ({}, n={n}", clock.name());
    if let Some(ghz) = clock.ghz() {
        print!(", TSC ≈ {ghz:.3} GHz");
    }
    println!(")");
    println!("  min     {:>10.2} ns", sorted[0] as f64);
    println!("  avg     {avg:>10.2} ns");
    println!("  p50     {:>10.2} ns", pct(0.50) as f64);
    println!("  p90     {:>10.2} ns", pct(0.90) as f64);
    println!("  p99     {:>10.2} ns", pct(0.99) as f64);
    println!("  p99.9   {:>10.2} ns", pct(0.999) as f64);
    println!("  max     {:>10.2} ns", sorted[n - 1] as f64);

    if matches!(clock.inner.kind, ClockKind::Instant) {
        println!(
            "  note: Instant resolution on this host is tens of ns; p50 is a floor, not a cycle count."
        );
    }

    if let Some(path) = csv {
        if let Err(e) = write_csv(path, samples, clock) {
            eprintln!("failed to write {}: {e}", path.display());
        } else {
            println!("wrote {} samples to {}", samples.len(), path.display());
        }
    }
}

fn write_csv(path: &Path, samples: &[u32], clock: &ReplayClock) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let mut w = BufWriter::new(File::create(path)?);
    writeln!(w, "latency_ns")?;
    // `samples` is already ns (Instant path, or rdtsc converted at record time).
    let _ = clock;
    for &ns in samples {
        writeln!(w, "{ns}")?;
    }
    w.flush()
}

// ---------------------------------------------------------------------------
// Pin (Linux)
// ---------------------------------------------------------------------------

fn maybe_pin(cpu: Option<usize>) {
    let Some(cpu) = cpu else { return };
    #[cfg(target_os = "linux")]
    {
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_ZERO(&mut set);
            libc::CPU_SET(cpu, &mut set);
            let rc = libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
            if rc != 0 {
                eprintln!("sched_setaffinity({cpu}) failed: {}", std::io::Error::last_os_error());
            } else {
                println!("pinned to cpu {cpu}");
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("--pin is Linux-only; ignoring (cpu={cpu})");
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() {
    let cfg = parse_args();
    maybe_pin(cfg.pin);

    println!("loading tape...");
    let load0 = Instant::now();
    let (ops, tape_stats) = if let Some(n) = cfg.synthetic {
        synthetic_tape(cfg.limit.unwrap_or(n).min(n))
    } else {
        let path = cfg.tape.as_deref().expect("tape");
        load_dbn(path, cfg.modify, cfg.limit)
    };
    println!("loaded {} kept ops in {:.2}s", ops.len(), load0.elapsed().as_secs_f64());
    println!(
        "tape mix:     add={}  cancel={}  modify={}  skip_side={}  skip_price={}  skip_other={}",
        tape_stats.adds,
        tape_stats.cancels,
        tape_stats.modifies,
        tape_stats.skipped_side,
        tape_stats.skipped_price,
        tape_stats.skipped_other
    );
    println!(
        "config:       slab={}  alloc={}  warmup={}  modify={}",
        cfg.slab,
        cfg.alloc.slug(),
        cfg.warmup,
        match cfg.modify {
            ModifyMode::Emulate => "emulate(cancel+add)",
            ModifyMode::Skip => "skip",
        }
    );
    if tape_stats.kept() == 0 {
        eprintln!("no ops to replay");
        std::process::exit(1);
    }

    let clock = ReplayClock::new(Clock::detect());
    println!("clock:        {}", clock.name());

    match cfg.which {
        ImplChoice::V1 => run_impl::<OB1<NullConsumer>>("v1", &ops, &cfg, &clock),
        ImplChoice::V2 => run_impl::<OB2<NullConsumer>>("v2", &ops, &cfg, &clock),
        ImplChoice::Both => {
            run_impl::<OB1<NullConsumer>>("v1", &ops, &cfg, &clock);
            run_impl::<OB2<NullConsumer>>("v2", &ops, &cfg, &clock);
        }
    }
}
