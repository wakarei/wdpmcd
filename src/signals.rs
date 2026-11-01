//! Signal handling through a self-pipe.
//!
//! The handlers installed here do almost nothing: they set a flag and write
//! one byte into a pipe, both of which are safe to do inside a signal
//! handler. The daemon reads the pipe from its event loop, where ordinary
//! code can react.
//!
//! `SIGALRM` is deliberately left alone: it is the watchdog (see the event
//! loop), and its default action, terminating the process, is the point.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::unistd::{pipe, read};
use signal_hook::SigId;
use signal_hook::consts::signal::{SIGCHLD, SIGHUP, SIGINT, SIGPIPE, SIGTERM};
use signal_hook::flag;
use signal_hook::low_level::pipe as self_pipe;

use crate::child;

/// Handle to the installed handlers.
pub struct Signals {
    read_end: OwnedFd,
    terminate: Arc<AtomicBool>,
    reload: Arc<AtomicBool>,
    _ids: Vec<SigId>,
}

impl Signals {
    /// Installs handlers for SIGTERM, SIGINT, SIGHUP and SIGCHLD, and makes
    /// SIGPIPE harmless.
    pub fn install() -> io::Result<Signals> {
        let (read_end, write_end) = pipe().map_err(io::Error::from)?;
        child::nonblocking(&read_end)?;
        let terminate = Arc::new(AtomicBool::new(false));
        let reload = Arc::new(AtomicBool::new(false));
        let mut ids = Vec::new();
        // Flags first, then the pipe: a reader that drains the pipe and then
        // looks at the flags can never miss a signal that arrived in between.
        ids.push(flag::register(SIGTERM, terminate.clone())?);
        ids.push(flag::register(SIGINT, terminate.clone())?);
        ids.push(flag::register(SIGHUP, reload.clone())?);
        for signal in [SIGTERM, SIGINT, SIGHUP, SIGCHLD] {
            let writer = write_end.try_clone()?;
            ids.push(self_pipe::register(signal, writer)?);
        }
        // SIGPIPE must not kill the daemon when a reader of a pipe goes away.
        // An empty handler is the same as ignoring the signal: the write just
        // fails with EPIPE.
        ids.push(flag::register(SIGPIPE, Arc::new(AtomicBool::new(false)))?);
        drop(write_end);
        Ok(Signals {
            read_end,
            terminate,
            reload,
            _ids: ids,
        })
    }

    /// The descriptor that becomes readable when a signal arrives.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.read_end.as_fd()
    }

    /// Empties the pipe. Call it before looking at the flags.
    pub fn drain(&self) {
        let mut sink = [0u8; 64];
        loop {
            match read(&self.read_end, &mut sink) {
                Ok(0) => return,
                Ok(_) => {}
                Err(_) => return,
            }
        }
    }

    /// True once SIGTERM or SIGINT has arrived.
    pub fn terminating(&self) -> bool {
        self.terminate.load(Ordering::SeqCst)
    }

    /// True once per SIGHUP.
    pub fn take_reload(&self) -> bool {
        self.reload.swap(false, Ordering::SeqCst)
    }

    /// Waits up to `duration` for a signal, draining the pipe. Returns false
    /// when the process should stop. Used before the event loop exists.
    pub fn wait(&self, duration: Duration) -> bool {
        let millis = u16::try_from(duration.as_millis().max(1)).unwrap_or(u16::MAX);
        let mut fds = [PollFd::new(self.read_end.as_fd(), PollFlags::POLLIN)];
        if poll(&mut fds, PollTimeout::from(millis)).is_ok_and(|n| n > 0) {
            self.drain();
        }
        !self.terminating()
    }
}
