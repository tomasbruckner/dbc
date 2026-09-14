//! Running one thing over N targets: bounded parallelism, one event
//! channel, index-ordered admission, and a cancel that also drains the
//! queue so every target reports a terminal outcome (spec §2). What a
//! target DOES is the caller's closure — this module never opens a
//! connection itself, so the GUI runner and the CLI drive the same loop
//! with different bodies.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dbc_core::CancelToken;
use tokio::sync::mpsc::Sender;

/// How many targets may be connected at once (spec §2).
pub const MAX_PARALLEL_TARGETS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetOutcome {
    Ok,
    Failed,
    Cancelled,
}

#[derive(Debug)]
pub enum TargetEvent<E> {
    /// The target acquired a slot and its body is about to run
    /// ("connecting" in the GUI).
    Started { target_ix: usize },
    Inner { target_ix: usize, event: E },
    Finished { target_ix: usize, outcome: TargetOutcome, elapsed: Duration },
}

/// Drive `body` over `n` targets. Admission is in index order behind a
/// semaphore of `max_parallel` permits. Each body gets its own inner
/// channel; a forwarder task tags its events with the target index onto
/// `tx`, so the body can reuse code written for a single-target channel.
/// Once `cancel` fires, targets that have not acquired a slot yet are
/// reported `Cancelled` without running (even while every slot is still
/// busy — the drain does not wait for a free permit); targets already
/// running see the same token through their own `cancel` clone (the
/// caller passes it into the body's captured state) and stop on their
/// own. A body that panics is reported `Failed`, same as one that
/// returns `false`; either way exactly one `Finished` is sent per target.
///
/// Contract on `body`: it must let its `Sender<E>` drop when it
/// completes (returns or panics). A body that stashes a clone of the
/// sender somewhere longer-lived than its own future will hang the
/// forwarder task — which never sees `recv() == None` — and hold its
/// semaphore permit forever.
pub async fn run_targets<F, Fut, E>(
    n: usize,
    max_parallel: usize,
    cancel: CancelToken,
    body: F,
    tx: Sender<TargetEvent<E>>,
) where
    F: Fn(usize, Sender<E>) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = bool> + Send + 'static,
    E: Send + 'static,
{
    let sem = Arc::new(tokio::sync::Semaphore::new(max_parallel.max(1)));
    let body = Arc::new(body);
    let mut set = tokio::task::JoinSet::new();
    for ix in 0..n {
        // Race the cancel signal against permit acquisition, biased toward
        // cancel: if every slot is busy when `cancel` fires, queued targets
        // must still drain immediately rather than wait for a slot to free
        // up (that slot may not free for as long as the running target's
        // body takes).
        let permit = tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            acquired = sem.clone().acquire_owned() => acquired.ok(),
        };
        let permit = match permit {
            Some(p) => p,
            None => {
                let _ = tx
                    .send(TargetEvent::Finished { target_ix: ix, outcome: TargetOutcome::Cancelled, elapsed: Duration::ZERO })
                    .await;
                continue;
            }
        };
        // A permit can still land in the same instant `cancel` fires (the
        // select above is racy by nature); re-check before admitting.
        if cancel.is_cancelled() {
            drop(permit);
            let _ = tx
                .send(TargetEvent::Finished { target_ix: ix, outcome: TargetOutcome::Cancelled, elapsed: Duration::ZERO })
                .await;
            continue;
        }
        let tx = tx.clone();
        let body = body.clone();
        set.spawn(async move {
            let _permit = permit;
            let started = Instant::now();
            let _ = tx.send(TargetEvent::Started { target_ix: ix }).await;
            let (inner_tx, mut inner_rx) = tokio::sync::mpsc::channel::<E>(dbc_core::CHANNEL_CAPACITY);
            let fwd_tx = tx.clone();
            let forwarder = tokio::spawn(async move {
                while let Some(event) = inner_rx.recv().await {
                    if fwd_tx.send(TargetEvent::Inner { target_ix: ix, event }).await.is_err() {
                        break;
                    }
                }
            });
            // Run the body in its own task so a panic unwinds only that
            // task: `JoinHandle::await` turns it into `Err(JoinError)`
            // instead of unwinding past the `Finished` send below and
            // leaving this target without a terminal event.
            let body_handle = tokio::spawn(body(ix, inner_tx));
            let ok = match body_handle.await {
                Ok(ok) => ok,
                Err(_) => false,
            };
            // The body (or its panic unwinding) dropped its sender; the
            // forwarder drains and ends.
            let _ = forwarder.await;
            let outcome = if ok { TargetOutcome::Ok } else { TargetOutcome::Failed };
            let _ = tx.send(TargetEvent::Finished { target_ix: ix, outcome, elapsed: started.elapsed() }).await;
        });
    }
    while set.join_next().await.is_some() {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    async fn collect<E: Send + 'static>(
        mut rx: tokio::sync::mpsc::Receiver<TargetEvent<E>>,
    ) -> Vec<TargetEvent<E>> {
        let mut out = Vec::new();
        while let Some(ev) = rx.recv().await {
            out.push(ev);
        }
        out
    }

    #[tokio::test]
    async fn every_target_gets_started_and_finished() {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let cancel = dbc_core::CancelToken::new();
        run_targets(3, 4, cancel, |ix, inner| async move {
            inner.send(ix * 10).await.unwrap();
            ix != 1
        }, tx)
        .await;
        let events = collect(rx).await;
        for ix in 0..3 {
            assert!(events.iter().any(|e| matches!(e, TargetEvent::Started { target_ix } if *target_ix == ix)));
            assert!(events.iter().any(|e| matches!(e, TargetEvent::Inner { target_ix, event } if *target_ix == ix && *event == ix * 10)));
        }
        let outcome = |ix: usize| events.iter().find_map(|e| match e {
            TargetEvent::Finished { target_ix, outcome, .. } if *target_ix == ix => Some(*outcome),
            _ => None,
        });
        assert_eq!(outcome(0), Some(TargetOutcome::Ok));
        assert_eq!(outcome(1), Some(TargetOutcome::Failed));
        assert_eq!(outcome(2), Some(TargetOutcome::Ok));
    }

    #[tokio::test]
    async fn never_more_than_max_parallel_in_flight() {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (f, p) = (in_flight.clone(), peak.clone());
        run_targets(9, 2, dbc_core::CancelToken::new(), move |_ix, _inner| {
            let (f, p) = (f.clone(), p.clone());
            async move {
                let now = f.fetch_add(1, Ordering::SeqCst) + 1;
                p.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                f.fetch_sub(1, Ordering::SeqCst);
                true
            }
        }, tx)
        .await;
        let _ = collect::<()>(rx).await;
        assert_eq!(peak.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cancel_marks_unstarted_targets_cancelled() {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let cancel = dbc_core::CancelToken::new();
        let c = cancel.clone();
        run_targets(5, 1, cancel, move |ix, _inner| {
            let c = c.clone();
            async move {
                if ix == 0 {
                    c.cancel();
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
                true
            }
        }, tx)
        .await;
        let events = collect::<()>(rx).await;
        let cancelled = events.iter().filter(|e| matches!(e, TargetEvent::Finished { outcome: TargetOutcome::Cancelled, .. })).count();
        assert_eq!(cancelled, 4, "targets 1..4 never started");
        assert!(events.iter().any(|e| matches!(e, TargetEvent::Finished { target_ix: 0, outcome: TargetOutcome::Ok, .. })));
        assert_eq!(events.iter().filter(|e| matches!(e, TargetEvent::Started { .. })).count(), 1);
    }

    #[tokio::test]
    async fn panicking_body_reports_failed_and_run_completes() {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        run_targets(3, 4, dbc_core::CancelToken::new(), |ix, _inner: tokio::sync::mpsc::Sender<()>| async move {
            if ix == 1 {
                panic!("boom");
            }
            true
        }, tx)
        .await;
        let events = collect::<()>(rx).await;
        let outcome = |ix: usize| events.iter().find_map(|e| match e {
            TargetEvent::Finished { target_ix, outcome, .. } if *target_ix == ix => Some(*outcome),
            _ => None,
        });
        assert_eq!(outcome(0), Some(TargetOutcome::Ok));
        assert_eq!(outcome(1), Some(TargetOutcome::Failed));
        assert_eq!(outcome(2), Some(TargetOutcome::Ok));
    }

    #[tokio::test]
    async fn cancel_drain_does_not_wait_for_a_free_permit() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let cancel = dbc_core::CancelToken::new();
        let c = cancel.clone();
        let run = run_targets(5, 1, cancel, move |ix, _inner: tokio::sync::mpsc::Sender<()>| {
            let c = c.clone();
            async move {
                if ix == 0 {
                    c.cancel();
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                true
            }
        }, tx);
        let collector = async {
            let mut out = Vec::new();
            while let Some(ev) = rx.recv().await {
                out.push(ev);
            }
            out
        };
        let (_, events) = tokio::join!(run, collector);

        let finished_pos = |ix: usize| {
            events
                .iter()
                .position(|e| matches!(e, TargetEvent::Finished { target_ix, .. } if *target_ix == ix))
                .unwrap_or_else(|| panic!("target {ix} never finished"))
        };
        let zero_finished_at = finished_pos(0);
        for ix in 1..5 {
            assert!(
                finished_pos(ix) < zero_finished_at,
                "target {ix}'s Cancelled should arrive before target 0's Finished, since the drain must not wait for a free permit"
            );
        }
    }

    #[tokio::test]
    async fn zero_targets_finishes_immediately() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        run_targets(0, 4, dbc_core::CancelToken::new(), |_ix, _inner: tokio::sync::mpsc::Sender<()>| async move { true }, tx).await;
        assert!(collect::<()>(rx).await.is_empty());
    }
}
