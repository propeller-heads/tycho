//! Code related to [progress_thread] that writes progress information,
//! like speed and count of suspicious, to stderr every second.
use std::{
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

/// Main function of thread that writes progress information,
/// like speed and count of suspicious, to stderr every second.
///
/// Exits when `is_running` is set to `false`.
/// Main thread will set `is_running` to `false` as soon
/// as all worker threads have finished.
pub fn progress_thread(counters: &Counters, is_running: &AtomicBool) {
    let instant_start = std::time::Instant::now();
    while is_running.load(Ordering::Relaxed) {
        {
            let elapsed = instant_start.elapsed();
            let count_simulated: usize = counters.simulated.count();
            let simulated_per_second = (count_simulated as f64 / elapsed.as_secs_f64()).round();
            writeln!(
                std::io::stderr().lock(),
                "secs: {}, simulated: {}, success: {}, suspicious: {}, request_param: {}, ignored: {}, warning: {}, simulated_per_second: {}",
                elapsed.as_secs(),
                count_simulated,
                counters.success.count(),
                counters.suspicious.count(),
                counters.request_param.count(),
                counters.ignored.count(),
                counters.warning.count(),
                simulated_per_second
            ).unwrap();
        }
        // print the counters to stderr every second
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

/// Numbers that provide useful information about the progress
/// of the simulation that [progress_thread] writes to stderr every second.
#[derive(Default, Clone)]
pub struct Counters {
    pub simulated: LazyAtomicCounter,
    pub request_param: LazyAtomicCounter,
    /// halted i.e. not reverted
    pub success: LazyAtomicCounter,
    pub suspicious: LazyAtomicCounter,
    pub ignored: LazyAtomicCounter,
    pub warning: LazyAtomicCounter,
}

/// Counter that can be efficiently incremented from multiple threads.
///
/// Incrementing [usize] is faster than incrementing [AtomicUsize].
/// Incrementing [AtomicUsize] millions of times per second from multiple threads
/// creates significant overhead.
///
/// [LazyAtomicCounter] amortizes the cost of incrementing shared [AtomicUsize]
/// by only updating [AtomicUsize] after thousands of increments of local [usize].
#[derive(Default, Clone)]
pub struct LazyAtomicCounter {
    shared: Arc<AtomicUsize>,
    local: usize,
}

impl LazyAtomicCounter {
    pub fn increment(&mut self) {
        self.local += 1;
        if self.local >= 10000 {
            // only do expensive operation for every thousandth cheap operation
            self.shared
                .fetch_add(self.local, Ordering::Relaxed);
            self.local = 0;
        }
    }

    pub fn count(&self) -> usize {
        self.shared.load(Ordering::Relaxed)
    }
}
