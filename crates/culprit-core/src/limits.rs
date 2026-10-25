//! Shared limits for interactive database work. Drivers own protocol cancellation; blocking workers observe a handle.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use crate::{Error, ErrorCode, Result};

pub const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
pub const OBSERVE_TIMEOUT: Duration = Duration::from_secs(10);
pub const PREVIEW_TIMEOUT: Duration = Duration::from_secs(5);
pub const MAX_RESULT_ROWS: usize = 10_000;
pub const MAX_RESULT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone)]
pub struct OperationHandle {
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
}

impl OperationHandle {
    pub fn stopped(&self) -> bool {
        self.cancelled.load(Ordering::Acquire) || Instant::now() >= self.deadline
    }

    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    pub fn check(&self) -> Result<()> {
        if self.stopped() { Err(timeout_error()) } else { Ok(()) }
    }
}

/// Keep this in the async caller, and clone its handle into native work. Dropping the caller cancels that work.
pub struct OperationGuard(OperationHandle);

impl OperationGuard {
    pub fn new(timeout: Duration) -> Result<Self> {
        let deadline =
            Instant::now().checked_add(timeout).ok_or_else(|| Error::invalid("The execution timeout is too large"))?;
        Ok(Self(OperationHandle { deadline, cancelled: Arc::new(AtomicBool::new(false)) }))
    }

    pub fn handle(&self) -> OperationHandle {
        self.0.clone()
    }
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        self.0.cancelled.store(true, Ordering::Release);
    }
}

pub fn timeout_error() -> Error {
    Error::new(ErrorCode::Timeout, "The database operation was cancelled or reached its execution deadline").with_hint(
        "Use a more selective query or try again. If this statement changes data, check its outcome before retrying.",
    )
}

/// Retained display rows, independent of the driver's buffering. A rejected row ends retention, never execution:
/// network drivers must still drain completion and errors before a write can be reported as successful.
pub struct ResultBudget {
    rows: usize,
    bytes: usize,
    used_rows: usize,
    used_bytes: usize,
    truncated: bool,
    byte_limit: bool,
}

impl ResultBudget {
    pub fn new(requested_rows: usize) -> Self {
        Self {
            rows: requested_rows.min(MAX_RESULT_ROWS),
            bytes: MAX_RESULT_BYTES,
            used_rows: 0,
            used_bytes: 0,
            truncated: false,
            byte_limit: false,
        }
    }

    pub fn retain(&mut self, bytes: usize) -> bool {
        if self.truncated || self.used_rows >= self.rows {
            self.truncated = true;
            return false;
        }
        if bytes > self.bytes.saturating_sub(self.used_bytes) {
            self.truncated = true;
            self.byte_limit = true;
            return false;
        }
        self.used_rows += 1;
        self.used_bytes += bytes;
        true
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn notice(&self) -> Option<String> {
        if self.byte_limit {
            Some("results limited to 8 MiB".into())
        } else if self.truncated {
            Some(format!("stopped at {}", self.rows))
        } else {
            None
        }
    }
}

/// Include the retained JSON cell storage, plus string data. Provider result cells are scalar values.
pub fn row_bytes(cells: usize, text_bytes: usize) -> usize {
    cells.saturating_mul(std::mem::size_of::<serde_json::Value>()).saturating_add(text_bytes)
}

/// Bound lossy UTF-8 conversion before allocating its result. Valid text keeps its actual size.
pub fn text_bytes(bytes: &[u8]) -> usize {
    if std::str::from_utf8(bytes).is_ok() { bytes.len() } else { bytes.len().saturating_mul(3) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_work_observes_caller_drop_and_expired_deadlines() {
        let guard = OperationGuard::new(QUERY_TIMEOUT).unwrap();
        let handle = guard.handle();
        assert!(!handle.stopped());
        drop(guard);
        assert_eq!(handle.check().unwrap_err().code, ErrorCode::Timeout);
        assert!(OperationGuard::new(Duration::ZERO).unwrap().handle().stopped());
        assert!(OperationGuard::new(Duration::MAX).is_err());
    }

    #[test]
    fn result_limits_keep_a_prefix_without_overflow_or_resuming_after_rejection() {
        let mut budget = ResultBudget::new(usize::MAX);
        assert!(budget.retain(MAX_RESULT_BYTES));
        assert!(!budget.retain(1));
        assert!(!budget.retain(0));
        assert!(budget.notice().unwrap().contains("8 MiB"));
        let mut budget = ResultBudget::new(usize::MAX);
        for _ in 0..MAX_RESULT_ROWS {
            assert!(budget.retain(0));
        }
        assert!(!budget.retain(0));
        let mut budget = ResultBudget::new(0);
        assert!(!budget.retain(0));
        assert_eq!(row_bytes(usize::MAX, usize::MAX), usize::MAX);
        assert_eq!(text_bytes("hello 🦀".as_bytes()), "hello 🦀".len());
        assert_eq!(text_bytes(&[0xff, 0xff]), 6);
    }
}
