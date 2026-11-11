// TAG [SPARK-117]
// TAG [SPARK-152]

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, TryLockError};
use std::task::{Context, Poll, Wake, Waker};

use tokio::task::{JoinError, JoinHandle};

type Outcome = Result<(), Arc<JoinError>>;

pub(super) struct Completion {
    state: Mutex<State>,
    changed: Condvar,
    waiters: Arc<Waiters>,
    task_waker: Waker,
}

struct State {
    handle: Option<JoinHandle<()>>,
    joining: bool,
    outcome: Option<Outcome>,
}

#[derive(Default)]
struct Waiters(Mutex<Vec<Waker>>);

impl Waiters {
    fn register(&self, waker: &Waker) -> bool {
        let mut waiters = match self.0.try_lock() {
            Ok(waiters) => waiters,
            Err(TryLockError::WouldBlock) => {
                waker.wake_by_ref();
                return false;
            }
            Err(TryLockError::Poisoned(_)) => panic!("spark: future waiters poisoned"),
        };
        if !waiters.iter().any(|current| current.will_wake(waker)) {
            waiters.push(waker.clone());
        }
        true
    }

    fn notify(&self) {
        let waiters = std::mem::take(&mut *self.0.lock().expect("spark: future waiters poisoned"));
        for waker in waiters {
            waker.wake();
        }
    }
}

impl Wake for Waiters {
    fn wake(self: Arc<Self>) {
        self.notify();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.notify();
    }
}

impl Completion {
    pub(super) fn new(handle: JoinHandle<()>) -> Self {
        let waiters = Arc::new(Waiters::default());
        let task_waker = Waker::from(waiters.clone());
        Self {
            state: Mutex::new(State {
                handle: Some(handle),
                joining: false,
                outcome: None,
            }),
            changed: Condvar::new(),
            waiters,
            task_waker,
        }
    }

    pub(super) fn wait(&self, propagate_panic: bool) -> bool {
        let outcome = tokio::task::block_in_place(|| self.join());
        let initialized = outcome.is_ok();
        if propagate_panic {
            outcome.expect("spark: spawned task panicked");
        }
        initialized
    }

    fn join(&self) -> Outcome {
        let handle = {
            let mut state = self.state.lock().expect("spark: future join poisoned");
            loop {
                if let Some(outcome) = &state.outcome {
                    return outcome.clone();
                }
                if !state.joining {
                    state.joining = true;
                    break state
                        .handle
                        .take()
                        .expect("spark: missing future join handle");
                }
                state = self
                    .changed
                    .wait(state)
                    .expect("spark: future join poisoned");
            }
        };
        let outcome = super::tokio_runtime().block_on(handle).map_err(Arc::new);
        {
            let mut state = self.state.lock().expect("spark: future join poisoned");
            state.joining = false;
            state.outcome = Some(outcome.clone());
        }
        self.changed.notify_all();
        self.waiters.notify();
        outcome
    }

    pub(super) fn poll(&self, waker: Option<&Waker>) -> bool {
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => {
                if let Some(waker) = waker {
                    waker.wake_by_ref();
                }
                return false;
            }
            Err(TryLockError::Poisoned(_)) => panic!("spark: future join poisoned"),
        };
        if let Some(outcome) = state.outcome.clone() {
            drop(state);
            outcome.expect("spark: spawned task panicked");
            return true;
        }
        if let Some(waker) = waker {
            if !self.waiters.register(waker) {
                return false;
            }
        }
        if state.joining {
            return false;
        }
        let handle = state
            .handle
            .as_mut()
            .expect("spark: missing future join handle");
        let mut context = Context::from_waker(&self.task_waker);
        let Poll::Ready(result) = Pin::new(handle).poll(&mut context) else {
            return false;
        };
        let outcome = result.map_err(Arc::new);
        state.handle = None;
        state.outcome = Some(outcome.clone());
        drop(state);
        self.changed.notify_all();
        self.waiters.notify();
        outcome.expect("spark: spawned task panicked");
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polling_does_not_report_ready_while_another_caller_is_joining() {
        let (release, held) = tokio::sync::oneshot::channel();
        let completion = Completion::new(super::super::tokio_runtime().spawn(async move {
            held.await.expect("release the held task");
        }));
        let premature = std::thread::scope(|scope| {
            let waiter = scope.spawn(|| completion.wait(true));
            while !completion.state.lock().expect("join state").joining {
                std::thread::yield_now();
            }
            let ready = completion.poll(None);
            release.send(()).expect("release task");
            waiter.join().expect("join waiter");
            ready
        });
        assert!(!premature);
    }
    #[test]
    fn concurrent_waiters_observe_completion_and_reuse_the_result() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (release, held) = tokio::sync::oneshot::channel();
        let finished = Arc::new(AtomicBool::new(false));
        let worker_finished = finished.clone();
        let completion = Completion::new(super::super::tokio_runtime().spawn(async move {
            held.await.expect("release worker");
            worker_finished.store(true, Ordering::Release);
        }));
        let start = std::sync::Barrier::new(5);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let start = &start;
                let completion = &completion;
                let finished = &finished;
                scope.spawn(move || {
                    start.wait();
                    completion.wait(true);
                    assert!(finished.load(Ordering::Acquire));
                });
            }
            start.wait();
            while !completion.state.lock().expect("join state").joining {
                std::thread::yield_now();
            }
            assert!(!completion.poll(None));
            release.send(()).expect("release worker");
        });
        assert!(completion.poll(None));
        completion.wait(true);
    }

    struct Signal(std::sync::mpsc::Sender<()>);
    impl Wake for Signal {
        fn wake(self: Arc<Self>) {
            let _ = self.0.send(());
        }
    }

    #[test]
    fn completion_wakes_every_registered_poller() {
        use std::time::Duration;
        let (release, held) = tokio::sync::oneshot::channel();
        let completion = Completion::new(super::super::tokio_runtime().spawn(async move {
            held.await.expect("release worker");
        }));
        let (send_first, first) = std::sync::mpsc::channel();
        let (send_second, second) = std::sync::mpsc::channel();
        let first_waker = Waker::from(Arc::new(Signal(send_first)));
        let second_waker = Waker::from(Arc::new(Signal(send_second)));
        assert!(!completion.poll(Some(&first_waker)));
        assert!(!completion.poll(Some(&second_waker)));
        release.send(()).expect("release worker");
        first
            .recv_timeout(Duration::from_secs(5))
            .expect("wake first poller");
        second
            .recv_timeout(Duration::from_secs(5))
            .expect("wake second poller");
        assert!(completion.poll(None));
    }
}
