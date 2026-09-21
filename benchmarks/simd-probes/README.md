# Exploratory SIMD probes

These are analysis artifacts for [`SIMD.md`](../../SIMD.md), not production implementations
or additions to the main workspace benchmark suite. They compare small kernels using hot
buffers, rotated sample order, and upper medians. They do not provide confidence intervals
or predict whole-stack performance.

From the repository root, test the portable probes before timing:

```sh
cargo test --manifest-path benchmarks/simd-probes/Cargo.toml
cargo run --release --manifest-path benchmarks/simd-probes/Cargo.toml --bin annexb_scan
cargo run --release --manifest-path benchmarks/simd-probes/Cargo.toml --bin receiver_loss
```

`annexb_scan` compares the current H.264 start-code loop with byte-search and substring-search
variants using pinned `memchr`. The synthetic cases include dense false matches as well as
mixed data and zero runs. The tests preserve the current scanner's handling of extra zeros
and nonzero start offsets.

`receiver_loss` compares a per-sequence bitmap walk with masked word popcounts. It preserves
the bitmap's cyclic reads, including ranges larger than the retained history. Integrating
it requires preserving `generate_report`'s exact endpoint semantics and testing full reports.

The Ogg CRC probe is ARM64-only and is deliberately excluded from the portable Cargo targets:

```sh
rustc --edition=2024 --test -C opt-level=3 benchmarks/simd-probes/ogg_crc.rs -o /tmp/rtc-ogg-crc-test
/tmp/rtc-ogg-crc-test
rustc --edition=2024 -C opt-level=3 benchmarks/simd-probes/ogg_crc.rs -o /tmp/rtc-ogg-crc-bench
/tmp/rtc-ogg-crc-bench
```

It compares the existing table recurrence with scalar and NEON bit reversal around ARM
hardware CRC instructions. Accelerated execution is skipped when CRC support is absent.

Record the toolchain, target, effective flags, input distribution, and machine for any
comparison. The published exploratory figures came from direct `rustc -C opt-level=3`
builds on ARM64; the Cargo entry points make the probes easier to rerun but do not imply
identical code generation under different flags or toolchains.
