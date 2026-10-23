//! One cancellation boundary for interactive provider futures. Dropping the operation drops the driver/session;
//! providers that launch native work keep an OperationGuard in their caller and pass its handle to that work.

use culprit_core::{
    Result,
    limits::{QUERY_TIMEOUT, timeout_error},
};
use std::{future::Future, time::Duration};

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
}
