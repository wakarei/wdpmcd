# FreeBSD port

`sysutils/wdpmcd` is a port skeleton for the FreeBSD ports tree.

`CARGO_CRATES` mirrors `Cargo.lock`; regenerate it with `make cargo-crates`
after changing dependencies. The two lines for the main distfile in `distinfo`
are placeholders until a `v0.3.0` tag exists on GitHub: run `make makesum` in
`sysutils/wdpmcd` once it does, which fills in the real hash and size.
