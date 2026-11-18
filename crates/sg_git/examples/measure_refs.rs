//! Local experiment. Measure the app's real History key on this repository.
use std::path::PathBuf;
use std::time::Instant;

#[expect(clippy::print_stdout, reason = "benchmark results are its output")]
fn main() -> anyhow::Result<()> {
    let paths: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    anyhow::ensure!(!paths.is_empty(), "Provide a repository path");
    let expected: Vec<_> = paths
        .iter()
        .map(|path| sg_git::history_refs(path))
        .collect::<anyhow::Result<_>>()?;
    let mut samples = Vec::with_capacity(125);
    for _ in 0..125 {
        let start = Instant::now();
        let keys: Vec<_> = paths
            .iter()
            .map(|path| sg_git::history_refs(path))
            .collect::<anyhow::Result<_>>()?;
        let elapsed = start.elapsed();
        assert_eq!(keys, expected);
        samples.push(elapsed.as_secs_f64() * 1000.0);
    }
    samples.sort_unstable_by(f64::total_cmp);
    println!(
        "{} worktree snapshot(s), 125 samples. p50={:.3} ms p95={:.3} ms",
        paths.len(),
        samples[62],
        samples[118]
    );
    Ok(())
}
