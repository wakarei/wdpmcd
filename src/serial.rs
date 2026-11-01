//! The PMC serial port: exclusive access, raw 9600 8N1 and non-blocking I/O.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::termios::{
    BaudRate, ControlFlags, FlushArg, SetArg, SpecialCharacterIndices, Termios, cfmakeraw,
    cfsetispeed, cfsetospeed, tcflush, tcgetattr, tcsetattr,
};

/// Which readiness the caller waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interest {
    Readable,
    Writable,
}

/// Byte transport between the daemon and the controller.
///
/// The real implementation is [`Port`].
pub trait Link {
    /// Waits until the link is ready or `timeout` expires. Returns whether it is ready.
    fn wait(&mut self, interest: Interest, timeout: Duration) -> io::Result<bool>;
    /// Reads what is available without blocking.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
    /// Writes what the link accepts without blocking.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize>;
}

/// An opened, locked and configured serial port.
#[derive(Debug)]
pub struct Port {
    file: Flock<File>,
    saved: Option<Termios>,
    path: PathBuf,
}

impl Port {
    /// Opens `path`, takes an exclusive lock and switches it to raw 9600 8N1.
    ///
    /// Fails if another process holds the lock, which is how a second daemon
    /// instance is refused.
    pub fn open(path: &Path) -> io::Result<Port> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
            .open(path)?;
        let file = Flock::lock(file, FlockArg::LockExclusiveNonblock).map_err(|(_, errno)| {
            if errno == Errno::EWOULDBLOCK {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "port is locked by another process",
                )
            } else {
                io::Error::from(errno)
            }
        })?;
        let mut port = Port {
            file,
            saved: None,
            path: path.to_path_buf(),
        };
        port.configure()?;
        Ok(port)
    }

    fn configure(&mut self) -> io::Result<()> {
        let saved = tcgetattr(self.file.as_fd()).map_err(io::Error::from)?;
        let mut raw = saved.clone();
        cfmakeraw(&mut raw);
        raw.control_flags |= ControlFlags::CS8 | ControlFlags::CREAD | ControlFlags::CLOCAL;
        raw.control_flags &= !(ControlFlags::PARENB | ControlFlags::CSTOPB | ControlFlags::CRTSCTS);
        raw.control_chars[SpecialCharacterIndices::VMIN as usize] = 0;
        raw.control_chars[SpecialCharacterIndices::VTIME as usize] = 0;
        cfsetispeed(&mut raw, BaudRate::B9600).map_err(io::Error::from)?;
        cfsetospeed(&mut raw, BaudRate::B9600).map_err(io::Error::from)?;
        tcsetattr(self.file.as_fd(), SetArg::TCSANOW, &raw).map_err(io::Error::from)?;
        tcflush(self.file.as_fd(), FlushArg::TCIOFLUSH).map_err(io::Error::from)?;
        self.saved = Some(saved);
        Ok(())
    }

    /// Path the port was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Duplicates the descriptor, e.g. for the panic hook.
    pub fn try_clone_file(&self) -> io::Result<File> {
        self.file.try_clone()
    }

    /// The descriptor the event loop waits on.
    pub fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.file.as_fd()
    }
}

impl Drop for Port {
    fn drop(&mut self) {
        if let Some(saved) = self.saved.take() {
            let _ = tcsetattr(self.file.as_fd(), SetArg::TCSANOW, &saved);
        }
    }
}

impl Link for Port {
    fn wait(&mut self, interest: Interest, timeout: Duration) -> io::Result<bool> {
        let flags = match interest {
            Interest::Readable => PollFlags::POLLIN,
            Interest::Writable => PollFlags::POLLOUT,
        };
        let millis = u16::try_from(timeout.as_millis()).unwrap_or(u16::MAX);
        let mut fds = [PollFd::new(self.file.as_fd(), flags)];
        match poll(&mut fds, PollTimeout::from(millis)) {
            Ok(0) => Ok(false),
            Ok(_) => {
                let revents = fds[0].revents().unwrap_or(PollFlags::empty());
                Ok(revents.intersects(flags | PollFlags::POLLHUP | PollFlags::POLLERR))
            }
            Err(Errno::EINTR) => Ok(false),
            Err(errno) => Err(io::Error::from(errno)),
        }
    }

    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self.file).read(buf)
    }

    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&*self.file).write(buf)
    }
}
