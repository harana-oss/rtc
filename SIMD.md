# SIMD opportunities

Analysis of the working tree on 2026-09-21. The strongest candidates for additional SIMD are
H.264 start-code scanning and audio layout conversion. FlexFEC payload XOR already
auto-vectorizes in an isolated compiler probe, and much of the cryptographic work delegates
to accelerated dependencies.

A second pass identified Ogg page checksums as a strong hardware-acceleration candidate
for recording/playback, plus narrower opportunities in feedback decoding and circular
buffer processing. Ogg measurements below separate hardware CRC gains from SIMD gains.

## Evidence and limits

The review covered the packet-processing, media, crypto, checksum, and benchmark code.
Isolated loop probes were compiled with `rustc 1.99.0-nightly (1ed2df61a 2026-08-04)`, LLVM
22.1.8, optimization level 3, targeting `aarch64-apple-darwin`.

The probes establish code-generation behavior for those loop shapes, not end-to-end
speedups or the exact instructions in every application build. No x86-64 measurements or
whole-stack profiling were performed. Audio prototypes matched the existing functions for
every frame count from 0 through 2,048. No production code was changed as part of this analysis.

The second pass used the same compiler settings and target. It added isolated Ogg CRC
timings and correctness/code-generation probes for feedback, TWCC gap clearing, and NACK
bit scans. These are described below; none establishes a whole-stack speedup. The Ogg
probe is retained in [`benchmarks/simd-probes/ogg_crc.rs`](benchmarks/simd-probes/ogg_crc.rs).

The third pass measured H.264 scanner alternatives and receiver-report bitmap loss counts,
and inspected batched PCM conversion and replay-window shifts. Reproducible probes and
their standalone build instructions are collected in
[`benchmarks/simd-probes/README.md`](benchmarks/simd-probes/README.md).

## 1. H.264 start-code scanning

Relevant code:

- [`H264Payloader::next_ind`](rtc-rtp/src/codec/h264/mod.rs) examines each byte and maintains a
  zero counter.
- [`H26xReader::next_nal`](rtc-media/src/io/h26x_reader/mod.rs) reads, processes, and appends
  individual bytes within its buffered reader.
- [`HevcPayloader::parse`](rtc-rtp/src/codec/h265/mod.rs) already uses `memchr::memmem`.

Use `memchr` or `memmem` to locate candidate start codes, validate their surrounding bytes,
and copy spans in bulk. `rtc-rtp` already depends on `memchr`; the crate provides SIMD search
implementations for x86-64 and ARM64. See the [memchr documentation](https://docs.rs/memchr/).

Preserve the existing treatment of mixed three- and four-byte prefixes, longer zero runs,
empty units, and prefixes spanning reader buffers. The H.265 implementation is evidence of
an available dependency, not a drop-in replacement for H.264's boundary semantics.

This is the first implementation candidate for workloads doing video packetization or
reading Annex B streams. Benchmark both the scanner and complete packetization/reading.

### Follow-up: choose the search strategy carefully

The retained [Annex B probe](benchmarks/simd-probes/annexb_scan.rs) copies `next_ind`'s loop
with a slice input and compares two alternatives using `memchr 2.8.3`:

- Find each `0x01`, then check whether two or more zeros precede it.
- Find `00 00 01` with `memmem::find`, then walk backward over additional preceding zeros
  without crossing the caller's `start` offset.

Both matched the original on every input of lengths 0–9 over the alphabet `{0, 1, 7}` at
every valid start offset, plus long zero prefixes through 4,096 bytes. This validates
scanner boundary behavior; streaming reader state and full packetization remain untested
by this probe.

Selected local hot-buffer timings, in nanoseconds per scan:

| Input | Bytes | Original loop | Find `0x01` candidates | Find `00 00 01` |
|---|---:|---:|---:|---:|
| Mixed bytes, trailing four-byte prefix | 64 | 49.8 | 8.4 | 12.1 |
| Mixed bytes, trailing four-byte prefix | 1,200 | 951.9 | 68.1 | 48.5 |
| Mixed bytes, trailing four-byte prefix | 16,384 | 12,272.1 | 746.8 | 548.1 |
| All `0x01`, no prefix | 1,200 | 792.2 | 7,775.5 | 47.2 |
| All zero, no prefix | 1,200 | 398.2 | 16.5 | 47.1 |

The substring variant was about 20–22× faster on the larger mixed samples. Finding each
`0x01` was roughly 10× slower than the original on dense false matches. Prefer substring
search as the first production experiment, and include adversarial byte distributions
in its benchmarks. Finder reuse across searches within a frame is another experiment;
the measured variant used `memmem::find` per call.

These are synthetic, hot-buffer scanner timings with six rotated-order samples, upper
medians, non-inlined kernels, and `black_box`. They exclude packetization, copying, and I/O,
and do not establish the same gain on actual encoded frames or the complete sender.

## 2. Audio layout conversion

[`deinterleaved_by` and `interleaved_by`](rtc-media/src/audio/buffer/layout.rs) use runtime
strides and indexed nested loops. This makes vectorization harder when the channel count
is unknown at compile time.

The isolated `i16` probes produced:

| Loop shape | ARM64 output |
|---|---|
| Existing functions with runtime channel count | Scalar |
| Existing deinterleave function with constant stereo channel count | NEON `ld2` |
| Existing interleave function with constant stereo channel count | Scalar |
| Stereo deinterleave using separate output slices | NEON `ld2` |
| Stereo interleave using separate input slices | NEON `st2` |

Add mono and stereo fast paths, retaining the generic fallback. For stereo, split the planar
buffer into left/right slices and zip those slices with interleaved `chunks_exact(2)` or
`chunks_exact_mut(2)`. Safe Rust was sufficient to obtain vector instructions in the probes;
start there before introducing architecture-specific intrinsics. Evaluate four-channel
specialization if representative workloads justify it.

The split-slice prototypes matched the existing conversions for frame counts 0 through
2,048. A production change should also exercise supported sample types, unusual channel
counts, and byte-order conversion paths.

The benefit is application-dependent: the review found no production calls to these PCM
utilities from the main peer-connection code. Improving them will not automatically speed
up forwarding of encoded audio packets.

## 3. Reliability of ARM crypto acceleration

This is hardware crypto acceleration rather than general SIMD, but it may have greater
practical impact than adding new vector loops.

[`.cargo/config.toml`](.cargo/config.toml) enables `aes_armv8`, required to select the ARM
hardware backend in the pinned `aes 0.8.4` dependency. These flags affect RustCrypto-backed
AES-CTR, CCM, CBC, and SRTP key derivation. The GCM AEAD paths delegated to `ring` or
`aws-lc-rs` do not depend on this RustCrypto flag.

Two build situations need attention:

- An application consuming RTC from another project does not automatically inherit this
  repository's Cargo configuration. Cargo discovers configuration from its invocation
  directory and ancestors, rather than loading each dependency's configuration.
- Exported `RUSTFLAGS` or `CARGO_ENCODED_RUSTFLAGS` can replace the configured flags and
  silently disable the accelerated RustCrypto backend.

See [Cargo configuration](https://doc.rust-lang.org/cargo/reference/config.html) and the
existing [benchmarking notes](docs/benchmarking.md#rustflags-silently-changes-the-numbers).
Those notes report a substantial penalty when the flags disappear; those timings were not
remeasured during this review.

Add a benchmark built from an external consumer project and document its required
configuration. Longer term, evaluate a dependency version or backend that enables ARM
acceleration without consumer-provided cfg flags. Verify that behavior before migrating.

## 4. CBC decryption batching

[`AesCbc::decrypt_blocks`](rtc-crypto/src/common.rs) decrypts one AES block per call.
Decrypting separate ciphertext blocks is independent; the chaining XOR uses the previous
ciphertext block after decryption.

Process bounded batches through the AES backend's multi-block decryption API, retaining
the original ciphertext needed for chaining. This gives the backend an opportunity to
exploit parallel block processing and instruction-level parallelism. Preserve in-place
behavior, the IV for the first block, and chaining across batch boundaries.

CBC encryption has a dependency on the previous encrypted block, so the same approach
does not apply there. Prioritize this work only for workloads using CBC suites. The CTR
path already delegates bulk processing to the `ctr` crate.

## 5. FlexFEC XOR and surrounding work

Relevant code:

- [`FlexFec03Encoder::encode_one`](rtc-interceptor/src/flexfec/draft03/encoder.rs)
- [`FlexFec03Decoder` recovery code](rtc-interceptor/src/flexfec/draft03/decoder.rs)

The byte-wise zipped payload XOR loop already generated four 128-bit NEON XORs per main
iteration in the isolated probe, with remainder handling. Explicit intrinsics therefore
have no demonstrated advantage yet.

Both paths allocate and marshal packets around the XOR. Investigate reusable scratch
storage and serializing each protected packet once per encoding group before replacing
the loop. Preserve the recovery representation: it covers everything after the fixed RTP
header, including CSRCs and extensions, not just `packet.payload`.

Benchmark the XOR separately from complete FEC encoding and recovery. Exercise unequal
packet lengths, short inputs, vector-width boundaries, RTP extensions, and loss patterns.
Only pursue explicit SIMD if profiling and generated assembly show an additional benefit.

## 6. STUN fingerprint CRC

[`fingerprint_value`](rtc-stun/src/fingerprint.rs) uses a precomputed, table-based IEEE
CRC-32 implementation. Compare it against an accelerated IEEE CRC implementation such as
[`crc32fast`](https://docs.rs/crc32fast/latest/crc32fast/), which offers SIMD acceleration.

Typical STUN messages are small, so dispatch and setup costs may limit the gain. Keep this
below payload scanning and layout conversion unless ICE/STUN processing dominates the
workload. Retain the protocol's final fingerprint XOR.

SCTP already uses hardware-dispatched `crc32c` in
[`packet.rs`](rtc-sctp/src/packet.rs) and [`util.rs`](rtc-sctp/src/util.rs). Its Castagnoli
polynomial differs from STUN's IEEE polynomial; the checksum implementations are not
interchangeable. Hardware CRC instructions are also distinct from general SIMD.

## 7. Ogg page checksums: measured hardware-acceleration candidate

[`OggWriter::write_page`](rtc-media/src/io/ogg_writer/mod.rs) checksums every output page
with a byte-at-a-time table recurrence. [`OggReader::parse_next_page`](rtc-media/src/io/ogg_reader/mod.rs)
does the same over the header, segment table, and payload when checksum verification is
enabled. Each reader/writer also builds and stores its own 256-entry checksum table.

This scans the entire encoded payload, making it more substantial than small header
operations. It affects Ogg recording/playback, not ordinary RTP forwarding.

Ogg uses polynomial `0x04c11db7`, non-reflected processing, initial state zero, and no final
XOR. The checksum field is zeroed while calculating and stored little-endian afterward.
See [Xiph's framing specification](https://xiph.org/ogg/doc/framing.html). Neither SCTP's
CRC-32C nor STUN's CRC configuration is interchangeable with this algorithm. The normal
`crc32fast::hash` API is not a drop-in replacement either.

An ARM64 prototype uses IEEE hardware CRC instructions after reversing the bits of each
input byte, with the accumulator reversed at entry and exit. Two variants were evaluated:

- Scalar bit reversal over 8-byte chunks plus hardware CRC.
- NEON `vrbitq_u8` over 16-byte chunks, followed by two hardware CRC updates.

The second variant emitted `rbit.16b` and `crc32x`. Both matched the existing recurrence
for four initial states, all starting alignments modulo 16, lengths 0–256 and selected
larger sizes through 65,307 bytes. The NEON version also matched continuation across
split updates. This checks the transformation against the existing implementation;
production integration still needs page-level fixtures and corruption tests.

Exploratory results from the current ARM64 host, in nanoseconds per buffer:

| Bytes | Existing table recurrence | Scalar reversal + CRC | NEON reversal + CRC |
|---:|---:|---:|---:|
| 80 | 134.3 | 4.7 | 3.2 |
| 1,200 | 3,287.2 | 121.3 | 120.7 |
| 8,192 | 23,008.0 | 938.9 | 936.7 |
| 65,307 | 183,718.5 | 7,625.3 | 7,627.7 |

These are hot-buffer kernel timings, using six rotated-order samples and the upper
median, with non-inlined calls and `black_box`. They exclude dispatch, table construction,
page assembly, allocation, and I/O; they have no statistical confidence intervals. The
machine's CPU model could not be read under the sandbox, so treat them as local exploratory
evidence rather than a portable performance claim.

The measured 24–27× advantage for the larger inputs comes primarily from **hardware CRC**.
NEON and scalar bit reversal were essentially tied at those sizes. Prefer the simplest
accelerated variant that wins complete reader/writer benchmarks rather than adding SIMD
for its own sake. A portable fallback can also share a static table instead of building
one per instance; slice-by-N CRC is another baseline to compare before choosing a backend.

Reproduce on an ARM64 machine with CRC support:

```sh
rustc --edition=2024 --test -C opt-level=3 benchmarks/simd-probes/ogg_crc.rs -o /tmp/rtc-ogg-crc-test
/tmp/rtc-ogg-crc-test
rustc --edition=2024 -C opt-level=3 benchmarks/simd-probes/ogg_crc.rs -o /tmp/rtc-ogg-crc-bench
/tmp/rtc-ogg-crc-bench
```

The probe skips accelerated execution without CRC support. An implementation should
select its backend once, retain a portable fallback, and preserve incremental checksum
updates so the reader does not need to concatenate its buffers.

## 8. RFC 8888 feedback: target decoding, verify encoding first

[`CcFeedbackReportBlock`](rtc-rtcp/src/transport_feedbacks/cc_feedback_report/mod.rs)
encodes and decodes contiguous runs of two-byte metric words. Each carries a received
bit, two ECN bits, and a 13-bit arrival offset. Those fields can be processed independently
across packets, unlike TWCC's cumulative arrival deltas.

The detailed compiler probes narrow the opportunity:

| Probe | ARM64 result |
|---|---|
| Existing metric packing through `bytes::BufMut::put_u16` | Auto-vectorized |
| Packing into `chunks_exact_mut(2)` | Also auto-vectorized |
| Existing metric decoding through `bytes::Buf::get_u16` and `Vec::push` | Scalar metric loop |
| Decode into a preallocated output slice | Still scalar |
| Mask-based rewrite of lost-packet handling | Still scalar |

The probes reused the actual metric type and word conversion methods, with a slice-backed
`Buf` and the locally resolved `bytes` dependency. They model the inner loops, not the
complete generic parser. Do not claim a new packing speedup merely from replacing
`put_u16`: the compiler already performed vector work in that probe.

The remaining experiment is a bounded bulk decoder for contiguous input, with explicit
SIMD or a layout that permits vector stores. Keep the generic `Buf` fallback because a
buffer may be fragmented. Avoid unsafe casts to the Rust-layout metric struct; construct
valid `bool` and `Ecn` fields. The cost of converting vector results to that struct may
erase the arithmetic gain, so this remains an unmeasured candidate.

Preserve lost-packet normalization: when the received bit is clear, the remaining bits
decode as zero. The probe checked all 65,536 wire words, and short/tail cases at different
byte offsets. Keep report bounds, odd-count padding, and reserved arrival offsets intact.
Measure realistic MTU-sized feedback: the recorder divides its byte budget between streams,
so the format's 16,384-metric maximum is not representative of every report.

[`StreamLog::metrics_after`](rtc-interceptor/src/rfc8888/stream_log.rs) is a separate
constraint: it performs per-sequence hash lookups/removals and time conversion. A bounded
ring with contiguous arrival/ECN storage could expose bulk processing, but would be a larger
data-structure change. Profile it before optimizing only the wire codec, and preserve the
rule that the first missing packet stops advancement of the retained report window.

## 9. TWCC circular-buffer gaps and scans

[`PacketArrivalTimeMap::set_not_received`](rtc-interceptor/src/twcc/arrival_time_map.rs)
sets each missing entry to `-1` through a masked circular index. For a range fitting within
the buffer, split at the wrap into at most two contiguous slices and use `.fill(-1)`.

The isolated indexed loop remained scalar. The two-slice prototype lowered to `memset`
calls, exposing the operation to the platform's bulk-memory implementation. This removes
per-element circular indexing; it is not evidence that custom SIMD is needed or that the
particular `memset` implementation was measured.

Equivalence checks covered power-of-two capacities through 512, negative and positive
sequence numbers, wrapping ranges, empty ranges, and full-capacity clears. Production
code must establish the range-length invariant after resizing. Keep the existing path
that handles a jump beyond the retained window without clearing every skipped sequence.

`reallocate` similarly copies through per-sequence circular indices. A small number of
contiguous copies can handle the old/new wrap boundaries. `find_next_at_or_after` and
`remove_old_packets` could scan contiguous timestamp blocks using comparison masks, but
benchmark this only for loss/reordering patterns with long gaps; normal short scans may
not repay dispatch/setup. Preserve sequence-order early termination when arrivals are
not time-ordered.

## 10. NACK bit scans: use whole words before SIMD

Two existing bitmaps still get scanned bit by bit:

- [`ReceiveLog::missing_seq_numbers`](rtc-interceptor/src/nack/receive_log.rs) tests every
  sequence number, despite storing receipt state in `Vec<u64>`.
- [`NackIterator::next`](rtc-rtcp/src/transport_feedbacks/transport_layer_nack/mod.rs)
  starts its bit-position search at zero on every call, despite already having a `u16`
  mask of the remaining losses.

For the receive log, mask the first/last words to the requested range, invert received
bits, skip zero words, and enumerate missing bits with `trailing_zeros` and `word &= word - 1`.
For the wire NACK iterator, use the same bit-scan operation directly on the remaining mask.
Keep wrapping sequence arithmetic and ascending output order.

An isolated receive-log prototype matched the current function across all supported
window sizes (64–32,768), all-received/all-missing/mixed patterns, sequence wraparound, and
five skip settings per scenario. The NACK-pair bit scan matched for all 65,536 `u16` masks.
The ten existing receive-log tests also passed in the copied probe module.

These are scalar word-processing improvements, not new SIMD. Larger bitmap batches might
eventually justify vector comparisons to skip several full words at once, but first measure
the simpler approach. Clearing intermediate gaps and finding the last consecutive packet
can likewise use word masks; `add` already uses a full-buffer `.fill(0)` for jumps larger
than its window, so that case is already addressed.

The [NACK responder](rtc-interceptor/src/nack/responder.rs) separately enumerates all 16
positions in every `NackPair::lost_packets`. It does not currently use `NackIterator`.
Any iterator improvement should be wired into this path, or the retransmission handler
will retain its existing scan. Preserve the base packet, ascending sequence order, and
wrapping additions.

## 11. Receiver-report loss counts: vectorize the bitmap reduction

[`ReceiverStream::generate_report`](rtc-interceptor/src/report/receiver_stream.rs) walks
each sequence number and checks one bit to count losses. Its receipt history is already
a `Vec<u64>` with 128 words, covering 8,192 packets. Unlike NACK generation, it only needs
a count, so it can use a reduction rather than enumerate individual missing packets.

Mask partial first/last words, split at the circular-buffer boundary, and sum
`(!word).count_ones()` over the full-word slices. This uses safe Rust. The
[retained prototype](benchmarks/simd-probes/receiver_loss.rs) generated NEON `cnt.16b` and
vector reduction instructions; the per-bit reference remained scalar. Explicit SIMD
intrinsics were unnecessary.

Local isolated timings for a mixed 8,192-bit bitmap, in nanoseconds per query:

| Sequence positions examined | Per-bit walk | Masked word popcount |
|---:|---:|---:|
| 32 | 29.2 | 3.5 |
| 256 | 254.3 | 9.1 |
| 1,024 | 1,024.5 | 10.3 |
| 8,191 | 8,223.2 | 31.8 |

The kernel tests cover zero/full/mixed words, partial words, sequence wrap, empty ranges,
and ranges longer than the bitmap capacity. They passed for bitmap sizes from 64 through
32,768 bits and selected counts through 65,535. As with the other probes, timings use
hot buffers and upper medians of six rotated-order samples, without confidence intervals.
They exclude report allocation, serialization, jitter updates, and scheduling.

Preserve the existing endpoints when integrating: `generate_report` counts from
`last_report_seq_num + 1` up to, but excluding, `last_seq_num`, and uses a separate wrapping
distance as the fraction-lost denominator. A kernel replacement must not silently change
that behavior. Nor does reproducing cyclic reads resolve the separate question of reporting
intervals that exceed retained history; those need explicit semantics and full-report tests.

This path is present in the default interceptor configuration, but receiver reports are
generated once per second by default. The large kernel improvement does not translate to
the same per-packet gain. Measure complete report generation with realistic packet rates
and many streams before assigning it a whole-stack priority.

The adjacent `process_rtp` gap-clearing loop also clears one bit per skipped sequence.
Masked edge words plus bulk-cleared full words can reduce that work, but preserve the
relationship between clearing old history and recording the newly received packet when
a jump wraps the bitmap. This was not implemented or timed in the prototype.

## 12. PCM sample conversion: expose batches, preserve arithmetic

[`Sample<i16>` / `Sample<f32>` conversions](rtc-media/src/audio/sample.rs) are per-sample,
inline operations. A slice loop invoking the existing conversions already produced SIMD
in both directions in the third-pass compiler probe:

- `i16` to `f32`: vector widening, integer-to-float conversion, selection, and `fdiv.4s`.
- `f32` to `i16`: vector comparisons/selections, multiplication, conversion, and narrowing.

A bulk conversion API, or fusing conversion with the stereo layout paths, could make this
behavior easier for applications to obtain and avoid intermediate buffers. There is no
evidence here that handwritten conversion intrinsics beat the compiler-generated loop.
These PCM helpers still have the application-usage limitation described in section 2.

Do not replace the division with reciprocal multiplication as an exact optimization:
an exhaustive local probe of all 65,536 `i16` inputs found **768 differing `f32` bit patterns**.
The current normalization also deliberately uses 32,768 for negative values and 32,767
for nonnegative values. Uniform scaling would change its endpoint behavior.

Any bulk API should preserve clamping, NaN behavior, float-to-integer conversion semantics,
tails, and exactness expectations. Benchmark it independently from allocation and layout
conversion. Approximate arithmetic would require an explicitly different contract.

## Lower-priority areas

- Small nonce, header, and key-derivation XORs offer little bulk work per call.
- Packet state machines and congestion-control updates have branches and sequential
  dependencies; vectorizing individual updates is unlikely to be the first useful change.
- NACK bitmap scans are better candidates for whole-word masks and bit scans before
  introducing SIMD; see the concrete candidates above.
- TWCC status-vector chunks contain only 7 or 14 symbols. The recorder allocates/clones
  small vectors around them; compact storage and run-length handling are better first
  experiments than dispatching SIMD per chunk.
- SDP already uses `BufRead::read_until` and `read_line` for major delimiters. Its per-field
  allocation and seek behavior should be profiled before adding another scanner. DNS names
  and RTP extension headers also have short, variable-length, branch-heavy structure.
- The default 64-bit replay window is only one word. Larger windows merit separate
  investigation, but custom SIMD should not complicate the common one-word path. A third-pass
  probe of the current multi-word shift and a simpler reverse loop specialized for a one-bit
  shift generated scalar code for both. The specialized loop matched the original for full-word windows
  of 1–128 words over 130 successive shifts, but no SIMD or timing benefit was demonstrated.
- AV1 variable-length size fields and SCTP SACK handling have short/dependent parses or
  per-chunk state updates. No new bulk SIMD candidate was established there in this pass.

Prefer auto-vectorization and existing accelerated dependencies first. If explicit SIMD
is justified, retain scalar fallbacks and dispatch appropriately for the target CPU.
Avoid requiring nightly solely for this work: `std::simd` remains experimental according
to the [Rust documentation](https://doc.rust-lang.org/std/simd/struct.Simd.html).

## Benchmark and implementation sequence

1. Add representative H.264 scanner and full packetization/reader benchmarks, then evaluate
   SIMD-backed search with unchanged boundary semantics.
2. Expand audio benchmarks to include mono/stereo, realistic frame counts, and relevant
   sample types. Compare allocation-free kernels and complete buffer conversions.
3. Verify crypto acceleration from an external consumer build and with the supported
   providers. Benchmark CBC batching separately if CBC is used.
4. Add FlexFEC encode/recovery benchmarks to determine whether allocation, serialization,
   or XOR dominates before choosing an optimization.
5. Evaluate STUN CRC alternatives on realistic message sizes.
6. For Ogg recording/playback workloads, prioritize a portable bulk-checksum API and
   hardware CRC backend; compare whole-page operations using both accelerated probes.
7. Add realistic RFC 8888 decode and TWCC loss/reordering benchmarks. Verify the inner
   loops' share of complete feedback processing before changing data layouts.
8. Measure word-wise NACK scans against the current bitmap walk before considering wider
   vector scans.
9. Replace receiver-report per-bit loss counting with a word reduction as a measured
   experiment, then benchmark full report generation and many-stream workloads.
10. If applications use PCM conversion heavily, expose slice conversion and fused layout
    paths while retaining the existing arithmetic semantics.

The current [audio benchmark](rtc-media/benches/bench.rs) uses only four-channel,
100,000-frame `i32` buffers. Its two conversion labels are reversed relative to the
operations performed. The [RTP benchmark](rtc-rtp/benches/bench.rs) measures packet
marshalling rather than H.264 scanning.

Compare changes on the same machine with identical toolchains, optimization settings,
CPU targeting, and effective rustflags. Measure ARM64 and x86-64 independently. Inspect
release assembly, validate correctness outside timed regions, and use the existing
[benchmark workflow](docs/benchmarking.md) to connect kernel results with whole-operation
and end-to-end costs.
