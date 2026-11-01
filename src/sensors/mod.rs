//! Temperature sources: CPU, disks (through smartctl) and the PMC itself.
//!
//! Every reading that can be slow comes from a helper process driven by the
//! event loop. A reading that is late is simply old: the fan policy decides
//! what an old value means, and this module never waits for one.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::child::{Event, Slot, WAIT_PAUSE, wait_bounded};
use crate::config::Config;
use crate::curve::{Temps, checked_temperature};
use crate::pmc::PmcError;
use crate::{log_notice, log_warn};

#[cfg(target_os = "freebsd")]
mod freebsd;
#[cfg(target_os = "linux")]
mod linux;

/// Disks in standby are reported at least this warm, so they are not woken.
const STANDBY_ASSUMED: f64 = 35.0;
/// How long the CPU temperature helper may run.
#[cfg(target_os = "freebsd")]
const CPU_TIMEOUT: Duration = Duration::from_secs(3);

/// Why a temperature could not be obtained.
#[derive(Debug)]
pub enum SensorError {
    Cpu(String),
    Disk(PathBuf, String),
    Pmc(PmcError),
}

impl fmt::Display for SensorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SensorError::Cpu(msg) => write!(f, "CPU temperature: {msg}"),
            SensorError::Disk(dev, msg) => write!(f, "disk {}: {msg}", dev.display()),
            SensorError::Pmc(err) => write!(f, "controller temperature: {err}"),
        }
    }
}

impl std::error::Error for SensorError {}

/// A set of readings with the time each one was started.
#[derive(Clone, Copy, Debug)]
pub struct Sample {
    pub temps: Temps,
    pub cpu_at: Instant,
    pub pmc_at: Instant,
    pub disk_at: Option<Instant>,
}

impl Sample {
    /// Names the first reading that is older than allowed, if any.
    ///
    /// CPU and controller readings may be at most `fast` old, disks at most `slow`.
    pub fn stale(&self, now: Instant, fast: Duration, slow: Duration) -> Option<&'static str> {
        let age = |at: Instant| now.saturating_duration_since(at);
        if age(self.cpu_at) > fast {
            Some("CPU")
        } else if age(self.pmc_at) > fast {
            Some("controller")
        } else if self.disk_at.is_some_and(|at| age(at) > slow) {
            Some("disk")
        } else {
            None
        }
    }
}

/// How long a stuck smartctl is tolerated before the log says so.
const DISK_STUCK_AFTER: Duration = Duration::from_secs(2);

/// One pass over the configured disks.
struct Round {
    started: Instant,
    results: Vec<Option<Result<f64, String>>>,
    pending: usize,
}

/// Reads all sensors. Disk temperatures come from smartctl, which can hang on
/// a dying drive, so they are fetched by helper processes and cached; the fan
/// loop only ever looks at the cache and how old it is.
pub struct Sensors {
    cpu_temp_file: Option<PathBuf>,
    #[cfg(target_os = "freebsd")]
    cpu_command: PathBuf,
    /// Cores to ask about; learned once from `hw.ncpu`.
    #[cfg(target_os = "freebsd")]
    cpu_cores: Option<usize>,
    /// What the running CPU helper was asked for.
    #[cfg(target_os = "freebsd")]
    cpu_asked: CpuAsked,
    cpu_cache: Option<(f64, Instant)>,
    cpu_error: Option<String>,
    cpu_checked: Option<Instant>,
    cpu_every: Duration,
    cpu_slot: Slot,
    disks: Vec<PathBuf>,
    smartctl: PathBuf,
    smartctl_timeout: Duration,
    disk_interval: Duration,
    disk_cache: Vec<f64>,
    disk_at: Option<Instant>,
    disk_error: Option<(PathBuf, String)>,
    disk_slots: Vec<Slot>,
    round: Option<Round>,
    stuck_logged: bool,
}

/// Which of the two CPU helpers is running.
#[cfg(target_os = "freebsd")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum CpuAsked {
    Cores,
    Temperatures,
}

impl Sensors {
    pub fn new(config: &Config) -> Sensors {
        let mut sensors = Sensors {
            cpu_temp_file: None,
            #[cfg(target_os = "freebsd")]
            cpu_command: PathBuf::new(),
            #[cfg(target_os = "freebsd")]
            cpu_cores: None,
            #[cfg(target_os = "freebsd")]
            cpu_asked: CpuAsked::Cores,
            cpu_cache: None,
            cpu_error: None,
            cpu_checked: None,
            cpu_every: Duration::from_secs(10),
            cpu_slot: Slot::new("CPU temperature"),
            disks: Vec::new(),
            smartctl: PathBuf::new(),
            smartctl_timeout: Duration::ZERO,
            disk_interval: Duration::ZERO,
            disk_cache: Vec::new(),
            disk_at: None,
            disk_error: None,
            disk_slots: Vec::new(),
            round: None,
            stuck_logged: false,
        };
        sensors.reconfigure(config);
        let now = Instant::now();
        sensors.service_cpu(now);
        sensors.maybe_start_round(now);
        sensors
    }

    /// Applies new settings. Cached disk readings and a running query are
    /// kept when the disks and smartctl are unchanged.
    pub fn reconfigure(&mut self, config: &Config) {
        if self.disks != config.disks || self.smartctl != config.smartctl {
            self.disk_cache.clear();
            self.disk_at = None;
            self.disk_error = None;
            self.round = None;
            for slot in &mut self.disk_slots {
                slot.stop();
            }
            self.disk_slots = config.disks.iter().map(|_| Slot::new("smartctl")).collect();
        }
        self.cpu_temp_file = config.cpu_temp_file.clone();
        #[cfg(target_os = "freebsd")]
        {
            self.cpu_command = config.sysctl_cmd.clone();
        }
        self.cpu_every = config.interval;
        self.disks = config.disks.clone();
        self.smartctl = config.smartctl.clone();
        self.smartctl_timeout = config.smartctl_timeout;
        self.disk_interval = config.disk_interval;
    }

    /// Builds a sample from the caches and a controller reading.
    ///
    /// Any sensor that failed outright fails the whole sample; a sensor that
    /// has merely gone quiet only ages, which [`Sample::stale`] judges.
    pub fn sample(&self, board: u8, pmc_at: Instant) -> Result<Sample, SensorError> {
        let (cpu, cpu_at) = match self.cpu_cache {
            Some(pair) => pair,
            None => {
                return Err(SensorError::Cpu(
                    self.cpu_error
                        .clone()
                        .unwrap_or_else(|| "no reading".to_owned()),
                ));
            }
        };
        if let Some(message) = &self.cpu_error {
            return Err(SensorError::Cpu(message.clone()));
        }
        let board = checked_temperature(f64::from(board)).ok_or_else(|| {
            SensorError::Pmc(PmcError::Protocol(format!("implausible {board} C")))
        })?;
        let disk = self.disk()?;
        Ok(Sample {
            temps: Temps {
                cpu,
                disk,
                pmc: board,
            },
            cpu_at,
            pmc_at,
            disk_at: self.disk_at.filter(|_| disk.is_some()),
        })
    }

    /// Advances every helper and starts the ones that are due.
    pub fn service(&mut self, now: Instant) {
        self.service_cpu(now);
        self.service_disks(now);
    }

    /// Descriptors that should wake the loop when they become readable.
    pub fn fds<'a>(&'a self, out: &mut Vec<std::os::fd::BorrowedFd<'a>>) {
        if let Some(fd) = self.cpu_slot.fd() {
            out.push(fd);
        }
        for slot in &self.disk_slots {
            if let Some(fd) = slot.fd() {
                out.push(fd);
            }
        }
    }

    /// Deadlines at which [`Sensors::service`] wants to run again.
    pub fn deadlines(&self, out: &mut Vec<Instant>) {
        if let Some(at) = self.cpu_slot.deadline() {
            out.push(at);
        }
        for slot in &self.disk_slots {
            if let Some(at) = slot.deadline() {
                out.push(at);
            }
        }
    }

    /// Kills any helper still running. Used on the way out; nothing waits.
    pub fn stop(&mut self) {
        self.cpu_slot.stop();
        for slot in &mut self.disk_slots {
            slot.stop();
        }
    }

    /// Waits briefly for the first CPU reading; for the one-shot dry run only.
    pub fn wait_for_cpu(&mut self) {
        #[cfg(target_os = "freebsd")]
        if self.cpu_temp_file.is_none() {
            let limit = Instant::now() + 3 * CPU_TIMEOUT;
            wait_bounded(limit, WAIT_PAUSE, || {
                self.service(Instant::now());
                self.cpu_checked.map(|_| ())
            });
        }
    }

    /// Waits briefly for the first disk reading; for the one-shot dry run only.
    pub fn wait_for_disks(&mut self) {
        if self.disk_at.is_some() {
            return;
        }
        let limit = Instant::now() + self.smartctl_timeout + Duration::from_secs(1);
        wait_bounded(limit, WAIT_PAUSE, || {
            self.service(Instant::now());
            let gave_up = self.round.is_none() && self.disk_error.is_some();
            (self.disk_at.is_some() || gave_up).then_some(())
        });
    }

    fn service_cpu(&mut self, now: Instant) {
        match self.cpu_slot.service(now) {
            Some(Event::Done { status, output }) => self.note_cpu_helper(status, &output, now),
            Some(Event::TimedOut) => {
                // The reading keeps its age; the staleness rule does the rest.
                self.cpu_error = None;
                self.cpu_checked = Some(now);
            }
            Some(Event::Reaped) | None => {}
        }
        if self.cpu_temp_file.is_some() || cfg!(target_os = "linux") {
            if self.cpu_due(now) {
                self.read_cpu_directly(now);
            }
        } else if self.cpu_due(now) {
            self.start_cpu_helper(now);
        }
    }

    /// Turns one finished CPU helper into a reading, a core count or an error.
    #[cfg(target_os = "freebsd")]
    fn note_cpu_helper(&mut self, status: i32, output: &str, now: Instant) {
        if status != 0 {
            self.cpu_error = Some(format!("sysctl exited with status {status}"));
            self.cpu_checked = Some(now);
            return;
        }
        if self.cpu_asked == CpuAsked::Cores {
            match freebsd::parse_core_count(output) {
                Ok(cores) => {
                    self.cpu_cores = Some(cores);
                    self.start_cpu_helper(now);
                }
                Err(message) => {
                    self.cpu_error = Some(message);
                    self.cpu_checked = Some(now);
                }
            }
            return;
        }
        let reading = match self.cpu_cores {
            Some(cores) => parse_cpu_temperature(output, cores),
            None => Err("the core count is unknown".to_owned()),
        };
        self.store_cpu(reading, now);
    }

    #[cfg(not(target_os = "freebsd"))]
    fn note_cpu_helper(&mut self, _status: i32, _output: &str, now: Instant) {
        self.cpu_checked = Some(now);
    }

    /// Keeps a reading, or the reason there is none.
    fn store_cpu(&mut self, reading: Result<f64, String>, now: Instant) {
        match reading {
            Ok(value) => match checked_temperature(value) {
                Some(value) => {
                    self.cpu_cache = Some((value, now));
                    self.cpu_error = None;
                }
                None => self.cpu_error = Some(format!("implausible reading {value} C")),
            },
            Err(message) => self.cpu_error = Some(message),
        }
        self.cpu_checked = Some(now);
    }

    #[cfg(target_os = "freebsd")]
    fn start_cpu_helper(&mut self, now: Instant) {
        if self.cpu_slot.busy() {
            return;
        }
        let command = match self.cpu_cores {
            Some(cores) => {
                self.cpu_asked = CpuAsked::Temperatures;
                freebsd::temperature_command(&self.cpu_command, cores)
            }
            None => {
                self.cpu_asked = CpuAsked::Cores;
                freebsd::core_count_command(&self.cpu_command)
            }
        };
        if let Err(e) = self.cpu_slot.start(command, CPU_TIMEOUT, now) {
            self.cpu_error = Some(e);
            self.cpu_checked = Some(now);
        }
    }

    #[cfg(not(target_os = "freebsd"))]
    fn start_cpu_helper(&mut self, _now: Instant) {}

    fn cpu_due(&self, now: Instant) -> bool {
        self.cpu_checked
            .is_none_or(|at| now.saturating_duration_since(at) >= self.cpu_every)
    }

    /// Reads a temperature file or the hwmon tree; both are cheap enough to do
    /// on the loop thread.
    fn read_cpu_directly(&mut self, now: Instant) {
        let reading = match &self.cpu_temp_file {
            Some(path) => read_millidegrees(path),
            None => platform_cpu(),
        };
        self.store_cpu(reading, now);
    }

    fn service_disks(&mut self, now: Instant) {
        if self.disks.is_empty() {
            return;
        }
        for (index, slot) in self.disk_slots.iter_mut().enumerate() {
            let Some(event) = slot.service(now) else {
                continue;
            };
            let Some(round) = self.round.as_mut() else {
                continue;
            };
            match event {
                Event::Done { status, output } => {
                    if self.stuck_logged {
                        log_notice!("smartctl answered again");
                        self.stuck_logged = false;
                    }
                    let previous = self.disk_cache.get(index).copied().unwrap_or(0.0);
                    let result = interpret_disk_status(status, &output, previous);
                    if let Some(slot_result) = round.results.get_mut(index)
                        && slot_result.is_none()
                    {
                        *slot_result = Some(result);
                        round.pending -= 1;
                    }
                }
                Event::TimedOut => {
                    if let Some(slot_result) = round.results.get_mut(index)
                        && slot_result.is_none()
                    {
                        *slot_result = Some(Err("no answer before the deadline".to_owned()));
                        round.pending -= 1;
                    }
                }
                Event::Reaped => {}
            }
        }
        if let Some(round) = self.round.as_ref()
            && round.pending > 0
        {
            let running = round.started.elapsed();
            if running > self.smartctl_timeout + DISK_STUCK_AFTER && !self.stuck_logged {
                log_warn!("smartctl query stuck for {:.0} s", running.as_secs_f64());
                self.stuck_logged = true;
            }
            return;
        }
        self.finish_round();
        self.maybe_start_round(now);
    }

    fn finish_round(&mut self) {
        let Some(round) = self.round.take() else {
            return;
        };
        let mut values = Vec::with_capacity(round.results.len());
        let mut failure = None;
        for (index, result) in round.results.into_iter().enumerate() {
            match result {
                Some(Ok(value)) => values.push(value),
                Some(Err(message)) => {
                    if failure.is_none() {
                        let dev = self.disks.get(index).cloned().unwrap_or_default();
                        failure = Some((dev, message));
                    }
                }
                None => {
                    if failure.is_none() {
                        let dev = self.disks.get(index).cloned().unwrap_or_default();
                        failure = Some((dev, "no reading".to_owned()));
                    }
                }
            }
        }
        match failure {
            None => {
                self.disk_cache = values;
                self.disk_at = Some(round.started);
                self.disk_error = None;
            }
            Some(error) => self.disk_error = Some(error),
        }
    }

    fn maybe_start_round(&mut self, now: Instant) {
        if self.disks.is_empty() || self.round.is_some() {
            return;
        }
        if self.disk_slots.iter().any(Slot::busy) {
            // A child from an earlier round is still out there; its slot stays
            // closed until the kernel has released it.
            return;
        }
        let due = self.disk_error.is_some()
            || self
                .disk_at
                .is_none_or(|at| now.saturating_duration_since(at) >= self.disk_interval);
        if !due {
            return;
        }
        let count = self.disks.len();
        for (index, slot) in self.disk_slots.iter_mut().enumerate() {
            let dev = self.disks[index].clone();
            let mut command = Command::new(&self.smartctl);
            command.args(["-n", "standby,3", "-A"]).arg(&dev);
            if let Err(e) = slot.start(command, self.smartctl_timeout, now) {
                log_warn!("cannot start smartctl: {e}");
                return;
            }
        }
        self.round = Some(Round {
            started: now,
            results: vec![None; count],
            pending: count,
        });
    }

    fn disk(&self) -> Result<Option<f64>, SensorError> {
        if self.disks.is_empty() {
            return Ok(None);
        }
        if let Some((dev, msg)) = &self.disk_error {
            return Err(SensorError::Disk(dev.clone(), msg.clone()));
        }
        if self.disk_at.is_none() {
            let why = if self.round.is_some() {
                "no reading yet"
            } else {
                "no reading"
            };
            return Err(SensorError::Disk(self.disks[0].clone(), why.to_owned()));
        }
        Ok(self.disk_cache.iter().copied().reduce(f64::max))
    }
}

/// Reads a millidegree file (Linux hwmon style).
fn read_millidegrees(path: &Path) -> Result<f64, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let milli: f64 = text
        .trim()
        .parse()
        .map_err(|_| format!("{}: not a number", path.display()))?;
    Ok(milli / 1000.0)
}

#[cfg(target_os = "linux")]
fn platform_cpu() -> Result<f64, String> {
    linux::cpu_temperature(Path::new("/sys/class/hwmon"))
}

#[cfg(not(target_os = "linux"))]
fn platform_cpu() -> Result<f64, String> {
    Err("no CPU sensor support on this platform; set cpu_temp_file".into())
}

/// Turns one finished smartctl run into a temperature.
fn interpret_disk_status(status: i32, output: &str, previous: f64) -> Result<f64, String> {
    if status == 3 && ["STANDBY", "SLEEP"].iter().any(|m| output.contains(m)) {
        return Ok(previous.max(STANDBY_ASSUMED));
    }
    if status & 7 != 0 {
        return Err(format!("smartctl exited with status {status}"));
    }
    let value = parse_smartctl_temperature(output)
        .ok_or_else(|| "no temperature in smartctl output".to_owned())?;
    checked_temperature(value).ok_or_else(|| format!("implausible reading {value} C"))
}

/// Parses the `NN.NC` lines that `sysctl -n dev.cpu.N.temperature` prints, one
/// per core, and returns the hottest. A line that is not a temperature, or a
/// count that does not match `cores`, fails the whole reading.
#[cfg(target_os = "freebsd")]
pub fn parse_cpu_temperature(text: &str, cores: usize) -> Result<f64, String> {
    let mut values = Vec::with_capacity(cores);
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let value = line
            .trim()
            .strip_suffix('C')
            .and_then(|value| value.trim().parse::<f64>().ok())
            .ok_or_else(|| format!("cannot read a temperature from {line:?}"))?;
        values.push(value);
    }
    if values.len() != cores {
        return Err(format!("{} temperatures for {cores} cores", values.len()));
    }
    values
        .into_iter()
        .reduce(f64::max)
        .ok_or_else(|| "no CPU temperature in the sysctl output".to_owned())
}

/// Extracts a drive temperature from `smartctl -A` text output.
///
/// ATA attribute 194 is preferred over 190; NVMe and SCSI print a
/// `Temperature:` line instead.
pub fn parse_smartctl_temperature(text: &str) -> Option<f64> {
    let attribute = |id: &str| {
        text.lines().find_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.first() == Some(&id) && fields.len() >= 10 {
                fields[9].parse::<u64>().ok().map(|raw| (raw & 0xff) as f64)
            } else {
                None
            }
        })
    };
    attribute("194").or_else(|| attribute("190")).or_else(|| {
        text.lines().find_map(|line| {
            let (label, rest) = line.split_once(':')?;
            if !label.trim().ends_with("Temperature") {
                return None;
            }
            let mut words = rest.split_whitespace();
            let value = words.next()?.parse::<f64>().ok()?;
            (words.next() == Some("Celsius")).then_some(value)
        })
    })
}
