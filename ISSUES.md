# Memory and performance issues

Analysis date: 2026-09-21.

Reviewed revision: `0e5393a8208879b9e07461bb31da9456a664b4a7`.

Scope: the peer-connection packet pipeline, SRTP, SCTP, and interceptors. The
findings below combine source inspection with isolated release-build allocation
probes. They are not an end-to-end performance profile, and no throughput or
latency improvement has yet been measured. All issues are open.

## Priorities

Implement issues 1–4 first, then measure pipeline and routing changes before
committing to larger refactors.

| Issue | Opportunity | Evidence / expected benefit |
| --- | --- | --- |
| 1 | Avoid retaining state for rejected SRTP packets | Measured 1,551,752 bytes retained after 10,000 rejected packets with distinct SSRCs |
| 2 | Remove unused SCTP working buffer | Saves 64 KiB per started transport by default, up to 256 KiB at the configured maximum |
| 3 | Bound backlogs by bytes | Current 256-message receive limit permits 16 MiB of payload at the default message-size limit |
| 4 | Reuse RTP headers and encrypt owned buffers | Measured encryption allocations fall from three to one per packet with one RTP extension when using the existing header-aware API |
| 5 | Reuse pipeline scratch queues | Removes repeated temporary queue allocations; CPU benefit needs measurement |
| 6 | Cache negotiated routing metadata | Avoids repeated string allocation and transceiver scans, especially with many tracks |
| 7 | Bound retransmission history by bytes and age | Reduces retained payload storage; also bounds work when sequence numbers jump |

## 1. Prevent rejected SRTP packets from retaining stream state

- [ ] Open

### Finding

[SRTP decryption](rtc-srtp/src/context/srtp.rs) calls
`get_srtp_ssrc_state()` before packet authentication succeeds. The
[context state lookup](rtc-srtp/src/context/mod.rs) inserts a new map entry and
replay detector for an unknown SSRC. Authentication failure leaves that entry
allocated. [SRTCP decryption](rtc-srtp/src/context/srtcp.rs) follows the same
pattern.

The allocation probe sent 10,000 rejected RTP packets to a single AES-GCM context
with a 64-packet replay window:

| Input | Retained heap growth |
| --- | ---: |
| Repeated SSRC | 260 bytes |
| Distinct SSRC for every packet | 1,551,752 bytes, approximately 1.48 MiB |

These are live allocation bytes before dropping the context, not process RSS.
The SRTCP finding is based on source inspection; the probe measured RTP only.

### Proposed change

Authenticate unknown SSRCs using temporary state and insert persistent state only
after successful authentication. Keep existing replay and rollover handling for
known streams. Consider explicit admission limits for authenticated streams as a
separate resource policy.

Do not blindly evict authenticated SSRC state: recreating it can reset replay
protection or lose rollover information under the same keys.

### Validation

Repeat the allocation probe for RTP and RTCP and verify rejected, distinct SSRCs
do not produce memory growth proportional to packet count. Cover valid first
packets, authentication failure, duplicate packets, sequence wraparound, and
rollover handling.

## 2. Remove the unused SCTP working buffer

- [ ] Open

### Finding

[SCTP transport startup](src/peer_connection/transport/sctp/mod.rs) resizes
`internal_buffer` to the negotiated maximum message size. Production consumers in
the [SCTP handler](src/peer_connection/handler/sctp.rs) only read `.len()` to enforce
message-size limits. They no longer use the buffer as scratch storage: receive
reassembly goes directly into the delivered payload.

The allocation is normally 64 KiB per started transport and can reach 256 KiB,
depending on configuration and negotiation. At 10,000 transports with a 64 KiB
negotiated limit, this represents approximately 625 MiB of unnecessary allocated
storage. This saving applies to started SCTP transports, not every peer connection.

### Proposed change

Remove the vector and use the existing negotiated numeric limit for receive and
send checks. Preserve the current behavior before negotiation and retain all
message-size checks. Update comments and tests that still describe or manipulate
the working buffer.

### Validation

Verify negotiated message-size reporting and boundary enforcement for default,
smaller remote, maximum, and configured-unbounded sizes. Compare live heap before
and after starting many SCTP transports.

## 3. Bound application backlogs by bytes as well as message count

- [ ] Open

### Finding

The [SCTP handler](src/peer_connection/handler/sctp.rs) limits the downstream
receive backlog to 256 messages. At 64 KiB per message, that permits 16 MiB of
payload; at the maximum 256 KiB message size, it permits 64 MiB. These are
calculated payload ceilings, not measured typical usage, and exclude metadata
and SCTP's own reassembly buffers.

SCTP's receive-window limit does not cap bytes already transferred into the
[application data queue](src/peer_connection/handler/mod.rs). Backpressure exists,
but the application queue's count-based bound is comparatively large for big
messages.

Undrained media and outbound application queues also warrant explicit resource
policies. [Data-channel sends](src/data_channel/mod.rs) account for outstanding
bytes, but accounting alone is not a hard queue limit.

### Proposed change

Add a configurable byte budget alongside the message-count budget. Account for
bytes entering and leaving the application queue, including intermediate output
within the current pipeline traversal. Define whether one oversized message may
exceed the budget, or reject incompatible configurations, so a valid large
message cannot stall forever.

Preserve the pending-stream retry mechanism: SCTP readability is edge-triggered,
so stopping a drain without arranging a retry can deadlock a stream. Continue to
keep data-channel backpressure independent from media delivery.

For other queues, define policies appropriate to the data: reliable messages need
backpressure or explicit rejection; media needs a deliberate latency/drop policy.

### Validation

Extend the scenarios in
[data-channel backpressure tests](tests/data_channel_backpressure_rtc2rtc.rs).
Exercise mixed message sizes, stalled consumers, resumed consumers, multiple
streams, and simultaneous media. Measure peak retained bytes and verify eventual
delivery and forward progress.

## 4. Reuse parsed RTP headers and encrypt owned buffers in place

- [ ] Open

### Finding

The [SRTP write handler](src/peer_connection/handler/srtp.rs) serializes an RTP
packet, then calls `encrypt_rtp()`, which parses the header again even though the
original packet still has it. The
[existing context API](rtc-srtp/src/context/srtp.rs) already provides
`encrypt_rtp_with_header()`.

For a warmed AES-GCM context and a serialized 1,220-byte RTP packet containing one
header extension, the allocation probe measured:

| API | Allocation calls per encryption |
| --- | ---: |
| `encrypt_rtp()` | 3 |
| `encrypt_rtp_with_header()` | 1 |

The count excludes packet construction, serialization, and context setup. It is
not a count for the complete peer-connection path and does not establish a
throughput improvement.

The [AES-GCM cipher](rtc-srtp/src/cipher/cipher_aead_aes_gcm.rs) also allocates a
new buffer and copies the entire serialized packet before encrypting its payload
in place. The [AES-CM cipher](rtc-srtp/src/cipher/cipher_aes_cm_hmac_sha1.rs) has a
similar outgoing copy. On receive, the handler calls `decrypt_rtp()` and then
`Packet::unmarshal()`, parsing the RTP header twice.

### Proposed change

First, use `encrypt_rtp_with_header()` in the handler. Then evaluate owned-buffer
encryption/decryption APIs accepting `BytesMut`, with capacity reserved for the
authentication tag and any protocol trailer. Marshal directly into that buffer.
Consider returning or retaining the parsed receive header so packet construction
does not parse it again.

Preserve authentication, replay-state commit ordering, padding, extension parsing,
and error behavior. Do not expose unauthenticated plaintext on failure.

### Validation

Repeat allocation counts with no extensions, multiple extensions, audio-sized
packets, and MTU-sized packets. Run existing profile, authentication, replay, and
round-trip coverage. Use the [SRTP benchmarks](rtc-srtp/benches/README.md) and an
end-to-end packet benchmark to establish actual throughput and latency effects.

## 5. Reuse pipeline scratch queues and avoid redundant traversal

- [ ] Open

### Finding

[Peer-connection reads](src/peer_connection/handler/mod.rs) create a temporary
`VecDeque` and push the input packet into it on every call. Write and event
traversals also use temporary queues. The
[interceptor chain](rtc-interceptor/src/chain.rs) constructs a fresh queue from
each input packet and discards it after walking the chain.

Nonempty temporary queues allocate storage that is not reused across calls.
An empty `VecDeque::new()` alone does not allocate.

`RTCPeerConnection::poll_write()` also walks every handler before returning a
packet, even when its final output queue already contains buffered output.

### Proposed change

Retain reusable scratch queues per direction, or evaluate an inline representation
for the common single-packet case with support for generated output. Evaluate
draining final buffered output before performing another full handler traversal.

Preserve the ordering of generated packets, retransmissions, and control traffic.
An empty incoming queue does not mean no work exists: pacers and jitter buffers
can release packets after timeouts. Bound retained scratch capacity if rare large
bursts would otherwise permanently inflate per-connection memory.

### Validation

Measure allocations per packet and CPU cost for single-packet and burst workloads
with short and long interceptor chains. Verify timeout-driven output, generated
RTCP, retransmissions, ordering, and starvation behavior.

## 6. Cache negotiated routing metadata

- [ ] Open

### Finding

[RTP header-extension lookup](src/peer_connection/handler/interceptor.rs) builds
owned URI strings to find negotiated MID/RID/RRID extension IDs and copies
extension payloads into owned strings. This lookup is reached from per-packet
stream establishment, including checks for repair-stream pairing.

Stream establishment repeatedly scans transceivers and coding parameters. The
[endpoint's track lookup](src/peer_connection/handler/endpoint.rs) scans them again
to associate an SSRC with a track. Some setup metadata, such as MID, is cloned
before the already-established-stream early return.

### Proposed change

Cache negotiated extension IDs, borrow extension text where possible, and
maintain SSRC-to-stream/track indexes for established streams. Move setup-only
cloning after the established-stream check where practical.

Invalidate caches on renegotiation, stream replacement, stop, and changes to
repair-stream pairing. Preserve MID/RID fallback for previously unseen SSRCs.

### Validation

Benchmark packet processing as track and simulcast-layer counts increase. Cover
declared and undeclared SSRCs, RTX arriving before primary media, late RRID
pairing, renegotiation, and stopped or replaced tracks. CPU gains remain
unmeasured and are likely workload-dependent.

## 7. Bound retransmission history by bytes and age

- [ ] Open

### Finding

The [NACK responder](rtc-interceptor/src/nack/responder.rs) defaults to a
1,024-packet send buffer per NACK-enabled local stream. A full buffer of
1,200-byte payloads references approximately 1.17 MiB of payload storage, plus
packet metadata and header allocations.

RTP payloads use shared `Bytes`, so cloning a packet does not deep-copy its payload.
The issue is retained lifetime: retransmission history keeps the backing storage
alive. Shared payloads across streams must not be double-counted when estimating
process-wide memory.

The [send buffer](rtc-interceptor/src/nack/send_buffer.rs) also clears missing
sequence numbers one at a time. A forward jump larger than the buffer revisits
the same slots repeatedly. The
[receive log](rtc-interceptor/src/nack/receive_log.rs) uses a similar gap-clearing
loop for its bitmap.

### Proposed change

Add byte and age limits alongside the packet limit, sized to the desired
retransmission recovery window. Expire stale history even if a stream becomes
idle. Expose enough metrics to assess the tradeoff between memory and recoverable
loss.

For large sequence jumps, clear the relevant buffer once rather than iterating
over every skipped sequence number. Preserve serial-number wraparound and
out-of-order semantics.

### Validation

Measure retained heap for active and idle streams and confirm age-based expiry
releases storage. Test loss recovery within and outside the configured window,
large gaps, duplicate packets, out-of-order packets, and sequence wraparound.

## Measurement plan

Use release builds and compare on the same machine, crypto provider, negotiated
profile, and dependency versions. Existing crypto optimizations and shared RTP
payloads should remain intact.

Workloads:

- Steady audio and MTU-sized video traffic.
- Many peer connections, tracks, and simulcast layers.
- Packet loss, reordering, retransmissions, and large sequence gaps.
- Stalled and resumed data-channel consumers with mixed message sizes.
- Connection and stream churn, including idle periods after traffic stops.
- Rejected packets with repeated and distinct SSRCs.

Record allocations per packet, retained heap per connection, peak queue bytes,
throughput, and p99 packet-processing latency. Include loss-recovery and delivery
behavior so lower memory usage is not mistaken for an improvement when it merely
drops more useful traffic.

## Allocation probe reproduction

The original isolated probe was created outside the repository at
`/tmp/rtc-memory-audit`. Its source is included below because that temporary path
is not durable. It used the local crate sources, the default `ring` provider, and
an optimized release build. Exact byte counts can vary by target and dependency
version.

Create a temporary Cargo binary with edition `2024` and these dependencies,
replacing `/path/to/rtc` with the checkout's absolute path:

```toml
[dependencies]
rtc-srtp = { path = "/path/to/rtc/rtc-srtp" }
rtc-crypto = { path = "/path/to/rtc/rtc-crypto" }
rtc-rtp = { path = "/path/to/rtc/rtc-rtp" }
rtc-shared = { path = "/path/to/rtc/rtc-shared", default-features = false, features = ["marshal"] }
```

Copy the checkout's `Cargo.lock` into the temporary project to retain dependency
versions, then run `cargo run --offline --release --manifest-path
/path/to/probe/Cargo.toml` from the checkout. Offline execution requires cached
dependencies. Use the following as `src/main.rs`:

```rust
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

use rtc_shared::marshal::Unmarshal;
use rtc_srtp::{
    context::Context,
    option::srtp_replay_protection,
    protection_profile::ProtectionProfile,
};

struct Counting;
static CALLS: AtomicIsize = AtomicIsize::new(0);
static LIVE: AtomicIsize = AtomicIsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            CALLS.fetch_add(1, Ordering::Relaxed);
            LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn main() {
    encryption_allocations();
    rejected_packet_retention();
}

fn encryption_allocations() {
    let provider = rtc_crypto::default_provider().unwrap();
    let mut packet = vec![0u8; 1220];
    packet[0] = 0x90;
    packet[1] = 96;
    packet[3] = 1;
    packet[11] = 1;
    packet[12..20].copy_from_slice(&[0xbe, 0xde, 0, 1, 0x10, 0x61, 0, 0]);
    let header = rtc_rtp::Header::unmarshal(&mut packet.as_slice()).unwrap();

    for reuse_header in [false, true] {
        let mut ctx = Context::new(
            &[0; 16], &[0; 12], ProtectionProfile::AeadAes128Gcm,
            None, None, provider.crypto(),
        ).unwrap();
        drop(ctx.encrypt_rtp(&packet).unwrap());
        let baseline = CALLS.load(Ordering::Relaxed);
        for _ in 0..10_000 {
            if reuse_header {
                drop(ctx.encrypt_rtp_with_header(&packet, &header).unwrap());
            } else {
                drop(ctx.encrypt_rtp(&packet).unwrap());
            }
        }
        let allocations = CALLS.load(Ordering::Relaxed) - baseline;
        println!(
            "reuse_header={reuse_header}: allocations per packet={}",
            allocations / 10_000,
        );
    }
}

fn rejected_packet_retention() {
    let provider = rtc_crypto::default_provider().unwrap();
    for unique in [false, true] {
        let mut ctx = Context::new(
            &[0; 16], &[0; 12], ProtectionProfile::AeadAes128Gcm,
            Some(srtp_replay_protection(64)), None, provider.crypto(),
        ).unwrap();
        let baseline = LIVE.load(Ordering::Relaxed);
        let mut packet = [0u8; 28];
        packet[0] = 0x80;
        packet[1] = 96;
        packet[3] = 1;
        for i in 0..10_000u32 {
            let ssrc = if unique { i } else { 1 };
            packet[8..12].copy_from_slice(&ssrc.to_be_bytes());
            assert!(ctx.decrypt_rtp(&packet).is_err());
        }
        let growth = LIVE.load(Ordering::Relaxed) - baseline;
        drop(ctx);
        println!("unique_ssrcs={unique}: retained heap growth={growth} bytes");
    }
}
```

This is an allocation-counting fixture, not a cryptographic traffic generator or
a timing benchmark. It deliberately repeats the encryption input; the resulting
ciphertexts are immediately discarded and must not be used as real traffic.
The counting allocator uses the trait's default implementations for allocation
operations it does not override, so counts should be compared using the same
fixture rather than treated as a complete allocator performance profile.
