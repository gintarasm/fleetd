use std::io::{self, Write};

use clap::Parser;
use fleet_perf::fake::FakeOutput;

/// Emit synthetic agent output: exactly `--bytes` bytes to stdout, then exit;
/// or, with `--forever`, until killed (or stdout closes).
#[derive(Parser)]
struct Args {
    #[arg(long, required_unless_present = "forever", conflicts_with = "forever")]
    bytes: Option<u64>,
    #[arg(long)]
    forever: bool,
    #[arg(long, default_value_t = 120)]
    line_len: usize,
    #[arg(long)]
    ansi: bool,
    #[arg(long, default_value_t = 0)]
    seed: u64,
}

fn main() -> io::Result<()> {
    let args = Args::parse();
    let mut gen_out = FakeOutput::new(args.seed, args.line_len, args.ansi);
    let mut stdout = io::stdout().lock();
    let mut buf = vec![0u8; 64 * 1024];
    let mut remaining = args.bytes.unwrap_or(u64::MAX);
    while remaining > 0 {
        let n = remaining.min(buf.len() as u64) as usize;
        gen_out.fill(&mut buf[..n]);
        stdout.write_all(&buf[..n])?;
        if !args.forever {
            remaining -= n as u64;
        }
    }
    stdout.flush()
}
