//! One bounded worker pool shared by scanning, comparison and tree traversal.
use rayon::prelude::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    OnceLock,
};

static THREADS: AtomicUsize = AtomicUsize::new(0);
static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();

pub fn configure(threads: usize) -> Result<(), String> {
    if threads > 256 {
        return Err("--threads must be between 0 (automatic) and 256".into());
    }
    if POOL.get().is_some() {
        return Err("worker pool is already in use".into());
    }
    THREADS.store(threads, Ordering::Relaxed);
    Ok(())
}

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
            .thread_name(|i| format!("uwgit-worker-{i}"))
            .build()
            .expect("create bounded Wasm worker pool")
    })
}

/// All logical CPUs available to this process by default; explicit --threads
/// still bounds the shared pool. Compression uses this same pool, not a second
/// nested set of codec workers.
pub fn worker_count() -> usize {
    pool().current_num_threads()
}

pub(crate) fn map<T: Send, F: Fn(usize) -> T + Sync + Send>(len: usize, f: F) -> Vec<T> {
    if len < 32 || THREADS.load(Ordering::Relaxed) == 1 {
        (0..len).map(f).collect()
    } else {
        pool().install(|| (0..len).into_par_iter().map(f).collect())
    }
}

pub(crate) fn join<A: Send, B: Send>(
    work: usize,
    a: impl FnOnce() -> A + Send,
    b: impl FnOnce() -> B + Send,
) -> (A, B) {
    if work < 32 * 1024 || THREADS.load(Ordering::Relaxed) == 1 {
        (a(), b())
    } else {
        pool().install(|| rayon::join(a, b))
    }
}

pub(crate) fn map_owned<T: Send, U: Send>(
    items: Vec<T>,
    f: impl Fn(T) -> U + Sync + Send,
) -> Vec<U> {
    if items.len() < 2 || THREADS.load(Ordering::Relaxed) == 1 {
        items.into_iter().map(f).collect()
    } else {
        pool().install(|| items.into_par_iter().map(f).collect())
    }
}
