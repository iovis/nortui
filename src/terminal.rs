use std::{io, os::fd::OwnedFd, sync::Arc};

use anyhow::{Context, Result};
use rustix::{
    io::fcntl_dupfd_cloexec,
    termios::{OptionalActions, Termios, tcgetattr, tcsetattr},
};

/// The original terminal attributes, kept independently of raw-mode bookkeeping.
#[derive(Clone)]
pub struct TerminalMode {
    descriptor: Arc<OwnedFd>,
    attributes: Termios,
}

impl TerminalMode {
    pub fn capture() -> Result<Self> {
        // Keep referring to this terminal even if stdin is replaced, without
        // leaking the duplicate descriptor into commands that we execute.
        let descriptor = fcntl_dupfd_cloexec(io::stdin(), 0)
            .context("Cannot preserve the terminal descriptor")?;
        let attributes = tcgetattr(&descriptor).context("Cannot preserve terminal settings")?;
        Ok(Self {
            descriptor: Arc::new(descriptor),
            attributes,
        })
    }

    pub fn restore(&self) -> Result<()> {
        tcsetattr(&*self.descriptor, OptionalActions::Now, &self.attributes)
            .context("Cannot restore original terminal settings")
    }
}
