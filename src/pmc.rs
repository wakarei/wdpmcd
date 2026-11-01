//! The PMC wire protocol.
//!
//! The controller speaks CR-terminated ASCII at 9600 baud. A bare name reads
//! a value (`FAN` is answered by `FAN=50`), `NAME=VALUE` writes one and is
//! answered by `ACK`, and a rejected command is answered by `ERR`. Numbers
//! are hexadecimal. The controller may also emit unsolicited `ALERT` lines.
//!
//! Two drivers speak this protocol:
//!
//! * [`Pmc`] blocks on one exchange at a time. It is used where waiting is
//!   the point: before the loop starts and on the way out.
//! * [`Engine`] never waits. It owns at most one command in flight and is
//!   driven by the event loop, which polls the descriptor and hands the
//!   engine a deadline to respect.

use std::fmt;
use std::io;
use std::time::{Duration, Instant};

use crate::log_warn;
use crate::serial::{Interest, Link};

/// Lowest duty cycle the daemon will ever request, in percent.
pub const MIN_DUTY: u8 = 30;
/// Highest duty cycle the controller accepts, in percent.
pub const MAX_DUTY: u8 = 99;
/// Default bound on one command/response exchange.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
/// Pause before another attempt at setting the fan.
pub const RETRY_PAUSE: Duration = Duration::from_millis(200);
/// Longest line the parser will buffer before giving up.
const MAX_LINE: usize = 4096;

/// Failure of a single PMC exchange.
#[derive(Debug)]
pub enum PmcError {
    /// No matching reply arrived before the deadline.
    Timeout(String),
    /// The controller answered `ERR`.
    Rejected(String),
    /// A reply matched but could not be parsed or was out of range.
    Protocol(String),
    /// The controller sent too much data without a line terminator.
    Overflow,
    /// Reading or writing the port failed.
    Io(io::Error),
    /// A write was refused before reaching the wire.
    Refused(String),
}

impl fmt::Display for PmcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PmcError::Timeout(cmd) => write!(f, "no reply to {cmd} from the PMC"),
            PmcError::Rejected(cmd) => write!(f, "the PMC rejected {cmd}"),
            PmcError::Protocol(msg) => write!(f, "protocol error: {msg}"),
            PmcError::Overflow => write!(f, "the PMC sent an over-long line"),
            PmcError::Io(err) => write!(f, "serial I/O error: {err}"),
            PmcError::Refused(msg) => write!(f, "refused: {msg}"),
        }
    }
}

impl std::error::Error for PmcError {}

impl From<io::Error> for PmcError {
    fn from(err: io::Error) -> Self {
        PmcError::Io(err)
    }
}

/// A reply that ends an exchange.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    /// `ACK`, the answer to a write.
    Ack,
    /// The hexadecimal value of a read.
    Value(u32),
}

/// Classification of one received line.
#[derive(Debug, PartialEq, Eq)]
enum Line<'a> {
    Ack,
    Err,
    Other(&'a str),
}

fn classify(line: &str) -> Line<'_> {
    match line {
        "ACK" => Line::Ack,
        "ERR" => Line::Err,
        other => Line::Other(other),
    }
}

/// Parses the hexadecimal value of a `NAME=VALUE` reply.
///
/// Returns `None` when `line` is not a reply for `name` at all, and an error
/// when it is one but its value is malformed.
pub fn parse_value(name: &str, line: &str) -> Option<Result<u32, PmcError>> {
    let rest = line.strip_prefix(name)?.strip_prefix('=')?;
    let digits = rest.trim();
    if digits.is_empty() || digits.len() > 4 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(Err(PmcError::Protocol(format!(
            "bad value in reply {line:?}"
        ))));
    }
    Some(
        u32::from_str_radix(digits, 16)
            .map_err(|_| PmcError::Protocol(format!("bad value in reply {line:?}"))),
    )
}

fn check_write_allowed(read_only: bool, command: &str) -> Result<(), PmcError> {
    if read_only && command.contains('=') {
        return Err(PmcError::Refused(format!(
            "{command} on a read-only connection"
        )));
    }
    Ok(())
}

/// Collects the reply expected for `command` from one received line.
fn match_reply(command: &str, is_write: bool, line: &str) -> Option<Result<Reply, PmcError>> {
    match classify(line) {
        Line::Err => Some(Err(PmcError::Rejected(command.to_owned()))),
        Line::Ack => is_write.then_some(Ok(Reply::Ack)),
        // A reply starts with the command's name followed by `=`, so an
        // unsolicited ALERT line can never get past the check below.
        Line::Other(text) => {
            let prefix = reply_prefix(command);
            if text.as_bytes().get(prefix.len()) != Some(&b'=') {
                return None;
            }
            parse_value(prefix, text).map(|result| result.map(Reply::Value))
        }
    }
}

/// The name in `command`: the part a reply to it starts with.
fn reply_prefix(command: &str) -> &str {
    command.split_once('=').map_or(command, |(name, _)| name)
}

/// A connection to the controller that waits for each reply.
///
/// This is the blocking half of the protocol, used before the loop starts and
/// on the way out. It drives the same [`Engine`] the loop uses, so both paths
/// share one state machine and one set of rules.
pub struct Pmc<L: Link> {
    engine: Engine<L>,
    timeout: Duration,
}

impl<L: Link> Pmc<L> {
    /// Wraps `link`; every exchange is bounded by `timeout`.
    pub fn new(link: L, timeout: Duration) -> Self {
        Pmc {
            engine: Engine::new(link, timeout),
            timeout,
        }
    }

    /// Makes every write fail locally, before anything reaches the wire.
    pub fn set_read_only(&mut self, read_only: bool) {
        self.engine.set_read_only(read_only);
    }

    /// Gives back the transport, so the blocking driver can be used again.
    pub fn into_inner(self) -> L {
        self.engine.into_inner()
    }

    /// Runs one command to its reply.
    pub fn request(&mut self, command: &str) -> Result<Reply, PmcError> {
        self.engine.set_timeout(self.timeout);
        self.engine.start(command, Instant::now())?;
        loop {
            if let Some(reply) = self.engine.poll(Instant::now()) {
                return reply;
            }
            let Some(deadline) = self.engine.deadline() else {
                return Err(PmcError::Timeout(command.to_owned()));
            };
            let now = Instant::now();
            if now >= deadline {
                continue;
            }
            let interest = if self.engine.wants_write() {
                Interest::Writable
            } else {
                Interest::Readable
            };
            self.engine.link_mut().wait(interest, deadline - now)?;
        }
    }

    fn read_value(&mut self, name: &str) -> Result<u32, PmcError> {
        match self.request(name)? {
            Reply::Value(value) => Ok(value),
            Reply::Ack => Err(PmcError::Protocol(format!("{name} was answered by ACK"))),
        }
    }

    /// Current fan duty cycle in percent.
    pub fn fan(&mut self) -> Result<u8, PmcError> {
        let value = self.read_value("FAN")?;
        match u8::try_from(value) {
            Ok(duty) if duty <= 100 => Ok(duty),
            _ => Err(PmcError::Protocol(format!("fan duty {value} out of range"))),
        }
    }

    /// Fan speed in revolutions per minute.
    pub fn rpm(&mut self) -> Result<u16, PmcError> {
        let value = self.read_value("RPM")?;
        u16::try_from(value).map_err(|_| PmcError::Protocol(format!("rpm {value} out of range")))
    }

    /// Controller board temperature in degrees Celsius.
    pub fn temperature(&mut self) -> Result<u8, PmcError> {
        let value = self.read_value("TMP")?;
        u8::try_from(value)
            .map_err(|_| PmcError::Protocol(format!("temperature {value} out of range")))
    }

    /// Sets the fan duty cycle and confirms it by reading the value back.
    ///
    /// Values outside [`MIN_DUTY`]..=[`MAX_DUTY`] are refused without
    /// touching the wire.
    pub fn set_fan(&mut self, duty: u8) -> Result<(), PmcError> {
        check_duty(duty)?;
        self.request(&format!("FAN={duty:02X}"))?;
        let actual = self.fan()?;
        if actual != duty {
            return Err(PmcError::Protocol(format!(
                "fan set to {duty}% but reads back {actual}%"
            )));
        }
        Ok(())
    }
}

/// Refuses a duty cycle outside the range the controller accepts.
pub fn check_duty(duty: u8) -> Result<(), PmcError> {
    if (MIN_DUTY..=MAX_DUTY).contains(&duty) {
        Ok(())
    } else {
        Err(PmcError::Refused(format!(
            "fan duty {duty}% is outside {MIN_DUTY}..={MAX_DUTY}"
        )))
    }
}

/// Cuts the next complete line out of `pending`, skipping blank ones.
fn take_line(pending: &mut Vec<u8>) -> Option<String> {
    loop {
        let end = pending.iter().position(|&b| b == b'\r' || b == b'\n')?;
        let raw: Vec<u8> = pending.drain(..=end).collect();
        let text = String::from_utf8_lossy(&raw[..raw.len() - 1])
            .trim()
            .to_owned();
        if !text.is_empty() {
            return Some(text);
        }
    }
}

/// The controller as the event loop sees it: one command in flight, no waits.
pub struct Engine<L: Link> {
    link: L,
    pending: Vec<u8>,
    out: Vec<u8>,
    inflight: Option<InFlight>,
    timeout: Duration,
    read_only: bool,
}

struct InFlight {
    command: String,
    is_write: bool,
    deadline: Instant,
}

impl<L: Link> Engine<L> {
    pub fn new(link: L, timeout: Duration) -> Self {
        Engine {
            link,
            pending: Vec::new(),
            out: Vec::new(),
            inflight: None,
            timeout,
            read_only: false,
        }
    }

    /// Makes every write fail locally, before anything reaches the wire.
    pub fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
    }

    /// Changes the bound on one exchange.
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// True while a command awaits its reply.
    pub fn busy(&self) -> bool {
        self.inflight.is_some()
    }

    /// When the command in flight gives up.
    pub fn deadline(&self) -> Option<Instant> {
        self.inflight.as_ref().map(|f| f.deadline)
    }

    /// True while bytes are still queued for the wire.
    pub fn wants_write(&self) -> bool {
        !self.out.is_empty()
    }

    /// Starts one command. Refused while another is in flight.
    pub fn start(&mut self, command: &str, now: Instant) -> Result<(), PmcError> {
        if self.busy() {
            return Err(PmcError::Refused(format!(
                "{command} while {} is still in flight",
                self.inflight
                    .as_ref()
                    .map_or("a command", |f| f.command.as_str())
            )));
        }
        check_write_allowed(self.read_only, command)?;
        // Whatever is already waiting belongs to an earlier exchange and can
        // never be the reply to this one.
        self.drain_input();
        self.out.clear();
        self.out.extend_from_slice(command.as_bytes());
        self.out.push(b'\r');
        self.inflight = Some(InFlight {
            command: command.to_owned(),
            is_write: command.contains('='),
            deadline: now + self.timeout,
        });
        self.flush()?;
        Ok(())
    }

    /// Pushes queued bytes to the wire as far as it will take them.
    pub fn flush(&mut self) -> Result<(), PmcError> {
        while !self.out.is_empty() {
            match self.link.write(&self.out) {
                Ok(0) => break,
                Ok(n) => {
                    self.out.drain(..n.min(self.out.len()));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    let failed = self.inflight.take();
                    self.out.clear();
                    return Err(match failed {
                        Some(flight) => PmcError::Io(io::Error::new(
                            e.kind(),
                            format!("{}: {e}", flight.command),
                        )),
                        None => e.into(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Takes in what has arrived and reports the end of the exchange, if any.
    ///
    /// Call it when the descriptor is readable and whenever a deadline has
    /// passed; it is cheap and idempotent otherwise.
    pub fn poll(&mut self, now: Instant) -> Option<Result<Reply, PmcError>> {
        if self.inflight.is_none() {
            self.drain_input();
            return None;
        }
        if let Err(e) = self.flush() {
            return Some(Err(e));
        }
        if let Err(e) = self.fill() {
            self.inflight = None;
            return Some(Err(e));
        }
        while let Some(line) = take_line(&mut self.pending) {
            let Some(flight) = self.inflight.as_ref() else {
                break;
            };
            let command = flight.command.clone();
            if let Some(reply) = match_reply(&command, flight.is_write, &line) {
                self.inflight = None;
                return Some(reply);
            }
        }
        if let Some(flight) = self.inflight.as_ref()
            && now >= flight.deadline
        {
            let command = flight.command.clone();
            self.inflight = None;
            self.out.clear();
            return Some(Err(PmcError::Timeout(command)));
        }
        None
    }

    fn fill(&mut self) -> Result<(), PmcError> {
        let mut chunk = [0u8; 256];
        loop {
            match self.link.read(&mut chunk) {
                // An idle serial port returns zero bytes, not `EAGAIN`, because
                // it runs with VMIN = 0. Silence is not a hang-up; a controller
                // that stops answering is caught by the deadline.
                Ok(0) => return Ok(()),
                Ok(n) => {
                    self.pending.extend_from_slice(&chunk[..n]);
                    if self.pending.len() > MAX_LINE && !self.pending.contains(&b'\r') {
                        self.pending.clear();
                        return Err(PmcError::Overflow);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Reads and drops whatever is waiting, so it cannot be taken for a reply.
    fn drain_input(&mut self) {
        let mut chunk = [0u8; 256];
        while let Ok(n) = self.link.read(&mut chunk) {
            if n == 0 {
                break;
            }
        }
        self.pending.clear();
    }

    /// Gives access to the transport.
    pub fn link(&self) -> &L {
        &self.link
    }

    /// Gives mutable access to the transport, for a blocking wait.
    pub fn link_mut(&mut self) -> &mut L {
        &mut self.link
    }

    /// Gives back the transport, for the blocking exit path.
    pub fn into_inner(self) -> L {
        self.link
    }
}

/// One "set the fan and confirm it" exchange, spread over loop iterations.
///
/// The states are Write, AwaitAck, ReadBack, AwaitValue, Compare and, when an
/// attempt fails, Retry; `Done` carries the verdict.
pub struct FanSetter {
    target: u8,
    attempts: u32,
    tried: u32,
    state: State,
    outcome: Option<Result<(), PmcError>>,
}

enum State {
    Write,
    AwaitAck,
    ReadBack,
    AwaitValue,
    RetryAt(Instant),
    Done,
}

impl FanSetter {
    /// A setter that tries `attempts` times before giving up.
    pub fn new(target: u8, attempts: u32) -> FanSetter {
        FanSetter {
            target,
            attempts: attempts.max(1),
            tried: 0,
            state: State::Write,
            outcome: None,
        }
    }

    /// What the request asked for.
    pub fn target(&self) -> u8 {
        self.target
    }

    /// When a paused setter wants to be called again.
    pub fn deadline(&self) -> Option<Instant> {
        match self.state {
            State::RetryAt(at) => Some(at),
            _ => None,
        }
    }

    /// Advances the exchange; `Some` once it is over.
    pub fn step<L: Link>(
        &mut self,
        engine: &mut Engine<L>,
        now: Instant,
    ) -> Option<Result<(), PmcError>> {
        loop {
            match self.state {
                State::Done => return self.outcome.take(),
                State::RetryAt(at) => {
                    if now < at {
                        return None;
                    }
                    self.state = State::Write;
                }
                State::Write => {
                    if let Err(e) = check_duty(self.target) {
                        self.give_up(e);
                        continue;
                    }
                    match engine.start(&format!("FAN={:02X}", self.target), now) {
                        Ok(()) => self.state = State::AwaitAck,
                        Err(e) => self.failed(e, now),
                    }
                }
                State::AwaitAck => match engine.poll(now) {
                    None => return None,
                    Some(Ok(Reply::Ack)) => self.state = State::ReadBack,
                    Some(Ok(Reply::Value(_))) => {
                        let e =
                            PmcError::Protocol("the fan write was answered by a value".to_owned());
                        self.failed(e, now);
                    }
                    Some(Err(e)) => self.failed(e, now),
                },
                State::ReadBack => match engine.start("FAN", now) {
                    Ok(()) => self.state = State::AwaitValue,
                    Err(e) => self.failed(e, now),
                },
                State::AwaitValue => match engine.poll(now) {
                    None => return None,
                    Some(Ok(Reply::Value(value))) => match u8::try_from(value) {
                        Ok(actual) if actual == self.target => {
                            self.state = State::Done;
                            self.outcome = Some(Ok(()));
                        }
                        Ok(actual) => {
                            let e = PmcError::Protocol(format!(
                                "fan set to {}% but reads back {actual}%",
                                self.target
                            ));
                            self.failed(e, now);
                        }
                        Err(_) => {
                            let e = PmcError::Protocol(format!(
                                "fan reads back an impossible value {value:#x}"
                            ));
                            self.failed(e, now);
                        }
                    },
                    Some(Ok(Reply::Ack)) => {
                        let e = PmcError::Protocol("the fan read was answered by ACK".to_owned());
                        self.failed(e, now);
                    }
                    Some(Err(e)) => self.failed(e, now),
                },
            }
        }
    }

    /// Records a failed attempt: pause, or give up when they are used up.
    fn failed(&mut self, error: PmcError, now: Instant) {
        self.tried += 1;
        if self.tried >= self.attempts {
            self.give_up(error);
            return;
        }
        log_warn!("setting the fan to {}% failed: {error}", self.target);
        self.state = State::RetryAt(now + RETRY_PAUSE);
    }

    fn give_up(&mut self, error: PmcError) {
        self.state = State::Done;
        self.outcome = Some(Err(error));
    }
}
