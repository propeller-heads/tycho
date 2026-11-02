use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crossbeam_deque::{Injector, Worker};
use tycho_router_model::{
    Telemetry,
    params::ParamsInner,
    progress::{Counters, progress_thread},
    worker::worker_thread,
};

fn main() {
    let global_queue_of_main_thread: Arc<Injector<ParamsInner>> = Arc::new(Injector::new());
    global_queue_of_main_thread.push(ParamsInner::default());

    // keep track of the first output so we don't print
    // the yaml document separator `---` before it
    let is_first_output_of_main_thread = Arc::new(AtomicBool::new(true));

    let mut all_stealers = Vec::new();
    let mut local_queues = Vec::new();
    let mut worker_is_idle = Vec::new();

    let tell_idle_workers_to_stop_of_main_thread = Arc::new(AtomicBool::new(false));

    for _ in 0..tycho_router_model::config::WORKER_THREAD_COUNT {
        let local_queue = Worker::new_lifo();
        all_stealers.push(local_queue.stealer());
        local_queues.push(local_queue);
        worker_is_idle.push(Arc::new(AtomicBool::new(false)));
    }

    let counters_of_main_thread = Counters::default();

    let mut worker_thread_join_handles: Vec<std::thread::JoinHandle<Telemetry>> = Vec::new();

    for (index_worker, local_queue) in local_queues.into_iter().enumerate() {
        // the following local variables assigned until `std::thread::spawn`
        // are worker local variables,
        // which are moved into the worker closure passed to `std::thread::spawn`.

        let mut stealers = Vec::new();
        // stealing from worker's own queue would be pointless.
        // each worker prefers stealing from its successor,
        // then second successor, etc, wrapping around.
        // this avoids that most workers steal from the first worker,
        // which would starve that worker and decrease its efficiency.
        for stealer in &all_stealers[(index_worker + 1)..] {
            stealers.push(stealer.clone());
        }
        for stealer in &all_stealers[..index_worker] {
            stealers.push(stealer.clone());
        }

        // since we need to move `local_queue` and `stealers` into the worker thread,
        // we have to make the closure that is spawned for the thread `move`.
        // `move` moves all values the closure uses into the closure.
        // as a result, we need to make clones of all Arcs for the worker thread
        // (hence the suffix `of_worker_thread`) so they can can be moved into the closure.
        let global_queue_of_worker_thread = Arc::clone(&global_queue_of_main_thread);
        let is_first_output_of_worker_thread = Arc::clone(&is_first_output_of_main_thread);
        let is_worker_idle_of_worker_thread = Arc::clone(&worker_is_idle[index_worker]);
        let tell_idle_workers_to_stop_of_worker_thread =
            Arc::clone(&tell_idle_workers_to_stop_of_main_thread);
        let counters_of_worker_thread = counters_of_main_thread.clone();

        worker_thread_join_handles.push(std::thread::spawn(move || {
            worker_thread(
                &global_queue_of_worker_thread,
                &local_queue,
                &stealers,
                &is_worker_idle_of_worker_thread,
                &tell_idle_workers_to_stop_of_worker_thread,
                &is_first_output_of_worker_thread,
                counters_of_worker_thread,
            )
        }));
    }

    let is_running_of_main_thread = Arc::new(AtomicBool::new(true));

    let is_running_of_progress_thread = Arc::clone(&is_running_of_main_thread);
    let counters_of_progress_thread = counters_of_main_thread;
    let progress_thread_join_handle = std::thread::spawn(move || {
        progress_thread(&counters_of_progress_thread, &is_running_of_progress_thread);
    });

    loop {
        // every 100ms this loop determines whether all workers are idle
        // and if all are idle tells all workers to stop
        let mut all_finished = true;
        let mut all_idle = true;
        for (index, handle) in worker_thread_join_handles
            .iter()
            .enumerate()
        {
            if !handle.is_finished() {
                all_finished = false;
                if !worker_is_idle[index].load(Ordering::Acquire) {
                    all_idle = false;
                }
            }
        }
        if all_finished {
            break;
        }
        if all_idle {
            // once all unfinished workers are idle, allow them to stop
            tell_idle_workers_to_stop_of_main_thread.store(true, Ordering::Release);
        }
        // without sleeping the main thread would do the expensive checks in the loop
        // needlessly fast and use 100% of one CPU core.
        // sleeping makes it use close to 0%.
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    // Merge the telemetry produced by all worker threads
    let mut telemetry = Telemetry::default();
    for handle in worker_thread_join_handles {
        match handle.join() {
            Ok(telemetry_worker_thread) => telemetry.merge_into(telemetry_worker_thread),
            Err(err) => std::panic::resume_unwind(err),
        }
    }

    // wait for the progress thread to print final statistics
    std::thread::sleep(std::time::Duration::from_secs(1));

    // tell progress thread to exit
    is_running_of_main_thread.store(false, Ordering::Relaxed);
    // wait for progress thread to exit
    match progress_thread_join_handle.join() {
        Ok(()) => {}
        Err(err) => std::panic::resume_unwind(err),
    }

    eprintln!();
    eprintln!("{telemetry}");
}
