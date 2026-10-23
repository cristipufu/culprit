//! One cancellation boundary for interactive provider futures. Dropping the operation drops the driver/session;
//! providers that launch native work keep an OperationGuard in their caller and pass its handle to that work.

use culprit_core::{
    Result,
    limits::{QUERY_TIMEOUT, timeout_error},
};
use std::{future::Future, time::Duration};

static BLOCKING_TASKS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// Run disk/CPU work in the shared bounded pool. Cancellation retires queued work before it can touch state.
pub async fn run_blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    let guard = culprit_core::limits::OperationGuard::new(QUERY_TIMEOUT)?;
    let handle = guard.handle();
    let permit = tokio::time::timeout(handle.remaining(), BLOCKING_TASKS.acquire())
        .await
        .map_err(|_| {
            culprit_core::Error::new(
                culprit_core::ErrorCode::Timeout,
                "Background workers are busy; try again after the current work finishes",
            )
        })?
        .map_err(|_| culprit_core::Error::internal("Background workers are unavailable"))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        handle.check()?;
        work()
    })
    .await
    .map_err(|_| culprit_core::Error::internal("The background task stopped before completing"))?
}

pub(crate) async fn run<T>(future: impl Future<Output = Result<T>>) -> Result<T> {
    within(QUERY_TIMEOUT, future).await
}

pub(crate) async fn within<T>(timeout: Duration, future: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(timeout, future).await.map_err(|_| timeout_error())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[tokio::test]
    async fn deadline_drops_the_provider_future_and_preserves_provider_errors() {
        let dropped = Arc::new(AtomicBool::new(false));
        let owned = Dropped(dropped.clone());
        let error = within(Duration::from_millis(10), async move {
            let _owned = owned;
            std::future::pending::<Result<()>>().await
        })
        .await
        .unwrap_err();
        assert_eq!(error.code, culprit_core::ErrorCode::Timeout);
        assert!(dropped.load(Ordering::Acquire));
        let error = run(async { Err::<(), _>(culprit_core::Error::invalid("bad statement")) }).await.unwrap_err();
        assert_eq!(error.code, culprit_core::ErrorCode::InvalidInput);
    }

    #[tokio::test]
    async fn caller_cancellation_drops_the_driver_instead_of_detaching_it() {
        let dropped = Arc::new(AtomicBool::new(false));
        let owned = Dropped(dropped.clone());
        let (started, ready) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(run(async move {
            let _owned = owned;
            let _ = started.send(());
            std::future::pending::<Result<()>>().await
        }));
        ready.await.unwrap();
        task.abort();
        let _ = task.await;
        assert!(dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn validation_and_execution_share_one_deadline() {
        let error = within(Duration::from_millis(20), async {
            tokio::time::sleep(Duration::from_millis(12)).await;
            tokio::time::sleep(Duration::from_millis(12)).await;
            Ok::<_, culprit_core::Error>(())
        })
        .await
        .unwrap_err();
        assert_eq!(error.code, culprit_core::ErrorCode::Timeout);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_work_leaves_the_async_executor_responsive() {
        let (release, waiting) = std::sync::mpsc::channel();
        let (started, ready) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(run_blocking(move || {
            let _ = started.send(());
            waiting
                .recv_timeout(Duration::from_secs(2))
                .map_err(|_| culprit_core::Error::internal("worker release timed out"))?;
            Ok(7)
        }));
        ready.await.unwrap();
        let responsive = !task.is_finished();
        release.send(()).unwrap();
        assert_eq!(task.await.unwrap().unwrap(), 7);
        assert!(responsive);
    }

    #[tokio::test]
    async fn cancelled_queued_work_never_starts() {
        let held = BLOCKING_TASKS.acquire_many(2).await.unwrap();
        let (started, ready) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(run_blocking(move || {
            let _ = started.send(());
            Ok(())
        }));
        tokio::task::yield_now().await;
        task.abort();
        let _ = task.await;
        drop(held);
        assert!(tokio::time::timeout(Duration::from_secs(2), ready).await.unwrap().is_err());
    }
}
