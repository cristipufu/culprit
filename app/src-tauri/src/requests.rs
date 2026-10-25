//! Ordered request ownership per window. Retirement drops the work future before its replacement gets a slot.
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use culprit_core::{Error, ErrorCode, Result};
use tokio::sync::{Notify, Semaphore};

struct Request {
    id: String,
    canceled: Arc<Notify>,
    started: bool,
}

#[derive(Default)]
pub(crate) struct WindowRequests {
    active: Mutex<HashMap<String, Request>>,
    gate: Option<Semaphore>,
}

fn canceled() -> Error {
    Error::new(ErrorCode::QueryFailed, "Request was canceled")
}

impl WindowRequests {
    /// Serialize preview work while allowing a replacement to wait for the canceled future to release its permit.
    pub(crate) fn serialized() -> Self {
        Self { gate: Some(Semaphore::new(1)), ..Self::default() }
    }

    /// Registration precedes the async invocation, so a late old invocation cannot replace a newer request.
    pub(crate) fn begin(&self, window: &str, id: String) -> Result<()> {
        if id.is_empty() || id.len() > 128 {
            return Err(Error::invalid("Invalid request id"));
        }
        let request = Request { id, canceled: Arc::new(Notify::new()), started: false };
        if let Some(previous) = self.active.lock().unwrap_or_else(|e| e.into_inner()).insert(window.into(), request) {
            previous.canceled.notify_one();
        }
        Ok(())
    }

    pub(crate) fn cancel(&self, window: &str, id: &str) {
        let mut requests = self.active.lock().unwrap_or_else(|e| e.into_inner());
        if requests.get(window).is_some_and(|request| request.id == id)
            && let Some(request) = requests.remove(window)
        {
            request.canceled.notify_one();
        }
    }

    pub(crate) fn close(&self, window: &str) {
        if let Some(request) = self.active.lock().unwrap_or_else(|e| e.into_inner()).remove(window) {
            request.canceled.notify_one();
        }
    }

    pub(crate) async fn run<T>(&self, window: &str, id: &str, work: impl Future<Output = Result<T>>) -> Result<T> {
        let signal = {
            let mut requests = self.active.lock().unwrap_or_else(|e| e.into_inner());
            let request =
                requests.get_mut(window).filter(|request| request.id == id && !request.started).ok_or_else(canceled)?;
            request.started = true;
            request.canceled.clone()
        };
        let _owner = Owner { requests: self, window, signal: signal.clone() };
        let work = async {
            let _permit = match &self.gate {
                Some(gate) => Some(gate.acquire().await.map_err(|_| canceled())?),
                None => None,
            };
            work.await
        };
        tokio::select! {
            biased;
            _ = signal.notified() => Err(canceled()),
            result = work => result,
        }
    }
}

struct Owner<'a> {
    requests: &'a WindowRequests,
    window: &'a str,
    signal: Arc<Notify>,
}

impl Drop for Owner<'_> {
    fn drop(&mut self) {
        let mut requests = self.requests.active.lock().unwrap_or_else(|e| e.into_inner());
        if requests.get(self.window).is_some_and(|request| Arc::ptr_eq(&request.canceled, &self.signal)) {
            requests.remove(self.window);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tokio::sync::{Semaphore, oneshot};

    #[tokio::test]
    async fn replacement_drops_slow_work_and_releases_its_search_slot() {
        let requests = Arc::new(WindowRequests::serialized());
        let gate = Arc::new(Semaphore::new(1));
        requests.begin("main", "old".into()).unwrap();
        let (started, ready) = oneshot::channel();
        let previous = requests.clone();
        let slot = gate.clone();
        let old = tokio::spawn(async move {
            previous
                .run("main", "old", async {
                    let _permit = slot.acquire().await.unwrap();
                    started.send(()).unwrap();
                    std::future::pending::<Result<()>>().await
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), ready).await.unwrap().unwrap();
        requests.begin("main", "new".into()).unwrap();
        let next = requests.run("main", "new", async {
            let _permit = gate.acquire().await.unwrap();
            Ok("new result")
        });
        assert_eq!(tokio::time::timeout(Duration::from_secs(2), next).await.unwrap().unwrap(), "new result");
        assert!(old.await.unwrap().is_err());
        assert!(requests.active.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn serialized_windows_do_not_start_work_until_the_previous_owner_retires() {
        let requests = Arc::new(WindowRequests::serialized());
        requests.begin("first", "a".into()).unwrap();
        let (started, ready) = oneshot::channel();
        let active = requests.clone();
        let old = tokio::spawn(async move {
            active
                .run("first", "a", async {
                    started.send(()).unwrap();
                    std::future::pending::<Result<()>>().await
                })
                .await
        });
        ready.await.unwrap();
        requests.begin("second", "b".into()).unwrap();
        let polled = Arc::new(AtomicBool::new(false));
        let next_requests = requests.clone();
        let observed = polled.clone();
        let next = tokio::spawn(async move {
            next_requests
                .run("second", "b", async {
                    observed.store(true, Ordering::SeqCst);
                    Ok(42)
                })
                .await
        });
        tokio::task::yield_now().await;
        assert!(!polled.load(Ordering::SeqCst));
        requests.close("first");
        assert!(old.await.unwrap().is_err());
        assert_eq!(tokio::time::timeout(Duration::from_secs(2), next).await.unwrap().unwrap().unwrap(), 42);
        assert!(polled.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn late_invocations_and_cancellations_cannot_retire_a_newer_search() {
        let requests = WindowRequests::default();
        requests.begin("main", "old".into()).unwrap();
        requests.begin("main", "new".into()).unwrap();
        let polled = AtomicBool::new(false);
        assert!(
            requests
                .run("main", "old", async {
                    polled.store(true, Ordering::SeqCst);
                    Ok(())
                })
                .await
                .is_err()
        );
        assert!(!polled.load(Ordering::SeqCst));
        requests.cancel("main", "old");
        assert_eq!(requests.run("main", "new", async { Ok(42) }).await.unwrap(), 42);
    }

    #[tokio::test]
    async fn cancellation_before_dispatch_and_window_close_start_no_work() {
        let requests = WindowRequests::default();
        for close in [false, true] {
            requests.begin("main", "pending".into()).unwrap();
            if close {
                requests.close("main");
            } else {
                requests.cancel("main", "pending");
            }
            let polled = AtomicBool::new(false);
            assert!(
                requests
                    .run("main", "pending", async {
                        polled.store(true, Ordering::SeqCst);
                        Ok(())
                    })
                    .await
                    .is_err()
            );
            assert!(!polled.load(Ordering::SeqCst));
        }
    }

    #[tokio::test]
    async fn windows_have_independent_searches_and_errors_are_retryable() {
        let requests = WindowRequests::default();
        requests.begin("first", "a".into()).unwrap();
        requests.begin("second", "b".into()).unwrap();
        requests.close("first");
        assert!(requests.run::<()>("second", "b", async { Err(Error::internal("test failure")) }).await.is_err());
        assert!(requests.active.lock().unwrap().is_empty());
        requests.begin("second", "retry".into()).unwrap();
        assert!(requests.run("second", "retry", async { Ok(()) }).await.is_ok());
    }

    #[tokio::test]
    async fn duplicate_dispatch_starts_no_work_and_dropped_callers_release_their_owner() {
        let requests = Arc::new(WindowRequests::default());
        requests.begin("main", "id".into()).unwrap();
        let (started, ready) = oneshot::channel();
        let active = requests.clone();
        let task = tokio::spawn(async move {
            active
                .run("main", "id", async {
                    started.send(()).unwrap();
                    std::future::pending::<Result<()>>().await
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), ready).await.unwrap().unwrap();
        let polled = AtomicBool::new(false);
        assert!(
            requests
                .run("main", "id", async {
                    polled.store(true, Ordering::SeqCst);
                    Ok(())
                })
                .await
                .is_err()
        );
        assert!(!polled.load(Ordering::SeqCst));
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(requests.active.lock().unwrap().is_empty());
    }
}
