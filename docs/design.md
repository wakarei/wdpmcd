# Design notes

Why the code behaves the way it does. The goal for 0.2 was to match what the
stock WD firmware does and to add nothing it didn't.

## One thread, one loop

Everything waits in a single `poll(2)` call with a computed timeout: the
serial port, the signal pipe, the helper output pipes and a small set of
deadlines. A fan cycle is a state machine spread over loop iterations, so a
controller that answers slowly costs latency in that cycle and nothing else.

External commands are child processes, at most one of each kind, killed at
their deadline and reaped without blocking. Their readings live in a cache
with a timestamp; a reading that is old fails high, which is the same rule the
daemon has always used for a silent disk.

`alarm(watchdog)` is re-armed every iteration. It only catches a loop that
cannot make progress at all (a wedged filesystem call, a bug), and its default
action, death by `SIGALRM`, is exactly what the service manager needs to start
over with the fan at the safe maximum.

## Power LED: what the button can show

WD documents the intended meaning of each state (knowledge base article 17203
and the PR2100 user manual), but not what the registers do to the button. The
map came from writing registers and looking at the power button:

| written, in order | read back (`LED` / `BLK` / `PLS`) | seen |
|---|---|---|
| `BLK=00 PLS=00 LED=01` | 01 / 00 / 00 | blue |
| `BLK=00 PLS=00 LED=01 PLS=00` | 01 / 00 / 00 | dark |
| `BLK=00 PLS=00 LED=01 BLK=00` | 01 / 00 / 00 | blue |
| `BLK=00 PLS=00 LED=02 BLK=02` | 02 / 02 / 00 | red, blinking |
| `LED=02`, `LED=04`, `LED=1F` | unchanged | red, green, yellow |
| `PLS=01` with `LED=00`, `BLK=00` | 00 / 00 / 01 | blue, breathing |

Bit 0 of `LED` is blue, bit 1 red, bit 2 green, and both lit read as yellow. A
blink sets its colour in `LED` as well as in `BLK`, or the lamp is dark between
blinks.

The second row is the rule that shapes the code: a write to `PLS` after the
`LED` write switches the blue channel off while the registers keep their
values. A look that is not the pulse therefore clears `PLS` first and writes
`LED` last, and the pulse is the only look that writes `PLS`, as its closing
write. Writes to `BLK` after `LED` do no harm.

The looks are then: blinking blue while starting and while stopping, steady
blue when up, blinking red when the device is at fault, steady red when
something needs attention (cable unplugged, power supply failure, UPS on
battery, volume above 95%). `docs/led-state-machine.md` has the full table and
the register values.

Blinking means "act now" and steady means "degraded but running". The only
blinking-red causes are a fan fail-safe and over-temperature, which is how the
stock firmware uses it too. A look may use `off`, `red`, `green`, `yellow` or
`blue`, steadily or blinking. `pulse` belongs to blue alone, because `PLS` has
one bit, so `red pulse` is rejected with the file line.

## A degraded pool does not touch the power LED

Stock firmware leaves the power LED on its up look when a volume is degraded
and reports it on the drive LEDs instead. The PMC has no command for the drive
LEDs, so wdpmcd cannot do that. `pool_cmd` lets you map pool trouble to the
power LED if you would rather have some signal than none; it is off by
default.

A pool above 95% full is different: stock shows steady red for that, so
`capacity_cmd` is on by default and only fires when a pool is nearly full.

## Power supply loss

The STA register on a PR2100 with one supply connected reads as one live
feed, permanently. Reporting that as a failure would leave the LED red on
every such unit, so only a supply that was seen up and then dropped counts as
lost. It clears when the supply comes back.

## Over-temperature

The condition latches at the top step of the fan curve for any of the CPU,
disk and controller sensors, and clears `hysteresis` degrees below it so the
LED does not flicker around the threshold. There is no upper bound: the LED
keeps blinking however far above the limit the sensor is. The stock thermal
warnings (0005, 0006) are covered by the same state.

## USB copy button

Left alone. On stock firmware the button only creates a backup job that the
user then starts from the dashboard, and the daemon would need to watch for
`ALERT` lines and the interrupt status register to see it at all. That is a
lot of new code next to the fan loop for little gain, so wdpmcd does not
touch the USB LED, the ISR or alert traffic. Power supply and drive presence
are read by the status poller instead, which is read-only.

## The LED never influences the fan

LED code runs after the fan cycle. A rejected write, a silent register or a
mismatched read-back is logged and retried; none of it feeds into a fan
decision. On exit the fan is confirmed at the safe maximum first and the LED
is written once afterwards, best effort. With `led = no` the daemon makes no
LED, BLK or PLS access at all.
