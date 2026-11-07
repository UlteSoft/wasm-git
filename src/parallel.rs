//! One bounded worker pool shared by scanning, comparison and tree traversal.
#[cfg(not(target_family = "wasm"))]
use rayon::prelude::*;
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(not(target_family = "wasm"))]
use std::sync::OnceLock;

static THREADS: AtomicUsize = AtomicUsize::new(0);
#[cfg(not(target_family = "wasm"))]
static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();

pub fn configure(threads: usize) -> Result<(), String> {
    if threads > 256 {
        return Err("--threads must be between 0 (automatic) and 256".into());
    }
    #[cfg(target_family = "wasm")]
    if threads > 1 {
        return Err("this WASI build supports one worker; use --threads 0 or 1".into());
    }
    #[cfg(not(target_family = "wasm"))]
    if POOL.get().is_some() {
        return Err("worker pool is already in use".into());
    }
    THREADS.store(threads, Ordering::Relaxed);
    Ok(())
}

#[cfg(not(target_family = "wasm"))]
fn pool() -> &'static rayon::ThreadPool {
    POOL.get_or_init(|| {
        let configured = THREADS.load(Ordering::Relaxed);
        let threads = if configured == 0 {
            std::thread::available_parallelism().map_or(1, usize::from)
        } else {
            configured
        };
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("wasm-git-worker-{i}"))
            .build()
            .expect("create bounded Wasm worker pool")
    })
}

/// All logical CPUs available to this process by default; explicit --threads
/// still bounds the shared pool. Compression uses this same pool, not a second
/// nested set of codec workers.
pub fn worker_count() -> usize {
    #[cfg(not(target_family = "wasm"))]
    {
        pool().current_num_threads()
    }
    #[cfg(target_family = "wasm")]
    {
        1
    }
}

pub(crate) fn map<T: Send, F: Fn(usize) -> T + Sync + Send>(len: usize, f: F) -> Vec<T> {
    #[cfg(not(target_family = "wasm"))]
    if len >= 32 && THREADS.load(Ordering::Relaxed) != 1 {
        return pool().install(|| (0..len).into_par_iter().map(f).collect());
    }
    (0..len).map(f).collect()
}

pub(crate) fn join<A: Send, B: Send>(
    work: usize,
    a: impl FnOnce() -> A + Send,
    b: impl FnOnce() -> B + Send,
) -> (A, B) {
    #[cfg(not(target_family = "wasm"))]
    if work >= 32 * 1024 && THREADS.load(Ordering::Relaxed) != 1 {
        return pool().install(|| rayon::join(a, b));
    }
    let _ = work;
    (a(), b())
}

pub(crate) fn map_owned<T: Send, U: Send>(
    items: Vec<T>,
    f: impl Fn(T) -> U + Sync + Send,
) -> Vec<U> {
    #[cfg(not(target_family = "wasm"))]
    if items.len() >= 2 && THREADS.load(Ordering::Relaxed) != 1 {
        return pool().install(|| items.into_par_iter().map(f).collect());
    }
    items.into_iter().map(f).collect()
}
