//! Instruction-count benchmarks (Callgrind via gungraun). Linux + valgrind only;
//! run through `fleet-perf run --suite instr`.

use std::hint::black_box;

use fleetd::Config;
use gungraun::{library_benchmark, library_benchmark_group, main};
use tempfile::TempDir;

/// Runs outside the measured region.
fn state_dir() -> TempDir {
    tempfile::tempdir().expect("create state dir")
}

#[library_benchmark]
#[bench::startup(setup = state_dir)]
fn fleetd_startup(dir: TempDir) {
    let cfg = Config {
        state_dir: dir.path().to_path_buf(),
    };
    let daemon = fleetd::startup(&cfg).expect("startup");
    drop(black_box(daemon));
}

library_benchmark_group!(name = startup; benchmarks = fleetd_startup);

main!(library_benchmark_groups = startup);
