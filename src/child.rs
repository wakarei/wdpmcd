//! Helper processes, started and collected without ever blocking the daemon.
//!
//! smartctl, zpool, ifconfig, sysctl and the optional UPS probe all run as
//! child processes. A child can hang for a long time (a dying disk keeps
//! smartctl in the kernel), so nothing here waits: the output pipe is switched
//! to non-blocking mode and polled like any other descriptor, and children are
//! collected with `waitpid(WNOHANG)`.
//!
//! One [`Slot`] holds at most one child of a given kind. When a child outlasts
//! its deadline it is killed and the slot stays closed until the kernel has
//! released it, so a stuck helper can never be joined by a second one.

use std::fs::File;
use std::io::Read;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nix::fcntl::{FcntlArg, OFlag, fcntl};

use crate::{log_notice, log_warn};

/// How much is read from a pipe at a time.
const CHUNK: usize = 4096;
/// Output beyond this is dropped; no helper legitimately prints megabytes.
const MAX_OUTPUT: usize = 1 << 20;
/// How often a bounded wait looks again.
pub const WAIT_PAUSE: Duration = Duration::from_millis(5);

/// What a serviced slot has to report.
#[derive(Debug)]
pub enum Event {
    /// The child exited; its complete output is here.
    Done { status: i32, output: String },
    /// The deadline passed. The child has been killed and its data is lost.
    TimedOut,
    /// A killed child finally went away; the slot is free again.
    Reaped,
}

/// One running helper.
struct Job {
    program: String,
    child: Child,
    out: Option<File>,
    buf: Vec<u8>,
    started: Instant,
    deadline: Instant,
    killed: bool,
}

impl Job {
    /// Reads everything the pipe holds right now. Returns false on EOF.
    fn drain(&mut self) -> bool {
        let Some(out) = self.out.as_mut() else {
            return true;
        };
        let mut chunk = [0u8; CHUNK];
        loop {
            match out.read(&mut chunk) {
                Ok(0) => return false,
                Ok(n) => {
                    if self.buf.len() < MAX_OUTPUT {
                        self.buf.extend_from_slice(&chunk[..n]);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return true,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return false,
            }
        }
    }

    fn output(&self) -> String {
        String::from_utf8_lossy(&self.buf).into_owned()
    }

    /// Exit status as the shell would report it, or -1 when killed by a signal.
    fn exit_code(status: std::process::ExitStatus) -> i32 {
        status.code().unwrap_or(-1)
    }
}

/// A single-child-at-a-time slot.
pub struct Slot {
    kind: &'static str,
    job: Option<Job>,
}

impl Slot {
    pub const fn new(kind: &'static str) -> Slot {
        Slot { kind, job: None }
    }

    /// True while a child is running or still dying.
    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// The descriptor to poll for output, if there is one yet.
    pub fn fd(&self) -> Option<BorrowedFd<'_>> {
        let job = self.job.as_ref()?;
        if job.killed {
            return None;
        }
        job.out.as_ref().map(AsFd::as_fd)
    }

    /// When the running child must be killed.
    pub fn deadline(&self) -> Option<Instant> {
        self.job
            .as_ref()
            .filter(|job| !job.killed)
            .map(|job| job.deadline)
    }

    /// Starts `command`; refuses while another child of this kind exists.
    pub fn start(
        &mut self,
        mut command: Command,
        timeout: Duration,
        now: Instant,
    ) -> Result<(), String> {
        if self.busy() {
            return Err(format!("{} is still running", self.kind));
        }
        let program = command.get_program().to_string_lossy().into_owned();
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot run {program}: {e}"))?;
        let out = match child.stdout.take() {
            Some(stdout) => {
                let file = File::from(OwnedFd::from(stdout));
                if let Err(e) = nonblocking(&file) {
                    let _ = child.kill();
                    return Err(format!("cannot set up {program}: {e}"));
                }
                Some(file)
            }
            None => None,
        };
        self.job = Some(Job {
            program,
            child,
            out,
            buf: Vec::new(),
            started: now,
            deadline: now + timeout,
            killed: false,
        });
        Ok(())
    }

    /// Advances the slot: collects output, kills a late child, reaps a killed
    /// one. Call it whenever the descriptor is readable and on every deadline.
    pub fn service(&mut self, now: Instant) -> Option<Event> {
        let job = self.job.as_mut()?;
        if job.killed {
            match job.child.try_wait() {
                Ok(Some(_)) => {
                    let program = job.program.clone();
                    self.job = None;
                    log_notice!("{program} has exited");
                    return Some(Event::Reaped);
                }
                _ => return None,
            }
        }
        if now >= job.deadline {
            let _ = job.child.kill();
            job.killed = true;
            job.out = None;
            let program = job.program.clone();
            let waited = now.saturating_duration_since(job.started);
            log_warn!(
                "{}: no answer from {program}, killed after {:.1} s",
                self.kind,
                waited.as_secs_f64()
            );
            return Some(Event::TimedOut);
        }
        job.drain();
        match job.child.try_wait() {
            Ok(Some(status)) => {
                let code = Job::exit_code(status);
                let output = job.output();
                let program = job.program.clone();
                self.job = None;
                if code != 0 {
                    log_notice!("{program} exited with status {code}");
                }
                Some(Event::Done {
                    status: code,
                    output,
                })
            }
            Ok(None) => None,
            Err(e) => {
                let program = job.program.clone();
                self.job = None;
                Some(Event::Done {
                    status: -1,
                    output: format!("cannot collect {program}: {e}"),
                })
            }
        }
    }

    /// Kills the running child, if any, and forgets the slot once it is gone.
    pub fn stop(&mut self) {
        if let Some(job) = self.job.as_mut() {
            let _ = job.child.kill();
            job.killed = true;
            job.out = None;
        }
    }
}

/// Switches a descriptor to non-blocking mode.
pub fn nonblocking(fd: &impl AsFd) -> std::io::Result<()> {
    let flags = fcntl(fd, FcntlArg::F_GETFL).map_err(std::io::Error::from)?;
    fcntl(
        fd,
        FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK),
    )
    .map_err(std::io::Error::from)?;
    Ok(())
}

/// Runs a command to completion, giving up after `timeout`.
///
/// Used before the event loop exists (FreeBSD discovery), where there is no
/// loop to return to; it polls the child instead of calling `wait()`.
#[cfg(target_os = "freebsd")]
pub fn run_bounded(command: Command, timeout: Duration) -> Result<(i32, String), String> {
    let mut slot = Slot::new("command");
    slot.start(command, timeout, Instant::now())?;
    let limit = timeout + Duration::from_millis(500);
    let result = wait_bounded(Instant::now() + limit, WAIT_PAUSE, || {
        match slot.service(Instant::now()) {
            Some(Event::Done { status, output }) => Some(Ok((status, output))),
            Some(Event::TimedOut) => Some(Err(format!(
                "timed out after {:.1} s",
                timeout.as_secs_f64()
            ))),
            Some(Event::Reaped) | None => None,
        }
    });
    match result {
        Some(done) => done,
        None => Err(format!("gave up after {:.1} s", limit.as_secs_f64())),
    }
}

/// Waits for `step` to report a result, looking again every `pause`, and gives
/// up when `deadline` passes.
///
/// This is for the paths that run without the event loop — start-up discovery
/// and the one-shot modes — where a short sleep is the only way to wait.
pub fn wait_bounded<T>(
    deadline: Instant,
    pause: Duration,
    mut step: impl FnMut() -> Option<T>,
) -> Option<T> {
    loop {
        if let Some(done) = step() {
            return Some(done);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(pause);
    }
}
