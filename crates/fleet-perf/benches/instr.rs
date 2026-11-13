//! Instruction-count benchmarks (Callgrind via gungraun). Linux + valgrind only;
//! run through `fleet-perf run --suite instr`.
//!
//! Local runs in Docker (`rust:1`) need `--security-opt seccomp=unconfined`,
//! otherwise valgrind fails.

use std::hint::black_box;

use bytes::{Bytes, BytesMut};
use fleet_node::Terminal;
use fleet_perf::fake::FakeOutput;
use fleet_proto::{Frame, FrameCodec};
use fleetd::Config;
use gungraun::{library_benchmark, library_benchmark_group, main};
use tempfile::TempDir;
use tokio_util::codec::{Decoder, Encoder};

const KIB_64: usize = 64 * 1024;
const COLS: u16 = 120;
const ROWS: u16 = 40;

/// Runs outside the measured region.
fn state_dir() -> TempDir {
    tempfile::tempdir().expect("create state dir")
}

#[library_benchmark]
#[bench::startup(setup = state_dir)]
fn fleetd_startup(dir: TempDir) -> TempDir {
    let cfg = Config {
        state_dir: dir.path().to_path_buf(),
    };
    let daemon = fleetd::startup(&cfg).expect("startup");
    drop(black_box(daemon));
    // Returned so the directory is removed after measurement, not inside it.
    dir
}

library_benchmark_group!(name = startup; benchmarks = fleetd_startup);

/// Output frames decoded per iteration of `frame_decode_output_64x64k`.
const DECODE_FRAMES: usize = 64;

/// `DECODE_FRAMES` back-to-back encoded `Output` frames of 64 KiB each, and
/// room for the decoded frames. Runs outside the measured region.
fn output_frames_64x64k() -> (BytesMut, Vec<Frame>) {
    let data = Bytes::from(vec![b'x'; KIB_64]);
    let mut buf = BytesMut::new();
    for agent_id in 0..DECODE_FRAMES as u64 {
        FrameCodec::new()
            .encode(
                Frame::Output {
                    agent_id,
                    data: data.clone(),
                },
                &mut buf,
            )
            .expect("encode");
    }
    (buf, Vec::with_capacity(DECODE_FRAMES))
}

#[library_benchmark]
#[bench::output_64x64k(setup = output_frames_64x64k)]
fn frame_decode_output_64x64k((mut buf, mut frames): (BytesMut, Vec<Frame>)) -> Vec<Frame> {
    let mut codec = FrameCodec::new();
    while let Some(frame) = codec.decode(black_box(&mut buf)).expect("decode") {
        frames.push(frame);
    }
    assert_eq!(frames.len(), DECODE_FRAMES);
    // Returned so dropping happens after measurement.
    frames
}

/// 64 KiB of deterministic agent-like ANSI output, as read from a PTY (ONLCR
/// turns `\n` into `\r\n`).
fn ansi_64k() -> Vec<u8> {
    let mut raw = vec![0u8; KIB_64];
    FakeOutput::new(1, 120, true).fill(&mut raw);
    let mut out = Vec::with_capacity(KIB_64 + KIB_64 / 64);
    for &b in &raw {
        if b == b'\n' {
            out.push(b'\r');
        }
        out.push(b);
    }
    out.truncate(KIB_64);
    out
}

fn blank_terminal_and_ansi() -> (Terminal, Vec<u8>) {
    (Terminal::new(COLS, ROWS), ansi_64k())
}

#[library_benchmark]
#[bench::ansi_64k(setup = blank_terminal_and_ansi)]
fn terminal_feed_64k((mut term, bytes): (Terminal, Vec<u8>)) -> (Terminal, Vec<u8>) {
    term.feed(black_box(&bytes));
    (term, bytes)
}

/// A 120x40 screen full of agent-like ANSI output.
fn full_screen() -> Terminal {
    let mut term = Terminal::new(COLS, ROWS);
    term.feed(&ansi_64k());
    term
}

#[library_benchmark]
#[bench::full_screen(setup = full_screen)]
fn snapshot_120x40(term: Terminal) -> (Vec<u8>, Terminal) {
    let snapshot = black_box(&term).snapshot();
    (snapshot, term)
}

library_benchmark_group!(
    name = hot_paths;
    benchmarks = frame_decode_output_64x64k, terminal_feed_64k, snapshot_120x40
);

main!(library_benchmark_groups = startup, hot_paths);
