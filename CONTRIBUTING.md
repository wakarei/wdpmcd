# Contributing

Bug reports and patches are welcome, particularly confirmation reports from
PR4100/EX4100 owners.

## Before sending a change

    cargo fmt --check
    cargo clippy --all-targets -- -D warnings

Changes are checked with `cargo fmt`, `clippy -D warnings` and a live run on
hardware. If you touch the fan-control path, say in the pull request what you
ran it against.

On FreeBSD also run `mandoc -Tlint -Wwarning man/*` when you change a manual
page and `shellcheck etc/rc.d/wdpmcd` when you change the rc script.

## Conventions

- Commits use [Conventional Commits](https://www.conventionalcommits.org/):
  `feat(pmc): ...`, `fix(daemon): ...`, `docs: ...`.
- No `unwrap`, `expect` or `panic!` on runtime paths; clippy denies them.
  There is no `unsafe` code: the crate is `#![forbid(unsafe_code)]`.
- The safety rules in the README are invariants. A change that lowers the
  floor, weakens a fail-high path, or lets `-q`/`-n` write is not acceptable.

## Protocol work

The PMC protocol is documented in the README. Please contribute protocol
facts from your own observation of the hardware, not by copying code from
GPL-licensed projects.
