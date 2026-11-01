//! Configuration file parsing.
//!
//! The format is one `key = value` pair per line; `#` starts a comment.
//! `disk` and `curve` may be repeated. See wdpmcd.conf(5).

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use crate::curve::{Curve, Step};
use crate::led::{LedSettings, Look};
use crate::pmc::{MAX_DUTY, MIN_DUTY};

/// Default configuration file location.
#[cfg(target_os = "freebsd")]
pub const DEFAULT_PATH: &str = "/usr/local/etc/wdpmcd.conf";
/// Default configuration file location.
#[cfg(not(target_os = "freebsd"))]
pub const DEFAULT_PATH: &str = "/etc/wdpmcd.conf";

#[cfg(target_os = "freebsd")]
const DEFAULT_DISKS: [&str; 2] = ["/dev/ada0", "/dev/ada1"];
#[cfg(not(target_os = "freebsd"))]
const DEFAULT_DISKS: [&str; 2] = ["/dev/sda", "/dev/sdb"];

#[cfg(target_os = "freebsd")]
const DEFAULT_SMARTCTL: &str = "/usr/local/sbin/smartctl";
#[cfg(not(target_os = "freebsd"))]
const DEFAULT_SMARTCTL: &str = "/usr/sbin/smartctl";

/// Prints one temperature per core on FreeBSD.
#[cfg(target_os = "freebsd")]
const DEFAULT_SYSCTL: &str = "/sbin/sysctl";

/// Parsed daemon settings.
#[derive(Clone, Debug)]
pub struct Config {
    pub device: Option<PathBuf>,
    pub pidfile: Option<PathBuf>,
    pub interval: Duration,
    pub disk_interval: Duration,
    pub safe_max: u8,
    pub min_rpm: u16,
    pub rpm_delay: Duration,
    pub startup_settle: Duration,
    pub pmc_timeout: Duration,
    pub discover_timeout: Duration,
    pub watchdog: Duration,
    pub max_failures: u32,
    pub disks: Vec<PathBuf>,
    pub smartctl: PathBuf,
    pub smartctl_timeout: Duration,
    pub cpu_temp_file: Option<PathBuf>,
    /// Where syslog datagrams are sent; the platform default when unset.
    pub syslog_socket: Option<PathBuf>,
    /// Program that prints the CPU temperature on FreeBSD.
    #[cfg(target_os = "freebsd")]
    pub sysctl_cmd: PathBuf,
    pub curve: Curve,
    pub status_interval: Duration,
    pub drive_bays: u8,
    pub metrics_dir: Option<PathBuf>,
    pub metrics_file: String,
    pub led: LedSettings,
}

/// A configuration problem with its location.
#[derive(Debug, PartialEq, Eq)]
pub struct ConfigError {
    pub line: Option<usize>,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "line {line}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for ConfigError {}

fn fail<T>(line: usize, message: impl Into<String>) -> Result<T, ConfigError> {
    Err(ConfigError {
        line: Some(line),
        message: message.into(),
    })
}

fn seconds(
    line: usize,
    key: &str,
    value: &str,
    min: f64,
    max: f64,
) -> Result<Duration, ConfigError> {
    match value.parse::<f64>() {
        Ok(v) if v.is_finite() && (min..=max).contains(&v) => Ok(Duration::from_secs_f64(v)),
        _ => fail(
            line,
            format!("{key} must be a number of seconds within {min}..={max}"),
        ),
    }
}

fn integer<T: std::str::FromStr + PartialOrd + fmt::Display + Copy>(
    line: usize,
    key: &str,
    value: &str,
    min: T,
    max: T,
) -> Result<T, ConfigError> {
    match value.parse::<T>() {
        Ok(v) if v >= min && v <= max => Ok(v),
        _ => fail(
            line,
            format!("{key} must be an integer within {min}..={max}"),
        ),
    }
}

impl Config {
    /// Settings used when no configuration file exists.
    pub fn defaults() -> Config {
        match Config::parse("") {
            Ok(config) => config,
            // The empty file always parses; the fallback only guards a future edit.
            Err(_) => Config::fallback(),
        }
    }

    fn fallback() -> Config {
        Config {
            device: None,
            pidfile: None,
            interval: Duration::from_secs(10),
            disk_interval: Duration::from_secs(30),
            safe_max: MAX_DUTY,
            min_rpm: 400,
            rpm_delay: Duration::from_secs(2),
            startup_settle: Duration::from_secs(6),
            pmc_timeout: crate::pmc::DEFAULT_TIMEOUT,
            discover_timeout: Duration::from_secs(20),
            watchdog: Duration::from_secs(60),
            max_failures: 30,
            disks: DEFAULT_DISKS.iter().map(PathBuf::from).collect(),
            smartctl: PathBuf::from(DEFAULT_SMARTCTL),
            smartctl_timeout: Duration::from_secs(5),
            cpu_temp_file: None,
            syslog_socket: None,
            #[cfg(target_os = "freebsd")]
            sysctl_cmd: PathBuf::from(DEFAULT_SYSCTL),
            curve: Curve::default(),
            status_interval: Duration::from_secs(10),
            drive_bays: crate::status::DEFAULT_BAYS,
            metrics_dir: None,
            metrics_file: crate::status::DEFAULT_FILE.to_owned(),
            led: LedSettings::default(),
        }
    }

    /// Parses configuration text.
    pub fn parse(text: &str) -> Result<Config, ConfigError> {
        let mut cfg = Config::fallback();
        let mut disks: Option<Vec<PathBuf>> = None;
        let mut steps: Vec<Step> = Vec::new();
        let mut min_duty = MIN_DUTY;
        let mut hysteresis = 3.0;
        let mut drop_step = 15u8;
        let mut drop_interval = Duration::from_secs(30);

        for (index, raw) in text.lines().enumerate() {
            let line = index + 1;
            let content = raw.split_once('#').map_or(raw, |(before, _)| before).trim();
            if content.is_empty() {
                continue;
            }
            let Some((key, value)) = content.split_once('=') else {
                return fail(line, "expected key = value");
            };
            let (key, value) = (key.trim(), value.trim());
            if value.is_empty() {
                return fail(line, format!("{key} has no value"));
            }
            match key {
                "device" => cfg.device = Some(PathBuf::from(value)),
                "pidfile" => cfg.pidfile = Some(PathBuf::from(value)),
                "interval" => cfg.interval = seconds(line, key, value, 0.1, 3600.0)?,
                "disk_interval" => cfg.disk_interval = seconds(line, key, value, 0.1, 86400.0)?,
                "safe_max" => cfg.safe_max = integer(line, key, value, MIN_DUTY, MAX_DUTY)?,
                "min_duty" => min_duty = integer(line, key, value, MIN_DUTY, MAX_DUTY)?,
                "min_rpm" => cfg.min_rpm = integer(line, key, value, 0, 20000)?,
                "rpm_delay" => cfg.rpm_delay = seconds(line, key, value, 0.0, 60.0)?,
                "startup_settle" => cfg.startup_settle = seconds(line, key, value, 0.0, 120.0)?,
                "pmc_timeout" => cfg.pmc_timeout = seconds(line, key, value, 0.1, 30.0)?,
                "discover_timeout" => cfg.discover_timeout = seconds(line, key, value, 0.0, 600.0)?,
                "watchdog" => cfg.watchdog = seconds(line, key, value, 15.0, 600.0)?,
                "max_failures" => cfg.max_failures = integer(line, key, value, 1, 100_000)?,
                "hysteresis" => {
                    hysteresis = match value.parse::<f64>() {
                        Ok(v) if v.is_finite() => v,
                        _ => return fail(line, "hysteresis must be a number"),
                    }
                }
                "drop_step" => drop_step = integer(line, key, value, 1, MAX_DUTY)?,
                "drop_interval" => drop_interval = seconds(line, key, value, 0.0, 3600.0)?,
                "disk" => {
                    let list = disks.get_or_insert_with(Vec::new);
                    if let Some(path) = off_when_none(value).map(PathBuf::from) {
                        list.push(path);
                    }
                }
                "smartctl" => cfg.smartctl = PathBuf::from(value),
                "smartctl_timeout" => cfg.smartctl_timeout = seconds(line, key, value, 0.1, 60.0)?,
                "cpu_temp_file" => cfg.cpu_temp_file = Some(PathBuf::from(value)),
                "syslog_socket" => cfg.syslog_socket = Some(PathBuf::from(value)),
                // The helper programs below only exist on FreeBSD. Their keys
                // are still accepted everywhere, so one configuration file can
                // be shared; elsewhere the value is parsed and dropped.
                #[cfg(target_os = "freebsd")]
                "sysctl_cmd" => cfg.sysctl_cmd = PathBuf::from(value),
                #[cfg(not(target_os = "freebsd"))]
                "sysctl_cmd" => {}
                "status_interval" => {
                    cfg.status_interval = seconds(line, key, value, 0.0, 86400.0)?;
                    if !cfg.status_interval.is_zero() && cfg.status_interval.as_millis() < 500 {
                        return fail(line, "status_interval must be 0 or at least 0.5 seconds");
                    }
                }
                "drive_bays" => {
                    cfg.drive_bays = integer(line, key, value, 1, crate::status::MAX_BAYS)?
                }
                "metrics_dir" => cfg.metrics_dir = Some(PathBuf::from(value)),
                "metrics_file" => {
                    if !valid_metrics_file(value) {
                        return fail(
                            line,
                            "metrics_file must be a plain file name ending in .prom",
                        );
                    }
                    cfg.metrics_file = value.to_owned();
                }
                "curve" => steps.push(parse_step(line, value)?),
                "led" => cfg.led.enabled = yes_no(line, key, value)?,
                "led_starting" => cfg.led.starting = look(line, key, value)?,
                "led_up" => cfg.led.up = look(line, key, value)?,
                "led_fault" => cfg.led.fault = look(line, key, value)?,
                "led_pool" => cfg.led.pool = look(line, key, value)?,
                "led_link" => cfg.led.link = look(line, key, value)?,
                "led_psu" => cfg.led.psu = look(line, key, value)?,
                "link_if" => cfg.led.link_if = off_when_none(value).map(str::to_owned),
                "link_carrier_file" => cfg.led.link_file = Some(PathBuf::from(value)),
                #[cfg(target_os = "freebsd")]
                "ifconfig_cmd" => cfg.led.ifconfig_cmd = PathBuf::from(value),
                #[cfg(not(target_os = "freebsd"))]
                "ifconfig_cmd" => {}
                "led_shutdown" => cfg.led.shutdown = look(line, key, value)?,
                "pool_cmd" => cfg.led.pool_cmd = off_when_none(value).map(PathBuf::from),
                "led_capacity" => cfg.led.capacity = look(line, key, value)?,
                "led_ups" => cfg.led.ups = look(line, key, value)?,
                "capacity_cmd" => cfg.led.capacity_cmd = off_when_none(value).map(PathBuf::from),
                "capacity_limit" => cfg.led.capacity_limit = integer(line, key, value, 1, 100)?,
                "ups_cmd" => cfg.led.ups_cmd = off_when_none(value).map(str::to_owned),
                "ups_interval" => cfg.led.ups_interval = seconds(line, key, value, 0.1, 86400.0)?,
                "pool_interval" => cfg.led.pool_interval = seconds(line, key, value, 0.1, 86400.0)?,
                _ => return fail(line, format!("unknown key {key:?}")),
            }
        }

        if let Some(disks) = disks {
            cfg.disks = disks;
        }
        let steps = if steps.is_empty() {
            Curve::default_steps()
        } else {
            steps
        };
        cfg.curve =
            Curve::new(steps, min_duty, hysteresis, drop_step, drop_interval).map_err(|e| {
                ConfigError {
                    line: None,
                    message: e.0,
                }
            })?;
        if cfg.safe_max < cfg.curve.floor() {
            return Err(ConfigError {
                line: None,
                message: "safe_max must not be below min_duty".into(),
            });
        }
        if cfg.disk_interval < cfg.interval {
            cfg.disk_interval = cfg.interval;
        }
        // The watchdog must outlast the longest legitimate silence: two cycles
        // plus one controller exchange.
        let needed = cfg.interval * 2 + cfg.pmc_timeout;
        if cfg.watchdog < needed {
            return Err(ConfigError {
                line: None,
                message: format!(
                    "watchdog must be at least 2 * interval + pmc_timeout ({:.1} s)",
                    needed.as_secs_f64()
                ),
            });
        }
        Ok(cfg)
    }
}

fn valid_metrics_file(name: &str) -> bool {
    name.len() > ".prom".len()
        && name.ends_with(".prom")
        && !name.starts_with('.')
        && !name.contains(['/', '\\', '\0'])
}

fn yes_no(line: usize, key: &str, value: &str) -> Result<bool, ConfigError> {
    match value {
        "yes" => Ok(true),
        "no" => Ok(false),
        _ => fail(line, format!("{key} must be yes or no")),
    }
}

fn look(line: usize, key: &str, value: &str) -> Result<Look, ConfigError> {
    Look::parse(value).or_else(|e| fail(line, format!("{key}: {e}")))
}

/// The value of a setting, unless it is the word `none`, which switches the
/// setting off.
fn off_when_none(value: &str) -> Option<&str> {
    (value != "none").then_some(value)
}

fn parse_step(line: usize, value: &str) -> Result<Step, ConfigError> {
    let fields: Vec<&str> = value.split_whitespace().collect();
    let [duty, cpu, disk, pmc] = fields.as_slice() else {
        return fail(line, "curve needs four fields: duty cpu disk pmc");
    };
    let temp = |s: &str| match s.parse::<f64>() {
        Ok(v) if v.is_finite() => Ok(v),
        _ => fail(line, format!("bad temperature {s:?}")),
    };
    Ok(Step {
        duty: integer(line, "curve duty", duty, MIN_DUTY, MAX_DUTY)?,
        cpu: temp(cpu)?,
        disk: temp(disk)?,
        pmc: temp(pmc)?,
    })
}
