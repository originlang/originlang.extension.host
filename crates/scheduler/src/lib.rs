//! In-process, monotonic scheduler.
//!
//! One worker thread owns all timers and runs actions serially, outside the
//! queue lock. That is the right shape for extension timers: callbacks may call
//! back into the host, and running them on the scheduler thread means the host
//! never has to reason about two callbacks of the same extension at once.
//!
//! Known limits, carried over deliberately from the reference engine:
//! timers are not persisted across host restarts, there is no automatic retry,
//! and cancellation of a running action is cooperative (it prevents recurrence,
//! it does not interrupt the current run).

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Callback invoked when a task is due.
pub type Action = Box<dyn FnMut() -> Result<(), String> + Send>;

/// A scheduled unit of work.
pub struct Task {
    /// Unique identifier; also used for cancellation and completion reports.
    pub id: String,
    /// Earliest instant the action may run.
    pub run_at: Instant,
    /// When set, the task re-arms after each run.
    pub interval: Option<Duration>,
    /// The work itself.
    pub action: Action,
}

impl Task {
    /// One-shot task due at `run_at`.
    pub fn once(id: impl Into<String>, run_at: Instant, action: Action) -> Self {
        Self {
            id: id.into(),
            run_at,
            interval: None,
            action,
        }
    }

    /// Recurring task: first run at `run_at`, then every `interval`.
    pub fn recurring(
        id: impl Into<String>,
        run_at: Instant,
        interval: Duration,
        action: Action,
    ) -> Self {
        Self {
            id: id.into(),
            run_at,
            interval: Some(interval),
            action,
        }
    }
}

/// Scheduler failure modes.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// The scheduler has been stopped.
    Stopped,
    /// A task with this id is queued or running.
    DuplicateId,
    /// Empty id, or a zero/absent interval where one was required.
    InvalidTask,
    /// A lock was poisoned.
    Poisoned,
}

/// Outcome of one task run, drained by the host to surface failures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    /// Task identifier.
    pub id: String,
    /// Result returned by the action.
    pub result: Result<(), String>,
}

struct State {
    stopped: bool,
    tasks: BTreeMap<String, Task>,
    running: Option<String>,
    cancelled: bool,
    completed: VecDeque<Completion>,
}

/// Maximum completions retained between drains.
const COMPLETION_BUFFER: usize = 256;

/// See the module documentation for threading and persistence guarantees.
pub struct Scheduler {
    shared: Arc<(Mutex<State>, Condvar)>,
    worker: Option<JoinHandle<()>>,
}

impl Scheduler {
    /// Starts the worker thread.
    pub fn new() -> Self {
        let shared = Arc::new((
            Mutex::new(State {
                stopped: false,
                tasks: BTreeMap::new(),
                running: None,
                cancelled: false,
                completed: VecDeque::new(),
            }),
            Condvar::new(),
        ));
        let worker_state = shared.clone();
        let worker = thread::Builder::new()
            .name("ext-scheduler".into())
            .spawn(move || run(worker_state))
            .ok();
        Self { shared, worker }
    }

    /// Queues a task.
    pub fn add(&self, task: Task) -> Result<(), Error> {
        if task.id.is_empty() || task.interval.is_some_and(|d| d.is_zero()) {
            return Err(Error::InvalidTask);
        }
        let (lock, signal) = &*self.shared;
        let mut state = lock.lock().map_err(|_| Error::Poisoned)?;
        if state.stopped {
            return Err(Error::Stopped);
        }
        if state.tasks.contains_key(&task.id) || state.running.as_deref() == Some(&task.id) {
            return Err(Error::DuplicateId);
        }
        state.tasks.insert(task.id.clone(), task);
        signal.notify_one();
        Ok(())
    }

    /// Removes a queued task, or marks a running one as non-recurring.
    ///
    /// Returns whether anything was cancelled.
    pub fn cancel(&self, id: &str) -> Result<bool, Error> {
        let (lock, signal) = &*self.shared;
        let mut state = lock.lock().map_err(|_| Error::Poisoned)?;
        if state.stopped {
            return Err(Error::Stopped);
        }
        let mut removed = state.tasks.remove(id).is_some();
        if state.running.as_deref() == Some(id) {
            state.cancelled = true;
            removed = true;
        }
        signal.notify_one();
        Ok(removed)
    }

    /// Returns and clears buffered completions.
    pub fn drain_completions(&self) -> Result<Vec<Completion>, Error> {
        Ok(self
            .shared
            .0
            .lock()
            .map_err(|_| Error::Poisoned)?
            .completed
            .drain(..)
            .collect())
    }

    /// Number of queued tasks, excluding a currently running one.
    pub fn pending(&self) -> Result<usize, Error> {
        Ok(self
            .shared
            .0
            .lock()
            .map_err(|_| Error::Poisoned)?
            .tasks
            .len())
    }

    /// Waits for the running action, discards queued tasks; idempotent.
    pub fn stop(&mut self) -> Result<(), Error> {
        {
            let (lock, signal) = &*self.shared;
            let mut state = lock.lock().map_err(|_| Error::Poisoned)?;
            state.stopped = true;
            state.tasks.clear();
            signal.notify_one();
        }
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| Error::Poisoned)?;
        }
        Ok(())
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn run(shared: Arc<(Mutex<State>, Condvar)>) {
    let (lock, signal) = &*shared;
    loop {
        let mut state = match lock.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        // Wait until some task is due, or until a new task/stop wakes us.
        let mut task = loop {
            if state.stopped {
                return;
            }
            let next = state
                .tasks
                .values()
                .min_by_key(|task| task.run_at)
                .map(|task| (task.id.clone(), task.run_at));
            match next {
                None => {
                    state = match signal.wait(state) {
                        Ok(state) => state,
                        Err(_) => return,
                    }
                }
                Some((id, at)) => {
                    if let Some(delay) = at.checked_duration_since(Instant::now()) {
                        state = match signal.wait_timeout(state, delay) {
                            Ok((state, _)) => state,
                            Err(_) => return,
                        };
                    } else {
                        match state.tasks.remove(&id) {
                            Some(task) => break task,
                            None => continue,
                        }
                    }
                }
            }
        };
        state.running = Some(task.id.clone());
        state.cancelled = false;
        drop(state);

        // A panicking action must not take the worker down with it: extensions
        // are untrusted, and a timer can be the first place a bug shows up.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(&mut task.action))
            .unwrap_or_else(|_| Err("scheduled task panicked".into()));

        let mut state = match lock.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        if state.completed.len() == COMPLETION_BUFFER {
            state.completed.pop_front();
        }
        state.completed.push_back(Completion {
            id: task.id.clone(),
            result,
        });
        state.running = None;
        if !state.stopped
            && !state.cancelled
            && let Some(interval) = task.interval
            && let Some(at) = Instant::now().checked_add(interval)
        {
            task.run_at = at;
            state.tasks.insert(task.id.clone(), task);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn task(id: &str, run_at: Instant, action: Action) -> Task {
        Task::once(id, run_at, action)
    }

    #[test]
    fn earlier_task_wakes_worker_and_queued_task_can_be_cancelled() {
        let mut scheduler = Scheduler::new();
        scheduler
            .add(task(
                "later",
                Instant::now() + Duration::from_secs(30),
                Box::new(|| panic!("cancelled task ran")),
            ))
            .unwrap();

        let (tx, rx) = mpsc::channel();
        scheduler
            .add(task(
                "now",
                Instant::now(),
                Box::new(move || {
                    tx.send(()).unwrap();
                    Ok(())
                }),
            ))
            .unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap();

        assert!(scheduler.cancel("later").unwrap());
        scheduler.stop().unwrap();
        scheduler.stop().unwrap();
        assert_eq!(
            scheduler.add(task("stopped", Instant::now(), Box::new(|| Ok(())))),
            Err(Error::Stopped)
        );
    }

    #[test]
    fn running_recurring_task_can_be_cancelled_and_errors_are_reported() {
        let mut scheduler = Scheduler::new();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        scheduler
            .add(Task::recurring(
                "repeat",
                Instant::now(),
                Duration::from_millis(1),
                Box::new(move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Err("expected failure".into())
                }),
            ))
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            scheduler.add(task("repeat", Instant::now(), Box::new(|| Ok(())))),
            Err(Error::DuplicateId)
        );
        assert!(scheduler.cancel("repeat").unwrap());
        release_tx.send(()).unwrap();
        scheduler.stop().unwrap();
        assert_eq!(
            scheduler.drain_completions().unwrap(),
            vec![Completion {
                id: "repeat".into(),
                result: Err("expected failure".into())
            }]
        );
    }

    #[test]
    fn panicking_action_is_recorded_not_propagated() {
        let mut scheduler = Scheduler::new();
        scheduler
            .add(task("boom", Instant::now(), Box::new(|| panic!("boom"))))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let completion = loop {
            if let Some(completion) = scheduler.drain_completions().unwrap().pop() {
                break completion;
            }
            assert!(Instant::now() < deadline, "定时任务未在预期时间内执行");
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(completion.id, "boom");
        assert!(completion.result.is_err());
        // 工人线程仍可用
        let (tx, rx) = mpsc::channel();
        scheduler
            .add(task(
                "after",
                Instant::now(),
                Box::new(move || {
                    tx.send(()).unwrap();
                    Ok(())
                }),
            ))
            .unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        scheduler.stop().unwrap();
    }

    #[test]
    fn zero_interval_and_empty_ids_are_rejected() {
        let scheduler = Scheduler::new();
        assert_eq!(
            scheduler.add(Task::recurring(
                "bad",
                Instant::now(),
                Duration::ZERO,
                Box::new(|| Ok(()))
            )),
            Err(Error::InvalidTask)
        );
        assert_eq!(
            scheduler.add(task("", Instant::now(), Box::new(|| Ok(())))),
            Err(Error::InvalidTask)
        );
    }
}
