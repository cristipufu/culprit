//! Owned child process trees, shared by cloud CLIs and SSH. Only the child receives modified environment/flags.
#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use tokio::process::Command;

pub(crate) struct OwnedChild {
    pub child: Box<dyn ChildWrapper>,
    pub finished: bool,
}

impl OwnedChild {
    pub fn spawn(command: Command) -> std::io::Result<Self> {
        #[cfg(windows)]
        let command = {
            let mut command = command;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW for desktop launches.
            command
        };
        let mut command = CommandWrap::from(command);
        command.wrap(KillOnDrop);
        #[cfg(unix)]
        command.wrap(ProcessGroup::leader());
        #[cfg(windows)]
        command.wrap(JobObject);
        Ok(Self { child: command.spawn()?, finished: false })
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.child.start_kill();
        }
    }
}
