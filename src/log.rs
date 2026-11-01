//! Logging to syslog with an optional copy on standard error.
//!
//! Messages go out as datagrams in the familiar `<PRI>ident[pid]: text` form
//! to the local syslog socket (`/var/run/log` on FreeBSD, `/dev/log`
//! elsewhere). The socket is unconnected and non-blocking: a message that
//! cannot be delivered is dropped rather than allowed to hold up the caller.
//! The `syslog_socket` setting and the `WDPMCD_SYSLOG_SOCKET` environment
//! variable override the path.

use std::fmt;
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// Severity of a log message, most severe first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Error,
    Warning,
    Notice,
    Info,
    Debug,
}

impl Level {
    /// Syslog severity, as used in the priority field.
    fn severity(self) -> u8 {
        match self {
            Level::Error => 3,
            Level::Warning => 4,
            Level::Notice => 5,
            Level::Info => 6,
            Level::Debug => 7,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warning => "warning",
            Level::Notice => "notice",
            Level::Info => "info",
            Level::Debug => "debug",
        }
    }
}

/// Syslog facility `daemon`, the one this program belongs to.
const FACILITY_DAEMON: u8 = 3;

static USE_SYSLOG: AtomicBool = AtomicBool::new(false);
static USE_STDERR: AtomicBool = AtomicBool::new(true);
static DEBUG: AtomicBool = AtomicBool::new(false);
static SOCKET: OnceLock<Option<UnixDatagram>> = OnceLock::new();
static DESTINATION: OnceLock<PathBuf> = OnceLock::new();

/// Points the datagrams at another socket. Called once, from the config.
pub fn set_socket_path(path: Option<PathBuf>) {
    if let Some(path) = path {
        let _ = DESTINATION.set(path);
    }
}

/// Selects the log destinations. Must be called once, early in `main`.
pub fn init(syslog: bool, stderr: bool, debug: bool) {
    USE_STDERR.store(stderr, Ordering::Relaxed);
    DEBUG.store(debug, Ordering::Relaxed);
    if syslog {
        USE_SYSLOG.store(SOCKET.get_or_init(open_syslog).is_some(), Ordering::Relaxed);
    }
}

/// Where the local syslog socket lives.
pub fn socket_path() -> PathBuf {
    if let Some(path) = DESTINATION.get() {
        return path.clone();
    }
    if let Some(path) = std::env::var_os("WDPMCD_SYSLOG_SOCKET") {
        return PathBuf::from(path);
    }
    #[cfg(target_os = "freebsd")]
    {
        PathBuf::from("/var/run/log")
    }
    #[cfg(not(target_os = "freebsd"))]
    {
        PathBuf::from("/dev/log")
    }
}

fn open_syslog() -> Option<UnixDatagram> {
    let socket = UnixDatagram::unbound().ok()?;
    socket.set_nonblocking(true).ok()?;
    Some(socket)
}

/// Formats one datagram the way syslogd expects to read it.
pub fn datagram(level: Level, message: &str, pid: u32) -> String {
    let priority = FACILITY_DAEMON * 8 + level.severity();
    format!("<{priority}>wdpmcd[{pid}]: {message}")
}

fn send_syslog(level: Level, message: &str) {
    let Some(socket) = SOCKET.get().and_then(Option::as_ref) else {
        return;
    };
    // A full queue or a missing daemon loses the message. The caller never
    // waits for the send.
    let _ = socket.send_to(
        datagram(level, message, std::process::id()).as_bytes(),
        socket_path(),
    );
}

/// Emits one message. Prefer the `log_*!` macros.
pub fn log(level: Level, args: fmt::Arguments<'_>) {
    if level == Level::Debug && !DEBUG.load(Ordering::Relaxed) {
        return;
    }
    let message = args.to_string();
    if USE_SYSLOG.load(Ordering::Relaxed) {
        send_syslog(level, &message);
    }
    if USE_STDERR.load(Ordering::Relaxed) {
        eprintln!("wdpmcd: {}: {message}", level.label());
    }
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => { $crate::log::log($crate::log::Level::Error, format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => { $crate::log::log($crate::log::Level::Warning, format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! log_notice {
    ($($arg:tt)*) => { $crate::log::log($crate::log::Level::Notice, format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => { $crate::log::log($crate::log::Level::Info, format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => { $crate::log::log($crate::log::Level::Debug, format_args!($($arg)*)) };
}
