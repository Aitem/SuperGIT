//! Compare complete statuses across several checkouts.
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context as _, Result};
use sg_git::WorktreeStatus;

type Reader = fn(&Path) -> Result<WorktreeStatus>;

fn read_all(paths: &[PathBuf], reader: Reader) -> Result<Vec<WorktreeStatus>> {
    std::thread::scope(|scope| {
        let threads: Vec<_> = paths
            .iter()
            .map(|path| scope.spawn(move || reader(path)))
            .collect();
        threads
            .into_iter()
            .map(|task| {
                task.join()
                    .map_err(|_| anyhow::anyhow!("Status worker panicked"))?
            })
            .collect()
    })
}

#[expect(clippy::print_stdout, reason = "benchmark results are its output")]
fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let mode = args
        .next()
        .context("Usage: measure_status compare|cli|git2 WORKTREE...")?;
    let paths: Vec<PathBuf> = args.map(PathBuf::from).collect();
    anyhow::ensure!(!paths.is_empty(), "Provide at least one worktree");

    const READERS: [(&str, Reader); 2] = [
        ("cli", sg_git::status),
        ("git2", sg_git::probe_native_status),
    ];
    let readers = match mode.to_str() {
        Some("compare") => READERS.as_slice(),
        Some("cli") => &READERS[..1],
        Some("git2") => &READERS[1..],
        _ => anyhow::bail!("Unknown backend"),
    };

    let expected = read_all(&paths, sg_git::status)?;
    let mut timings = vec![Vec::with_capacity(30); readers.len()];
    for &(_, reader) in readers {
        assert_eq!(
            read_all(&paths, reader)?,
            expected,
            "Git backend parity mismatch"
        );
    }
    // Alternate the execution order so both backends see the same warm-up.
    for sample in 0..30 {
        for offset in 0..readers.len() {
            let index = (sample + offset) % readers.len();
            let start = Instant::now();
            let result = read_all(&paths, readers[index].1)?;
            let elapsed = start.elapsed();
            assert_eq!(result, expected, "Git backend parity mismatch");
            timings[index].push(elapsed.as_secs_f64() * 1000.0);
        }
    }
    for ((name, _), mut samples) in readers.iter().zip(timings) {
        samples.sort_unstable_by(f64::total_cmp);
        println!(
            "{name} · {} worktrees, 30 polls · p50 {:.2} ms · p95 {:.2} ms",
            paths.len(),
            (samples[14] + samples[15]) / 2.0,
            samples[28]
        );
    }
    Ok(())
}
