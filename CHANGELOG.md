# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
uses [Semantic Versioning](https://semver.org/). A version gets a date when it
is tagged.

## [0.3.0] - unreleased

### Fixed
- The power LED stayed dark when the system was up, and blue never showed at
  all. The colour map is bit 0 blue, bit 1 red, bit 2 green, red and green
  together yellow; a blink sets its colour in `LED` as well as in `BLK`. Up is
  steady blue again, starting and stopping blink blue.
- A write to `PLS` after the `LED` write switched the blue channel off while
  the registers kept their values. Looks that are not the pulse now clear
  `PLS` before `BLK` and `LED`, with `LED` written last, and the pulse writes
  `LED=00`, `BLK=00` and then `PLS=01`.
- `red pulse` and `green pulse` are configuration errors with the file line,
  instead of a colour that never appears; the pulse register carries one bit.

### Changed
- The daemon is one thread running one `poll(2)` loop. Nothing in the fan path
  blocks: the serial port is non-blocking, every external command runs as a
  child process whose output is read from a non-blocking pipe, and every wait
  is a deadline.
- A helper that hangs is killed at its deadline and reaped with
  `waitpid(WNOHANG)`; no second command of the same kind is started until it
  has been reaped. A stuck disk can no longer hold up the loop.
- Signals reach the loop through a self-pipe instead of a handler that writes
  to the transport by hand.
- Syslog is written through an unconnected non-blocking datagram socket, so a
  syslogd that stops reading cannot stall the loop.
- The metrics file is written under a temporary name and renamed over the old
  one, without an fsync: a scrape never sees a partial file, and the loop never
  waits on the disk.
- LED register writes go through the same non-blocking engine as the rest of
  the protocol, spread over loop iterations instead of blocking on each one.
- On FreeBSD the CPU temperature comes from a bounded `sysctl` helper and the
  carrier state from a bounded `ifconfig`, so no `sysctl(3)` or `ioctl(2)`
  call sits on the loop thread. The helper asks `hw.ncpu` once and then names
  every `dev.cpu.N.temperature` OID, so `coretemp.tjmax` never counts as a
  temperature.
- The systemd unit restarts after 2 s instead of 5 s, so a watchdog kill is
  followed quickly by a start-up that sets the safe maximum.

### Added
- `SIGALRM` is an armed watchdog: the loop re-arms it on every iteration and
  its default action ends the process if the loop stops making progress. New
  `watchdog` setting, 15 to 600 seconds, default 60, at least
  `2 * interval + pmc_timeout`. The rc script and the systemd unit both
  restart the daemon after a death by signal.
- `syslog_socket` sets the datagram socket path (default `/var/run/log` on
  FreeBSD, `/dev/log` elsewhere); `WDPMCD_SYSLOG_SOCKET` does the same from the
  environment.
- `sysctl_cmd` and `ifconfig_cmd` name the helper programs used on FreeBSD for
  the CPU temperature and the interface status.

### Removed
- Worker threads and the `sysctlbyname(3)` wrapper: helpers are child
  processes now.
- All `unsafe` code, including the hand-written signal handler. The crate is
  compiled with `#![forbid(unsafe_code)]`.

## [0.2.1] - unreleased

### Fixed
- A hung smartctl or zpool could stall the fan loop. After the timeout the
  child was killed and then waited for, and a process stuck in the kernel on
  a failing disk does not die until it leaves it, so the wait could last
  forever with no fan writes and no fail-safe. A killed child that does not
  go away within 200 ms is now handed to a background thread.
- smartctl, zpool and the UPS command no longer run on the fan-loop thread.
  The loop reads the latest result and its age: a disk reading that gets too
  old fails high as before, a stuck pool or UPS query keeps the last verdict
  and is logged once. Only one query of each kind runs at a time.

### Changed
- The first pool, capacity and UPS verdicts arrive one poll after start
  instead of during the first LED update.

## [0.2.0] - unreleased

### Added
- Status monitoring: STA (power supplies) and DP0 (drive presence) are polled,
  changes are logged to syslog (plus one startup line) and an optional
  node_exporter textfile (`wdpmcd.prom`) is written atomically. New keys
  `status_interval`, `metrics_dir`, `metrics_file`, `drive_bays`.
- Front-panel power LED control (LED, BLK, PLS registers): blinking blue while
  starting, steady blue when up, blinking red on a fan fail-safe or
  over-temperature, steady red on link down or a lost power supply, blinking
  blue on a clean stop (stock WD firmware looks). A pool that is merely
  degraded does not touch the power LED (`pool_cmd`, opt-in). Clears the stuck
  BLK=01 blink. Looks are configurable; see docs/led-state-machine.md.
- Power LED: steady red when a pool is above `capacity_limit` (95%, on by
  default, `capacity_cmd`, `led_capacity`) and when a configured `ups_cmd`
  reports on battery (`led_ups`, inert by default); stock-vs-wdpmcd table in
  docs/led-state-machine.md.

## [0.1.1]

### Fixed
- Linux autodetect failed on kernels that insert serial-core port devices
  between the tty and the PCI function (Debian 13, kernel 6.12): discovery now
  walks up the canonical sysfs path to the first PCI address component.
- The signal handler now saves and restores errno around write(2).

## [0.1.0]

### Added
- Fan control daemon for the PR2100 power management controller on FreeBSD
  and Linux: temperature curve with hysteresis, 30 % duty floor, fail-high on
  any protocol or sensor fault, safe maximum with read-back on SIGTERM.
- Automatic discovery of the PMC UART (PCI 8086:228c) on both systems.
- `-q` (read-only probe), `-n` (dry run) and `-s` (one-shot set) modes.
- Manual pages `wdpmcd(8)` and `wdpmcd.conf(5)`, an `rc.d` script, a systemd
  unit and a FreeBSD port skeleton.
