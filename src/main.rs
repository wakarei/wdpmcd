//! wdpmcd: temperature-driven fan control for the WD My Cloud PR2100/PR4100
//! power management controller.
//!
//! The daemon runs one thread and waits in one `poll(2)` call; see `event`.
#![forbid(unsafe_code)]

mod child;
mod config;
mod curve;
mod daemon;
mod discover;
mod event;
mod led;
mod link;
mod log;
mod pidfile;
mod pmc;
mod sensors;
mod serial;
mod signals;
mod status;

use std::path::PathBuf;
use std::process::ExitCode;

use daemon::Options;

const USAGE: &str = "\
usage: wdpmcd [-d] [-f conf] [-p pidfile] [-D device]
       wdpmcd [-f conf] [-D device] -q | -n | -s percent
       wdpmcd -V | -h

  -d          log to standard error as well as syslog, with debug detail
  -f conf     configuration file
  -p pidfile  pidfile
  -D device   serial device of the PMC (default: autodetect)
  -q          print FAN, RPM and TMP from the PMC; never writes
  -n          read all sensors, print the duty that would be chosen; never writes
  -s percent  set the fan once (30-99), confirm by reading it back
  -V          print the version
  -h          print this help";

#[derive(Debug, PartialEq)]
enum Mode {
    Daemon,
    Query,
    DryRun,
    Set(u8),
    Version,
    Help,
}

#[derive(Debug)]
struct Cli {
    mode: Mode,
    debug: bool,
    options: Options,
}

fn parse_args<I: Iterator<Item = String>>(mut args: I) -> Result<Cli, String> {
    let mut cli = Cli {
        mode: Mode::Daemon,
        debug: false,
        options: Options::default(),
    };
    let mut mode_flags = 0;
    let mut set_mode = |cli: &mut Cli, mode: Mode| {
        mode_flags += 1;
        cli.mode = mode;
        if mode_flags > 1 {
            Err("-q, -n and -s are mutually exclusive".to_owned())
        } else {
            Ok(())
        }
    };
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| {
            args.next()
                .ok_or_else(|| format!("{flag} needs an argument"))
        };
        match arg.as_str() {
            "-d" => cli.debug = true,
            "-f" => cli.options.config_path = Some(PathBuf::from(value("-f")?)),
            "-p" => cli.options.pidfile = Some(PathBuf::from(value("-p")?)),
            "-D" => cli.options.device = Some(PathBuf::from(value("-D")?)),
            "-q" => set_mode(&mut cli, Mode::Query)?,
            "-n" => set_mode(&mut cli, Mode::DryRun)?,
            "-s" => {
                let text = value("-s")?;
                let duty = text
                    .parse::<u8>()
                    .ok()
                    .filter(|d| (pmc::MIN_DUTY..=pmc::MAX_DUTY).contains(d))
                    .ok_or_else(|| {
                        format!(
                            "-s takes a percentage within {}..={}, got {text:?}",
                            pmc::MIN_DUTY,
                            pmc::MAX_DUTY
                        )
                    })?;
                set_mode(&mut cli, Mode::Set(duty))?;
            }
            "-V" => {
                return Ok(Cli {
                    mode: Mode::Version,
                    ..cli
                });
            }
            "-h" => {
                return Ok(Cli {
                    mode: Mode::Help,
                    ..cli
                });
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(cli)
}

fn main() -> ExitCode {
    let cli = match parse_args(std::env::args().skip(1)) {
        Ok(cli) => cli,
        Err(message) => {
            eprintln!("wdpmcd: {message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match cli.mode {
        Mode::Version => {
            println!("wdpmcd {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Mode::Help => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => {}
    }

    let daemonized = cli.mode == Mode::Daemon;
    log::init(
        daemonized || matches!(cli.mode, Mode::Set(_)),
        !daemonized || cli.debug,
        cli.debug,
    );
    let result = daemon::load_config(cli.options.config_path.as_deref()).and_then(|config| {
        log::set_socket_path(config.syslog_socket.clone());
        match cli.mode {
            Mode::Daemon => daemon::run(cli.options.clone(), config),
            Mode::Query => daemon::query(&cli.options, &config),
            Mode::DryRun => daemon::dry_run(&cli.options, &config),
            Mode::Set(duty) => daemon::set_once(&cli.options, &config, duty),
            Mode::Version | Mode::Help => Ok(()),
        }
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            log_error!("{message}");
            ExitCode::FAILURE
        }
    }
}
