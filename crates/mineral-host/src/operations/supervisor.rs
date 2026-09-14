//! The supervisor: accept work, run it, and let anyone watch.
//!
//! A long use case cannot be a request that blocks until it finishes. The
//! supervisor turns one into an *operation*: `start` returns an identity
//! immediately, progress is observable while it runs, and the result — or the
//! typed failure — stays readable afterwards.
//!
//! It is transport-neutral. Progress travels through the same
//! [`crate::runtime::Progress`] sink the CLI already used, and a subscriber is a
//! plain channel receiver; a Web adapter will forward those events as SSE
//! without changing anything here.
//!
//! ## Concurrency
//!
//! One workspace runs **at most one mutating operation at a time**. A
//! publication, a backup, a backup bootstrap and a human decision all write to
//! the same Git worktree, SQLite databases, object store and remote ref; letting
//! two of them run would be a race the engine is not asked to survive. A second
//! mutating request is refused *before* it reaches the engine, with
//! [`StartError::WorkspaceBusy`] naming the operation that holds the workspace.
//!
//! Read-only operations are not gated. Watching a publication is the normal
//! case, and a `doctor` scan that had to wait for it would be useless exactly
//! when it is wanted.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, Sender, channel},
    },
    thread,
    time::SystemTime,
};

use crate::runtime::Progress;

use super::{
    executor::OperationExecutor,
    model::{
        OperationErrorCode, OperationEvent, OperationFailure, OperationId, OperationKind,
        OperationRequest, OperationResult, OperationSnapshot, OperationState, ProgressEvent,
        ProgressKind, StartError,
    },
};

/// Starts operations and lets callers observe them.
pub struct OperationSupervisor {
    executor: Arc<dyn OperationExecutor>,
    shared: Arc<Mutex<Shared>>,
}

/// The state one supervisor owns.
struct Shared {
    next_id: OperationId,
    /// The mutating operation that currently holds the workspace, if any.
    active_mutation: Option<OperationId>,
    records: BTreeMap<OperationId, Arc<Record>>,
}

/// Everything known about one operation, shared with its worker thread and its
/// subscribers.
struct Record {
    id: OperationId,
    kind: OperationKind,
    queued_at: SystemTime,
    inner: Mutex<RecordInner>,
}

struct RecordInner {
    state: OperationState,
    started_at: Option<SystemTime>,
    finished_at: Option<SystemTime>,
    progress: Vec<ProgressEvent>,
    next_sequence: u64,
    result: Option<Arc<OperationResult>>,
    failure: Option<OperationFailure>,
    subscribers: Vec<Sender<OperationEvent>>,
}

impl Record {
    fn new(id: OperationId, kind: OperationKind) -> Self {
        Self {
            id,
            kind,
            queued_at: SystemTime::now(),
            inner: Mutex::new(RecordInner {
                state: OperationState::Queued,
                started_at: None,
                finished_at: None,
                progress: Vec::new(),
                next_sequence: 0,
                result: None,
                failure: None,
                subscribers: Vec::new(),
            }),
        }
    }

    /// Appends one progress line and fans it out.
    fn push(&self, kind: ProgressKind, message: &str) {
        let mut inner = self.lock();
        inner.next_sequence += 1;
        let event = ProgressEvent {
            sequence: inner.next_sequence,
            kind,
            message: message.to_owned(),
        };
        inner.progress.push(event.clone());
        for subscriber in &inner.subscribers {
            // A subscriber that has gone away is not an error: the record keeps
            // the history, and anyone may subscribe later and read it.
            let _ = subscriber.send(OperationEvent::Progress(event.clone()));
        }
    }

    /// Locks the inner state, recovering from a poisoned mutex.
    ///
    /// A panic in a use case must not make the *record* unusable: the failure is
    /// what a caller needs to read, so poisoning is deliberately ignored here.
    fn lock(&self) -> std::sync::MutexGuard<'_, RecordInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn is_terminal(&self) -> bool {
        self.lock().state.is_terminal()
    }

    /// Records the terminal state and releases every subscriber.
    fn finish(&self, outcome: Result<OperationResult, OperationFailure>) {
        let mut inner = self.lock();
        inner.finished_at = Some(SystemTime::now());
        let state = match outcome {
            Ok(result) => {
                inner.result = Some(Arc::new(result));
                OperationState::Succeeded
            }
            Err(failure) => {
                inner.failure = Some(failure);
                OperationState::Failed
            }
        };
        inner.state = state;
        // Sending before clearing is what guarantees a subscriber observes the
        // terminal event: dropping the senders is what ends its iteration.
        for subscriber in &inner.subscribers {
            let _ = subscriber.send(OperationEvent::Finished { state });
        }
        inner.subscribers.clear();
    }

    fn snapshot(&self) -> OperationSnapshot {
        let inner = self.lock();
        OperationSnapshot {
            id: self.id,
            kind: self.kind.clone(),
            state: inner.state,
            queued_at: self.queued_at,
            started_at: inner.started_at,
            finished_at: inner.finished_at,
            progress: inner.progress.clone(),
            result: inner.result.clone(),
            failure: inner.failure.clone(),
        }
    }
}

/// A stream of everything one operation reports.
///
/// Iterating yields the progress lines already recorded, then every line as it
/// happens, and ends after [`OperationEvent::Finished`]. A subscription made
/// after the operation finished replays the whole history and then ends, which
/// is what makes "start, then subscribe" and "subscribe, then start" behave the
/// same for a caller.
pub struct Subscription {
    receiver: Receiver<OperationEvent>,
}

impl Subscription {
    /// Blocks until the next event, or until the operation is done.
    pub fn recv(&self) -> Option<OperationEvent> {
        self.receiver.recv().ok()
    }

    /// Takes the next event if one is already available.
    pub fn try_recv(&self) -> Option<OperationEvent> {
        self.receiver.try_recv().ok()
    }
}

impl Iterator for Subscription {
    type Item = OperationEvent;

    fn next(&mut self) -> Option<Self::Item> {
        self.receiver.recv().ok()
    }
}

impl OperationSupervisor {
    /// Builds a supervisor over one executor.
    pub fn new(executor: Arc<dyn OperationExecutor>) -> Self {
        Self {
            executor,
            shared: Arc::new(Mutex::new(Shared {
                next_id: OperationId::first(),
                active_mutation: None,
                records: BTreeMap::new(),
            })),
        }
    }

    /// Accepts an operation and returns its identity immediately.
    ///
    /// The work runs on its own thread, so a caller never waits for a
    /// publication to be told its identity. A mutating request is refused while
    /// another mutating operation holds the workspace, and the refusal happens
    /// here — the engine is never entered.
    pub fn start(&self, request: OperationRequest) -> Result<OperationId, StartError> {
        let kind = request.kind();
        let (id, record) = {
            let mut shared = self.lock();
            if kind.is_mutating()
                && let Some(active) = shared.active_mutation
                && shared
                    .records
                    .get(&active)
                    .is_some_and(|record| !record.is_terminal())
            {
                return Err(StartError::WorkspaceBusy {
                    active_operation_id: active,
                });
            }
            let id = shared.next_id;
            shared.next_id = id.next();
            let record = Arc::new(Record::new(id, kind.clone()));
            shared.records.insert(id, Arc::clone(&record));
            if kind.is_mutating() {
                shared.active_mutation = Some(id);
            }
            (id, record)
        };

        let executor = Arc::clone(&self.executor);
        let shared = Arc::clone(&self.shared);
        // The thread is detached on purpose: the operation's life is observed
        // through the record, not through a join handle, and a supervisor that
        // is dropped while work is running must not block on it.
        thread::Builder::new()
            .name(format!("mineral-op-{id}"))
            .spawn(move || {
                record.lock().state = OperationState::Running;
                record.lock().started_at = Some(SystemTime::now());
                let progress: Arc<dyn Progress> = Arc::new(RecordProgress {
                    record: Arc::clone(&record),
                });
                // A panicking use case must not leave the workspace locked: the
                // supervisor turns it into a typed failure like any other, so the
                // next operation is possible and a caller learns what happened.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    executor.execute(&request, progress)
                }))
                .unwrap_or_else(|_| {
                    Err(OperationFailure::Application {
                        code: OperationErrorCode::OperationPanicked,
                        message: "the operation panicked; the workspace is as the use case \
                                  left it"
                            .to_owned(),
                        causes: Vec::new(),
                    })
                });
                // The workspace is released *before* anyone is told the operation
                // finished: a subscriber that sees `Finished` must be able to
                // start the next operation immediately, without a race against
                // the gate being cleared.
                {
                    let mut shared = shared
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if shared.active_mutation == Some(id) {
                        shared.active_mutation = None;
                    }
                }
                record.finish(outcome);
            })
            .expect("the host can spawn a worker thread");

        Ok(id)
    }

    /// Everything known about one operation.
    pub fn snapshot(&self, id: OperationId) -> Option<OperationSnapshot> {
        self.record(id).map(|record| record.snapshot())
    }

    /// Every operation this supervisor has accepted, oldest first.
    pub fn snapshots(&self) -> Vec<OperationSnapshot> {
        self.lock()
            .records
            .values()
            .map(|record| record.snapshot())
            .collect()
    }

    /// Streams one operation's progress and completion.
    pub fn subscribe(&self, id: OperationId) -> Option<Subscription> {
        let record = self.record(id)?;
        let (sender, receiver) = channel();
        let mut inner = record.lock();
        // Replay makes a late subscriber indistinguishable from an early one.
        for event in &inner.progress {
            let _ = sender.send(OperationEvent::Progress(event.clone()));
        }
        if inner.state.is_terminal() {
            let _ = sender.send(OperationEvent::Finished { state: inner.state });
            drop(sender);
        } else {
            inner.subscribers.push(sender);
        }
        Some(Subscription { receiver })
    }

    /// Waits for an operation to finish and returns its final snapshot.
    ///
    /// Waiting is expressed as subscribing and draining, so there is one
    /// mechanism rather than two: a caller that wants the result is simply a
    /// subscriber that keeps nothing.
    pub fn wait(&self, id: OperationId) -> Option<OperationSnapshot> {
        {
            let subscription = self.subscribe(id)?;
            for event in subscription {
                if matches!(event, OperationEvent::Finished { .. }) {
                    break;
                }
            }
        }
        self.snapshot(id)
    }

    /// The mutating operation that currently holds the workspace, if any.
    pub fn active_mutation(&self) -> Option<OperationId> {
        self.lock().active_mutation
    }

    fn record(&self, id: OperationId) -> Option<Arc<Record>> {
        self.lock().records.get(&id).map(Arc::clone)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The bridge from the existing progress vocabulary to the operation's.
///
/// This is the whole point of having had a sink: a use case already reports
/// through it, so observing an operation needed no change to any use case.
struct RecordProgress {
    record: Arc<Record>,
}

impl Progress for RecordProgress {
    fn stage(&self, message: &str) {
        self.record.push(ProgressKind::Stage, message);
    }

    fn detail(&self, message: &str) {
        self.record.push(ProgressKind::Detail, message);
    }
}
