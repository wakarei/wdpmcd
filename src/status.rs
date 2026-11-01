//! Controller status: the STA and DP0 registers, change logging and the
//! node_exporter textfile.
//!
//! This module is deliberately independent of the fan control path. It only
//! reads from the controller, and nothing it does can change the fan duty.
//!
//! Register meanings:
//!
//! * `STA`: bit 1 is set while power supply 2 is up, bit 2 while power supply
//!   1 is up. Other bits are exported raw and otherwise ignored.
//! * `DP0`: bit N is *cleared* while a drive sits in bay N and set while the
//!   bay is empty. Bits above the populated bays read as set.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::log::Level;

/// `STA` bit for power supply 1.
const STA_PSU1_UP: u8 = 0b0000_0100;
/// `STA` bit for power supply 2.
const STA_PSU2_UP: u8 = 0b0000_0010;
/// Most drive bays any supported model has.
pub const MAX_BAYS: u8 = 4;
/// Default number of drive bays (My Cloud PR2100).
pub const DEFAULT_BAYS: u8 = 2;
/// Default name of the metrics file.
pub const DEFAULT_FILE: &str = "wdpmcd.prom";

/// One reading of both registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub sta: u8,
    pub dp0: u8,
}

impl Snapshot {
    pub fn psu_up(self, psu: u8) -> bool {
        match psu {
            1 => self.sta & STA_PSU1_UP != 0,
            _ => self.sta & STA_PSU2_UP != 0,
        }
    }

    pub fn drive_present(self, slot: u8) -> bool {
        slot < 8 && self.dp0 & (1 << slot) == 0
    }

    /// Human-readable summary used in log lines.
    pub fn describe(self, bays: u8) -> String {
        let state = |up: bool| if up { "up" } else { "down" };
        let mut text = format!(
            "STA=0x{:02x} (PSU1 {}, PSU2 {}), DP0=0x{:02x} (",
            self.sta,
            state(self.psu_up(1)),
            state(self.psu_up(2)),
            self.dp0
        );
        for slot in 0..bays {
            if slot > 0 {
                text.push_str(", ");
            }
            let drive = if self.drive_present(slot) {
                "present"
            } else {
                "empty"
            };
            text.push_str(&format!("bay{slot} {drive}"));
        }
        text.push(')');
        text
    }
}

/// Renders the textfile-collector exposition for `reading`.
///
/// `None` means the registers could not be read: only the health gauge is
/// emitted, so no stale value is ever presented as current.
pub fn render(reading: Option<Snapshot>, bays: u8) -> String {
    let mut out = String::new();
    out.push_str("# HELP wdpmcd_status_read_ok 1 if the last read of STA and DP0 succeeded.\n");
    out.push_str("# TYPE wdpmcd_status_read_ok gauge\n");
    out.push_str(&format!(
        "wdpmcd_status_read_ok {}\n",
        u8::from(reading.is_some())
    ));
    let Some(snap) = reading else {
        return out;
    };
    out.push_str("# HELP wdpmcd_sta Raw value of the PMC STA register.\n");
    out.push_str("# TYPE wdpmcd_sta gauge\n");
    out.push_str(&format!("wdpmcd_sta {}\n", snap.sta));
    out.push_str("# HELP wdpmcd_dp0 Raw value of the PMC DP0 (drive presence) register.\n");
    out.push_str("# TYPE wdpmcd_dp0 gauge\n");
    out.push_str(&format!("wdpmcd_dp0 {}\n", snap.dp0));
    out.push_str("# HELP wdpmcd_psu_up 1 if the power supply feed is live.\n");
    out.push_str("# TYPE wdpmcd_psu_up gauge\n");
    for psu in 1..=2u8 {
        out.push_str(&format!(
            "wdpmcd_psu_up{{psu=\"{psu}\"}} {}\n",
            u8::from(snap.psu_up(psu))
        ));
    }
    out.push_str("# HELP wdpmcd_drive_present 1 if a drive is present in the bay.\n");
    out.push_str("# TYPE wdpmcd_drive_present gauge\n");
    for slot in 0..bays {
        out.push_str(&format!(
            "wdpmcd_drive_present{{slot=\"{slot}\"}} {}\n",
            u8::from(snap.drive_present(slot))
        ));
    }
    out
}

/// Writes `content` to `dir/name` in one step as far as a reader is
/// concerned: the text goes to a temporary file beside the target, which is
/// renamed over it, so a scrape sees either the old file or the new one and
/// never half of either.
///
/// Nothing is flushed to disk. A scrape only needs the bytes to be visible,
/// and an fsync here would put the disk on the loop thread; the rename is
/// visible as soon as it returns. The temporary name does not end in
/// `.prom`, so the collector ignores it.
pub fn write_atomic(dir: &Path, name: &str, content: &str) -> io::Result<()> {
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut file: File = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o644)
            .open(&tmp)?;
        file.write_all(content.as_bytes())?;
        fs::rename(&tmp, dir.join(name))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Remembers which power supplies have been seen up, to spot a loss.
///
/// A supply that has never been up (this unit shows only one live) is never
/// reported as lost. A loss clears by itself when the supply returns.
#[derive(Debug, Default)]
pub struct PsuWatch {
    seen_up: [bool; 2],
    lost: bool,
}

impl PsuWatch {
    /// Digests one reading; an unreadable STA keeps the previous verdict.
    pub fn observe(&mut self, reading: &Result<Snapshot, String>) {
        let Ok(snap) = reading else {
            return;
        };
        let mut lost = false;
        for (index, seen) in self.seen_up.iter_mut().enumerate() {
            let up = snap.psu_up(index as u8 + 1);
            *seen |= up;
            lost |= *seen && !up;
        }
        self.lost = lost;
    }

    /// True while a supply that was up is down.
    pub fn lost(&self) -> bool {
        self.lost
    }
}

/// Remembers what was last reported so that only changes reach the log.
#[derive(Debug, Default)]
pub struct Tracker {
    last: Option<Snapshot>,
    started: bool,
    read_failing: bool,
}

impl Tracker {
    /// Digests one reading and returns the log lines it warrants, if any.
    ///
    /// The first successful reading yields one startup line; afterwards a line
    /// appears only when a register changes. A read failure is reported once
    /// when it begins and once when it clears.
    pub fn observe(
        &mut self,
        reading: &Result<Snapshot, String>,
        bays: u8,
    ) -> Vec<(Level, String)> {
        let mut events = Vec::new();
        match reading {
            Err(why) => {
                if !self.read_failing {
                    events.push((Level::Warning, format!("cannot read STA/DP0: {why}")));
                }
                self.read_failing = true;
            }
            Ok(now) => {
                if self.read_failing {
                    events.push((Level::Notice, "STA/DP0 readable again".to_owned()));
                    self.read_failing = false;
                }
                match self.last {
                    None if !self.started => {
                        events.push((Level::Info, format!("status: {}", now.describe(bays))));
                    }
                    None => {}
                    Some(before) if before != *now => {
                        events.extend(changes(before, *now, bays));
                    }
                    Some(_) => {}
                }
                self.started = true;
                self.last = Some(*now);
            }
        }
        events
    }
}

fn changes(before: Snapshot, now: Snapshot, bays: u8) -> Vec<(Level, String)> {
    let mut events = Vec::new();
    for psu in 1..=2u8 {
        if before.psu_up(psu) != now.psu_up(psu) {
            let (level, word) = if now.psu_up(psu) {
                (Level::Notice, "up")
            } else {
                (Level::Warning, "down")
            };
            events.push((level, format!("power supply {psu} is {word}")));
        }
    }
    for slot in 0..bays {
        if before.drive_present(slot) != now.drive_present(slot) {
            let word = if now.drive_present(slot) {
                "inserted"
            } else {
                "removed"
            };
            events.push((Level::Notice, format!("drive {word} in bay {slot}")));
        }
    }
    events.push((Level::Info, format!("status: {}", now.describe(bays))));
    events
}
