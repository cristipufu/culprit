//! One cloud search per window. Replacing or retiring it drops the provider future and its CLI children.
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use culprit_core::{Error, ErrorCode, Result};
use tokio::sync::Notify;

struct Search {
    id: String,
    canceled: Arc<Notify>,
    started: bool,
}

#[derive(Default)]
pub(crate) struct DiscoveryRequests(Mutex<HashMap<String, Search>>);

fn canceled() -> Error {
    Error::new(ErrorCode::QueryFailed, "Cloud discovery was canceled")
}

impl DiscoveryRequests {
    /// Registration precedes the async invocation, so a late old invocation cannot replace a newer search.
    pub(crate) fn begin(&self, window: &str, id: String) -> Result<()> {
        if id.is_empty() || id.len() > 128 {
            return Err(Error::invalid("Invalid discovery request id"));
        }
        let search = Search { id, canceled: Arc::new(Notify::new()), started: false };
        if let Some(previous) = self.0.lock().unwrap_or_else(|e| e.into_inner()).insert(window.into(), search) {
            previous.canceled.notify_one();
        }
        Ok(())
    }

    pub(crate) fn cancel(&self, window: &str, id: &str) {
        let mut searches = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if searches.get(window).is_some_and(|search| search.id == id)
            && let Some(search) = searches.remove(window)
        {
            search.canceled.notify_one();
        }
    }

    pub(crate) fn close(&self, window: &str) {
        if let Some(search) = self.0.lock().unwrap_or_else(|e| e.into_inner()).remove(window) {
            search.canceled.notify_one();
        }
    }

    pub(crate) async fn run<T>(&self, window: &str, id: &str, work: impl Future<Output = Result<T>>) -> Result<T> {
        let signal = {
            let mut searches = self.0.lock().unwrap_or_else(|e| e.into_inner());
            let search =
                searches.get_mut(window).filter(|search| search.id == id && !search.started).ok_or_else(canceled)?;
            search.started = true;
            search.canceled.clone()
        };
        let _owner = Owner { requests: self, window, signal: signal.clone() };
        tokio::select! {
            biased;
            _ = signal.notified() => Err(canceled()),
            result = work => result,
        }
    }
}

struct Owner<'a> {
    requests: &'a DiscoveryRequests,
    window: &'a str,
    signal: Arc<Notify>,
}

impl Drop for Owner<'_> {
    fn drop(&mut self) {
        let mut searches = self.requests.0.lock().unwrap_or_else(|e| e.into_inner());
        if searches.get(self.window).is_some_and(|search| Arc::ptr_eq(&search.canceled, &self.signal)) {
            searches.remove(self.window);
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
        let requests = Arc::new(DiscoveryRequests::default());
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
        assert!(requests.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn late_invocations_and_cancellations_cannot_retire_a_newer_search() {
        let requests = DiscoveryRequests::default();
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
        let requests = DiscoveryRequests::default();
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
        let requests = DiscoveryRequests::default();
        requests.begin("first", "a".into()).unwrap();
        requests.begin("second", "b".into()).unwrap();
        requests.close("first");
        assert!(requests.run::<()>("second", "b", async { Err(Error::internal("test failure")) }).await.is_err());
        assert!(requests.0.lock().unwrap().is_empty());
        requests.begin("second", "retry".into()).unwrap();
        assert!(requests.run("second", "retry", async { Ok(()) }).await.is_ok());
    }

    #[tokio::test]
    async fn duplicate_dispatch_starts_no_work_and_dropped_callers_release_their_owner() {
        let requests = Arc::new(DiscoveryRequests::default());
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
        assert!(requests.0.lock().unwrap().is_empty());
    }
}
