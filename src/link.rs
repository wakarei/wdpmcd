//! Network link (carrier) state of the cabled interface.
//!
//! Stock WD shows steady red ("action required") when the network cable is
//! unplugged. This module only answers one question: is there carrier on the
//! configured interface? `None` means "cannot tell" (no such interface, no
//! media status), which is never treated as a fault.
//!
//! Linux answers from sysfs. FreeBSD has no readable file for it, so
//! `ifconfig <interface>` is asked instead, through the same bounded helper
//! machinery as every other external command.

use std::fs;
use std::os::fd::BorrowedFd;
#[cfg(target_os = "freebsd")]
use std::path::Path;
use std::path::PathBuf;
#[cfg(target_os = "freebsd")]
use std::process::Command;
use std::time::{Duration, Instant};

use crate::child::{Event, Slot};
use crate::{log_notice, log_warn};

/// Where the carrier flag comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkSource {
    /// Interface name, e.g. `re0` (FreeBSD) or `eth0` (Linux).
    pub interface: String,
    /// When set, a file holding `0` or `1` replaces the platform lookup,
    /// which is handy for unusual setups.
    pub carrier_file: Option<PathBuf>,
    /// Program asked for the interface status.
    #[cfg(target_os = "freebsd")]
    pub command: PathBuf,
}

impl LinkSource {
    /// A source for `interface`, with the platform's own tool for the status
    /// question; settings may name another one (FreeBSD).
    pub fn new(interface: String, carrier_file: Option<PathBuf>) -> LinkSource {
        LinkSource {
            interface,
            carrier_file,
            #[cfg(target_os = "freebsd")]
            command: PathBuf::from(DEFAULT_IFCONFIG),
        }
    }

    /// Names another program to ask for the interface status.
    #[cfg(target_os = "freebsd")]
    pub fn with_command(mut self, command: PathBuf) -> LinkSource {
        self.command = command;
        self
    }
}

/// The program that prints the interface status on FreeBSD.
#[cfg(target_os = "freebsd")]
pub const DEFAULT_IFCONFIG: &str = "/sbin/ifconfig";

/// Parses the content of a carrier file or of a sysfs `carrier` file.
pub fn parse_carrier(text: &str) -> Option<bool> {
    match text.trim() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

/// Parses the media status line of `ifconfig <interface>`.
///
/// Only the two statuses the driver reports for carrier are understood;
/// everything else counts as "cannot tell".
pub fn parse_ifconfig_status(text: &str) -> Option<bool> {
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("status:") else {
            continue;
        };
        return match rest.trim() {
            "active" => Some(true),
            "no carrier" => Some(false),
            _ => None,
        };
    }
    None
}

/// Reads the carrier state without starting anything: a configured file, or
/// the sysfs attribute on Linux. `None` means the platform has to be asked.
pub fn carrier_sync(source: &LinkSource) -> Option<bool> {
    if let Some(file) = &source.carrier_file {
        return parse_carrier(&fs::read_to_string(file).ok()?);
    }
    platform_carrier(&source.interface)
}

#[cfg(target_os = "linux")]
fn platform_carrier(interface: &str) -> Option<bool> {
    if interface.is_empty() || interface.contains('/') {
        return None;
    }
    let dir = std::path::Path::new("/sys/class/net").join(interface);
    if !dir.is_dir() {
        return None;
    }
    match fs::read_to_string(dir.join("carrier")) {
        Ok(text) => parse_carrier(&text),
        // Reading `carrier` of an interface that is administratively down
        // fails with EINVAL: there is no link either.
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => Some(false),
        Err(_) => None,
    }
}

#[cfg(not(target_os = "linux"))]
fn platform_carrier(_interface: &str) -> Option<bool> {
    None
}

/// The command that prints the interface status on FreeBSD.
#[cfg(target_os = "freebsd")]
pub fn status_command(interface: &str, program: &Path) -> Command {
    let mut command = Command::new(program);
    command.arg(interface);
    command
}

/// Tracks the carrier and logs it when it changes.
pub struct Carrier {
    slot: Slot,
    interface: String,
    down: bool,
    unknown_logged: bool,
    checked: Option<Instant>,
}

impl Default for Carrier {
    fn default() -> Self {
        Carrier::new()
    }
}

impl Carrier {
    pub fn new() -> Carrier {
        Carrier {
            slot: Slot::new("network link"),
            interface: String::new(),
            down: false,
            unknown_logged: false,
            checked: None,
        }
    }

    /// True while the configured link has no carrier.
    pub fn down(&self) -> bool {
        self.down
    }

    /// Re-reads the carrier when the poll is due and returns whether the link
    /// is down. An interface whose state cannot be read is never a fault: the
    /// last verdict stands.
    pub fn poll(&mut self, now: Instant, source: &LinkSource, every: Duration) -> bool {
        if self.interface != source.interface {
            self.interface = source.interface.clone();
            self.down = false;
            self.unknown_logged = false;
            self.checked = None;
            self.slot.stop();
        }
        if let Some(event) = self.slot.service(now) {
            match event {
                Event::Done { status: 0, output } => {
                    self.observe(parse_ifconfig_status(&output));
                }
                Event::Done { status, .. } => {
                    self.unavailable(&format!("ifconfig exited with status {status}"));
                }
                Event::TimedOut => self.unavailable("ifconfig did not answer"),
                Event::Reaped => {}
            }
            self.checked = Some(now);
        }
        let due = self
            .checked
            .is_none_or(|at| now.saturating_duration_since(at) >= every);
        if due && !self.slot.busy() {
            match carrier_sync(source) {
                Some(up) => {
                    self.observe(Some(up));
                    self.checked = Some(now);
                }
                None => self.ask_platform(now, source),
            }
        }
        self.down
    }

    /// Descriptors that should wake the loop when they become readable.
    pub fn fd<'a>(&'a self, out: &mut Vec<BorrowedFd<'a>>) {
        if let Some(fd) = self.slot.fd() {
            out.push(fd);
        }
    }

    /// Deadlines at which the poll wants to run again.
    pub fn deadlines(&self, out: &mut Vec<Instant>) {
        if let Some(at) = self.slot.deadline() {
            out.push(at);
        }
    }

    /// Kills a query still running. Nothing waits for it.
    pub fn stop(&mut self) {
        self.slot.stop();
    }

    #[cfg(target_os = "freebsd")]
    fn ask_platform(&mut self, now: Instant, source: &LinkSource) {
        let command = status_command(&source.interface, &source.command);
        match self.slot.start(command, Duration::from_secs(3), now) {
            Ok(()) => {}
            Err(e) => {
                self.unavailable(&e);
                self.checked = Some(now);
            }
        }
    }

    #[cfg(not(target_os = "freebsd"))]
    fn ask_platform(&mut self, now: Instant, _source: &LinkSource) {
        self.observe(None);
        self.checked = Some(now);
    }

    fn observe(&mut self, verdict: Option<bool>) {
        match verdict {
            Some(up) => {
                self.unknown_logged = false;
                if self.down == up {
                    self.down = !up;
                    if self.down {
                        log_warn!("network link {} is down", self.interface);
                    } else {
                        log_notice!("network link {} is up", self.interface);
                    }
                }
            }
            None => self.unavailable("state unavailable"),
        }
    }

    fn unavailable(&mut self, why: &str) {
        if !self.unknown_logged {
            log_warn!("network link {}: {why}", self.interface);
        }
        self.unknown_logged = true;
    }
}
