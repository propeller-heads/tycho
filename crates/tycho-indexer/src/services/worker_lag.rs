//! Measures how late the request workers poll futures that are ready.
use std::{
    cell::Cell,
    time::{Duration, Instant},
};

const PROBE_PERIOD: Duration = Duration::from_millis(100);

thread_local! {
    static PROBE_RUNNING: Cell<bool> = const { Cell::new(false) };
}

/// Starts, on the calling actix worker, a task that sleeps [`PROBE_PERIOD`] in a loop and records
/// how much later than asked it wakes in `rpc_worker_poll_lag_ms`. A worker that is busy with
/// other work polls the task late. Starts at most one task per thread; the task ends with the
/// worker.
///
/// # Panics
///
/// Panics when called outside an actix worker (a tokio `LocalSet`).
pub(super) fn spawn_poll_lag_probe() {
    if PROBE_RUNNING.with(|running| running.replace(true)) {
        return;
    }
    actix_web::rt::spawn(async {
        loop {
            let due = Instant::now() + PROBE_PERIOD;
            actix_web::rt::time::sleep_until(due.into()).await;
            let lag = Instant::now().saturating_duration_since(due);
            metrics::histogram!("rpc_worker_poll_lag_ms").record(lag.as_secs_f64() * 1000.0);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[actix_web::test]
    async fn test_starts_one_probe_per_thread() {
        spawn_poll_lag_probe();
        spawn_poll_lag_probe();

        assert!(PROBE_RUNNING.with(Cell::get));
        // The probe must keep running next to other tasks without panicking.
        actix_web::rt::time::sleep(PROBE_PERIOD * 3).await;
    }
}
