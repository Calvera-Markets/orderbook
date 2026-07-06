//! Post-process criterion's per-bench `estimates.json` files into one flat
//! report — JSON (with host metadata) plus a CSV companion — written under
//! `benches/logs/`. Lets cross-host result comparison be a `diff` instead of
//! a hand-typed table.
//!
//! Usage:
//!   cargo run --release --example report
//!
//! Inputs:  ../../target/criterion/<flat_id>/new/estimates.json
//!          (criterion flattens "v1/mixed" → "v1_mixed" on disk; we restore
//!           the slash for the report by splitting on the first underscore.)
//! Outputs: benches/logs/results-<YYYYMMDD-HHMMSS>.{json,csv}

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct Report {
    host: Host,
    timestamp: String,
    results: Vec<Record>,
}

#[derive(Serialize)]
struct Host {
    os: String,
    arch: String,
    family: String,
}

#[derive(Serialize)]
struct Record {
    id: String,
    median_ns: f64,
    low_ns: f64,
    high_ns: f64,
}

#[derive(Deserialize)]
struct Estimates {
    median: Estimate,
}

#[derive(Deserialize)]
struct Estimate {
    point_estimate: f64,
    confidence_interval: ConfidenceInterval,
}

#[derive(Deserialize)]
struct ConfidenceInterval {
    lower_bound: f64,
    upper_bound: f64,
}

fn timestamp() -> String {
    // Use the `date` command — same format the bench logger uses.
    Command::new("date")
        .arg("+%Y%m%d-%H%M%S")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Restore the `/` in the bench id from criterion's on-disk dir name.
/// Criterion flattens every `/` to `_`, so `v1/cancel_heavy/cap_N/alloc_K`
/// lands on disk as `v1_cancel_heavy_cap_N_alloc_K`. Workload names contain
/// `_` (e.g. `cancel_heavy`, `add_cancel`), so naive split-on-first-`_`
/// works for `impl/workload` but mangles the optional suffixes.
///
/// Approach: peel off recognised suffixes (`_alloc_<slug>`, `_cap_<digits>`)
/// from the right, then split the remaining head on the first `_`.
const ALLOC_SLUGS: &[&str] = &["system", "madvise", "hugetlb"];

fn id_from_dir(dir_name: &str) -> String {
    let mut tail: Vec<String> = Vec::new();
    let mut head: String = dir_name.to_string();

    loop {
        if let Some(idx) = head.rfind("_alloc_") {
            let slug = &head[idx + "_alloc_".len()..];
            if ALLOC_SLUGS.contains(&slug) {
                tail.push(head[idx + 1..].to_string());
                head.truncate(idx);
                continue;
            }
        }
        if let Some(idx) = head.rfind("_cap_") {
            let n = &head[idx + "_cap_".len()..];
            if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) {
                tail.push(head[idx + 1..].to_string());
                head.truncate(idx);
                continue;
            }
        }
        break;
    }

    let mut id = head.replacen('_', "/", 1);
    tail.reverse(); // we peeled right-to-left; restore original order
    for segment in tail {
        id.push('/');
        id.push_str(&segment);
    }
    id
}

fn collect_records(criterion_dir: &Path) -> Vec<Record> {
    let mut records = Vec::new();
    let Ok(entries) = fs::read_dir(criterion_dir) else {
        eprintln!("warning: {} not found — run `cargo bench --bench engine` first", criterion_dir.display());
        return records;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let path = entry.path();
        let est_path = path.join("new/estimates.json");
        if !est_path.exists() {
            continue;
        }
        let text = match fs::read_to_string(&est_path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("skipping {}: {}", est_path.display(), e);
                continue;
            }
        };
        let est: Estimates = match serde_json::from_str(&text) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("skipping {}: parse error {}", est_path.display(), e);
                continue;
            }
        };
        let dir_name = entry.file_name().to_string_lossy().to_string();
        records.push(Record {
            id: id_from_dir(&dir_name),
            median_ns: est.median.point_estimate,
            low_ns: est.median.confidence_interval.lower_bound,
            high_ns: est.median.confidence_interval.upper_bound,
        });
    }
    records.sort_by(|a, b| a.id.cmp(&b.id));
    records
}

fn write_csv(records: &[Record], path: &Path) -> std::io::Result<()> {
    let mut f = fs::File::create(path)?;
    writeln!(f, "id,median_ns,low_ns,high_ns")?;
    for r in records {
        writeln!(f, "{},{:.3},{:.3},{:.3}", r.id, r.median_ns, r.low_ns, r.high_ns)?;
    }
    Ok(())
}

fn main() {
    // crate root = CARGO_MANIFEST_DIR; workspace root = two dirs up
    // (matches the existing bin/*.sh scripts).
    let crate_root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let workspace_root = crate_root
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf();
    let criterion_dir = workspace_root.join("target/criterion");

    let records = collect_records(&criterion_dir);
    if records.is_empty() {
        eprintln!("no benches found under {} — nothing to report", criterion_dir.display());
        std::process::exit(1);
    }

    let report = Report {
        host: Host {
            os: env::consts::OS.to_string(),
            arch: env::consts::ARCH.to_string(),
            family: env::consts::FAMILY.to_string(),
        },
        timestamp: timestamp(),
        results: records,
    };

    let logs_dir = crate_root.join("benches/logs");
    fs::create_dir_all(&logs_dir).expect("create benches/logs");
    let json_path = logs_dir.join(format!("results-{}.json", report.timestamp));
    let csv_path = logs_dir.join(format!("results-{}.csv", report.timestamp));

    fs::write(&json_path, serde_json::to_string_pretty(&report).expect("serialize json"))
        .expect("write json");
    write_csv(&report.results, &csv_path).expect("write csv");

    println!("→ host: {} / {} ({})", report.host.os, report.host.arch, report.host.family);
    println!("→ {} results", report.results.len());
    for r in &report.results {
        println!("  {:32} median={:>7.2} ns  ci=[{:>6.2}, {:>6.2}]", r.id, r.median_ns, r.low_ns, r.high_ns);
    }
    println!("→ json: {}", json_path.display());
    println!("→ csv:  {}", csv_path.display());
}
