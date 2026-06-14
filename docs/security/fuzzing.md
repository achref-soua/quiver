# Fuzzing

Quiver fuzzes the two parsers that touch untrusted input — the **wire protocol**
(an attacker-supplied search filter) and the **on-disk format** (a corrupt or
hostile data file) — so malformed input is always rejected cleanly rather than
panicking, reading out of bounds, or hanging. This is the parser-robustness
verification the [threat model](./threat-model.md) calls for (tampering and
denial-of-service against the parse paths).

Targets live in [`fuzz/`](../../fuzz) and use `cargo-fuzz` / libFuzzer. The
`fuzz/` crate is its **own workspace**, so the nightly toolchain and the
libFuzzer dependencies never reach the stable workspace build or `cargo deny`.

## Targets

| Target | Parser under test | Property |
| --- | --- | --- |
| `filter_json` | `serde_json::from_slice::<quiver_query::Filter>` | a search filter parsed from arbitrary JSON bytes never panics |
| `page_decode` | `quiver_core::page::parse_page` | arbitrary bytes read as a 16 KiB page are rejected by the magic/version/type/CRC checks — never panic, never read out of bounds |
| `wal_decode` | `quiver_core::wal::read_all` | a torn or corrupt WAL file recovers to a point-in-time replay or a clean error, never panics |

## Running

Requires a nightly toolchain and `cargo-fuzz`:

```bash
rustup toolchain install nightly
cargo install cargo-fuzz

cargo +nightly fuzz build                                  # build all targets
cargo +nightly fuzz run filter_json -- -max_total_time=60  # fuzz one for 60s
just fuzz filter_json                                      # convenience wrapper (60s default)
```

A crash writes a reproducer to `fuzz/artifacts/<target>/`; replay it with
`cargo +nightly fuzz run <target> <artifact>`.

## Status

A bounded local pass (~25 s per target) on development hardware found **no
crashes**:

| Target | Runs | Coverage (features) | Result |
| --- | --- | --- | --- |
| `filter_json` | ~3.9M | 2039 | clean |
| `page_decode` | ~11.2M | 95 | clean |
| `wal_decode` | ~0.87M | 166 | clean |

These are smoke-level runs that wire the targets into the workflow and catch
obvious faults — not a long soak. The durable value is that the targets exist
and run clean, so a maintainer or CI can fuzz for longer (raise
`-max_total_time`, seed a corpus) on any change to a parser. The run counts are
host-dependent (exec/s scales with the machine) and are recorded as evidence the
targets run, not as a benchmark.
