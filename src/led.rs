//! Front-panel power LED control.
//!
//! This module drives three PMC registers, all hexadecimal like the rest of
//! the protocol:
//!
//! * `LED` - steady colour: bit 0 blue, bit 1 red, bit 2 green (red and green
//!   together read as yellow); bits 3 and 4 belong to the USB button LED and
//!   are preserved.
//! * `BLK` - blink mask, same bit layout as `LED` (five bits); the USB bits
//!   are preserved here too, so USB feedback from another module survives.
//! * `PLS` - power LED pulse, bit 0, breathing blue.
//!
//! Order matters on the wire. A write to `PLS` after the `LED` write switches
//! the blue channel off while the registers keep their values, so blue goes
//! dark for as long as that register is written last. Every look therefore
//! clears `PLS` before it touches `LED`, and the pulse is the only look that
//! writes `PLS` at all, as its last write. Writes to `BLK` after `LED` are
//! harmless.
//!
//! Protocol source: the register names, bit assignments and value ranges are
//! facts taken from the public WDCommunity/wdnas-hwtools documentation of the
//! PMC protocol (GPL). This is an independent implementation; no code was
//! copied. How the colours and patterns look on the hardware is not documented
//! there; what the button of this unit shows is in `docs/design.md`.
//!
//! The state machine is documented in `docs/led-state-machine.md`. Colours
//! and patterns of each state are configuration (`led_up`, `led_fault`,
//! `led_shutdown`), so the final choice needs no code change.
//!
//! LED trouble never affects fan control: every error here is logged and
//! retried later, and nothing in this module is called on the fan's path.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::child::{Event, Slot};
use crate::link::{Carrier, LinkSource};
use crate::pmc::{Pmc, PmcError, Reply};
use crate::serial::Link;
use crate::{log_debug, log_info, log_notice, log_warn};

// The bit map of the power button. `LED` and `BLK` share it; `PLS` has a bit
// of its own for the breathing pulse, and the rule that it is never written
// after `LED` is in the module comment.
/// Bit of `LED`/`BLK` that shows blue.
pub const BLUE: u8 = 0b001;
/// Bit of `LED`/`BLK` that shows red.
pub const RED: u8 = 0b010;
/// Bit of `LED`/`BLK` that shows green.
pub const GREEN: u8 = 0b100;
/// Red and green together: the button shows yellow.
pub const YELLOW: u8 = RED | GREEN;
/// Bit of `PLS` that starts the breathing blue pulse.
pub const PULSE_BIT: u8 = 0b001;
/// Bits of `LED`/`BLK` that belong to the power LED.
pub const POWER_MASK: u8 = 0b00111;
/// Bits of `LED`/`BLK` that belong to the USB button LED; never touched.
pub const USB_MASK: u8 = 0b11000;

/// How long one pool health query may run.
const POOL_TIMEOUT: Duration = Duration::from_secs(5);

/// Interface whose carrier is watched unless configured otherwise: the cabled
/// port of the PR2100 on FreeBSD. Elsewhere the name is site-specific, so the
/// check is off until `link_if` is set.
#[cfg(target_os = "freebsd")]
const DEFAULT_LINK_IF: Option<&str> = Some("re0");
#[cfg(not(target_os = "freebsd"))]
const DEFAULT_LINK_IF: Option<&str> = None;

/// The `zpool` program queried for pool capacity unless configured otherwise.
#[cfg(target_os = "freebsd")]
pub const DEFAULT_ZPOOL: &str = "/sbin/zpool";
#[cfg(not(target_os = "freebsd"))]
pub const DEFAULT_ZPOOL: &str = "/usr/sbin/zpool";

/// How a colour is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pattern {
    Steady,
    Blink,
    /// Breathing blue; the only colour the controller pulses.
    Pulse,
}

/// A colour of the power LED and how it is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Look {
    /// A combination of [`BLUE`], [`RED`] and [`GREEN`], or zero for off.
    pub colour: u8,
    pub pattern: Pattern,
}

/// Breathing blue, the look of the pulse register.
pub const BLUE_PULSE: Look = Look {
    colour: BLUE,
    pattern: Pattern::Pulse,
};

/// The register values that produce a [`Look`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Registers {
    pub led: u8,
    pub blk: u8,
    pub pls: u8,
}

/// Colour names and the bits that show them. This is the one place where a
/// name becomes `LED`/`BLK` bits; a different button would change nothing else.
const COLOURS: [(&str, u8); 5] = [
    ("off", 0),
    ("blue", BLUE),
    ("red", RED),
    ("green", GREEN),
    ("yellow", YELLOW),
];

impl Look {
    /// Parses `COLOUR [steady|blink|pulse]`. Any colour may be steady or
    /// blinking; only blue may pulse, because `PLS` has one bit.
    pub fn parse(text: &str) -> Result<Look, String> {
        let fields: Vec<String> = text
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect();
        let (name, pattern) = match fields.as_slice() {
            [name] => (name.as_str(), "steady"),
            [name, pattern] => (name.as_str(), pattern.as_str()),
            _ => return Err("expected COLOUR [steady|blink|pulse]".to_owned()),
        };
        let colour = match COLOURS.iter().find(|(n, _)| *n == name) {
            Some(&(_, colour)) => colour,
            None => {
                let names: Vec<&str> = COLOURS.iter().map(|(n, _)| *n).collect();
                return Err(format!(
                    "unknown colour {name:?} (one of {})",
                    names.join(", ")
                ));
            }
        };
        let pattern = match pattern {
            "steady" => Pattern::Steady,
            "blink" => Pattern::Blink,
            "pulse" => Pattern::Pulse,
            other => return Err(format!("unknown pattern {other:?}")),
        };
        match (colour, pattern) {
            (0, Pattern::Steady) => Ok(Look { colour: 0, pattern }),
            (0, _) => Err("off cannot blink or pulse".to_owned()),
            (BLUE, Pattern::Pulse) => Ok(BLUE_PULSE),
            (_, Pattern::Pulse) => Err("the controller can only pulse blue".to_owned()),
            (colour, pattern) => Ok(Look { colour, pattern }),
        }
    }

    /// The register values for this look, without the USB LED bits.
    pub fn registers(self) -> Registers {
        // A blink sets the colour in both registers: the steady one has to
        // carry the colour as well, or the lamp stays dark between blinks.
        let (led, blk, pls) = match self.pattern {
            Pattern::Steady => (self.colour, 0, 0),
            Pattern::Blink => (self.colour, self.colour, 0),
            Pattern::Pulse => (0, 0, PULSE_BIT),
        };
        Registers { led, blk, pls }
    }
}

impl fmt::Display for Look {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = COLOURS
            .iter()
            .find(|(_, c)| *c == self.colour)
            .map_or("?", |(n, _)| *n);
        match self.pattern {
            Pattern::Steady => write!(f, "{name}"),
            Pattern::Blink => write!(f, "{name} blink"),
            Pattern::Pulse => write!(f, "{name} pulse"),
        }
    }
}

/// LED-related settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedSettings {
    pub enabled: bool,
    /// Shown from daemon start until the first control cycle. Default:
    /// blinking blue.
    pub starting: Look,
    /// Shown while the system is up and healthy. Default: steady blue.
    pub up: Look,
    /// Shown while the fan fail-safe is in force (and when the daemon gives
    /// up). Default: blinking red. Wins over a degraded pool.
    pub fault: Look,
    /// Shown while a pool is not ONLINE. Default: steady red.
    pub pool: Look,
    /// Shown while the cabled network link is down. Stock WD: steady red.
    pub link: Look,
    /// Shown while a power supply that was up is down. Stock WD: steady red.
    pub psu: Look,
    /// Shown while any pool is fuller than `capacity_limit`. Stock WD (alert
    /// 1001): steady red.
    pub capacity: Look,
    /// Shown while the configured UPS runs on battery. Stock WD (0031): steady red.
    pub ups: Look,
    /// Interface whose carrier is watched; `None` disables the link check.
    pub link_if: Option<String>,
    /// A file holding `0`/`1` overrides the carrier lookup.
    pub link_file: Option<PathBuf>,
    /// Program asked for the interface status on FreeBSD.
    #[cfg(target_os = "freebsd")]
    pub ifconfig_cmd: PathBuf,
    /// Shown after a clean stop. Default: blinking blue, as on stock firmware.
    pub shutdown: Look,
    /// Program run as `CMD list -H -o health`; `None` disables the pool check.
    pub pool_cmd: Option<PathBuf>,
    pub pool_interval: Duration,
    /// Program run as `CMD list -H -o capacity`; `None` disables the capacity
    /// check. On by default: it can only fire when a pool is nearly full.
    pub capacity_cmd: Option<PathBuf>,
    /// A pool fuller than this many percent shows `capacity`.
    pub capacity_limit: u8,
    /// Command line (split on blanks, no shell) printing the UPS status; `None`
    /// (the default) leaves the UPS state inert.
    pub ups_cmd: Option<String>,
    pub ups_interval: Duration,
}

impl Default for LedSettings {
    fn default() -> Self {
        LedSettings {
            enabled: true,
            starting: Look {
                colour: BLUE,
                pattern: Pattern::Blink,
            },
            up: Look {
                colour: BLUE,
                pattern: Pattern::Steady,
            },
            fault: Look {
                colour: RED,
                pattern: Pattern::Blink,
            },
            pool: Look {
                colour: RED,
                pattern: Pattern::Steady,
            },
            link: Look {
                colour: RED,
                pattern: Pattern::Steady,
            },
            psu: Look {
                colour: RED,
                pattern: Pattern::Steady,
            },
            capacity: Look {
                colour: RED,
                pattern: Pattern::Steady,
            },
            ups: Look {
                colour: RED,
                pattern: Pattern::Steady,
            },
            link_if: DEFAULT_LINK_IF.map(str::to_owned),
            link_file: None,
            #[cfg(target_os = "freebsd")]
            ifconfig_cmd: PathBuf::from(crate::link::DEFAULT_IFCONFIG),
            shutdown: Look {
                colour: BLUE,
                pattern: Pattern::Blink,
            },
            pool_cmd: None,
            pool_interval: Duration::from_secs(60),
            capacity_cmd: Some(PathBuf::from(DEFAULT_ZPOOL)),
            capacity_limit: 95,
            ups_cmd: None,
            ups_interval: Duration::from_secs(15),
        }
    }
}

impl LedSettings {
    /// Where the carrier state comes from, or `None` while the link watch is
    /// switched off.
    fn link_source(&self) -> Option<LinkSource> {
        let source = LinkSource::new(self.link_if.clone()?, self.link_file.clone());
        #[cfg(target_os = "freebsd")]
        let source = source.with_command(self.ifconfig_cmd.clone());
        Some(source)
    }
}

/// What the LED is currently meant to say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedState {
    Starting,
    Up,
    FanFault,
    OverTemp,
    LinkDown,
    PsuLost,
    PoolFault,
    CapacityFull,
    UpsOnBattery,
    Shutdown,
}

/// Everything the LED decision depends on, gathered by the daemon.
#[derive(Clone, Copy, Debug, Default)]
pub struct Inputs {
    /// No control cycle has completed yet.
    pub starting: bool,
    /// The fan fail-safe is in force.
    pub fan_fault: bool,
    /// The hottest sensor is at or above the top curve step.
    pub over_temp: bool,
    /// A power supply that was up is down.
    pub psu_lost: bool,
}

impl fmt::Display for LedState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LedState::Starting => "starting",
            LedState::Up => "up",
            LedState::FanFault => "fan fault",
            LedState::OverTemp => "over-temperature",
            LedState::LinkDown => "network link down",
            LedState::PsuLost => "power supply lost",
            LedState::PoolFault => "pool fault",
            LedState::CapacityFull => "pool nearly full",
            LedState::UpsOnBattery => "UPS on battery",
            LedState::Shutdown => "shutdown",
        })
    }
}

/// Decides whether `zpool list -H -o health` output shows a sick pool.
pub fn pools_degraded(output: &str) -> bool {
    output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .any(|l| l != "ONLINE")
}

/// Decides whether `zpool list -H -o capacity` output shows a pool fuller
/// than `limit` percent. Lines that are not a percentage are ignored.
pub fn pools_over_capacity(output: &str, limit: u8) -> bool {
    output
        .lines()
        .map(str::trim)
        .filter_map(|l| l.strip_suffix('%').unwrap_or(l).trim().parse::<f64>().ok())
        .any(|used| used > f64::from(limit))
}

/// Decides whether a UPS status line says "on battery": NUT prints `OB`
/// (optionally with `DISCHRG`, `LB`, ...), apcupsd prints `ONBATT`.
pub fn ups_on_battery(output: &str) -> bool {
    output
        .split_whitespace()
        .any(|t| t.eq_ignore_ascii_case("OB") || t.eq_ignore_ascii_case("ONBATT"))
}

/// Periodic external query with a cached verdict: the machinery shared by
/// the pool health, pool capacity and UPS checks. The query runs as a helper
/// process, so a command stuck in the kernel never holds up the caller; the
/// verdict is whatever the last finished query said.
struct PollWatch {
    /// What is being watched, for the log.
    what: &'static str,
    /// What the log says when the verdict turns true / false.
    words: (&'static str, &'static str),
    verdict: bool,
    checked: Option<Instant>,
    unreadable: bool,
    slot: Slot,
    judge: Box<dyn Fn(&str) -> bool>,
}

impl PollWatch {
    fn new(
        what: &'static str,
        words: (&'static str, &'static str),
        judge: Box<dyn Fn(&str) -> bool>,
    ) -> Self {
        PollWatch {
            what,
            words,
            verdict: false,
            checked: None,
            unreadable: false,
            slot: Slot::new(what),
            judge,
        }
    }

    /// Returns the cached verdict, starting `command` at most once per
    /// `interval` and never while the previous one is still running. A
    /// failing query tells nothing; the old verdict stays and is logged once.
    fn poll(&mut self, now: Instant, interval: Duration, command: Option<Command>) -> bool {
        if let Some(event) = self.slot.service(now) {
            match event {
                Event::Done { status: 0, output } => {
                    self.unreadable = false;
                    let verdict = (self.judge)(&output);
                    if verdict != self.verdict {
                        log_notice!(
                            "{}: {}",
                            self.what,
                            if verdict { self.words.0 } else { self.words.1 }
                        );
                    }
                    self.verdict = verdict;
                }
                Event::Done { status, .. } => {
                    self.note_unreadable(&format!("query exited with {status}"));
                }
                Event::TimedOut => self.note_unreadable("no answer before the deadline"),
                Event::Reaped => {}
            }
            self.checked = Some(now);
        }
        let due = self
            .checked
            .is_none_or(|at| now.saturating_duration_since(at) >= interval);
        if due && !self.slot.busy() {
            self.checked = Some(now);
            match command {
                None => self.verdict = false,
                Some(command) => {
                    if let Err(e) = self.slot.start(command, POOL_TIMEOUT, now) {
                        self.note_unreadable(&e);
                    }
                }
            }
        }
        self.verdict
    }

    fn note_unreadable(&mut self, why: &str) {
        if !self.unreadable {
            log_warn!("{} unavailable: {why}", self.what);
        }
        self.unreadable = true;
    }

    fn fd<'a>(&'a self, out: &mut Vec<std::os::fd::BorrowedFd<'a>>) {
        if let Some(fd) = self.slot.fd() {
            out.push(fd);
        }
    }

    fn deadlines(&self, out: &mut Vec<Instant>) {
        if let Some(at) = self.slot.deadline() {
            out.push(at);
        }
    }

    fn stop(&mut self) {
        self.slot.stop();
    }
}

/// Builds the three external watches for a set of settings.
fn watches(settings: &LedSettings) -> (PollWatch, PollWatch, PollWatch) {
    let limit = settings.capacity_limit;
    (
        PollWatch::new(
            "pool health",
            ("not ONLINE", "ONLINE"),
            Box::new(pools_degraded),
        ),
        PollWatch::new(
            "pool capacity",
            ("over the limit", "within the limit"),
            Box::new(move |text| pools_over_capacity(text, limit)),
        ),
        PollWatch::new("UPS", ("on battery", "on mains"), Box::new(ups_on_battery)),
    )
}

fn zpool_command(cmd: &Path, column: &str) -> Command {
    let mut command = Command::new(cmd);
    command.args(["list", "-H", "-o", column]);
    command
}

fn ups_command(line: &str) -> Option<Command> {
    let mut words = line.split_whitespace();
    let program = words.next()?;
    let mut command = Command::new(program);
    command.args(words);
    Some(command)
}

/// Owns the LED state machine.
pub struct LedDriver {
    settings: LedSettings,
    applied: Option<LedState>,
    failed_at: Option<Instant>,
    pool: PollWatch,
    capacity: PollWatch,
    ups: PollWatch,
    link: Carrier,
}

impl LedDriver {
    pub fn new(settings: LedSettings) -> Self {
        let (pool, capacity, ups) = watches(&settings);
        LedDriver {
            applied: None,
            failed_at: None,
            pool,
            capacity,
            ups,
            link: Carrier::new(),
            settings,
        }
    }

    /// Installs new settings; the LED is rewritten on the next update.
    pub fn reconfigure(&mut self, settings: LedSettings) {
        if settings != self.settings {
            self.pool.stop();
            self.capacity.stop();
            self.ups.stop();
            let (pool, capacity, ups) = watches(&settings);
            self.pool = pool;
            self.capacity = capacity;
            self.ups = ups;
            self.applied = None;
            self.failed_at = None;
            self.settings = settings;
        }
    }

    /// Advances the pool, capacity, UPS and link watches. Never waits.
    pub fn poll(&mut self, now: Instant, every: Duration) {
        if !self.settings.enabled {
            return;
        }
        let pool = self
            .settings
            .pool_cmd
            .clone()
            .map(|cmd| zpool_command(&cmd, "health"));
        self.pool.poll(now, self.settings.pool_interval, pool);
        let capacity = self
            .settings
            .capacity_cmd
            .clone()
            .map(|cmd| zpool_command(&cmd, "capacity"));
        self.capacity
            .poll(now, self.settings.pool_interval, capacity);
        let ups = self.settings.ups_cmd.as_deref().and_then(ups_command);
        self.ups.poll(now, self.settings.ups_interval, ups);
        if let Some(source) = self.settings.link_source() {
            self.link.poll(now, &source, every);
        }
    }

    /// Decides what the LED should show and, when that is not what it shows
    /// now, returns the sequence that puts it there.
    ///
    /// Priority, as on stock WD: blinking red (fan fault, over-temperature)
    /// beats steady red (network link, power supply, UPS, pool capacity, pool
    /// health), which beats the starting look and the up look. A failed
    /// write is retried no sooner than `retry_gap` later.
    pub fn update(
        &mut self,
        now: Instant,
        inputs: Inputs,
        retry_gap: Duration,
    ) -> Option<LedApply> {
        if !self.settings.enabled {
            return None;
        }
        let want = if inputs.fan_fault {
            LedState::FanFault
        } else if inputs.over_temp {
            LedState::OverTemp
        } else if self.link.down() {
            LedState::LinkDown
        } else if inputs.psu_lost {
            LedState::PsuLost
        } else if self.ups.verdict {
            LedState::UpsOnBattery
        } else if self.capacity.verdict {
            LedState::CapacityFull
        } else if self.pool.verdict {
            LedState::PoolFault
        } else if inputs.starting {
            LedState::Starting
        } else {
            LedState::Up
        };
        if self.applied == Some(want) {
            return None;
        }
        if self
            .failed_at
            .is_some_and(|t| now.saturating_duration_since(t) < retry_gap)
        {
            return None;
        }
        Some(LedApply::new(want, self.look(want)))
    }

    /// Records how an apply sequence ended.
    pub fn note(&mut self, state: LedState, result: Result<(), PmcError>, now: Instant) {
        match result {
            Ok(()) => {
                self.applied = Some(state);
                self.failed_at = None;
                log_notice!("front LED: {state}");
            }
            Err(e) => {
                self.failed_at = Some(now);
                log_warn!("front LED: cannot show {state}: {e}");
            }
        }
    }

    /// Final LED state at exit: `Shutdown` after a clean stop, the fan-fault
    /// look when the daemon gives up. Best effort, one attempt.
    pub fn finish<L: Link>(&mut self, pmc: &mut Pmc<L>, clean: bool) {
        if !self.settings.enabled {
            return;
        }
        let state = if clean {
            LedState::Shutdown
        } else {
            LedState::FanFault
        };
        let mut apply = LedApply::new(state, self.look(state));
        let mut result = Ok(());
        while let Some(op) = apply.next() {
            let reply = pmc.request(&op.text());
            if let Some(verdict) = apply.feed(reply) {
                result = verdict;
                break;
            }
        }
        match result {
            Ok(()) => {
                self.applied = Some(state);
                log_info!("front LED: {state}");
            }
            Err(e) => log_warn!("front LED: cannot show {state} at exit: {e}"),
        }
    }

    /// The look configured for `state`.
    pub fn look(&self, state: LedState) -> Look {
        match state {
            LedState::Starting => self.settings.starting,
            LedState::Up => self.settings.up,
            LedState::FanFault | LedState::OverTemp => self.settings.fault,
            LedState::LinkDown => self.settings.link,
            LedState::PsuLost => self.settings.psu,
            LedState::PoolFault => self.settings.pool,
            LedState::CapacityFull => self.settings.capacity,
            LedState::UpsOnBattery => self.settings.ups,
            LedState::Shutdown => self.settings.shutdown,
        }
    }

    /// Descriptors that should wake the loop when they become readable.
    pub fn fds<'a>(&'a self, out: &mut Vec<std::os::fd::BorrowedFd<'a>>) {
        if !self.settings.enabled {
            return;
        }
        self.pool.fd(out);
        self.capacity.fd(out);
        self.ups.fd(out);
        self.link.fd(out);
    }

    /// Deadlines at which [`LedDriver::poll`] wants to run again.
    pub fn deadlines(&self, out: &mut Vec<Instant>) {
        if !self.settings.enabled {
            return;
        }
        self.pool.deadlines(out);
        self.capacity.deadlines(out);
        self.ups.deadlines(out);
        self.link.deadlines(out);
    }

    /// Kills any query still running. Nothing waits for it.
    pub fn stop(&mut self) {
        self.pool.stop();
        self.capacity.stop();
        self.ups.stop();
        self.link.stop();
    }
}

/// One command of a look being written to the controller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Read(&'static str),
    Write(&'static str, u8),
}

impl Op {
    /// The command as it goes on the wire.
    pub fn text(&self) -> String {
        match self {
            Op::Read(name) => (*name).to_owned(),
            Op::Write(name, value) => format!("{name}={value:02X}"),
        }
    }
}

/// The register writes for one [`LedState`], spread over loop iterations.
///
/// The order carries the blue channel. A look that is not the pulse clears
/// `PLS` first and writes `LED` last; the pulse writes `LED` and `BLK` off
/// first and sets `PLS` last. Nothing writes `PLS` after `LED`, which would
/// switch blue off again. The USB bits found in the registers are preserved.
pub struct LedApply {
    state: LedState,
    regs: Registers,
    usb_led: u8,
    usb_blk: u8,
    step: Step,
    outcome: Option<Result<(), PmcError>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    ReadLed,
    ReadBlk,
    ClearPls,
    WriteBlk,
    WriteLed,
    WritePls,
    VerifyLed,
    VerifyBlk,
    Done,
}

impl LedApply {
    pub fn new(state: LedState, look: Look) -> LedApply {
        LedApply {
            state,
            regs: look.registers(),
            usb_led: 0,
            usb_blk: 0,
            step: Step::ReadLed,
            outcome: None,
        }
    }

    /// The state this sequence produces.
    pub fn state(&self) -> LedState {
        self.state
    }

    /// The next command to send, or `None` when the sequence is over.
    pub fn next(&self) -> Option<Op> {
        match self.step {
            Step::ReadLed => Some(Op::Read("LED")),
            Step::ReadBlk => Some(Op::Read("BLK")),
            Step::ClearPls => Some(Op::Write("PLS", 0)),
            Step::WriteBlk => Some(Op::Write("BLK", self.regs.blk | self.usb_blk)),
            Step::WriteLed => Some(Op::Write("LED", self.regs.led | self.usb_led)),
            Step::WritePls => Some(Op::Write("PLS", self.regs.pls)),
            Step::VerifyLed => Some(Op::Read("LED")),
            Step::VerifyBlk => Some(Op::Read("BLK")),
            Step::Done => None,
        }
    }

    /// Feeds the reply to the command last returned by [`LedApply::next`].
    pub fn feed(&mut self, reply: Result<Reply, PmcError>) -> Option<Result<(), PmcError>> {
        if self.step == Step::Done {
            return self.outcome.take();
        }
        let result = self.advance(reply);
        if self.step == Step::Done {
            return Some(result);
        }
        if result.is_err() {
            self.step = Step::Done;
            return Some(result);
        }
        None
    }

    fn advance(&mut self, reply: Result<Reply, PmcError>) -> Result<(), PmcError> {
        let value = |reply: Result<Reply, PmcError>| match reply {
            Ok(Reply::Value(value)) => Ok(value),
            Ok(Reply::Ack) => Err(PmcError::Protocol(
                "a register read was answered by ACK".to_owned(),
            )),
            Err(e) => Err(e),
        };
        match self.step {
            Step::ReadLed => {
                self.usb_led = match value(reply) {
                    Ok(v) => (v as u8) & USB_MASK,
                    Err(PmcError::Rejected(_)) => 0,
                    Err(e) => return Err(e),
                };
                self.step = Step::ReadBlk;
                Ok(())
            }
            Step::ReadBlk => {
                self.usb_blk = match value(reply) {
                    Ok(v) => {
                        let v = v as u8;
                        if v & POWER_MASK != 0 && v & POWER_MASK != self.regs.blk {
                            log_info!("clearing blink state BLK={v:02X}");
                        }
                        v & USB_MASK
                    }
                    Err(PmcError::Rejected(_)) => 0,
                    Err(e) => return Err(e),
                };
                // The pulse turns the colour registers off before it starts;
                // every other look gets the pulse out of the way first and
                // leaves the colour write for last.
                self.step = if self.regs.pls != 0 {
                    Step::WriteLed
                } else {
                    Step::ClearPls
                };
                Ok(())
            }
            Step::ClearPls => {
                acknowledge(reply)?;
                self.step = Step::WriteBlk;
                Ok(())
            }
            Step::WriteBlk => {
                acknowledge(reply)?;
                self.step = if self.regs.pls != 0 {
                    Step::WritePls
                } else {
                    Step::WriteLed
                };
                Ok(())
            }
            Step::WriteLed => {
                acknowledge(reply)?;
                self.step = if self.regs.pls != 0 {
                    Step::WriteBlk
                } else {
                    Step::VerifyLed
                };
                Ok(())
            }
            Step::WritePls => {
                acknowledge(reply)?;
                self.step = Step::VerifyLed;
                Ok(())
            }
            Step::VerifyLed => {
                let got = value(reply)? as u8;
                let want = self.regs.led | self.usb_led;
                if got != want {
                    return Err(PmcError::Protocol(format!(
                        "LED set to {want:02X} but reads back {got:02X}"
                    )));
                }
                self.step = Step::VerifyBlk;
                Ok(())
            }
            Step::VerifyBlk => {
                let got = value(reply)? as u8;
                let want = self.regs.blk | self.usb_blk;
                if got != want {
                    return Err(PmcError::Protocol(format!(
                        "BLK set to {want:02X} but reads back {got:02X}"
                    )));
                }
                log_debug!(
                    "LED={:02X} BLK={:02X} PLS={:02X}",
                    self.regs.led | self.usb_led,
                    self.regs.blk | self.usb_blk,
                    self.regs.pls
                );
                self.step = Step::Done;
                Ok(())
            }
            Step::Done => Ok(()),
        }
    }
}

fn acknowledge(reply: Result<Reply, PmcError>) -> Result<(), PmcError> {
    match reply {
        Ok(Reply::Ack) => Ok(()),
        Ok(Reply::Value(_)) => Err(PmcError::Protocol(
            "a register write was answered by a value".to_owned(),
        )),
        Err(e) => Err(e),
    }
}
