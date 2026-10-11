use crate::finding::{CheckInfo, Finding};
use crate::model::Snapshot;

/// A check is a pure function from a snapshot to findings. It never touches a live connection.
pub trait Check: Send + Sync {
    fn info(&self) -> CheckInfo;
    fn run(&self, snapshot: &Snapshot) -> Vec<Finding>;
}
