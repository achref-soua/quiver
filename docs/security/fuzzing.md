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

The `v1.0.0` pass ran each target for **900 s** (15 minutes) on the reference
hardware — a 12th Gen Intel Core i7-12700H (10 physical / 20 logical cores),
15 GiB RAM, under WSL2 — for **~342 million executions in total**, and found
**no crashes**:

| Target | Runs | Duration | Result |
| --- | --- | --- | --- |
| `filter_json` | 43,650,901 | 901 s | clean |
| `page_decode` | 279,437,122 | 901 s | clean |
| `wal_decode` | 18,887,581 | 901 s | clean |

`fuzz/artifacts/` is empty after the run: libFuzzer writes a reproducer there for
any crash, timeout or OOM, so an empty directory is the assertion, not the
absence of one.

Reproduce with `just fuzz <target> 900`. The exec counts are host-dependent
(exec/s scales with the machine and with input size, which is why `page_decode`
runs an order of magnitude hotter than `wal_decode`) and are recorded as evidence
of how much ground the run covered — not as a performance benchmark.

Earlier releases recorded a ~25 s smoke pass per target, which was enough to wire
the targets into the workflow and catch obvious faults. This one is a real soak.
It is still not a proof of absence: fuzzing shows the absence of the bugs it
found, and a longer run on a seeded corpus can always find more. The durable
value is that the targets exist and run clean, so a maintainer can fuzz for
longer on any change to a parser.
