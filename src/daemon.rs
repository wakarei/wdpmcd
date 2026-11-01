//! Starting the daemon, the one-shot command modes, and the exit path.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::curve::Controller;
use crate::event::Daemon;
use crate::led::LedDriver;
use crate::pidfile::Pidfile;
use crate::pmc::{Engine, Pmc, PmcError};
use crate::sensors::Sensors;
use crate::serial::Port;
use crate::signals::Signals;
use crate::{discover, log_error, log_info, log_notice, log_warn};

/// How often a fan write is attempted when the controller misbehaves.
pub const FORCE_ATTEMPTS: u32 = 5;
/// Pause between forced attempts.
const FORCE_PAUSE: Duration = Duration::from_millis(200);

/// Command-line settings that outlive configuration reloads.
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub config_path: Option<PathBuf>,
    pub pidfile: Option<PathBuf>,
    pub device: Option<PathBuf>,
}

/// Loads the configuration. A missing default file yields the defaults; a
/// missing explicitly requested file is an error.
pub fn load_config(path: Option<&Path>) -> Result<Config, String> {
    let (path, explicit) = match path {
        Some(p) => (p, true),
        None => (Path::new(crate::config::DEFAULT_PATH), false),
    };
    match std::fs::read_to_string(path) {
        Ok(text) => Config::parse(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => Ok(Config::defaults()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Opens the PMC port, discovering it when no device was configured.
pub fn open_port(
    options: &Options,
    config: &Config,
    signals: Option<&Signals>,
) -> Result<Port, String> {
    let device = match options.device.clone().or_else(|| config.device.clone()) {
        Some(device) => device,
        None => discover_with_retry(config.discover_timeout, signals)?,
    };
    Port::open(&device).map_err(|e| format!("{}: {e}", device.display()))
}

fn discover_with_retry(timeout: Duration, signals: Option<&Signals>) -> Result<PathBuf, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match discover::find() {
            Ok(path) => return Ok(path),
            Err(e) if Instant::now() >= deadline => return Err(e),
            Err(_) => {}
        }
        match signals {
            Some(s) if !s.wait(Duration::from_millis(500)) => {
                return Err("terminated while looking for the PMC".to_owned());
            }
            Some(_) => {}
            None => std::thread::sleep(Duration::from_millis(500)),
        }
    }
}

/// Makes a best-effort attempt to drive the fan to `duty` if the process panics.
pub fn install_panic_hook(port: File, duty: u8) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = (&port).write_all(format!("FAN={duty:02X}\r").as_bytes());
        log_error!("fatal: {info}");
        previous(info);
    }));
}

/// Sets `duty` with read-back, retrying a few times.
///
/// This is the blocking path, used before the loop starts and on the way out,
/// where waiting is preferable to giving up on the fan.
pub fn force_fan(pmc: &mut Pmc<Port>, duty: u8, attempts: u32) -> Result<(), PmcError> {
    let mut last = PmcError::Refused("no attempt made".to_owned());
    for attempt in 1..=attempts {
        match pmc.set_fan(duty) {
            Ok(()) => return Ok(()),
            Err(e) => {
                log_warn!("setting fan to {duty}% failed (attempt {attempt}/{attempts}): {e}");
                last = e;
                if attempt < attempts {
                    std::thread::sleep(FORCE_PAUSE);
                }
            }
        }
    }
    Err(last)
}

/// Runs the daemon until it is told to stop. Returns the process exit status.
pub fn run(options: Options, config: Config) -> Result<(), String> {
    let signals = Signals::install().map_err(|e| format!("cannot install signal handlers: {e}"))?;
    let port = open_port(&options, &config, Some(&signals))?;
    let pidfile_path = options.pidfile.clone().or_else(|| config.pidfile.clone());
    let _pidfile = match pidfile_path {
        Some(path) => {
            Some(Pidfile::create(&path).map_err(|e| format!("pidfile {}: {e}", path.display()))?)
        }
        None => None,
    };
    let path = port.path().to_path_buf();
    if let Ok(clone) = port.try_clone_file() {
        install_panic_hook(clone, config.safe_max);
    }
    let mut pmc = Pmc::new(port, config.pmc_timeout);
    log_notice!("starting on {}", path.display());

    // Start-up pins the fan to the safe maximum before anything slow happens,
    // and confirms it, so a daemon that dies later still leaves it there.
    match pmc.fan() {
        Ok(previous) => log_info!("fan was at {previous}%"),
        Err(e) => log_warn!("cannot read the fan setting: {e}"),
    }
    force_fan(&mut pmc, config.safe_max, FORCE_ATTEMPTS)
        .map_err(|e| format!("the PMC does not accept fan commands: {e}"))?;
    log_info!("fan set to {}% while sensors settle", config.safe_max);

    let engine = Engine::new(pmc.into_inner(), config.pmc_timeout);
    let sensors = Sensors::new(&config);
    let controller = Controller::new(config.curve.clone(), config.safe_max);
    let led = LedDriver::new(config.led.clone());
    let mut daemon = Daemon::new(options, config, engine, sensors, controller, led, signals);
    let outcome = daemon.run();
    daemon.shutdown(outcome)
}

/// `-q`: print the controller's readings without writing anything.
pub fn query(options: &Options, config: &Config) -> Result<(), String> {
    let port = open_port(options, config, None)?;
    let path = port.path().display().to_string();
    let mut pmc = Pmc::new(port, config.pmc_timeout);
    pmc.set_read_only(true);
    let fan = pmc.fan().map_err(|e| e.to_string())?;
    let rpm = pmc.rpm().map_err(|e| e.to_string())?;
    let temp = pmc.temperature().map_err(|e| e.to_string())?;
    println!("device: {path}");
    println!("FAN={fan}% (0x{fan:02x})");
    println!("RPM={rpm} (0x{rpm:04x})");
    println!("TMP={temp}C (0x{temp:02x})");
    Ok(())
}

/// `-s PCT`: set the duty once and confirm it by reading it back.
pub fn set_once(options: &Options, config: &Config, duty: u8) -> Result<(), String> {
    let port = open_port(options, config, None)?;
    let mut pmc = Pmc::new(port, config.pmc_timeout);
    pmc.set_fan(duty).map_err(|e| e.to_string())?;
    log_notice!("fan set to {duty}% and confirmed");
    println!("FAN={duty}% confirmed");
    Ok(())
}

/// `-n`: read every sensor and print the duty the curve would choose.
pub fn dry_run(options: &Options, config: &Config) -> Result<(), String> {
    let port = open_port(options, config, None)?;
    let mut pmc = Pmc::new(port, config.pmc_timeout);
    pmc.set_read_only(true);
    let mut sensors = Sensors::new(config);
    sensors.wait_for_cpu();
    sensors.wait_for_disks();
    let board = pmc.temperature().map_err(|e| e.to_string())?;
    let sample = sensors
        .sample(board, Instant::now())
        .map_err(|e| e.to_string())?;
    let temps = sample.temps;
    let duty = config.curve.requested(&temps, 0.0);
    println!("CPU: {:.1}C", temps.cpu);
    match temps.disk {
        Some(d) => println!("disks: {d:.1}C"),
        None => println!("disks: not monitored"),
    }
    println!("PMC: {:.1}C", temps.pmc);
    println!("duty: {duty}% (dry run, nothing written)");
    Ok(())
}
