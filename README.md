# wdpmcd

[![CI](https://github.com/wakarei/wdpmcd/actions/workflows/ci.yml/badge.svg)](https://github.com/wakarei/wdpmcd/actions/workflows/ci.yml)

A fan and front-panel LED daemon for the Western Digital My Cloud PR2100 and
PR4100 NAS enclosures, for FreeBSD and Linux.

The fan in these enclosures is run by a small power management controller
(PMC) that the CPU talks to over a serial port. Without a driver for it the
fan stays at whatever speed the controller last used. `wdpmcd` reads the CPU,
disk and controller temperatures, chooses a duty cycle from a fan curve and
writes it to the PMC. Every error path drives the fan *up*, never down.

- One thread, one `poll(2)` loop. No worker threads and no `unsafe` code.
- One small binary, no async runtime, three direct dependencies (`libc`, `nix`,
  `signal-hook`).
- Finds the PMC UART by PCI identity; no device name to configure.
- Never runs the fan below 30 %. Every fault runs it at 99 % and confirms it.
- Drives the power button LED too: blinking blue while starting and stopping,
  steady blue when up, red when something is wrong.
- A watchdog kills a wedged daemon; the supervisor restarts it and start-up
  sets the safe maximum again.

## Supported hardware

| Model            | Status                                             |
|------------------|----------------------------------------------------|
| My Cloud PR2100  | Verified: FreeBSD 15.1, protocol checked on hardware |
| My Cloud PR4100  | Untested; same PMC protocol, expected to work      |
| My Cloud EX4100  | Untested; same PMC protocol, expected to work      |

On the PR2100 the PMC is wired to the SoC's second high-speed UART
(PCI `8086:228c`, `00:1e.4`): `/dev/cuau3` on FreeBSD, `/dev/ttyS2` on
Linux, 9600 baud 8N1. If you try another model, `wdpmcd -q` is a safe first
step because it only reads.

## Install

### FreeBSD

    pkg install rust smartmontools
    cargo build --release
    install -m 755 target/release/wdpmcd /usr/local/sbin/
    install -m 755 etc/rc.d/wdpmcd /usr/local/etc/rc.d/
    install -m 644 etc/wdpmcd.conf.sample /usr/local/etc/wdpmcd.conf
    install -m 644 man/wdpmcd.8 /usr/local/share/man/man8/
    install -m 644 man/wdpmcd.conf.5 /usr/local/share/man/man5/
    echo 'coretemp_load="YES"' >> /boot/loader.conf   # CPU temperatures
    kldload coretemp
    sysrc wdpmcd_enable=YES
    wdpmcd -q          # check that the controller answers
    service wdpmcd start

A port skeleton lives in `port/sysutils/wdpmcd`.

### Debian

    apt install cargo smartmontools lm-sensors
    cargo build --release
    install -m 755 target/release/wdpmcd /usr/local/sbin/
    install -m 644 etc/wdpmcd.conf.sample /etc/wdpmcd.conf
    install -m 644 etc/systemd/wdpmcd.service /etc/systemd/system/
    echo coretemp > /etc/modules-load.d/coretemp.conf && modprobe coretemp
    wdpmcd -q
    systemctl enable --now wdpmcd

Edit the `disk` lines of the configuration to match your drives
(`/dev/ada0`, `/dev/ada1` on FreeBSD; `/dev/sda`, `/dev/sdb` on Linux).

## Usage

    wdpmcd [-d] [-f conf] [-p pidfile] [-D device]     run the daemon
    wdpmcd -q                                          print FAN, RPM, TMP; never writes
    wdpmcd -n                                          print sensors and chosen duty; never writes
    wdpmcd -s percent                                  set the fan once (30-99), read back
    wdpmcd -V

The daemon stays in the foreground and logs to syslog; the rc script and the
systemd unit take care of backgrounding. See `wdpmcd(8)` and `wdpmcd.conf(5)`.

The default curve (duty, CPU, disk, controller degrees Celsius; the hottest
sensor decides):

    45 %   50   40   45
    60 %   60   44   50
    80 %   70   48   55
    99 %   80   52   60

Increases are immediate. Decreases need every sensor 3 degrees below the
limit, are at most 15 points at a time and at least 30 s apart.

## Status monitoring and metrics

Every `status_interval` seconds (default 10) the daemon reads the PMC's `STA`
(power supply) and `DP0` (drive presence) registers. It logs one startup line
to syslog, then a line only when something changes (a power supply goes down
or comes back, a drive is removed or inserted, or the registers become
unreadable or readable again). It never logs an unchanged state.

If `metrics_dir` is set, each poll also rewrites a node_exporter textfile
collector file there (`wdpmcd.prom` unless `metrics_file` says otherwise). The
file is written to a temporary dot-file in the same directory and renamed
over the target, so a scraper never sees a partial file, and the loop never
waits on the disk for it. Point
node_exporter at the directory with `--collector.textfile.directory`.

| Metric | Labels | Meaning |
|--------|--------|---------|
| `wdpmcd_status_read_ok` | | 1 if the last read of STA and DP0 succeeded, else 0 (all other metrics are then omitted) |
| `wdpmcd_sta` | | raw `STA` register value (decimal) |
| `wdpmcd_dp0` | | raw `DP0` register value (decimal) |
| `wdpmcd_psu_up` | `psu="1"`, `psu="2"` | 1 if that power supply feed is live (STA bit 2 = PSU 1, bit 1 = PSU 2) |
| `wdpmcd_drive_present` | `slot="0"`..`"N-1"` | 1 if a drive is in the bay (DP0 bit N cleared = present) |

Configuration keys (see `wdpmcd.conf(5)`):

| Key | Default | Meaning |
|-----|---------|---------|
| `status_interval` | `10` | Seconds between polls; `0` turns status monitoring off; otherwise at least 0.5 |
| `metrics_dir` | unset | Directory for the textfile; unset means no file is written (syslog still happens) |
| `metrics_file` | `wdpmcd.prom` | File name inside `metrics_dir`; a plain name ending in `.prom` |
| `drive_bays` | `2` | Number of bays to report, 1-4 (2 for the PR2100, 4 for the PR4100) |

Status polling is read-only and independent of fan control. It is skipped
while the fan path is recovering from a fault, so the file may go stale
during a fail-safe; node_exporter's `node_textfile_mtime_seconds` shows that.
A failure to write the file is logged once, not on every poll.

## Front-panel LED

The same controller drives the power button LED, and `wdpmcd` keeps it in step
with what the daemon is doing. The defaults:

| State | Look | Configuration key |
|-------|------|-------------------|
| starting, no control cycle done yet | blinking blue | `led_starting` |
| healthy | steady blue | `led_up` |
| fan fail-safe in force, or over-temperature | blinking red | `led_fault` |
| network link down, power supply lost, UPS on battery, pool nearly full | steady red | `led_link`, `led_psu`, `led_ups`, `led_capacity` |
| clean stop | blinking blue | `led_shutdown` |

Blinking red means act now; steady red means degraded but still running. A
look is `COLOUR [steady|blink|pulse]`, with the colours `off`, `blue`, `red`,
`green` and `yellow`, so every one of the states above can be changed without
touching the code. Every colour works steadily or blinking; `pulse` asks for
blue, because the pulse register has one bit.

The order of the register writes matters on this button: a write to `PLS` after
the `LED` write puts the blue channel out. `wdpmcd` clears `PLS` before it
writes the colour, and only a pulse writes `PLS` at all, as its last write.
`docs/design.md` has the measurement.

Pool health and a UPS are opt-in because both need an external command:
`pool_cmd` maps a pool that is not `ONLINE` to `led_pool`, and `ups_cmd` maps a
UPS reporting `OB` or `ONBATT` to `led_ups`. Stock firmware leaves the power
LED alone for a degraded pool and says nothing, and `pool_cmd` follows that
default. `capacity_cmd`, which shows steady red above 95 % full, is on by
default. `led = no` removes every LED register access.

## Safety model

1. The fan is never commanded below 30 %. Out-of-range requests are refused
   before anything is written.
2. Every exchange with the PMC is bounded by a timeout. Silence, garbage, an
   over-long line, a malformed value or `ERR` is a fault.
3. External commands (smartctl, zpool, ifconfig, sysctl, the UPS query) run as
   child processes on non-blocking pipes, one of each at a time. A helper that
   hangs is killed at its deadline and reaped without blocking. It can make a
   reading go stale, which fails high, but it cannot delay a fan write.
4. A fault, a lost or implausible sensor, a reading older than three
   intervals, a fan setting that does not read back, or a fan slower than
   400 RPM all set the safe maximum (99 %) and hold it until the cause is gone.
5. On `SIGTERM`/`SIGINT` the safe maximum is set, retried and confirmed by
   read-back before exit.
6. A panic hook makes a last attempt to write the safe maximum. The rc script
   and the systemd unit run `wdpmcd -s 99` after the daemon stops as a second
   line of defence.
7. The serial port is locked exclusively; a second instance is refused.
8. `-q` and `-n` use a connection that refuses writes before they reach the
   wire.
9. The loop re-arms `alarm(watchdog)` on every iteration. A loop that stops
   making progress is ended by the kernel, and the service manager starts the
   daemon again, which puts the fan at the safe maximum before anything else.
10. LED work runs after the fan cycle, never inside it. A rejected write or a
   silent register is logged and retried; it cannot change a fan decision.

The watchdog is a last resort for a wedged process, not a way to recover from
a fault: faults are handled inside the loop, and only a hang reaches it. On
FreeBSD the rc script runs the daemon under `daemon(8) -r -R 5`; the systemd
unit uses `Restart=always` and `RestartSec=2`. `wdpmcd -s 99` also runs after
the daemon stops on either system.

The daemon is not a substitute for the PMC's own firmware protections; the
curve is a conservative local policy, not a manufacturer rating.

## Dependencies

| Crate  | Why |
|--------|-----|
| `nix`  | termios, `poll`, `alarm`, `waitpid` and `flock`, all without hand-written `unsafe` |
| `signal-hook` | the self-pipe that turns signals into loop events |
| `libc` | two `O_NOCTTY`/`O_NONBLOCK` bits and one `EINVAL` comparison |

There is no serde, no clap and no async runtime: the configuration is a
`key = value` file and the command line has eight options. wdpmcd is written
in 100% safe Rust (`#![forbid(unsafe_code)]`), and CI fails if the word
appears in `src/` outside a comment.

## The PMC protocol

CR-terminated ASCII, hexadecimal values. `FAN` is answered by `FAN=50`,
`RPM` by `RPM=100e`, `TMP` by `TMP=20` (degrees Celsius). `FAN=40` sets the
duty to 64 % and is answered by `ACK`; rejected commands are answered by
`ERR`. The controller may emit unsolicited `ALERT` lines, which are ignored.

## Troubleshooting

- **`no uart(4) device with PCI id 8086:228c`** (FreeBSD) – the machine is
  not a PR2100, or `uart` does not attach. Check `sysctl dev.uart`.
- **`no ttyS device at PCI 0000:00:1e.4`** (Linux) – check
  `ls -l /sys/class/tty/ttyS*/device`; the 8250 driver must be present.
  `device = /dev/ttyS2` in the configuration skips detection.
- **`port is locked by another process`** – the daemon (or another tool) has
  the port. Stop the service before running `-q` or `-s`.
- **`CPU temperature: ... coretemp`** – load `coretemp`, or point
  `cpu_temp_file` at a file holding millidegrees.
- **`FAIL-SAFE` in the log** – the message says which sensor or exchange
  failed. The fan stays at 99 % until it recovers.
- **Disks show `smartctl exited with status ...`** – install smartmontools
  and check the `disk` and `smartctl` settings.
- **The power LED never changes** – `led = no` in the configuration turns off
  every LED register access. A look the parser rejects, such as `red pulse`,
  stops the daemon at startup with the file name and line number.

## Acknowledgements

The PMC wire protocol was learnt from
[WDCommunity/wdnas-hwtools](https://github.com/WDCommunity/wdnas-hwtools)
(GPL-3.0). This project is a clean-room implementation: only protocol facts
were taken from it, no code, and the protocol behaviour was checked
independently against a PR2100.

## Licence

BSD 2-Clause, see `LICENSE`.
