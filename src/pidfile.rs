//! Pidfile handling.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::sys::signal::kill;
use nix::unistd::Pid;

/// A pidfile owned by this process; removed again on drop.
#[derive(Debug)]
pub struct Pidfile {
    path: PathBuf,
    pid: u32,
}

/// Returns the pid recorded in `path`, if the file exists and is well formed.
fn recorded_pid(path: &Path) -> Option<i32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn is_alive(pid: i32) -> bool {
    match kill(Pid::from_raw(pid), None) {
        Ok(()) => true,
        Err(errno) => errno == Errno::EPERM,
    }
}

impl Pidfile {
    /// Writes the current pid to `path`, refusing if a live process owns it.
    pub fn create(path: &Path) -> io::Result<Pidfile> {
        let me = std::process::id();
        if let Some(pid) = recorded_pid(path)
            && u32::try_from(pid).ok() != Some(me)
            && is_alive(pid)
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("already running as pid {pid}"),
            ));
        }
        let mut staging = path.as_os_str().to_owned();
        staging.push(".tmp");
        let staging = PathBuf::from(staging);
        fs::write(&staging, format!("{me}\n"))?;
        fs::rename(&staging, path)?;
        Ok(Pidfile {
            path: path.to_path_buf(),
            pid: me,
        })
    }
}

impl Drop for Pidfile {
    fn drop(&mut self) {
        if recorded_pid(&self.path).and_then(|p| u32::try_from(p).ok()) == Some(self.pid) {
            let _ = fs::remove_file(&self.path);
        }
    }
}
