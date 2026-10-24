//! Bounded cloud CLI output. Cancellation kills the owned child instead of leaving inventory collection running.
use std::process::{Output, Stdio};
use std::time::Duration;

use culprit_core::{Error, ErrorCode, Result};
#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use tokio::{io::AsyncReadExt, process::Command};

pub(crate) const MAX_OUTPUT: usize = 2 * 1024 * 1024;
const MAX_STDERR: usize = 16 * 1024;
const TIMEOUT: Duration = Duration::from_secs(15);

struct OwnedChild {
    child: Box<dyn ChildWrapper>,
    finished: bool,
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.child.start_kill();
        }
    }
}

async fn read(reader: impl tokio::io::AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| Error::new(ErrorCode::ConnectionFailed, "Could not read cloud CLI output"))?;
    if bytes.len() > limit {
        return Err(Error::new(ErrorCode::QueryFailed, "The cloud CLI response exceeded its size limit")
            .with_hint("Narrow the search or choose specific regions"));
    }
    Ok(bytes)
}

pub(crate) async fn output(command: Command) -> Result<Output> {
    within(command, TIMEOUT).await
}

async fn within(mut command: Command, timeout: Duration) -> Result<Output> {
    tokio::time::timeout(timeout, async {
        command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut command = CommandWrap::from(command);
        command.wrap(KillOnDrop);
        #[cfg(unix)]
        command.wrap(ProcessGroup::leader());
        #[cfg(windows)]
        command.wrap(JobObject);
        let child = command.spawn().map_err(|error| {
            Error::new(
                if error.kind() == std::io::ErrorKind::NotFound {
                    ErrorCode::NotFound
                } else {
                    ErrorCode::ConnectionFailed
                },
                "Could not start the cloud CLI",
            )
        })?;
        let mut child = OwnedChild { child, finished: false };
        let stdout = child.child.stdout().take().expect("piped stdout");
        let stderr = child.child.stderr().take().expect("piped stderr");
        let result = tokio::try_join!(read(stdout, MAX_OUTPUT), read(stderr, MAX_STDERR), async {
            child
                .child
                .wait()
                .await
                .map_err(|_| Error::new(ErrorCode::ConnectionFailed, "The cloud CLI stopped unexpectedly"))
        });
        match result {
            Ok((stdout, stderr, status)) => {
                child.finished = true;
                Ok(Output { stdout, stderr, status })
            }
            Err(error) => {
                let _ = child.child.start_kill();
                let _ = child.child.wait().await;
                child.finished = true;
                Err(error)
            }
        }
    })
    .await
    .map_err(|_| {
        Error::new(ErrorCode::Timeout, "The cloud CLI timed out")
            .with_hint("Check the CLI login and narrow the search or choose specific regions")
    })?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn output_limits_are_enforced_while_reading() {
        assert_eq!(read(&b"1234"[..], 4).await.unwrap(), b"1234");
        assert_eq!(read(&b"12345"[..], 4).await.unwrap_err().code, ErrorCode::QueryFailed);
    }

    #[test]
    fn child_fixture() {
        use std::io::Write;
        match std::env::var("CULPRIT_TEST_CLOUD_CHILD").as_deref() {
            Ok("large") => {
                let _ = std::io::stdout().write_all(&vec![b'x'; MAX_OUTPUT + 1]);
            }
            Ok("wrapper") => {
                let mut command = std::process::Command::new(std::env::current_exe().unwrap());
                command
                    .args(["--exact", "cloud_process::tests::child_fixture", "--nocapture"])
                    .env("CULPRIT_TEST_CLOUD_CHILD", "wait");
                let _ = command.spawn().unwrap().wait();
            }
            Ok("wait") => {
                let path = std::path::PathBuf::from(std::env::var("CULPRIT_TEST_CLOUD_MARKER").unwrap());
                std::fs::write(path.with_extension("ready"), b"ready").unwrap();
                std::thread::sleep(Duration::from_secs(1));
                std::fs::write(path, b"finished").unwrap();
            }
            _ => {}
        }
    }

    fn child(kind: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "cloud_process::tests::child_fixture", "--nocapture"])
            .env("CULPRIT_TEST_CLOUD_CHILD", kind);
        command
    }

    #[tokio::test]
    async fn a_large_cli_response_is_stopped_before_full_buffering() {
        assert_eq!(output(child("large")).await.unwrap_err().code, ErrorCode::QueryFailed);
    }

    #[tokio::test]
    async fn caller_cancellation_kills_the_child() {
        let path = std::env::temp_dir().join(format!(
            "culprit-cloud-child-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let ready = path.with_extension("ready");
        let mut command = child("wrapper");
        command.env("CULPRIT_TEST_CLOUD_MARKER", &path);
        let task = tokio::spawn(async move { output(command).await });
        for _ in 0..500 {
            if ready.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ready.exists(), "The owned test child did not start");
        task.abort();
        let _ = task.await;
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(!path.exists(), "A canceled CLI child continued running");
        let _ = std::fs::remove_file(ready);
    }
}
