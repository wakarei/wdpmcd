//! Locating the PMC serial port.
//!
//! The controller hangs off the second high-speed UART of the SoC, PCI
//! 8086:228c at 00:1e.4. The tty number depends on the operating system and
//! on which other UARTs are enabled, so the port is identified by PCI
//! identity rather than by name.

use std::path::PathBuf;

#[cfg(target_os = "freebsd")]
mod freebsd;
#[cfg(target_os = "linux")]
mod linux;

/// PCI address of the PMC UART, in sysfs notation.
#[cfg(target_os = "linux")]
const PCI_ADDRESS: &str = "0000:00:1e.4";

/// True if a FreeBSD `%pnpinfo` string names the PMC UART (vendor 8086, device 228c).
#[cfg(target_os = "freebsd")]
pub fn pnpinfo_matches(pnpinfo: &str) -> bool {
    let field = |key: &str| {
        pnpinfo
            .split_whitespace()
            .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
    };
    field("vendor") == Some("0x8086") && field("device") == Some("0x228c")
}

/// True if a FreeBSD `%location` string places the device at slot 30, function 4.
#[cfg(target_os = "freebsd")]
pub fn location_matches(location: &str) -> bool {
    let has = |wanted: &str| location.split_whitespace().any(|word| word == wanted);
    has("slot=30") && has("function=4")
}

/// Picks the uart unit whose PCI identity is the PMC's out of the
/// `sysctl dev.uart` dump.
#[cfg(target_os = "freebsd")]
pub fn parse_uart_units(text: &str) -> Option<u32> {
    let mut found: Option<u32> = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let Some(rest) = key.trim().strip_prefix("dev.uart.") else {
            continue;
        };
        let Some((unit, leaf)) = rest.split_once('.') else {
            continue;
        };
        let Ok(unit) = unit.parse::<u32>() else {
            continue;
        };
        let matched = match leaf {
            "%pnpinfo" => pnpinfo_matches(value),
            "%location" => location_matches(value),
            _ => false,
        };
        if matched {
            found = Some(found.map_or(unit, |seen: u32| seen.min(unit)));
        }
    }
    found
}

/// Finds the PMC serial device node on this system.
pub fn find() -> Result<PathBuf, String> {
    #[cfg(target_os = "freebsd")]
    {
        freebsd::find()
    }
    #[cfg(target_os = "linux")]
    {
        linux::find(
            std::path::Path::new("/sys/class/tty"),
            std::path::Path::new("/dev"),
        )
    }
    #[cfg(not(any(target_os = "freebsd", target_os = "linux")))]
    {
        Err("automatic PMC discovery is not supported here; set device".to_owned())
    }
}
