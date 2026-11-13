//! Instruction-count benchmarks (Callgrind via gungraun). Linux + valgrind only;
//! run through `fleet-perf run --suite instr`.
//!
//! Local runs in Docker (`rust:1`) need `--security-opt seccomp=unconfined`,
//! otherwise valgrind fails.

use std::hint::black_box;

use bytes::{Bytes, BytesMut};
use fleet_node::Terminal;
use fleet_node::history::History;
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

// 64 KiB of PTY-style output scrolls a 120x40 screen many times, so the history
// capture has lines to emit. Unlike terminal_feed_64k, this counts the drain.
#[library_benchmark]
#[bench::ansi_64k(setup = blank_terminal_and_ansi)]
fn terminal_feed_scroll_64k((mut term, bytes): (Terminal, Vec<u8>)) -> (Terminal, Vec<u8>) {
    term.feed(black_box(&bytes));
    let events = term.drain_history();
    assert!(!events.is_empty(), "scroll produced no history events");
    black_box(&events);
    // Returned so dropping happens after measurement.
    drop(events);
    (term, bytes)
}

const HISTORY_LINE_BYTES: usize = 100;
const PUSH_LINES: usize = 1_000;
const READ_PREFILL_LINES: usize = 100_000;

/// A deterministic rendered line of `HISTORY_LINE_BYTES` bytes.
fn history_line(i: usize) -> Vec<u8> {
    let mut line = format!("line {i:08} ").into_bytes();
    line.resize(HISTORY_LINE_BYTES, b'a' + (i % 26) as u8);
    line
}

/// Temp dir and the lines to push. Runs outside the measured region.
fn push_setup() -> (TempDir, Vec<Vec<u8>>) {
    (state_dir(), (0..PUSH_LINES).map(history_line).collect())
}

#[library_benchmark]
#[bench::lines_1k(setup = push_setup)]
fn history_push_1k_lines((dir, lines): (TempDir, Vec<Vec<u8>>)) -> (TempDir, Vec<Vec<u8>>) {
    let mut history = History::create(dir.path()).expect("create history");
    for line in black_box(&lines) {
        history.push(line);
    }
    assert_eq!(history.info().next_line, PUSH_LINES as u64);
    drop(black_box(history));
    (dir, lines)
}

/// A flushed 100 000-line history. Runs outside the measured region.
fn prefilled_history() -> (TempDir, History) {
    let dir = state_dir();
    let mut history = History::create(dir.path()).expect("create history");
    for i in 0..READ_PREFILL_LINES {
        history.push(&history_line(i));
    }
    history.flush().expect("flush history");
    let info = history.info();
    assert_eq!(info.first_line, 0, "prefill must not be evicted");
    assert_eq!(info.next_line, READ_PREFILL_LINES as u64);
    (dir, history)
}

#[library_benchmark]
#[bench::lines_100k(setup = prefilled_history)]
fn history_read_page((dir, history): (TempDir, History)) -> (TempDir, History) {
    let info = history.info();
    // 1 000 lines from the middle of history, which is well off the tail.
    let page = history
        .read_page(info.generation, 50_000, 1_000)
        .expect("read page");
    assert_eq!(page.lines.len(), 1_000);
    black_box(&page);
    drop(page);
    (dir, history)
}

library_benchmark_group!(
    name = history;
    benchmarks = terminal_feed_scroll_64k, history_push_1k_lines, history_read_page
);

main!(library_benchmark_groups = startup, hot_paths, history);
