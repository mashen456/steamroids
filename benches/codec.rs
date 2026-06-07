//! Benchmarks for the framing hot path — the CM codec and the GC envelope.
//!
//! These are the per-message paths a fleet hammers, so they're where allocations
//! and copies matter most.
//!
//! ```text
//! cargo bench --bench codec
//! ```

// Benchmarks don't need doc coverage; `criterion_group!` also emits an undocumented
// public fn that would otherwise trip the crate's `missing_docs` lint.
#![allow(missing_docs)]

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use prost::Message as _;

use steamroids::codec;
use steamroids::gc;
use steamroids::proto::gc::CMsgProtoBufHeader as GcHeader;
use steamroids::proto::CMsgProtoBufHeader;

// k_EMsgClientLogon — a representative client message type.
const EMSG: u32 = 5514;
// k_EMsgClientFromGC / a CS2 PlayersProfile GC msgtype, for the GC path.
const EMSG_FROM_GC: u32 = 5453;
const GC_MSGTYPE: u32 = 9128;

/// A realistic routing header (steamid + session + job id, as the driver stamps).
fn routing_header() -> CMsgProtoBufHeader {
    CMsgProtoBufHeader {
        steamid: Some(76_561_198_000_000_000),
        client_sessionid: Some(42),
        jobid_source: Some(1234),
        ..Default::default()
    }
}

/// A non-trivial body (~few hundred bytes) so copies/encoding are measurable.
fn body_bytes() -> Vec<u8> {
    CMsgProtoBufHeader {
        steamid: Some(76_561_198_000_000_000),
        client_sessionid: Some(7),
        target_job_name: Some("Service.Method#1".repeat(8)),
        error_message: Some("x".repeat(128)),
        ..Default::default()
    }
    .encode_to_vec()
}

fn bench_encode(c: &mut Criterion) {
    let header = routing_header();
    let body = body_bytes();

    let mut g = c.benchmark_group("codec/encode");
    g.throughput(Throughput::Bytes(body.len() as u64));
    g.bench_function("encode_raw", |b| {
        b.iter(|| codec::encode_raw(black_box(EMSG), black_box(&header), black_box(&body)));
    });
    // `encode` with a protobuf body (header + body straight into one buffer).
    let body_msg = routing_header();
    g.bench_function("encode", |b| {
        b.iter(|| codec::encode(black_box(EMSG), black_box(&header), black_box(&body_msg)));
    });
    g.finish();
}

fn bench_decode(c: &mut Criterion) {
    let frame = codec::encode_raw(EMSG, &routing_header(), &body_bytes());

    let mut g = c.benchmark_group("codec/decode");
    g.throughput(Throughput::Bytes(frame.len() as u64));
    g.bench_function("decode", |b| {
        b.iter(|| codec::decode(black_box(&frame)).unwrap());
    });
    g.bench_function("try_decode", |b| {
        b.iter(|| codec::try_decode(black_box(&frame)).unwrap());
    });
    g.finish();
}

fn bench_gc(c: &mut Criterion) {
    let gc_header = GcHeader::default();
    let body = body_bytes();

    let mut g = c.benchmark_group("gc/envelope");
    g.bench_function("wrap", |b| {
        b.iter(|| {
            gc::wrap(
                black_box(730),
                black_box(GC_MSGTYPE),
                black_box(&gc_header),
                black_box(&body),
            )
        });
    });

    // Build the `ClientFromGC` SteamMessage the way the driver would deliver it.
    let client = gc::wrap(730, GC_MSGTYPE, &gc_header, &body);
    let from_gc = codec::SteamMessage {
        emsg: EMSG_FROM_GC,
        header: CMsgProtoBufHeader::default(),
        body: client.encode_to_vec(),
    };
    g.bench_function("unwrap", |b| {
        b.iter(|| gc::unwrap(black_box(&from_gc)).unwrap());
    });
    g.finish();
}

criterion_group!(benches, bench_encode, bench_decode, bench_gc);
criterion_main!(benches);
