// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Micro-benchmark comparing today's file I/O dispatch shapes on identical workloads:
//! `blocking` (`spawn_blocking` around a positional syscall), `tokiofs` (`tokio::fs`), and
//! `loreio` (this crate).
//!
//! One process runs one engine. Sharing a process let the engines contaminate each other —
//! whichever ran second read a data file the first had evicted, and inherited its warmed thread
//! pools and allocator — which was worth 3x and looked exactly like an engine difference.
//!
//! Warm suite, all three engines, one child process each:
//! `cargo run --release -p lore-io --example bench -- warm`
//!
//! One engine only, which is what to run under `perf`:
//! `cargo run --release -p lore-io --example bench -- warm loreio`
//!
//! Cold suite (real device reads), one engine per process:
//! 1. `cargo run --release -p lore-io --example bench -- prepare-cold`
//! 2. Drop caches: `sudo sh -c 'sync; echo 3 > /proc/sys/vm/drop_caches'`
//!    (required on ZFS, where the ARC ignores `posix_fadvise`; unnecessary elsewhere, including
//!    macOS, where the harness evicts each file through `msync(MS_INVALIDATE)`)
//! 3. `cargo run --release -p lore-io --example bench -- cold`
//!
//! A single round decides nothing — see the noise floor discussion in BENCHMARKS.md. The suite
//! modes run the protocol that does: N rounds with the engine order alternated, then a median,
//! a range and a position control. `cold-suite` additionally rotates which copy of the cold data
//! set each engine reads, because the copies are not equivalent and a fixed assignment biases an
//! engine for the whole run; give it a round count divisible by the engine count.
//! `cargo run --release -p lore-io --example bench -- warm-suite 6`
//! `cargo run --release -p lore-io --example bench -- cold-suite 6`
//!
//! Set `LORE_BENCH_DIR` to place benchmark files on a specific filesystem.

// Linking lore-base installs its `#[global_allocator]`, so the benchmark allocates through
// `LoreAllocator` — rpmalloc by default, `std::alloc::System` under `LORE_ALLOCATOR=system` — the
// same path a Lore process takes. This is load-bearing rather than incidental: reads allocate
// their buffer inside the I/O job specifically because the process allocator caches spans per
// thread, and on glibc malloc that premise does not hold.
#[allow(unused_extern_crates)]
extern crate lore_base;

use std::fs::File;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Instant;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream;
use lore_io::BackendKind;
use lore_io::IoDriver;
use lore_io::IoFile;
use lore_io::OpenOptions;

// Operation counts are sized so each warm phase runs for roughly a second rather than tens of
// milliseconds. A phase shorter than that measures the machine — frequency ramp from idle,
// cache state, whatever else is running — rather than the engines, and no number of repeats
// recovers a signal from it. The cost is a warm run of about ten seconds per engine.
const FILE_SIZE: u64 = 256 * 1024 * 1024;
const READ_LARGE_SIZE: usize = 64 * 1024;
const READ_LARGE_OPS: usize = 524_288;
const READ_LARGE_CONCURRENCY: usize = 64;
const READ_SMALL_SIZE: usize = 4 * 1024;
const READ_SMALL_OPS: usize = 786_432;
const READ_SMALL_CONCURRENCY: usize = 128;
const WRITE_SIZE: usize = 256 * 1024;
const WRITE_OPS: usize = 16_384;
const WRITE_CONCURRENCY: usize = 32;
const SMALL_FILE_SIZE: usize = 4 * 1024;
const SMALL_FILE_COUNT: usize = 16_384;
const SMALL_FILE_CONCURRENCY: usize = 64;

const COLD_DIR_NAME: &str = "lore-io-bench-cold";
const COLD_LARGE_FILE_SIZE: u64 = 256 * 1024 * 1024;
const COLD_4K_FILE_SIZE: u64 = 512 * 1024 * 1024;

/// Stride between cold 4 KiB reads. Reading each block from its own
/// 128 KiB region guarantees every op hits storage: ZFS caches whole
/// records (128 KiB default) in the ARC, so a second 4 KiB read inside an
/// already-touched record would be a cache hit, and the spacing also
/// defeats page-cache readahead on other filesystems.
const COLD_READ_STRIDE: u64 = 128 * 1024;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str);

    // The suite modes take a round count where the other modes take an engine, and they only
    // spawn children, so they are dispatched before the engine argument is parsed and before a
    // runtime this process will never use is built.
    match mode {
        Some("warm-suite") => return run_suite("warm", rounds_from(args.get(1))),
        Some("cold-suite") => return run_suite("cold", rounds_from(args.get(1))),
        _ => {}
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(blocking_pool_threads())
        .build()
        .expect("failed to build benchmark runtime");
    let engine = args.get(1).map(String::as_str);
    // The cold modes take the data set to read as a third argument, because which copy an engine
    // is given is worth more than the engines differ by. Omitted, an engine reads its own.
    let dataset = args.get(2).map(String::as_str);
    let selected = engine.map(engine_from_tag);
    match (mode, selected) {
        (Some("warm"), None) => run_each_in_its_own_process("warm"),
        (Some("warm"), Some(Some(engine))) => runtime.block_on(run_warm(engine)),
        (Some("read"), Some(Some(engine))) => runtime.block_on(run_read_only(engine)),
        (Some("cold"), None) => run_each_in_its_own_process("cold"),
        (Some("cold"), Some(Some(engine))) => runtime.block_on(run_cold(engine, dataset)),
        (Some("prepare-cold"), _) => runtime.block_on(prepare_cold()),
        (Some("cold-baseline"), _) => run_cold_baseline(),
        (_, Some(None)) => {
            eprintln!(
                "unknown engine \"{}\" (engines: {})",
                engine.unwrap_or_default(),
                ENGINE_TAGS.join(", ")
            );
            std::process::exit(2);
        }
        _ => {
            eprintln!(
                "usage: bench <warm|read|cold> [{}]\n       bench <warm-suite|cold-suite> [rounds]\n       bench prepare-cold\n       bench cold-baseline",
                ENGINE_TAGS.join("|")
            );
            std::process::exit(2);
        }
    }
}

/// Runs `mode` once per engine, each in a fresh process.
///
/// Process isolation is the point: page cache, thread pools and allocator state all carry between
/// engines otherwise, and the engine that runs second inherits whatever the first left behind.
/// A sync and a settle between children keeps writeback from one out of the next.
fn run_each_in_its_own_process(mode: &str) {
    let exe = std::env::current_exe().expect("current executable");
    for tag in ENGINE_TAGS {
        run_engine_in_child(&exe, mode, tag, "", "");
    }
}

/// Runs one engine in a fresh process, echoing its output as it arrives, and returns the lines.
///
/// The sync and the settle belong here rather than at the call site: every path that starts an
/// engine child owes the next one a quiet device. An empty `tag` runs a mode that takes no engine,
/// and an empty `dataset` leaves the engine reading its own.
fn run_engine_in_child(
    exe: &Path,
    mode: &str,
    tag: &str,
    dataset: &str,
    prefix: &str,
) -> Vec<String> {
    use std::io::BufRead;

    let mut command = std::process::Command::new(exe);
    command.arg(mode);
    if !tag.is_empty() {
        command.arg(tag);
    }
    if !dataset.is_empty() {
        command.arg(dataset);
    }
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("failed to run the engine child process");
    let stdout = child.stdout.take().expect("child process stdout");
    let mut lines = Vec::new();
    for line in std::io::BufReader::new(stdout).lines() {
        let line = line.expect("failed to read the engine child's output");
        println!("{prefix}{line}");
        lines.push(line);
    }
    let status = child
        .wait()
        .expect("failed to wait for the engine child process");
    assert!(status.success(), "engine {tag} exited with {status}");
    sync_filesystem();
    std::thread::sleep(std::time::Duration::from_secs(3));
    lines
}

// ---------------------------------------------------------------------------
// Suite modes: the noise-floor protocol, executable
// ---------------------------------------------------------------------------

/// Default rounds for a suite run. BENCHMARKS.md asks for at least six: absolute ops/s has moved
/// 38% between batches an hour apart, so fewer rounds measures the sitting rather than the engine.
const DEFAULT_SUITE_ROUNDS: usize = 6;

fn rounds_from(argument: Option<&String>) -> usize {
    let rounds = argument.map_or(DEFAULT_SUITE_ROUNDS, |value| {
        value
            .parse()
            .expect("rounds must be a positive whole number")
    });
    assert!(rounds > 0, "rounds must be at least 1");
    rounds
}

/// One phase result from one engine in one round.
struct Sample {
    workload: String,
    tag: &'static str,
    forward: bool,
    ops_per_sec: f64,
}

/// Runs `mode` for `rounds` rounds with the engine order alternated, then reports medians.
///
/// This is BENCHMARKS.md's noise-floor protocol rather than a convenience wrapper. A single round
/// cannot distinguish an engine difference from where the round sat, so the summary reports three
/// things: the median, the round-to-round range that says whether a median is readable at all, and
/// a position control. Odd rounds run the engines in the order [`ENGINE_TAGS`] lists and even
/// rounds reverse it, so an effect that follows position shows up as a fwd/rev ratio away from
/// 1.00x instead of masquerading as an engine result — which is exactly the trap that invalidated
/// this file's first set of numbers.
///
/// Every engine still runs in its own process; no process ever hosts two.
fn run_suite(mode: &str, rounds: usize) {
    let exe = std::env::current_exe().expect("current executable");
    let mut samples: Vec<Sample> = Vec::new();

    for round in 1..=rounds {
        let forward = round % 2 == 1;
        let order = if forward { "fwd" } else { "rev" };
        let mut tags = ENGINE_TAGS;
        if !forward {
            tags.reverse();
        }
        println!("=== round {round}/{rounds} ({order}) ===");
        for tag in tags {
            // Rotating by the engine's own index, not by its position in this round's order, keeps
            // the assignment a bijection: within a round the three engines still read three
            // different copies, so no engine warms the file the next one reads and the single
            // cache drop before the suite still serves all three.
            let dataset = if mode == "cold" {
                let engine_index = ENGINE_TAGS
                    .iter()
                    .position(|&name| name == tag)
                    .expect("every tag is an engine");
                ENGINE_TAGS[(engine_index + round - 1) % ENGINE_TAGS.len()]
            } else {
                ""
            };
            let prefix = format!("[r{round}-{order}] ");
            for line in run_engine_in_child(&exe, mode, tag, dataset, &prefix) {
                if let Some(sample) = parse_sample(&line, tag, forward) {
                    samples.push(sample);
                }
            }
        }
    }

    // The baseline runs last and in its own child, on the same data-set generation the rounds just
    // read. Running it in the same sitting is the point: what it measures is a property of these
    // files now, and a rate carried over from an earlier sitting has measured 1.77× off.
    let baselines = if mode == "cold" {
        if !rounds.is_multiple_of(ENGINE_TAGS.len()) {
            println!(
                "note: {rounds} rounds does not divide by {} engines, so the data-set rotation is",
                ENGINE_TAGS.len()
            );
            println!("      unbalanced and some engine read the fast copy more often than another");
        }
        println!("=== baseline ===");
        let mut rates: Vec<(String, String, f64)> = Vec::new();
        for line in run_engine_in_child(&exe, "cold-baseline", "", "", "[baseline] ") {
            if let Some((tag, workload, ops_per_sec)) = parse_baseline(&line) {
                rates.push((tag, workload, ops_per_sec));
            }
        }
        rates
    } else {
        Vec::new()
    };

    print_suite_summary(&samples, rounds);
    print_baseline_summary(&samples, &baselines);
}

/// Recovers a rate from a [`report_baseline`] line, which names the data set rather than an engine.
fn parse_baseline(line: &str) -> Option<(String, String, f64)> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() != 7 {
        return None;
    }
    let tag = fields[0].strip_prefix("baseline:")?;
    fields[2].parse::<u64>().ok()?;
    let ops_per_sec = fields[5].parse::<f64>().ok()?;
    Some((tag.to_string(), fields[1].to_string(), ops_per_sec))
}

/// Each engine's median over what plain threads get from the same data sets.
///
/// The rotation makes every engine read every copy, so the denominator is the mean across the
/// copies and is the same for all three — it is not correcting a per-engine bias, because there is
/// no longer one to correct. What it reports is distance to the hardware: a column near 1.00 is an
/// engine getting everything the device has, and a low one is an engine that is the bottleneck.
///
/// The spread column is the size of the confound the rotation cancels. When it is near 1.00 the
/// copies agreed and the medians above could have been read directly; on APFS it has reached 1.78×.
fn print_baseline_summary(samples: &[Sample], baselines: &[(String, String, f64)]) {
    if baselines.is_empty() {
        return;
    }

    let mut workloads: Vec<&str> = Vec::new();
    for (_, workload, _) in baselines {
        if !workloads.contains(&workload.as_str()) {
            workloads.push(workload);
        }
    }

    println!();
    println!("engine over what plain threads get from the same data sets, rotation cancelling");
    println!("which copy each engine read; spread is max/min across the copies");
    println!(
        "{:<28} {:>10} {:>10} {:>10} {:>9}",
        "workload", "blocking", "tokiofs", "loreio", "spread"
    );
    for workload in &workloads {
        let rates: Vec<f64> = baselines
            .iter()
            .filter(|(_, phase, _)| phase == workload)
            .map(|(_, _, rate)| *rate)
            .collect();
        let mean = rates.iter().sum::<f64>() / rates.len() as f64;
        print!("{workload:<28}");
        for tag in ENGINE_TAGS {
            let engine = median_of(samples, workload, tag, None);
            if mean > 0.0 {
                print!(" {:>10.2}", engine / mean);
            } else {
                print!(" {:>10}", "-");
            }
        }
        let low = rates.iter().copied().fold(f64::INFINITY, f64::min);
        let high = rates.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        if low > 0.0 {
            println!(" {:>8.2}x", high / low);
        } else {
            println!(" {:>9}", "-");
        }
    }
}

/// Recovers a phase result from a [`report`] line, ignoring headers and `pool_stats` lines.
fn parse_sample(line: &str, tag: &'static str, forward: bool) -> Option<Sample> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() != 7 {
        return None;
    }
    // The header has the same field count, so both numeric columns must parse.
    fields[2].parse::<u64>().ok()?;
    let ops_per_sec = fields[5].parse::<f64>().ok()?;
    Some(Sample {
        workload: fields[1].to_string(),
        tag,
        forward,
        ops_per_sec,
    })
}

fn samples_for(samples: &[Sample], workload: &str, tag: &str, order: Option<bool>) -> Vec<f64> {
    samples
        .iter()
        .filter(|sample| {
            sample.workload == workload
                && sample.tag == tag
                && order.is_none_or(|forward| forward == sample.forward)
        })
        .map(|sample| sample.ops_per_sec)
        .collect::<Vec<_>>()
}

fn median_of(samples: &[Sample], workload: &str, tag: &str, order: Option<bool>) -> f64 {
    let mut values = samples_for(samples, workload, tag, order);
    if values.is_empty() {
        return f64::NAN;
    }
    values.sort_by(|left, right| left.partial_cmp(right).expect("ops/s is never NaN"));
    let middle = values.len() / 2;
    if values.len() % 2 == 1 {
        values[middle]
    } else {
        (values[middle - 1] + values[middle]) / 2.0
    }
}

fn range_of(samples: &[Sample], workload: &str, tag: &str) -> String {
    let values = samples_for(samples, workload, tag, None);
    let low = values.iter().copied().fold(f64::INFINITY, f64::min);
    let high = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    format!("{low:.0}-{high:.0}")
}

fn print_suite_summary(samples: &[Sample], rounds: usize) {
    let mut workloads: Vec<&str> = Vec::new();
    for sample in samples {
        if !workloads.contains(&sample.workload.as_str()) {
            workloads.push(&sample.workload);
        }
    }

    println!();
    println!("median of {rounds} rounds, one process per engine, engine order alternated");
    println!(
        "{:<28} {:>10} {:>10} {:>10} {:>9} {:>9}",
        "workload", "blocking", "tokiofs", "loreio", "vs blk", "vs tfs"
    );
    for workload in &workloads {
        let blocking = median_of(samples, workload, "blocking", None);
        let tokiofs = median_of(samples, workload, "tokiofs", None);
        let loreio = median_of(samples, workload, "loreio", None);
        println!(
            "{:<28} {:>10.0} {:>10.0} {:>10.0} {:>8.2}x {:>8.2}x",
            workload,
            blocking,
            tokiofs,
            loreio,
            loreio / blocking,
            loreio / tokiofs
        );
    }

    println!();
    println!("round-to-round range, which is what says whether a difference is readable");
    println!(
        "{:<28} {:>19} {:>19} {:>19}",
        "workload", "blocking", "tokiofs", "loreio"
    );
    for workload in &workloads {
        println!(
            "{:<28} {:>19} {:>19} {:>19}",
            workload,
            range_of(samples, workload, "blocking"),
            range_of(samples, workload, "tokiofs"),
            range_of(samples, workload, "loreio")
        );
    }

    if rounds > 1 {
        println!();
        println!("position control: median reverse-order / median forward-order");
        println!("a ratio far from 1.00x means engine order is leaking into the result");
        println!(
            "{:<28} {:>10} {:>10} {:>10}",
            "workload", "blocking", "tokiofs", "loreio"
        );
        for workload in &workloads {
            print!("{workload:<28}");
            for tag in ENGINE_TAGS {
                let forward = median_of(samples, workload, tag, Some(true));
                let reverse = median_of(samples, workload, tag, Some(false));
                print!(" {:>9.2}x", reverse / forward);
            }
            println!();
        }
    }
}

/// The `blocking` engine's pool size: what `lore-base` builds, unless `LORE_BENCH_BLOCKING_THREADS`
/// overrides it.
///
/// The knob is the baseline's counterpart to `LORE_IO_POOL_THREADS`, and it exists because a cap
/// sweep against `loreio` alone cannot tell an engine result from a host one. Both engines sit on
/// the same host curve — reducing this pool reproduced `lore-io`'s 2.16× at cap 8 on Host B, which
/// is what established that the cap was not the variable — and without a knob that control means
/// patching this file.
fn blocking_pool_threads() -> usize {
    let cores = std::thread::available_parallelism().map_or(2, |count| count.get());
    let formula = std::cmp::min(2 * (cores + 1), 128);
    let Ok(value) = std::env::var("LORE_BENCH_BLOCKING_THREADS") else {
        return formula;
    };
    match value.trim().parse::<usize>() {
        Ok(threads) if threads > 0 => threads,
        _ => {
            eprintln!(
                "bench: unusable LORE_BENCH_BLOCKING_THREADS \"{value}\"; using {formula} instead"
            );
            formula
        }
    }
}

fn bench_root() -> PathBuf {
    std::env::var_os("LORE_BENCH_DIR").map_or_else(std::env::temp_dir, PathBuf::from)
}

fn alloc_outside() -> bool {
    static OUTSIDE: OnceLock<bool> = OnceLock::new();
    *OUTSIDE.get_or_init(|| std::env::var_os("LORE_BENCH_ALLOC_OUTSIDE").is_some())
}

// ---------------------------------------------------------------------------
// Warm suite
// ---------------------------------------------------------------------------

async fn run_warm(engine: Engine) {
    let dir = TempDir::new();
    let driver = IoDriver::new(BackendKind::Psync).expect("psync backend");
    let data_path = dir.path.join(format!("data-{}", engine.tag()));

    // Written immediately before it is read, and by this process alone, so the read phases start
    // from a cache state this engine produced rather than one another engine left behind.
    write_random_file(&driver, &data_path, FILE_SIZE).await;

    print_header();
    read_phases(&engine, &data_path).await;
    bench_writes(&engine, &dir.path).await;
    bench_small_files(&engine, &dir.path).await;
}

/// The read phases alone, for profiling: `perf` then covers reads and the data-file write rather
/// than the whole suite.
async fn run_read_only(engine: Engine) {
    let dir = TempDir::new();
    let driver = IoDriver::new(BackendKind::Psync).expect("psync backend");
    let data_path = dir.path.join(format!("data-{}", engine.tag()));
    write_random_file(&driver, &data_path, FILE_SIZE).await;

    print_header();
    read_phases(&engine, &data_path).await;
}

async fn read_phases(engine: &Engine, data_path: &Path) {
    let offsets = random_offsets(READ_LARGE_OPS, FILE_SIZE, READ_LARGE_SIZE);
    read_phase(
        engine,
        data_path,
        "read-64KiB-warm-c64",
        READ_LARGE_SIZE,
        offsets,
        READ_LARGE_CONCURRENCY,
    )
    .await;
    let offsets = random_offsets(READ_SMALL_OPS, FILE_SIZE, READ_SMALL_SIZE);
    read_phase(
        engine,
        data_path,
        "read-4KiB-warm-c128",
        READ_SMALL_SIZE,
        offsets,
        READ_SMALL_CONCURRENCY,
    )
    .await;
}

async fn bench_writes(engine: &Engine, dir: &Path) {
    let path = dir.join(format!("write-{}", engine.tag()));
    let started = Instant::now();
    match engine {
        Engine::BlockingPool => {
            let file = Arc::new(
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&path)
                    .expect("failed to create write file"),
            );
            stream::iter(0..WRITE_OPS)
                .map(|index| {
                    let file = Arc::clone(&file);
                    async move {
                        blocking_write_all_at(file, WRITE_SIZE, (index * WRITE_SIZE) as u64).await;
                    }
                })
                .buffer_unordered(WRITE_CONCURRENCY)
                .collect::<Vec<_>>()
                .await;
            let file = Arc::clone(&file);
            blocking_sync_data(file).await;
        }
        Engine::TokioFs => {
            stream::iter(0..WRITE_OPS)
                .map(|index| {
                    let path = path.clone();
                    async move {
                        tokio_fs_write_all_at(path, WRITE_SIZE, (index * WRITE_SIZE) as u64).await;
                    }
                })
                .buffer_unordered(WRITE_CONCURRENCY)
                .collect::<Vec<_>>()
                .await;
            // Opened for writing, not just reading: `FlushFileBuffers` needs write access on the
            // handle, so a read-only one fails with `Access is denied` on Windows. `fdatasync` on
            // a read-only descriptor succeeds on Linux, which is why this went unnoticed there.
            tokio::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .await
                .expect("open for sync")
                .sync_data()
                .await
                .expect("sync failed");
        }
        Engine::LoreIo(driver) => {
            let file = driver
                .open(
                    &path,
                    &OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(true),
                )
                .await
                .expect("failed to create write file");
            stream::iter(0..WRITE_OPS)
                .map(|index| {
                    let file = file.clone();
                    async move {
                        file.write_all_at(vec![0u8; WRITE_SIZE], (index * WRITE_SIZE) as u64)
                            .await
                            .expect("write failed");
                    }
                })
                .buffer_unordered(WRITE_CONCURRENCY)
                .collect::<Vec<_>>()
                .await;
            file.sync_data().await.expect("sync failed");
        }
    }
    report(
        engine,
        "write-256KiB-seq-c32+sync",
        WRITE_OPS,
        WRITE_OPS * WRITE_SIZE,
        started,
    );
    let _ = std::fs::remove_file(&path);
}

async fn bench_small_files(engine: &Engine, dir: &Path) {
    let subdir = dir.join(format!("small-{}", engine.tag()));
    std::fs::create_dir_all(&subdir).expect("failed to create small-file dir");

    let started = Instant::now();
    small_files_create_phase(engine, &subdir).await;
    small_files_read_phase(engine, &subdir).await;
    report(
        engine,
        "small-files-4KiB-wr+rd-c64",
        SMALL_FILE_COUNT * 2,
        SMALL_FILE_COUNT * SMALL_FILE_SIZE * 2,
        started,
    );
    let _ = std::fs::remove_dir_all(&subdir);
}

// ---------------------------------------------------------------------------
// Cold suite
// ---------------------------------------------------------------------------

async fn prepare_cold() {
    let dir = bench_root().join(COLD_DIR_NAME);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("failed to create cold bench dir");
    let driver = IoDriver::new(BackendKind::Psync).expect("psync backend");

    // The small-file trees go first so the data-file traffic written after
    // them ages them out of cache before the drop; data written last is
    // what survives a cache shrink.
    for tag in ENGINE_TAGS {
        let subdir = dir.join(format!("small-{tag}"));
        std::fs::create_dir_all(&subdir).expect("failed to create small-file dir");
        let payload = Bytes::from(vec![0xabu8; SMALL_FILE_SIZE]);
        stream::iter(0..SMALL_FILE_COUNT)
            .map(|index| {
                let path = subdir.join(format!("file-{index}"));
                let payload = payload.clone();
                let driver = driver.clone();
                async move {
                    driver
                        .write_file_bytes(path, payload, false)
                        .await
                        .expect("create failed");
                }
            })
            .buffer_unordered(SMALL_FILE_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
    }
    for tag in ENGINE_TAGS {
        write_random_file(
            &driver,
            &dir.join(format!("data64k-{tag}")),
            COLD_LARGE_FILE_SIZE,
        )
        .await;
        write_random_file(
            &driver,
            &dir.join(format!("data4k-{tag}")),
            COLD_4K_FILE_SIZE,
        )
        .await;
    }
    sync_filesystem();

    println!("cold data prepared in {}", dir.display());
    println!("now drop caches, then run the cold suite:");
    println!("  sudo sh -c 'sync; echo 3 > /proc/sys/vm/drop_caches'");
    println!("  cargo run --release -p lore-io --example bench -- cold");
}

/// Runs the cold phases against the data set named by `dataset`, defaulting to the engine's own.
///
/// The two are separate because the copies are not equivalent: which one an engine is given is
/// worth up to 1.78× on this benchmark's own access pattern, for reasons that live in the drive
/// rather than in anything the filesystem exposes. A fixed assignment therefore biases an engine
/// for the whole run, which is why [`run_suite`] rotates it.
async fn run_cold(engine: Engine, dataset: Option<&str>) {
    let dir = bench_root().join(COLD_DIR_NAME);
    assert!(
        dir.is_dir(),
        "cold data not found in {} — run prepare-cold first",
        dir.display()
    );

    let tag = dataset.unwrap_or_else(|| engine.tag());
    println!("cold suite (assumes caches were dropped after prepare-cold), data set {tag}");
    print_header();
    {
        let path = dir.join(format!("data64k-{tag}"));
        evict_from_page_cache(&path);
        let slots = COLD_LARGE_FILE_SIZE / READ_LARGE_SIZE as u64;
        let offsets = permuted_offsets(slots, slots as usize, READ_LARGE_SIZE as u64);
        read_phase(
            &engine,
            &path,
            "read-64KiB-cold-c64",
            READ_LARGE_SIZE,
            offsets,
            READ_LARGE_CONCURRENCY,
        )
        .await;

        let path = dir.join(format!("data4k-{tag}"));
        evict_from_page_cache(&path);
        let slots = COLD_4K_FILE_SIZE / COLD_READ_STRIDE;
        let offsets = permuted_offsets(slots, slots as usize, COLD_READ_STRIDE);
        read_phase(
            &engine,
            &path,
            "read-4KiB-cold-rec-c128",
            READ_SMALL_SIZE,
            offsets,
            READ_SMALL_CONCURRENCY,
        )
        .await;

        let subdir = dir.join(format!("small-{tag}"));
        for index in 0..SMALL_FILE_COUNT {
            evict_from_page_cache(&subdir.join(format!("file-{index}")));
        }
        let started = Instant::now();
        small_files_read_phase(&engine, &subdir).await;
        report(
            &engine,
            "small-files-4KiB-rd-cold-c64",
            SMALL_FILE_COUNT,
            SMALL_FILE_COUNT * SMALL_FILE_SIZE,
            started,
        );
    }
}

// ---------------------------------------------------------------------------
// Phases shared between suites
// ---------------------------------------------------------------------------

enum Engine {
    BlockingPool,
    TokioFs,
    LoreIo(IoDriver),
}

impl Engine {
    fn name(&self) -> String {
        match self {
            Engine::BlockingPool => "blocking-pool".to_string(),
            Engine::TokioFs => "tokio-fs".to_string(),
            Engine::LoreIo(driver) => format!("lore-io({})", driver.backend_name()),
        }
    }

    fn tag(&self) -> &'static str {
        match self {
            Engine::BlockingPool => "blocking",
            Engine::TokioFs => "tokiofs",
            Engine::LoreIo(_) => "loreio",
        }
    }
}

/// Every engine, in the order [`ENGINE_TAGS`] names them.
const ENGINE_TAGS: [&str; 3] = ["blocking", "tokiofs", "loreio"];

fn engine_from_tag(tag: &str) -> Option<Engine> {
    match tag {
        "blocking" => Some(Engine::BlockingPool),
        "tokiofs" => Some(Engine::TokioFs),
        "loreio" => Some(Engine::LoreIo(
            IoDriver::new(BackendKind::Psync).expect("psync backend"),
        )),
        _ => None,
    }
}

/// What the device gives for one cold phase, issued from plain threads with no engine in the path.
///
/// Every engine reads its own copy of the cold data set, so that one engine's process cannot delete
/// or warm what another still needs. Those copies do not read back at the same rate: on APFS they
/// have measured up to 1.62× apart, written seconds apart by the same code with the same contents,
/// which is larger than any engine difference this benchmark reports. A fixed assignment of engine
/// to file is invisible to the suite's own controls, because rounds, alternation and the position
/// control all vary *when* an engine runs and this varies *what it reads*.
///
/// So each engine's cold result is divided by its own file's rate here. The ratio is what carries
/// meaning; the ops/s are a property of a file on a drive on a day.
///
/// Threads rather than concurrency: this issues the same permuted `pread`s at the phase's own
/// concurrency, one thread per in-flight request, which is the ceiling an engine dispatching the
/// same syscalls can reach and not a target it should beat.
fn cold_baseline(path: &Path, size: usize, offsets: &[u64], concurrency: usize) -> f64 {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    evict_from_page_cache(path);
    let file = Arc::new(File::open(path).expect("failed to open data file"));
    let next = AtomicUsize::new(0);
    let started = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..concurrency {
            scope.spawn(|| {
                let mut buffer = vec![0u8; size];
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&offset) = offsets.get(index) else {
                        break;
                    };
                    read_exact_at_impl(&file, &mut buffer, offset).expect("read failed");
                }
            });
        }
    });
    offsets.len() as f64 / started.elapsed().as_secs_f64()
}

/// The whole-file counterpart of [`cold_baseline`], for the phase that opens every file it reads.
fn cold_baseline_small_files(subdir: &Path, count: usize, concurrency: usize) -> f64 {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    for index in 0..count {
        evict_from_page_cache(&subdir.join(format!("file-{index}")));
    }
    let next = AtomicUsize::new(0);
    let started = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..concurrency {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= count {
                        break;
                    }
                    std::fs::read(subdir.join(format!("file-{index}"))).expect("read failed");
                }
            });
        }
    });
    count as f64 / started.elapsed().as_secs_f64()
}

/// Runs [`cold_baseline`] over every engine's copy of the cold data set.
///
/// Output is one `baseline` line per file in the same column layout the phases use, so a suite can
/// parse both from one child's stdout.
fn run_cold_baseline() {
    let dir = bench_root().join(COLD_DIR_NAME);
    assert!(
        dir.is_dir(),
        "cold data not found in {} — run prepare-cold first",
        dir.display()
    );

    println!("cold baseline: plain threads, one per in-flight request, no engine");
    print_header();
    for tag in ENGINE_TAGS {
        let path = dir.join(format!("data64k-{tag}"));
        let slots = COLD_LARGE_FILE_SIZE / READ_LARGE_SIZE as u64;
        let offsets = permuted_offsets(slots, slots as usize, READ_LARGE_SIZE as u64);
        let ops = offsets.len();
        report_baseline(
            tag,
            "read-64KiB-cold-c64",
            ops,
            ops * READ_LARGE_SIZE,
            cold_baseline(&path, READ_LARGE_SIZE, &offsets, READ_LARGE_CONCURRENCY),
        );

        let path = dir.join(format!("data4k-{tag}"));
        let slots = COLD_4K_FILE_SIZE / COLD_READ_STRIDE;
        let offsets = permuted_offsets(slots, slots as usize, COLD_READ_STRIDE);
        let ops = offsets.len();
        report_baseline(
            tag,
            "read-4KiB-cold-rec-c128",
            ops,
            ops * READ_SMALL_SIZE,
            cold_baseline(&path, READ_SMALL_SIZE, &offsets, READ_SMALL_CONCURRENCY),
        );

        report_baseline(
            tag,
            "small-files-4KiB-rd-cold-c64",
            SMALL_FILE_COUNT,
            SMALL_FILE_COUNT * SMALL_FILE_SIZE,
            cold_baseline_small_files(
                &dir.join(format!("small-{tag}")),
                SMALL_FILE_COUNT,
                SMALL_FILE_CONCURRENCY,
            ),
        );
    }
}

async fn read_phase(
    engine: &Engine,
    path: &Path,
    workload: &str,
    size: usize,
    offsets: Vec<u64>,
    concurrency: usize,
) {
    let ops = offsets.len();
    let started = Instant::now();
    match engine {
        Engine::BlockingPool => {
            let file = Arc::new(File::open(path).expect("failed to open data file"));
            stream::iter(offsets)
                .map(|offset| {
                    let file = Arc::clone(&file);
                    async move { blocking_read_exact_at(file, size, offset).await }
                })
                .buffer_unordered(concurrency)
                .collect::<Vec<_>>()
                .await;
        }
        Engine::TokioFs => {
            let path = path.to_path_buf();
            stream::iter(offsets)
                .map(|offset| {
                    let path = path.clone();
                    async move { tokio_fs_read_exact_at(path, size, offset).await }
                })
                .buffer_unordered(concurrency)
                .collect::<Vec<_>>()
                .await;
        }
        Engine::LoreIo(driver) => {
            let file = driver
                .open(path, &OpenOptions::new().read(true))
                .await
                .expect("failed to open data file");
            stream::iter(offsets)
                .map(|offset| {
                    let file = file.clone();
                    async move {
                        file.read_exact_at(size, offset).await.expect("read failed");
                    }
                })
                .buffer_unordered(concurrency)
                .collect::<Vec<_>>()
                .await;
        }
    }
    report(engine, workload, ops, ops * size, started);
    if matches!(engine, Engine::LoreIo(_)) {
        let stats = lore_io::pool_stats();
        println!(
            "{:<18} {:<28} threads {}/{} peak {}  queue peak {}",
            "",
            "  pool",
            stats.threads,
            stats.max_threads,
            stats.threads_high_water,
            stats.queue_high_water
        );
    }
}

async fn small_files_create_phase(engine: &Engine, subdir: &Path) {
    let payload = Bytes::from(vec![0xabu8; SMALL_FILE_SIZE]);
    match engine {
        Engine::BlockingPool => {
            stream::iter(0..SMALL_FILE_COUNT)
                .map(|index| {
                    let path = subdir.join(format!("file-{index}"));
                    let payload = payload.clone();
                    async move { blocking_write_new_file(path, payload).await }
                })
                .buffer_unordered(SMALL_FILE_CONCURRENCY)
                .collect::<Vec<_>>()
                .await;
        }
        Engine::TokioFs => {
            stream::iter(0..SMALL_FILE_COUNT)
                .map(|index| {
                    let path = subdir.join(format!("file-{index}"));
                    let payload = payload.clone();
                    async move {
                        tokio::fs::write(path, payload)
                            .await
                            .expect("create failed");
                    }
                })
                .buffer_unordered(SMALL_FILE_CONCURRENCY)
                .collect::<Vec<_>>()
                .await;
        }
        Engine::LoreIo(driver) => {
            stream::iter(0..SMALL_FILE_COUNT)
                .map(|index| {
                    let path = subdir.join(format!("file-{index}"));
                    let payload = payload.clone();
                    let driver = driver.clone();
                    async move {
                        driver
                            .write_file_bytes(path, payload, false)
                            .await
                            .expect("create failed");
                    }
                })
                .buffer_unordered(SMALL_FILE_CONCURRENCY)
                .collect::<Vec<_>>()
                .await;
        }
    }
}

async fn small_files_read_phase(engine: &Engine, subdir: &Path) {
    match engine {
        Engine::BlockingPool => {
            stream::iter(0..SMALL_FILE_COUNT)
                .map(|index| {
                    let path = subdir.join(format!("file-{index}"));
                    async move { blocking_read_whole_file(path).await }
                })
                .buffer_unordered(SMALL_FILE_CONCURRENCY)
                .collect::<Vec<_>>()
                .await;
        }
        Engine::TokioFs => {
            stream::iter(0..SMALL_FILE_COUNT)
                .map(|index| {
                    let path = subdir.join(format!("file-{index}"));
                    async move {
                        tokio::fs::read(path).await.expect("read failed");
                    }
                })
                .buffer_unordered(SMALL_FILE_CONCURRENCY)
                .collect::<Vec<_>>()
                .await;
        }
        Engine::LoreIo(driver) => {
            stream::iter(0..SMALL_FILE_COUNT)
                .map(|index| {
                    let path = subdir.join(format!("file-{index}"));
                    let driver = driver.clone();
                    async move {
                        driver.read_file_bytes(&path).await.expect("read failed");
                    }
                })
                .buffer_unordered(SMALL_FILE_CONCURRENCY)
                .collect::<Vec<_>>()
                .await;
        }
    }
}

// ---------------------------------------------------------------------------
// Data preparation and cache eviction
// ---------------------------------------------------------------------------

async fn write_random_file(driver: &IoDriver, path: &Path, size: u64) {
    let file: IoFile = driver
        .open(
            path,
            &OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true),
        )
        .await
        .expect("failed to create data file");
    let chunk = 1024 * 1024;
    let mut rng = XorShift::new(0x5eed ^ size);
    for index in 0..(size as usize / chunk) {
        let mut buffer = vec![0u8; chunk];
        for value in buffer.iter_mut() {
            *value = rng.next() as u8;
        }
        file.write_all_at(buffer, (index * chunk) as u64)
            .await
            .expect("failed to write data file");
    }
    file.sync_data().await.expect("failed to sync data file");
}

fn random_offsets(ops: usize, file_size: u64, size: usize) -> Vec<u64> {
    let mut rng = XorShift::new(0xdeadbeef ^ size as u64);
    let slots = file_size / size as u64;
    (0..ops)
        .map(|_| (rng.next() % slots) * size as u64)
        .collect()
}

/// Offsets covering `take` distinct slots in random order, so a cold run
/// never re-reads a block it already pulled into cache.
fn permuted_offsets(slots: u64, take: usize, stride: u64) -> Vec<u64> {
    let mut rng = XorShift::new(0xc01dcafe ^ slots);
    let mut indices: Vec<u64> = (0..slots).collect();
    for i in (1..indices.len()).rev() {
        let j = (rng.next() % (i as u64 + 1)) as usize;
        indices.swap(i, j);
    }
    indices.truncate(take);
    indices.into_iter().map(|index| index * stride).collect()
}

/// Drops a file's clean pages from the page cache. Effective on page-cache
/// filesystems (ext4, xfs); a no-op on ZFS, whose ARC requires the
/// `drop_caches` step from the cold-suite instructions.
#[cfg(all(target_family = "unix", not(target_vendor = "apple")))]
fn evict_from_page_cache(path: &Path) {
    use std::os::fd::AsRawFd;
    let Ok(file) = File::open(path) else {
        return;
    };
    let _ = file.sync_all();
    // Safety: Calling OS functions
    unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
}

/// Drops a file's cached pages on Apple platforms, which have no `posix_fadvise`.
///
/// `msync(MS_INVALIDATE)` over a mapping of the whole file invalidates that file's resident pages
/// in the unified buffer cache, and needs no privilege — unlike `purge`, which is what
/// `drop_caches` is here and which refuses without root. `F_NOCACHE` and `F_GLOBAL_NOCACHE` are
/// the candidates that look right and neither evicts anything: they change how a descriptor
/// reads, not what the cache holds.
///
/// Verified rather than assumed: a 256 MiB file re-read at 17-18 GiB/s before the call and
/// 3.3 GiB/s after it across three rounds, which is this machine's SSD sequential rate, and
/// 4 KiB whole files at 16 us before and 43 us after. The same probe measured no change at all
/// from either `F_NOCACHE` spelling.
#[cfg(all(target_family = "unix", target_vendor = "apple"))]
fn evict_from_page_cache(path: &Path) {
    use std::os::fd::AsRawFd;
    let Ok(file) = File::open(path) else {
        return;
    };
    let _ = file.sync_all();
    let Ok(length) = file.metadata().map(|metadata| metadata.len() as usize) else {
        return;
    };
    if length == 0 {
        return;
    }
    // Safety: Calling OS functions
    unsafe {
        let mapping = libc::mmap(
            std::ptr::null_mut(),
            length,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        );
        if mapping != libc::MAP_FAILED {
            libc::msync(mapping, length, libc::MS_INVALIDATE);
            libc::munmap(mapping, length);
        }
    }
}

/// Drops a file's cached pages on Windows, which has no `posix_fadvise`.
///
/// Opening a file with `FILE_FLAG_NO_BUFFERING` makes the cache manager flush and purge that
/// file's section, so the next buffered read of it reaches the device. That is the per-file
/// equivalent of the `posix_fadvise` call above and needs no privilege, unlike clearing the
/// standby list, which is what the machine-wide tools do and what `drop_caches` is on Linux.
/// Verified rather than assumed: a 1 GiB file re-read at 4.6 GiB/s before the purge and 469 MiB/s
/// after it, which is this drive's sequential rate.
#[cfg(target_family = "windows")]
fn evict_from_page_cache(path: &Path) {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;

    let _ = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_NO_BUFFERING)
        .open(path);
}

#[cfg(not(any(target_family = "unix", target_family = "windows")))]
fn evict_from_page_cache(_path: &Path) {}

#[cfg(target_family = "unix")]
fn sync_filesystem() {
    // Safety: Calling OS functions
    unsafe { libc::sync() };
}

/// No machine-wide flush on Windows: `FlushFileBuffers` on a volume handle needs administrator
/// rights, which nothing else about running this benchmark does. The settle between engine
/// children still runs, and each cold phase purges the files it is about to read.
#[cfg(not(target_family = "unix"))]
fn sync_filesystem() {}

// ---------------------------------------------------------------------------
// Blocking-pool engine
// ---------------------------------------------------------------------------

// The blocking-pool engine deliberately reproduces today's dispatch shape
// (tokio::fs / lore_spawn_blocking! both round-trip through the tokio
// blocking pool), so it uses spawn_blocking directly as the baseline.
#[allow(clippy::disallowed_methods)]
async fn blocking_read_exact_at(file: Arc<File>, size: usize, offset: u64) {
    // `LORE_BENCH_ALLOC_OUTSIDE` moves the allocation to the calling task, which is where lore-io
    // does it. Both spellings are `vec![0u8; size]`; what differs by default is the thread it runs
    // on — the baseline allocates on the pool thread that will receive the read and frees on the
    // worker that awaits it, lore-io does both on the worker.
    if alloc_outside() {
        let mut buffer = vec![0u8; size];
        tokio::task::spawn_blocking(move || {
            read_exact_at_impl(&file, &mut buffer, offset).expect("read failed");
            buffer
        })
        .await
        .expect("blocking task failed");
    } else {
        tokio::task::spawn_blocking(move || {
            let mut buffer = vec![0u8; size];
            read_exact_at_impl(&file, &mut buffer, offset).expect("read failed");
            buffer
        })
        .await
        .expect("blocking task failed");
    }
}

#[allow(clippy::disallowed_methods)]
async fn blocking_write_all_at(file: Arc<File>, size: usize, offset: u64) {
    tokio::task::spawn_blocking(move || {
        let buffer = vec![0u8; size];
        write_all_at_impl(&file, &buffer, offset).expect("write failed");
    })
    .await
    .expect("blocking task failed");
}

#[allow(clippy::disallowed_methods)]
async fn blocking_sync_data(file: Arc<File>) {
    tokio::task::spawn_blocking(move || file.sync_data().expect("sync failed"))
        .await
        .expect("blocking task failed");
}

#[allow(clippy::disallowed_methods)]
async fn blocking_write_new_file(path: PathBuf, payload: Bytes) {
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .expect("create failed");
        file.write_all(payload.as_ref()).expect("write failed");
        file.metadata().expect("stat failed")
    })
    .await
    .expect("blocking task failed");
}

#[allow(clippy::disallowed_methods)]
async fn blocking_read_whole_file(path: PathBuf) {
    tokio::task::spawn_blocking(move || std::fs::read(path).expect("read failed"))
        .await
        .expect("blocking task failed");
}

// ---------------------------------------------------------------------------
// tokio::fs engine
// ---------------------------------------------------------------------------

// tokio::fs has no positional read or write: `tokio::fs::File` is AsyncSeek + AsyncRead, so an
// offset is reached by seeking, and the file offset is per file description. `try_clone` is a
// `dup`, which shares that offset, so concurrent operations at different offsets cannot share a
// handle — each one opens its own. That open is not an artefact of the benchmark; it is what the
// API forces on any caller doing concurrent positional I/O, and it is the reason a migration to
// tokio::fs would cost more than the blocking-pool baseline it would replace.
async fn tokio_fs_read_exact_at(path: PathBuf, size: usize, offset: u64) {
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncSeekExt;

    let mut file = tokio::fs::File::open(&path).await.expect("open failed");
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .expect("seek failed");
    let mut buffer = vec![0u8; size];
    file.read_exact(&mut buffer).await.expect("read failed");
}

async fn tokio_fs_write_all_at(path: PathBuf, size: usize, offset: u64) {
    use tokio::io::AsyncSeekExt;
    use tokio::io::AsyncWriteExt;

    let mut file = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .await
        .expect("open failed");
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .expect("seek failed");
    file.write_all(&vec![0u8; size])
        .await
        .expect("write failed");
}

#[cfg(target_family = "unix")]
fn read_exact_at_impl(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buffer, offset)
}

#[cfg(target_family = "unix")]
fn write_all_at_impl(file: &File, buffer: &[u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buffer, offset)
}

#[cfg(target_family = "windows")]
fn read_exact_at_impl(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buffer.len() {
        let read = file.seek_read(&mut buffer[done..], offset + done as u64)?;
        assert!(read > 0, "unexpected end of file");
        done += read;
    }
    Ok(())
}

#[cfg(target_family = "windows")]
fn write_all_at_impl(file: &File, buffer: &[u8], offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buffer.len() {
        done += file.seek_write(&buffer[done..], offset + done as u64)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Reporting and utilities
// ---------------------------------------------------------------------------

fn print_header() {
    println!(
        "{:<18} {:<28} {:>8} {:>9} {:>8} {:>10} {:>9}",
        "engine", "workload", "ops", "MiB", "secs", "ops/s", "MiB/s"
    );
}

/// Shortest a phase may run before its result is worth less than it looks.
///
/// Phase sizes are fixed in the source and were chosen against the devices available at the time,
/// so a faster one shrinks them: the cold phases are 4096 operations, about a second on a SATA SSD
/// and 90 ms on an `NVMe`. A phase that short measures frequency ramp and cache state alongside the
/// engine, and no number of repeats recovers the signal — this prints rather than adjusts because
/// the fixed sizes are what make two people's runs comparable.
const SHORT_PHASE_SECONDS: f64 = 0.25;

fn report(engine: &Engine, workload: &str, ops: usize, bytes: usize, started: Instant) {
    let seconds = started.elapsed().as_secs_f64();
    let mib = bytes as f64 / (1024.0 * 1024.0);
    println!(
        "{:<18} {:<28} {:>8} {:>9.1} {:>8.3} {:>10.0} {:>9.1}",
        engine.name(),
        workload,
        ops,
        mib,
        seconds,
        ops as f64 / seconds,
        mib / seconds
    );
    if seconds < SHORT_PHASE_SECONDS {
        println!(
            "{:<18} {:<28} ran {seconds:.3}s, under the {SHORT_PHASE_SECONDS:.2}s floor — this device outpaces the phase size",
            "", "warning",
        );
    }
}

/// A [`run_cold_baseline`] line, tagged by the engine whose data set it measured rather than by an
/// engine that ran, in the column layout [`report`] uses.
fn report_baseline(tag: &str, workload: &str, ops: usize, bytes: usize, ops_per_sec: f64) {
    let mib = bytes as f64 / (1024.0 * 1024.0);
    let seconds = ops as f64 / ops_per_sec;
    println!(
        "{:<18} {:<28} {:>8} {:>9.1} {:>8.3} {:>10.0} {:>9.1}",
        format!("baseline:{tag}"),
        workload,
        ops,
        mib,
        seconds,
        ops_per_sec,
        mib / seconds
    );
}

struct XorShift {
    state: u64,
}

impl XorShift {
    fn new(seed: u64) -> XorShift {
        XorShift { state: seed | 1 }
    }

    fn next(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> TempDir {
        let path = bench_root().join(format!("lore-io-bench-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("failed to create bench dir");
        TempDir { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
