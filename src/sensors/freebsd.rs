//! CPU temperature from coretemp(4), one OID per core.
//!
//! Two helpers run here, both through the same non-blocking slot: `hw.ncpu`
//! once to learn how many cores to ask about, then one `sysctl` invocation
//! that names every `dev.cpu.N.temperature` OID. Asking for the bare
//! `dev.cpu` subtree instead would drag in `coretemp.tjmax` (90.0 C on a
//! PR2100) and read as a hot CPU.

use std::path::Path;
use std::process::Command;

/// The command that prints the number of cores.
pub fn core_count_command(program: &Path) -> Command {
    let mut command = Command::new(program);
    command.args(["-n", "hw.ncpu"]);
    command
}

/// The command printing one temperature per core, in core order.
pub fn temperature_command(program: &Path, cores: usize) -> Command {
    let mut command = Command::new(program);
    command.arg("-n");
    for core in 0..cores {
        command.arg(format!("dev.cpu.{core}.temperature"));
    }
    command
}

/// Parses `sysctl -n hw.ncpu`. Anything but one plausible count is an error.
pub fn parse_core_count(text: &str) -> Result<usize, String> {
    let text = text.trim();
    match text.parse::<usize>() {
        Ok(cores) if (1..=1024).contains(&cores) => Ok(cores),
        _ => Err(format!("hw.ncpu reads {text:?}")),
    }
}
