# Front-panel LED state machine

The colours and patterns below are defaults. Each one is a configuration
key, so changing a look needs no code change.

## Protocol facts

Register names, bit assignments and ranges come from the public WDCommunity
`wdnas-hwtools` documentation of the PMC protocol (GPL). Only those facts are
used; the implementation in `src/led.rs` is independent and no code was copied.

| Register | Meaning | Bits |
|---|---|---|
| `LED` | steady colour | 0 blue, 1 red, 2 green, both yellow; 3 USB red, 4 USB blue |
| `BLK` | blink mask | same layout as `LED`, five bits |
| `PLS` | breathing pulse | bit 0 only (blue) |

All values are hexadecimal; a write is answered by `ACK`. The USB bits (3, 4)
of `LED` and `BLK` are read first and written back unchanged, so USB-button
feedback from another module is never disturbed.

Blue is bit 0. It shows steadily or blinking like the other colours, and
`PLS=01` is the breathing pulse. Two facts fix the write order:

| written, in order | read back (LED/BLK/PLS) | seen |
|---|---|---|
| `BLK=00 PLS=00 LED=01` | 01 / 00 / 00 | blue |
| `BLK=00 PLS=00 LED=01 PLS=00` | 01 / 00 / 00 | dark |
| `BLK=00 PLS=00 LED=01 BLK=00` | 01 / 00 / 00 | blue |

A `PLS` write after the `LED` write drops the blue channel; `BLK` writes after
`LED` change nothing. So a look that is not the pulse writes `PLS=00` before
`BLK` and `LED`, with `LED` last, and the pulse writes `LED=00` and `BLK=00`
first and `PLS=01` last.

`docs/design.md` records the measurement behind all of this.

## States

| State | Meaning | Default look | LED / BLK / PLS | Config key |
|---|---|---|---|---|
| starting | daemon running, no control cycle done yet | blinking blue | 01 / 01 / 00 | `led_starting` |
| up | healthy | steady blue | 01 / 00 / 00 | `led_up` |
| fan fault | fan fail-safe in force, or the daemon gave up | blinking red | 02 / 02 / 00 | `led_fault` |
| over-temperature | hottest sensor at/above the top curve step | blinking red (`led_fault`) | 02 / 02 / 00 | `led_fault` |
| network link down | no carrier on `link_if` | steady red | 02 / 00 / 00 | `led_link` |
| power supply lost | a supply that was up is down | steady red | 02 / 00 / 00 | `led_psu` |
| UPS on battery | the configured `ups_cmd` reports OB/ONBATT | steady red | 02 / 00 / 00 | `led_ups` |
| pool nearly full | any pool above `capacity_limit` (95) percent | steady red | 02 / 00 / 00 | `led_capacity` |
| pool fault | a pool is not ONLINE (opt-in: `pool_cmd`) | steady red | 02 / 00 / 00 | `led_pool` |
| stopped | clean stop (SIGTERM/SIGINT) | blinking blue | 01 / 01 / 00 | `led_shutdown` |

A look is `COLOUR [steady|blink|pulse]`. The colours are `off`, `blue`, `red`,
`green` and `yellow` (red and green together, seen as yellow). Every colour
works steadily or blinking; `pulse` needs blue, since `PLS` has a single bit.
`red pulse` and `green pulse` are configuration errors with the file line and a
plain message. A file that asks for one is fatal at startup like any other bad
setting; on SIGHUP the running configuration stays in force.

## Stock WD power LED versus wdpmcd

Stock states are from WD knowledge-base article 17203. "Same" means the same
colour and pattern; every look is configurable.

| Stock state (code) | Stock LED | wdpmcd | Default | Notes |
|---|---|---|---|---|
| System up | steady blue | up | same | `led_up` |
| Starting up / rebooting | blinking blue | starting | same | `led_starting` |
| System shutting down | blinking blue | stopped (`led_shutdown`) | same | after the fan is confirmed at safe max |
| Fan not working | blinking red | fan fault | on, same | fan fail-safe flag; also when the daemon gives up |
| Over-temperature / thermal shutdown imminent (0005, 0006) | blinking red | over-temperature | on, same | any sensor at or above the top curve step (CPU 80, disk 52, controller 60 by default); stays blinking however far above, clears `hysteresis` below |
| Network cable unplugged | steady red | network link down | on, same | `link_if`; default `re0` on FreeBSD, off elsewhere until set |
| Power supply failure | steady red | power supply lost | on, same | only a supply that was seen up and dropped |
| Volume above 95% capacity (1001) | steady red | pool nearly full | on, same | `capacity_cmd` (default zpool), `capacity_limit` 95, `led_capacity` |
| UPS on battery (0031) | steady red | UPS on battery | OFF: needs `ups_cmd` | inert without a UPS command; `led_ups` |
| Volume degraded | stays on the up look | pool fault | OFF, same as stock | opt-in via `pool_cmd`; `led_pool` |
| Drive failure / SMART | drive LED, not the power LED | not implemented | n/a | drive LEDs have no PMC command |
| USB copy button / ALERT events | USB LED | not implemented | n/a | out of scope |

### Pool capacity

Every `pool_interval` seconds (the pool poll shared with the health check) the
daemon runs `capacity_cmd list -H -o capacity` and shows `led_capacity` while
any line is a percentage greater than `capacity_limit` (so exactly 95% is
fine). It is on by default because it only fires when a pool is nearly full.
Lines that are not a percentage (`-`) are ignored; a missing or failing
command keeps the previous verdict, is logged once, and is never a fault.
`capacity_cmd = none` turns it off.

### UPS

With `ups_cmd` set (a program and arguments, split on blanks, no shell) the
daemon runs it every `ups_interval` seconds (default 15) and treats the word
`OB` (NUT, e.g. `upsc myups ups.status` printing `OB DISCHRG`) or `ONBATT`
(apcupsd, `apcaccess -p STATUS`) as on battery. Without `ups_cmd` nothing
runs and the state never appears. A failing command keeps the previous verdict.

## Transitions

```
   (daemon start)
        |  clear BLK/PLS, show starting
        v
    STARTING --first cycle, no fault--> UP <---------------+
        |                                |                  |
        | fault                          | fault            | recovered
        v                                v                  |
   FAN FAULT / POOL FAULT <--------------+------------------+
        |
   SIGTERM/SIGINT, fan confirmed at safe max ---> STOPPED (exit 0)
   daemon gives up / fan not confirmed ---------> FAN FAULT (exit 1)
```

Priority when several apply: blinking red (fan fault, then over-temperature),
then steady red (network link, power supply, UPS, pool capacity, pool health), then starting, then up. Faults clear by themselves when the condition goes away.

Inputs:

* Fan fault is read from the fail-safe flag the control loop already keeps
  (`last_status`); the loop itself is untouched.
* Pool fault: every `pool_interval` seconds (default 60) the daemon runs
  `pool_cmd list -H -o health` (default `/sbin/zpool` on FreeBSD,
  `/usr/sbin/zpool` elsewhere; `none` disables) and treats any line other
  than `ONLINE` as a fault. The query runs as a helper process; a failed,
  missing or stuck query keeps the previous verdict and is logged once; it is
  never a fault by itself.

* Over-temperature: computed from the readings the fan cycle already takes
  (one assignment, no effect on the duty). It latches at the top step of the
  curve (any of CPU, disk, controller) and clears `hysteresis` degrees below.
* Network link: each LED update reads the carrier of `link_if` (FreeBSD
  through `ifconfig_cmd`, Linux `/sys/class/net/IF/carrier`).
  Unreadable state or a missing interface is never a fault. `link_carrier_file`
  (a file holding 0 or 1) replaces the lookup.
* Power supply: the STA poll remembers which supplies it has seen up. Only a
  supply that was seen up and then drops is a fault; one that was never up
  (this unit shows a single live supply) never is. An unreadable STA keeps the
  previous verdict. Needs `status_interval` greater than 0.

## Writing a state

For every change the daemon reads `LED` and `BLK` (keeping the USB bits), then
writes in the order the hardware needs. A look that is not the pulse writes
`PLS=00`, then `BLK` (which clears a stuck blink such as the boot firmware's
`BLK=01`), then `LED`; the pulse writes `LED=00`, `BLK=00`, then `PLS=01`. The
last two commands read `LED` and `BLK` back. Nothing writes `PLS` after `LED`,
which would put the blue channel out. A mismatch or error is logged and retried
after three intervals.

## Safety

* LED code runs after the fan cycle, never inside it; LED errors, rejected
  writes or a silent register cannot change a fan decision.
* On exit the fan is confirmed at the safe maximum first; the LED is written
  afterwards, once, best effort.
* `led = no` removes every LED/BLK/PLS access, which leaves the daemon
  behaving exactly as v0.1.1 did.
