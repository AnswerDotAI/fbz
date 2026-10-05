use std::{
    collections::HashMap,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};

use rayon::{ThreadPool, ThreadPoolBuilder};

use crate::{Error, Result};

pub(crate) struct Job<T> { pub key: usize, pub reservation: usize, pub payload: T }

pub(crate) struct PipelineLimits { pub memory: usize, pub active: usize }

struct State {
    next: usize,
    reserved: usize,
    active: usize,
    cancelled: bool,
}

struct Budget {
    limit: usize,
    max_active: usize,
    state: Mutex<State>,
    wake: Condvar,
}

impl Budget {
    fn new(limit: usize, max_active: usize) -> Self {
        Self { limit, max_active, state: Mutex::new(State { next: 0, reserved: 0, active: 0, cancelled: false }), wake: Condvar::new() }
    }

    fn next<T>(&self, jobs: &[Job<T>]) -> Option<usize> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        loop {
            if state.cancelled || state.next >= jobs.len() { return None; }
            let reservation = jobs[state.next].reservation;
            if state.active < self.max_active && reservation <= self.limit.saturating_sub(state.reserved) {
                let next = state.next;
                state.next += 1;
                state.reserved += reservation;
                state.active += 1;
                return Some(next);
            }
            state = self.wake.wait(state).unwrap_or_else(|error| error.into_inner());
        }
    }

    fn complete(&self, reservation: usize, retained: usize) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        debug_assert!(retained <= reservation);
        state.reserved = state.reserved.saturating_sub(reservation).saturating_add(retained.min(reservation));
        self.wake.notify_all();
    }

    fn retire(&self, retained: usize) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.reserved = state.reserved.saturating_sub(retained);
        state.active = state.active.saturating_sub(1);
        self.wake.notify_all();
    }

    fn cancel(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.cancelled = true;
        self.wake.notify_all();
    }
}

type Message<T> = (usize, usize, T);

fn execute_job<T, O>(
    index: usize,
    jobs: &[Job<T>],
    budget: &Budget,
    sender: &mpsc::Sender<Message<O>>,
    execute: &impl Fn(&T) -> O,
    retained_size: &impl Fn(&O) -> usize,
) -> bool {
    let job = &jobs[index];
    let value = execute(&job.payload);
    let retained = retained_size(&value).min(job.reservation);
    budget.complete(job.reservation, retained);
    if sender.send((job.key, retained, value)).is_ok() { true } else { budget.retire(retained); false }
}

pub(crate) struct OrderedResults<'a, T> { receiver: mpsc::Receiver<Message<T>>, ready: HashMap<usize, (usize, T)>, budget: &'a Budget }

impl<T> OrderedResults<'_, T> {
    pub fn take(&mut self, key: usize) -> Result<T> {
        while !self.ready.contains_key(&key) {
            let (received_key, retained, value) = self.receiver.recv().map_err(|_| Error::InvalidConfiguration("parallel decoder stopped early".into()))?;
            self.ready.insert(received_key, (retained, value));
        }
        let (retained, value) = self.ready.remove(&key).unwrap();
        self.budget.retire(retained);
        Ok(value)
    }

    pub fn discard_before(&mut self, key: usize) {
        let stale: Vec<_> = self.ready.keys().copied().filter(|&candidate| candidate < key).collect();
        for candidate in stale {
            let (retained, _) = self.ready.remove(&candidate).unwrap();
            self.budget.retire(retained);
        }
    }
}

pub(crate) fn run_ordered<T, O, R>(
    pool: &ThreadPool,
    jobs: &[Job<T>],
    limits: PipelineLimits,
    execute: impl Fn(&T) -> O + Sync,
    retained_size: impl Fn(&O) -> usize + Sync,
    consume: impl FnOnce(&mut OrderedResults<'_, O>) -> Result<R>,
) -> Result<R>
where
    T: Sync,
    O: Send,
{
    if jobs.iter().any(|job| job.reservation > limits.memory) {
        return Err(Error::InvalidConfiguration("a parallel job reservation exceeds the memory limit".into()));
    }
    let budget = Budget::new(limits.memory, limits.active);
    let (sender, receiver) = mpsc::channel();
    thread::scope(|scope| {
        let worker = scope.spawn(|| {
            pool.broadcast(|_| {
                while let Some(index) = budget.next(jobs) { if !execute_job(index, jobs, &budget, &sender, &execute, &retained_size) { return; } }
            });
        });
        let result = { let mut results = OrderedResults { receiver, ready: HashMap::new(), budget: &budget }; consume(&mut results) };
        budget.cancel();
        worker.join().map_err(|_| Error::InvalidConfiguration("parallel decoder worker panicked".into()))?;
        result
    })
}

enum TryNext { Ready(usize), Pending, Cancelled }

impl Budget {
    fn try_next<T>(&self, jobs: &[Job<T>]) -> TryNext {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.cancelled { return TryNext::Cancelled; }
        if state.next >= jobs.len() { return TryNext::Pending; }
        let reservation = jobs[state.next].reservation;
        if state.active >= self.max_active || reservation > self.limit.saturating_sub(state.reserved) { return TryNext::Pending; }
        let next = state.next;
        state.next += 1;
        state.reserved += reservation;
        state.active += 1;
        TryNext::Ready(next)
    }

    fn wait_briefly(&self) {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let _ = self.wake.wait_timeout(state, std::time::Duration::from_millis(1));
    }

    fn notify(&self) { self.wake.notify_all(); }
}

pub(crate) struct Lease { retained: usize }

pub(crate) struct StagedResults<'a, O, S, Q> {
    primary: OrderedResults<'a, O>,
    stage_sender: mpsc::Sender<Message<S>>,
    stage_receiver: mpsc::Receiver<(usize, Q)>,
    stage_ready: HashMap<usize, Q>,
}

impl<'a, O, S, Q> StagedResults<'a, O, S, Q> {
    pub fn take_primary(&mut self, key: usize) -> Result<(Lease, O)> {
        while !self.primary.ready.contains_key(&key) {
            let (received_key, retained, value) =
                self.primary.receiver.recv().map_err(|_| Error::InvalidConfiguration("parallel decoder stopped early".into()))?;
            self.primary.ready.insert(received_key, (retained, value));
        }
        let (retained, value) = self.primary.ready.remove(&key).unwrap();
        Ok((Lease { retained }, value))
    }

    pub fn retire(&self, lease: Lease) { self.primary.budget.retire(lease.retained); }

    pub fn submit(&self, key: usize, lease: Lease, payload: S) -> Result<()> {
        if let Err(mpsc::SendError((_, retained, _))) = self.stage_sender.send((key, lease.retained, payload)) {
            self.primary.budget.retire(retained);
            return Err(Error::InvalidConfiguration("parallel decoder stopped before staged work was submitted".into()));
        }
        self.primary.budget.notify();
        Ok(())
    }

    pub fn take_stage(&mut self, key: usize) -> Result<Q> {
        while !self.stage_ready.contains_key(&key) {
            let (received_key, value) = self.stage_receiver.recv().map_err(|_| Error::InvalidConfiguration("parallel staged worker stopped early".into()))?;
            self.stage_ready.insert(received_key, value);
        }
        Ok(self.stage_ready.remove(&key).unwrap())
    }
}

pub(crate) fn run_staged_ordered<T, O, S, Q, R>(
    worker_count: usize,
    jobs: &[Job<T>],
    limits: PipelineLimits,
    execute: impl Fn(&T) -> O + Sync,
    retained_size: impl Fn(&O) -> usize + Sync,
    execute_stage: impl Fn(S) -> Q + Sync,
    consume: impl FnOnce(&mut StagedResults<'_, O, S, Q>) -> Result<R>,
) -> Result<R>
where
    T: Sync,
    O: Send,
    S: Send,
    Q: Send,
{
    if jobs.iter().any(|job| job.reservation > limits.memory) {
        return Err(Error::InvalidConfiguration("a parallel job reservation exceeds the memory limit".into()));
    }
    let budget = Budget::new(limits.memory, limits.active);
    let (primary_sender, primary_receiver) = mpsc::channel();
    let (stage_sender, stage_job_receiver) = mpsc::channel();
    let stage_job_receiver = Mutex::new(stage_job_receiver);
    let (stage_result_sender, stage_receiver) = mpsc::channel();
    thread::scope(|scope| {
        let workers: Vec<_> = (0..worker_count)
            .map(|_| {
                scope.spawn(|| {
                    loop {
                        let stage = stage_job_receiver.lock().unwrap_or_else(|error| error.into_inner()).try_recv();
                        match stage {
                            Ok((key, retained, payload)) => {
                                let value = execute_stage(payload);
                                budget.retire(retained);
                                if stage_result_sender.send((key, value)).is_err() { return; }
                                continue;
                            }
                            Err(mpsc::TryRecvError::Disconnected) => return,
                            Err(mpsc::TryRecvError::Empty) => {}
                        }
                        match budget.try_next(jobs) {
                            TryNext::Ready(index) => {
                                if !execute_job(index, jobs, &budget, &primary_sender, &execute, &retained_size) { return; }
                            }
                            TryNext::Pending => budget.wait_briefly(),
                            TryNext::Cancelled => return,
                        }
                    }
                })
            })
            .collect();
        let result = {
            let primary = OrderedResults { receiver: primary_receiver, ready: HashMap::new(), budget: &budget };
            let mut results = StagedResults { primary, stage_sender, stage_receiver, stage_ready: HashMap::new() };
            consume(&mut results)
        };
        budget.cancel();
        for worker in workers { worker.join().map_err(|_| Error::InvalidConfiguration("parallel decoder worker panicked".into()))?; }
        result
    })
}

type StreamingMessage<T> = (usize, usize, std::thread::Result<T>);

/// A bounded, long-lived ordered worker pool for streaming producers.
///
/// Reservations cover both a job's owned input and its retained result until
/// the coordinator consumes that result. This is deliberately conservative:
/// codecs can account for temporary worker allocations in the reservation too.
pub(crate) struct StreamingOrdered<T> {
    pool: ThreadPool,
    sender: mpsc::Sender<StreamingMessage<T>>,
    receiver: mpsc::Receiver<StreamingMessage<T>>,
    ready: HashMap<usize, (usize, std::thread::Result<T>)>,
    next_submit: usize,
    next_take: usize,
    reserved: usize,
    active: usize,
    max_active: usize,
    memory_limit: usize,
    cancelled: Arc<AtomicBool>,
}

impl<T: Send + 'static> StreamingOrdered<T> {
    pub fn new(threads: usize, memory_limit: usize, thread_name: &'static str) -> Result<Self> {
        let max_active = threads.max(1);
        let pool = ThreadPoolBuilder::new()
            .num_threads(max_active)
            .thread_name(move |index| format!("{thread_name}-{index}"))
            .build()
            .map_err(|error| Error::InvalidConfiguration(error.to_string()))?;
        let (sender, receiver) = mpsc::channel();
        Ok(Self {
            pool,
            sender,
            receiver,
            ready: HashMap::new(),
            next_submit: 0,
            next_take: 0,
            reserved: 0,
            active: 0,
            max_active,
            memory_limit,
            cancelled: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn can_submit(&self, reservation: usize) -> bool { self.active < self.max_active && reservation <= self.memory_limit.saturating_sub(self.reserved) }

    pub fn submit(&mut self, reservation: usize, job: impl FnOnce() -> T + Send + 'static) -> Result<()> {
        if reservation > self.memory_limit { return Err(Error::InvalidConfiguration("a streaming job reservation exceeds the memory limit".into())); }
        if !self.can_submit(reservation) { return Err(Error::InvalidConfiguration("streaming pipeline is full".into())); }
        let key = self.next_submit;
        self.next_submit += 1;
        self.active += 1;
        self.reserved += reservation;
        let sender = self.sender.clone();
        let cancelled = Arc::clone(&self.cancelled);
        self.pool.spawn(move || {
            if cancelled.load(Ordering::Relaxed) { return; }
            let result = catch_unwind(AssertUnwindSafe(job));
            let _ = sender.send((key, reservation, result));
        });
        Ok(())
    }

    pub fn take_next(&mut self) -> Result<T> {
        if self.next_take >= self.next_submit { return Err(Error::InvalidConfiguration("streaming pipeline has no pending result".into())); }
        while !self.ready.contains_key(&self.next_take) {
            let message = self.receiver.recv().map_err(|_| Error::InvalidConfiguration("streaming worker stopped early".into()))?;
            self.ready.insert(message.0, (message.1, message.2));
        }
        let (reservation, result) = self.ready.remove(&self.next_take).unwrap();
        self.next_take += 1;
        self.active -= 1;
        self.reserved -= reservation;
        result.map_err(|_| Error::InvalidConfiguration("streaming worker panicked".into()))
    }

    pub fn has_pending(&self) -> bool { self.next_take < self.next_submit }
}

impl<T> Drop for StreamingOrdered<T> { fn drop(&mut self) { self.cancelled.store(true, Ordering::Relaxed); } }
