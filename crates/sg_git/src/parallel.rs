//! Independent slow work — `git`/`gh` subprocesses, file reads — side by
//! side instead of one after another. Callers stay synchronous: these block
//! until every piece is done, on threads of their own, so the GPUI
//! background pool is not filled with waiting subprocesses.

use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::thread;

/// Threads for a per-item fan-out (worktrees, submodules, files): enough to
/// overlap subprocess waits, few enough that dozens of worktrees do not
/// start dozens of `git` processes at once.
pub fn workers() -> usize {
    thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(2, 8)
}

/// Run `a` here and `b` on another thread; both results, as if called in
/// sequence (a panic in either reaches the caller).
pub fn join<A, B>(a: impl FnOnce() -> A, b: impl FnOnce() -> B + Send) -> (A, B)
where
    B: Send,
{
    thread::scope(|scope| {
        let b = scope.spawn(b);
        let a = a();
        (a, b.join().unwrap_or_else(|panic| resume_unwind(panic)))
    })
}

/// `f` over `items` on up to [`workers`] threads; results keep the order of
/// `items`. An item whose `f` panicked yields `Err` with the panic, the
/// others still run.
pub fn map<T, R, F>(items: &[T], f: F) -> Vec<thread::Result<R>>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    let run = |item: &T| catch_unwind(AssertUnwindSafe(|| f(item)));
    let workers = workers().min(items.len());
    if workers <= 1 {
        return items.iter().map(run).collect();
    }
    let next = AtomicUsize::new(0);
    let slots: Vec<Mutex<Option<thread::Result<R>>>> =
        items.iter().map(|_| Mutex::new(None)).collect();
    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let ix = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(ix) else {
                        break;
                    };
                    let result = run(item);
                    *slots[ix].lock().unwrap_or_else(PoisonError::into_inner) = Some(result);
                }
            });
        }
    });
    slots
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .unwrap_or_else(PoisonError::into_inner)
                .expect("each worker fills the slots it takes")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn map_keeps_order_runs_side_by_side_and_isolates_panics() {
        let items: Vec<u64> = (0..workers() as u64).collect();
        let started = Instant::now();
        let results = map(&items, |n| {
            thread::sleep(Duration::from_millis(100));
            assert_ne!(*n, 1, "item 1 fails");
            n * 10
        });
        // One batch of sleeps, not one per item.
        assert!(started.elapsed() < Duration::from_millis(100 * items.len() as u64));
        assert_eq!(results[0].as_ref().ok(), Some(&0));
        assert!(results[1].is_err());
        assert_eq!(
            results[items.len() - 1].as_ref().ok(),
            Some(&((items.len() as u64 - 1) * 10))
        );
        assert!(map(&[] as &[u8], |_| ()).is_empty());
    }

    #[test]
    fn join_returns_both_results() {
        let started = Instant::now();
        let (a, b) = join(
            || {
                thread::sleep(Duration::from_millis(100));
                "a"
            },
            || {
                thread::sleep(Duration::from_millis(100));
                2
            },
        );
        assert_eq!((a, b), ("a", 2));
        assert!(started.elapsed() < Duration::from_millis(190));
    }
}
