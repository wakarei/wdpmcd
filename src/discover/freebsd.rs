//! PMC discovery through the `dev.uart` sysctl tree.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use super::parse_uart_units;
use crate::child;

/// How long `sysctl` may run during discovery.
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(5);

pub fn find() -> Result<PathBuf, String> {
    let mut command = Command::new("/sbin/sysctl");
    command.arg("dev.uart");
    let (status, text) = child::run_bounded(command, DISCOVER_TIMEOUT)?;
    if status != 0 {
        return Err(format!("sysctl dev.uart exited with status {status}"));
    }
    let unit = parse_uart_units(&text).ok_or_else(|| {
        "no uart(4) device with PCI id 8086:228c (slot 30, function 4) found".to_owned()
    })?;
    Ok(PathBuf::from(format!("/dev/cuau{unit}")))
}
