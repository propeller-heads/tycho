//! Code related to [worker_thread]s and how they [find_work]
//! using work-stealing.
use std::{
    io::Write,
    sync::atomic::{AtomicBool, Ordering},
};

use crossbeam_deque::{Injector, Steal, Stealer, Worker};

use crate::{
    Error, Outcome, Telemetry,
    params::{Params, ParamsInner, RequestParamError},
    progress::Counters,
    simulate,
};

/// Returns a [worker_thread]'s next work item. First looks in local queue, then tries
/// to steal work from other workers, and finally looks in global queue.
///
/// Most of the time the next work item will be found in the local queue.
/// Checking the local queue is very fast as it requires no waiting
/// on other threads.
///
/// In other words, each [worker_thread] can just do its thing more than 99% of the time
/// without having to wait on other [worker_thread]s.
///
/// Without local queues so much time is spent waiting on other threads,
/// to get the global queue's mutex,
/// that concurrency barely increases overall performance.
pub fn find_work(
    local_queue: &Worker<ParamsInner>,
    stealers: &[Stealer<ParamsInner>],
    global_queue: &Injector<ParamsInner>,
) -> Option<ParamsInner> {
    // first try to get work item from the worker's thread local queue.
    // very efficient as it has no synchronization overhead.
    if let Some(work) = local_queue.pop() {
        return Some(work);
    }

    // if the thread local queue is empty,
    // try to steal work item from other worker threads.
    for stealer in stealers {
        loop {
            match stealer.steal() {
                Steal::Empty => {
                    break;
                }
                Steal::Success(work) => {
                    return Some(work);
                }
                Steal::Retry => {}
            }
        }
    }

    // if all worker's queues are empty,
    // try to steal work from the global queue.
    // the global queue is also called "injector" because it
    // is used to inject work into the system from outside.
    // the simulation only injects a single initial work item into the global queue.
    // all other work items are created from that initial work item.
    loop {
        match global_queue.steal() {
            Steal::Empty => {
                break;
            }
            Steal::Success(work) => {
                return Some(work);
            }
            Steal::Retry => {}
        }
    }

    None
}

/// Returns a [worker_thread]'s next work item.
/// Calls [find_work] but, unlike [find_work],
/// doesn't simply return `None` if no work was found.
/// Instead, it enters an idle state,
/// which it communicates via `is_worker_idle`, and retries [find_work] every 100ms.
/// This prevents [worker_thread]s from exiting during a short window at the beginning
/// of the simulation when a single [worker_thread] works on
/// the single initial work item and no work is available for the other [worker_thread]s.
///
/// The main thread checks every 100ms whether all [worker_thread]s that are still
/// running are idle. If all are idle, `tell_idle_workers_to_stop` is set to `true`
/// telling all [worker_thread]s to stop.
pub fn find_work_unless_all_idle(
    local_queue: &Worker<ParamsInner>,
    stealers: &[Stealer<ParamsInner>],
    global_queue: &Injector<ParamsInner>,
    is_worker_idle: &AtomicBool,
    tell_idle_workers_to_stop: &AtomicBool,
) -> Option<ParamsInner> {
    match find_work(local_queue, stealers, global_queue) {
        Some(work) => Some(work),
        None => {
            // all of the idle management happens only if a worker doesn't find any work.
            // if we were to break here, then several threads would simply exit
            // in the beginning when there's a small window when only
            // one thread is working on the initial work item and all queues
            // are empty.
            // only exit if all queues are empty after a short wait.

            // worker found no work.
            // mark it as idle.
            // if all workers are marked as idle,
            // main thread will set `tell_idle_workers_to_stop` to `true`,
            // which in turn will cause all workers to exit.
            is_worker_idle.store(true, Ordering::Release);
            eprintln!("worker thread {:?} idle", std::thread::current().id());

            loop {
                if tell_idle_workers_to_stop.load(Ordering::Acquire) {
                    // the main thread has detected that all running
                    // workers are idle
                    return None;
                }
                // back off
                // give main thread some time to detect that all workers are idle
                std::thread::sleep(std::time::Duration::from_millis(100));
                // try to find work again
                if let Some(work) = find_work(local_queue, stealers, global_queue) {
                    eprintln!(
                        "worker thread {:?} busy (after it was idle)",
                        std::thread::current().id()
                    );
                    is_worker_idle.store(false, Ordering::Release);
                    return Some(work);
                }
            }
        }
    }
}

/// Main function of worker thread.
/// The main function spawns this [WORKER_THREAD_COUNT](crate::config::WORKER_THREAD_COUNT) times.
/// In a loop [finds work](crate::worker::find_work),
/// calls [simulate], handles [Error]s, resolves [RequestParam](crate::params::RequestParam)s,
/// checks whether successful runs result in [suspicious](Outcome::is_suspicious)
/// outcomes, and writes suspicious [Outcome]s as YAML docs to stdout.
pub fn worker_thread(
    global_queue: &Injector<ParamsInner>,
    local_queue: &Worker<ParamsInner>,
    stealers: &[Stealer<ParamsInner>],
    is_worker_idle: &AtomicBool,
    tell_idle_workers_to_stop: &AtomicBool,
    is_first_output: &AtomicBool,
    mut counters: Counters,
) -> Telemetry {
    eprintln!("worker thread {:?} spawned", std::thread::current().id());
    let mut telemetry = Telemetry::default();
    while let Some(work) = find_work_unless_all_idle(
        local_queue,
        stealers,
        global_queue,
        is_worker_idle,
        tell_idle_workers_to_stop,
    ) {
        let params = Params::from(work);
        let result = simulate(&params);

        counters.simulated.increment();
        if !matches!(result, Err(Error::RequestParamError(_))) {
            // don't count incomplete params that result in param request
            for (key, value) in params.0.iter() {
                telemetry.param_simulated(key, value);
            }
        }

        match result {
            Ok((state, vault, log)) => {
                counters.success.increment();
                for (key, value) in params.0.iter() {
                    telemetry.param_success(key, value);
                }
                let executors = params.executors();
                telemetry.executors_success(executors.clone());

                let outcome = Outcome::new(params, state, vault, log);

                if outcome.is_suspicious() {
                    let mut stdout = std::io::stdout().lock();
                    // don't write the YAML document separator before the first document
                    if !is_first_output.swap(false, Ordering::SeqCst) {
                        writeln!(stdout, "---").unwrap();
                    }
                    counters.suspicious.increment();
                    telemetry.executors_suspicious(executors);
                    yaml_serde::to_writer(stdout, &outcome).unwrap();
                }
            }
            Err(Error::RequestParamError(RequestParamError::RequestParam(param_request))) => {
                counters.request_param.increment();
                for params in param_request {
                    local_queue.push(params);
                }
            }
            // ignore reverts
            Err(Error::Revert { reason }) => {
                telemetry.executors_revert(params.executors(), reason);
            }
            // Somewhere in the model it was decided that it is not
            // worth continuing the simulation with the given parameters
            Err(Error::Ignore { reason }) => {
                counters.ignored.increment();
                telemetry.executors_ignore(params.executors(), reason);
            }
            Err(Error::Warning { reason }) => {
                counters.warning.increment();
                telemetry.executors_warning(params.executors(), reason);
                // TODO print out warning if enabled to be able to debug it
            }
            Err(err) => panic!("{err:?}"),
        }
    }
    eprintln!("worker thread {:?} finished", std::thread::current().id());
    telemetry
}
