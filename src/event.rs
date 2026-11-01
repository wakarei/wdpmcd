//! The event loop.
//!
//! One thread, one `poll(2)` per iteration, and every wait expressed as a
//! deadline. Nothing here sleeps, reads or writes blocking: the controller is
//! driven by [`Engine`], helper processes by their slots, and the ready set
//! tells the loop which of them has something to say.
//!
//! Each iteration:
//!
//! 1. re-arm the `alarm(2)` watchdog,
//! 2. drain the signal pipe and act on what it said,
//! 3. service every helper, the controller and the deadline set,
//! 4. compute the next deadline and `poll(2)` until it.
//!
//! A loop that stops making progress is killed by SIGALRM, whose default
//! action ends the process; the supervisor then starts it again, and start-up
//! drives the fan to the safe maximum before anything else.

use std::os::fd::BorrowedFd;
use std::time::{Duration, Instant};

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::unistd::alarm;

use crate::config::Config;
use crate::curve::Controller;
use crate::daemon::{self, FORCE_ATTEMPTS, Options, force_fan};
use crate::led::{Inputs as LedInputs, LedApply, LedDriver};
use crate::pmc::{Engine, FanSetter, Pmc, PmcError, Reply};
use crate::sensors::Sensors;
use crate::serial::Port;
use crate::signals::Signals;
use crate::status::{self, PsuWatch, Snapshot, Tracker};
use crate::{log_debug, log_error, log_info, log_notice, log_warn};

/// A reading older than this many intervals is not trusted.
const STALE_INTERVALS: u32 = 3;
/// A normal fan write gets one attempt: a refusal must reach the fail-safe
/// instead of being retried here.
const CYCLE_ATTEMPTS: u32 = 1;
/// Shortest wait the loop will ask for.
const MIN_WAIT: Duration = Duration::from_millis(1);
/// `poll(2)` takes milliseconds in a 16-bit field.
const MAX_WAIT: Duration = Duration::from_millis(60_000);

/// What the loop is currently doing with the controller.
enum Busy {
    Idle,
    Cycle(Run),
    Status(StatusRead),
    Led(LedApply),
}

/// One sense-decide-act round in progress.
struct Run {
    phase: Phase,
    duty: u8,
    failsafe: bool,
    status: String,
}

enum Phase {
    /// Waiting for the controller temperature.
    Temp,
    /// Writing the fan duty and reading it back.
    Set { setter: FanSetter, recovery: bool },
    /// Waiting out `rpm_delay` before asking for the fan speed.
    RpmWait(Instant),
    /// Waiting for the fan speed.
    Rpm,
}

/// Reading STA and DP0, one command each.
enum StatusRead {
    Sta,
    Dp0 { sta: u8 },
}

/// The daemon's whole state, driven one step at a time.
pub struct Daemon {
    options: Options,
    config: Config,
    engine: Engine<Port>,
    sensors: Sensors,
    controller: Controller,
    led: LedDriver,
    signals: Signals,
    started: Instant,
    failures: u32,
    hold: Option<String>,
    last_status: Option<(u8, bool)>,
    tracker: Tracker,
    psu: PsuWatch,
    metrics_failing: bool,
    cycled: bool,
    over_temp: bool,
    next_cycle: Instant,
    next_status: Instant,
    busy: Busy,
    fatal: Option<String>,
}

impl Daemon {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        options: Options,
        config: Config,
        engine: Engine<Port>,
        sensors: Sensors,
        controller: Controller,
        led: LedDriver,
        signals: Signals,
    ) -> Daemon {
        let now = Instant::now();
        let settle = config.startup_settle;
        Daemon {
            options,
            config,
            engine,
            sensors,
            controller,
            led,
            signals,
            started: now,
            failures: 0,
            hold: None,
            last_status: None,
            tracker: Tracker::default(),
            psu: PsuWatch::default(),
            metrics_failing: false,
            cycled: false,
            over_temp: false,
            next_cycle: now + settle,
            next_status: now,
            busy: Busy::Idle,
            fatal: None,
        }
    }

    /// Runs until a signal arrives or something is fatal. Returns the exit
    /// verdict; the caller then runs the plain, blocking exit path.
    pub fn run(&mut self) -> Result<(), String> {
        loop {
            let now = Instant::now();
            self.arm_watchdog();
            self.signals.drain();
            if self.signals.terminating() {
                return Ok(());
            }
            if self.signals.take_reload() {
                self.reload();
            }
            self.service(now);
            if let Some(reason) = self.fatal.take() {
                return Err(reason);
            }
            if self.signals.terminating() {
                return Ok(());
            }
            let wait = self.wait_time(Instant::now());
            self.wait(wait);
        }
    }

    /// Re-arms the watchdog, which kills the process if the loop wedges.
    fn arm_watchdog(&self) {
        let secs = self.config.watchdog.as_secs().max(1);
        let _ = alarm::set(u32::try_from(secs).unwrap_or(u32::MAX));
    }

    /// Waits for readiness or the next deadline, whichever comes first.
    fn wait(&self, wait: Duration) {
        let mut fds: Vec<BorrowedFd<'_>> = Vec::with_capacity(8);
        fds.push(self.engine.link().fd());
        fds.push(self.signals.fd());
        self.sensors.fds(&mut fds);
        self.led.fds(&mut fds);
        let port_flags = if self.engine.wants_write() {
            PollFlags::POLLIN | PollFlags::POLLOUT
        } else {
            PollFlags::POLLIN
        };
        let mut pollfds: Vec<PollFd<'_>> = fds
            .iter()
            .enumerate()
            .map(|(index, fd)| {
                let flags = if index == 0 {
                    port_flags
                } else {
                    PollFlags::POLLIN
                };
                PollFd::new(*fd, flags)
            })
            .collect();
        let millis = u16::try_from(wait.as_millis()).unwrap_or(u16::MAX);
        // A bad descriptor or an interrupted wait is not fatal: the next pass
        // re-computes the deadlines and tries again.
        let _ = poll(&mut pollfds, PollTimeout::from(millis));
    }

    /// How long the loop may sleep before something is due.
    fn wait_time(&self, now: Instant) -> Duration {
        let mut due = self.next_cycle;
        if !self.config.status_interval.is_zero() {
            due = due.min(self.next_status);
        }
        if let Some(at) = self.engine.deadline() {
            due = due.min(at);
        }
        if let Busy::Cycle(run) = &self.busy {
            match &run.phase {
                Phase::Set { setter, .. } => {
                    if let Some(at) = setter.deadline() {
                        due = due.min(at);
                    }
                }
                Phase::RpmWait(at) => due = due.min(*at),
                Phase::Temp | Phase::Rpm => {}
            }
        }
        let mut extra = Vec::new();
        self.sensors.deadlines(&mut extra);
        self.led.deadlines(&mut extra);
        for at in extra {
            due = due.min(at);
        }
        // Never sleep past half the watchdog, even if every deadline is far.
        let cap = (self.config.watchdog / 2).clamp(MIN_WAIT, MAX_WAIT);
        due.saturating_duration_since(now).clamp(MIN_WAIT, cap)
    }

    /// One pass over everything that can change without a new event.
    fn service(&mut self, now: Instant) {
        self.sensors.service(now);
        self.led.poll(now, self.config.interval);
        self.step_busy(now);
        if matches!(self.busy, Busy::Idle) && now >= self.next_cycle {
            self.start_cycle(now);
        }
        if matches!(self.busy, Busy::Idle) {
            self.poll_status(now);
        }
        if matches!(self.busy, Busy::Idle) {
            self.led_tick(now);
        }
    }

    /// Advances whatever holds the controller.
    fn step_busy(&mut self, now: Instant) {
        let busy = std::mem::replace(&mut self.busy, Busy::Idle);
        self.busy = match busy {
            Busy::Idle => Busy::Idle,
            Busy::Cycle(run) => self.step_cycle(run, now),
            Busy::Status(step) => match self.engine.poll(now) {
                Some(reply) => self.advance_status(step, reply, now),
                None => Busy::Status(step),
            },
            Busy::Led(apply) => match self.engine.poll(now) {
                Some(reply) => self.advance_led(apply, reply, now),
                None => Busy::Led(apply),
            },
        };
    }

    fn step_cycle(&mut self, run: Run, now: Instant) -> Busy {
        match &run.phase {
            // The fan setter polls the controller itself while it is writing
            // and reading back.
            Phase::Set { .. } => self.drive_setter(run, now),
            _ => match self.engine.poll(now) {
                Some(reply) => self.advance_cycle(run, reply, now),
                None => self.drive_pending(run, now),
            },
        }
    }

    /// Handles the phases that start a command when their time has come.
    fn drive_pending(&mut self, mut run: Run, now: Instant) -> Busy {
        if let Phase::RpmWait(at) = run.phase {
            if now < at {
                return Busy::Cycle(run);
            }
            match self.engine.start("RPM", now) {
                Ok(()) => {
                    run.phase = Phase::Rpm;
                    return Busy::Cycle(run);
                }
                Err(e) => {
                    let reason = format!("FAIL-SAFE: cannot read the fan speed: {e}");
                    return self.fan_too_slow(run, &reason, None, now);
                }
            }
        }
        Busy::Cycle(run)
    }

    fn start_cycle(&mut self, now: Instant) {
        self.next_cycle = grid_after(self.next_cycle, self.config.interval, now);
        let run = Run {
            phase: Phase::Temp,
            duty: self.config.safe_max,
            failsafe: true,
            status: String::new(),
        };
        let ready = if let Some(reason) = self.hold.take() {
            Err(reason)
        } else if self.failures > 0 {
            Err("the previous fan setting was not confirmed".to_owned())
        } else {
            Ok(())
        };
        match ready {
            Ok(()) => match self.engine.start("TMP", now) {
                Ok(()) => self.busy = Busy::Cycle(run),
                Err(e) => {
                    let reason = e.to_string();
                    self.busy = self.begin_recovery(run, &reason, now);
                }
            },
            Err(reason) => self.busy = self.begin_recovery(run, &reason, now),
        }
    }

    /// Switches the cycle to the fail-safe duty and starts writing it.
    fn begin_recovery(&mut self, mut run: Run, reason: &str, now: Instant) -> Busy {
        run.duty = self
            .controller
            .fail_high(self.config.safe_max, self.started.elapsed());
        run.failsafe = true;
        run.status = format!("FAIL-SAFE: {reason}");
        log_debug!("{}; requesting {}%", run.status, run.duty);
        run.phase = Phase::Set {
            setter: FanSetter::new(run.duty, CYCLE_ATTEMPTS),
            recovery: false,
        };
        self.drive_setter(run, now)
    }

    /// Advances the fan setter until it needs more time or finishes.
    fn drive_setter(&mut self, mut run: Run, now: Instant) -> Busy {
        let phase = std::mem::replace(&mut run.phase, Phase::Rpm);
        let Phase::Set {
            mut setter,
            recovery,
        } = phase
        else {
            run.phase = phase;
            return Busy::Cycle(run);
        };
        match setter.step(&mut self.engine, now) {
            None => {
                run.phase = Phase::Set { setter, recovery };
                Busy::Cycle(run)
            }
            Some(Ok(())) => {
                self.failures = 0;
                if recovery {
                    return self.finish_cycle();
                }
                run.phase = Phase::RpmWait(now + self.config.rpm_delay);
                Busy::Cycle(run)
            }
            Some(Err(e)) => {
                let duty = setter.target();
                self.failures += 1;
                log_error!("setting fan to {duty}% failed: {e}");
                let reason = format!("FAIL-SAFE: {e}");
                self.report(self.config.safe_max, true, &reason, None);
                if duty == self.config.safe_max {
                    return self.finish_cycle();
                }
                run.phase = Phase::Set {
                    setter: FanSetter::new(self.config.safe_max, FORCE_ATTEMPTS),
                    recovery: true,
                };
                self.drive_setter(run, now)
            }
        }
    }

    /// Handles a reply for the cycle in flight.
    fn advance_cycle(
        &mut self,
        mut run: Run,
        reply: Result<Reply, PmcError>,
        now: Instant,
    ) -> Busy {
        match run.phase {
            Phase::Temp => {
                let board = reply_value(reply, "the temperature read").and_then(|value| {
                    u8::try_from(value)
                        .map_err(|_| format!("controller temperature {value} out of range"))
                });
                let reading = board.and_then(|board| {
                    let sample = self.sensors.sample(board, now).map_err(|e| e.to_string())?;
                    let fast = self.config.interval * STALE_INTERVALS;
                    let slow = self.config.disk_interval * STALE_INTERVALS;
                    match sample.stale(now, fast, slow) {
                        Some(which) => Err(format!("{which} reading is stale")),
                        None => Ok(sample),
                    }
                });
                match reading {
                    Ok(sample) => {
                        let temps = sample.temps;
                        run.duty = self.controller.decide(&temps, self.started.elapsed());
                        run.failsafe = false;
                        self.over_temp = self.config.curve.over_temperature(&temps, self.over_temp);
                        let disk = temps
                            .disk
                            .map_or_else(|| "n/a".to_owned(), |d| format!("{d:.1}C"));
                        run.status =
                            format!("CPU {:.1}C, disks {disk}, PMC {:.1}C", temps.cpu, temps.pmc);
                        log_debug!("{}; requesting {}%", run.status, run.duty);
                        run.phase = Phase::Set {
                            setter: FanSetter::new(run.duty, CYCLE_ATTEMPTS),
                            recovery: false,
                        };
                        self.drive_setter(run, now)
                    }
                    Err(reason) => self.begin_recovery(run, &reason, now),
                }
            }
            Phase::Rpm => {
                let verdict = reply_value(reply, "the fan speed read").and_then(|value| {
                    u16::try_from(value).map_err(|_| format!("fan speed {value} out of range"))
                });
                match verdict {
                    Ok(rpm) if rpm >= self.config.min_rpm => {
                        self.report(run.duty, run.failsafe, &run.status, Some(rpm));
                        self.finish_cycle()
                    }
                    Ok(rpm) => {
                        let reason =
                            format!("FAIL-SAFE: fan at {rpm} RPM, below {}", self.config.min_rpm);
                        self.fan_too_slow(run, &reason, Some(rpm), now)
                    }
                    Err(e) => {
                        let reason = format!("FAIL-SAFE: cannot read the fan speed: {e}");
                        self.fan_too_slow(run, &reason, None, now)
                    }
                }
            }
            Phase::Set { setter, recovery } => {
                run.phase = Phase::Set { setter, recovery };
                self.drive_setter(run, now)
            }
            Phase::RpmWait(at) => {
                run.phase = Phase::RpmWait(at);
                self.drive_pending(run, now)
            }
        }
    }

    /// Holds the next cycle in the fail-safe after a fan that spins too slowly
    /// or cannot be measured, then writes the safe maximum.
    fn fan_too_slow(&mut self, mut run: Run, reason: &str, rpm: Option<u16>, now: Instant) -> Busy {
        self.hold = Some(if rpm.is_some() {
            "the fan was too slow in the previous cycle".to_owned()
        } else {
            "the fan speed could not be read in the previous cycle".to_owned()
        });
        self.controller
            .fail_high(self.config.safe_max, self.started.elapsed());
        self.report(self.config.safe_max, true, reason, rpm);
        run.phase = Phase::Set {
            setter: FanSetter::new(self.config.safe_max, FORCE_ATTEMPTS),
            recovery: true,
        };
        self.drive_setter(run, now)
    }

    fn finish_cycle(&mut self) -> Busy {
        self.cycled = true;
        self.next_cycle = grid_after(self.next_cycle, self.config.interval, Instant::now());
        if self.failures >= self.config.max_failures {
            self.fatal = Some(format!(
                "the PMC has not accepted a fan setting for {} cycles",
                self.failures
            ));
        }
        Busy::Idle
    }

    /// Logs when the duty or the fail-safe state changes.
    fn report(&mut self, duty: u8, failsafe: bool, status: &str, rpm: Option<u16>) {
        if self.last_status != Some((duty, failsafe)) {
            let rpm = rpm.map_or_else(String::new, |r| format!(", {r} RPM"));
            if failsafe {
                log_warn!("fan {duty}%{rpm}; {status}");
            } else {
                log_info!("fan {duty}%{rpm}; {status}");
            }
            self.last_status = Some((duty, failsafe));
        }
    }

    /// Reads STA and DP0, logs changes and refreshes the metrics file.
    ///
    /// Strictly read-only towards the controller, and nothing here feeds back
    /// into the fan decision. Skipped while the fan path is recovering from a
    /// fault so that it never competes with the fail-safe for the port.
    fn poll_status(&mut self, now: Instant) {
        let interval = self.config.status_interval;
        if interval.is_zero() || now < self.next_status {
            return;
        }
        if self.failures > 0 || self.hold.is_some() {
            return;
        }
        self.next_status = now + interval;
        match self.engine.start("STA", now) {
            Ok(()) => self.busy = Busy::Status(StatusRead::Sta),
            Err(e) => log_warn!("cannot read STA: {e}"),
        }
    }

    fn advance_status(
        &mut self,
        step: StatusRead,
        reply: Result<Reply, PmcError>,
        now: Instant,
    ) -> Busy {
        let value = reply_value(reply, "a register read").and_then(|value| {
            u8::try_from(value).map_err(|_| format!("register value {value} out of range"))
        });
        match (step, value) {
            (StatusRead::Sta, Ok(sta)) => match self.engine.start("DP0", now) {
                Ok(()) => Busy::Status(StatusRead::Dp0 { sta }),
                Err(e) => {
                    self.finish_status(Err(e.to_string()));
                    Busy::Idle
                }
            },
            (StatusRead::Dp0 { sta }, Ok(dp0)) => {
                self.finish_status(Ok(Snapshot { sta, dp0 }));
                Busy::Idle
            }
            // Both registers are read the same way, so either failure ends the
            // look.
            (_, Err(why)) => {
                self.finish_status(Err(why));
                Busy::Idle
            }
        }
    }

    fn finish_status(&mut self, reading: Result<Snapshot, String>) {
        self.psu.observe(&reading);
        let bays = self.config.drive_bays;
        for (level, text) in self.tracker.observe(&reading, bays) {
            crate::log::log(level, format_args!("{text}"));
        }
        let Some(dir) = self.config.metrics_dir.clone() else {
            return;
        };
        let text = status::render(reading.ok(), bays);
        match status::write_atomic(&dir, &self.config.metrics_file, &text) {
            Ok(()) => {
                if self.metrics_failing {
                    log_notice!("metrics file {} is writable again", dir.display());
                    self.metrics_failing = false;
                }
            }
            Err(e) => {
                if !self.metrics_failing {
                    log_error!("cannot write the metrics file in {}: {e}", dir.display());
                    self.metrics_failing = true;
                }
            }
        }
    }

    /// Updates the front LED from the fan fail-safe state and the pool health.
    /// Nothing here feeds back into the fan decision.
    fn led_tick(&mut self, now: Instant) {
        let inputs = LedInputs {
            starting: !self.cycled,
            fan_fault: self.last_status.is_some_and(|(_, failsafe)| failsafe),
            over_temp: self.over_temp,
            psu_lost: self.psu.lost(),
        };
        let gap = self.config.interval * STALE_INTERVALS;
        if let Some(apply) = self.led.update(now, inputs, gap) {
            self.busy = self.drive_led(apply, now);
        }
    }

    /// Starts the first command of a look, or reports it as done.
    fn drive_led(&mut self, apply: LedApply, now: Instant) -> Busy {
        match apply.next() {
            Some(op) => match self.engine.start(&op.text(), now) {
                Ok(()) => Busy::Led(apply),
                Err(e) => {
                    self.led.note(apply.state(), Err(e), now);
                    Busy::Idle
                }
            },
            None => {
                self.led.note(apply.state(), Ok(()), now);
                Busy::Idle
            }
        }
    }

    fn advance_led(
        &mut self,
        mut apply: LedApply,
        reply: Result<Reply, PmcError>,
        now: Instant,
    ) -> Busy {
        if let Some(result) = apply.feed(reply) {
            self.led.note(apply.state(), result, now);
            return Busy::Idle;
        }
        self.drive_led(apply, now)
    }

    fn reload(&mut self) {
        match daemon::load_config(self.options.config_path.as_deref()) {
            Ok(config) => {
                self.sensors.reconfigure(&config);
                self.controller.set_curve(config.curve.clone());
                self.engine.set_timeout(config.pmc_timeout);
                self.led.reconfigure(config.led.clone());
                self.config = config;
                log_notice!("configuration reloaded");
            }
            Err(e) => log_error!("reload failed, keeping the old settings: {e}"),
        }
    }

    /// Drives the fan to the safe maximum one last time and finishes the LED.
    ///
    /// This is the one place allowed to block: the loop is over, a stuck
    /// controller must not stop the fan from being made safe, and the whole
    /// path is bounded by [`FORCE_ATTEMPTS`] exchanges.
    pub fn shutdown(mut self, outcome: Result<(), String>) -> Result<(), String> {
        let _ = alarm::cancel();
        self.sensors.stop();
        self.led.stop();
        let duty = self.config.safe_max;
        if outcome.is_ok() {
            log_notice!("terminating: setting fan to {duty}%");
        }
        let mut pmc = Pmc::new(self.engine.into_inner(), self.config.pmc_timeout);
        let forced = force_fan(&mut pmc, duty, FORCE_ATTEMPTS);
        self.led.finish(&mut pmc, outcome.is_ok() && forced.is_ok());
        match (outcome, forced) {
            (Ok(()), Ok(())) => {
                log_debug!("fan confirmed at {duty}%; exiting");
                Ok(())
            }
            (Ok(()), Err(e)) => Err(format!("could not confirm the fan at {duty}% on exit: {e}")),
            (Err(reason), _) => Err(reason),
        }
    }
}

/// The number in a read reply, or the reason there is none.
///
/// `what` names the read, in the words the log uses for it.
fn reply_value(reply: Result<Reply, PmcError>, what: &str) -> Result<u32, String> {
    match reply {
        Ok(Reply::Value(value)) => Ok(value),
        Ok(Reply::Ack) => Err(format!("{what} was answered by ACK")),
        Err(e) => Err(e.to_string()),
    }
}

/// The first slot of the cycle grid after `now`.
///
/// The grid is fixed: slots are one `interval` apart from the previous start,
/// so a cycle that overruns its slot slips to the next free one instead of
/// pushing every later start back by its own running time, and a missed slot
/// is never replayed in a burst. Without an interval there is no grid, and the
/// next cycle is due at once.
fn grid_after(grid: Instant, interval: Duration, now: Instant) -> Instant {
    if interval.is_zero() {
        return now;
    }
    let mut at = grid;
    while at <= now {
        at += interval;
    }
    at
}
