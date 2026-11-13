//! Instruction-count suite: runs the gungraun bench (`benches/instr.rs`) under
//! Callgrind and maps its `Ir` counts to results metrics. Linux + valgrind only.

use std::collections::BTreeMap;
use std::process::{Command, Stdio};

use crate::results::{Measurement, Results, Suite};

pub const STARTUP_INSTRUCTIONS: &str = "fleetd_startup_instructions";
pub const FRAME_DECODE_OUTPUT_64X64K_INSTRUCTIONS: &str = "frame_decode_output_64x64k_instructions";
pub const TERMINAL_FEED_64K_INSTRUCTIONS: &str = "terminal_feed_64k_instructions";
pub const SNAPSHOT_120X40_INSTRUCTIONS: &str = "snapshot_120x40_instructions";
pub const TERMINAL_FEED_SCROLL_64K_INSTRUCTIONS: &str = "terminal_feed_scroll_64k_instructions";
pub const HISTORY_PUSH_1K_LINES_INSTRUCTIONS: &str = "history_push_1k_lines_instructions";
pub const HISTORY_READ_PAGE_INSTRUCTIONS: &str = "history_read_page_instructions";
/// Bench function name -> metric name.
const METRICS: &[(&str, &str)] = &[
    ("fleetd_startup", STARTUP_INSTRUCTIONS),
    (
        "frame_decode_output_64x64k",
        FRAME_DECODE_OUTPUT_64X64K_INSTRUCTIONS,
    ),
    ("terminal_feed_64k", TERMINAL_FEED_64K_INSTRUCTIONS),
    ("snapshot_120x40", SNAPSHOT_120X40_INSTRUCTIONS),
    (
        "terminal_feed_scroll_64k",
        TERMINAL_FEED_SCROLL_64K_INSTRUCTIONS,
    ),
    ("history_push_1k_lines", HISTORY_PUSH_1K_LINES_INSTRUCTIONS),
    ("history_read_page", HISTORY_READ_PAGE_INSTRUCTIONS),
];
/// Every metric the instr suite emits.
pub const METRIC_NAMES: &[&str] = &[
    STARTUP_INSTRUCTIONS,
    FRAME_DECODE_OUTPUT_64X64K_INSTRUCTIONS,
    TERMINAL_FEED_64K_INSTRUCTIONS,
    SNAPSHOT_120X40_INSTRUCTIONS,
    TERMINAL_FEED_SCROLL_64K_INSTRUCTIONS,
    HISTORY_PUSH_1K_LINES_INSTRUCTIONS,
    HISTORY_READ_PAGE_INSTRUCTIONS,
];

/// Parse `--output-format=json` output (one summary object per line) into results.
pub fn parse_summaries(stdout: &str) -> Result<Results, String> {
    let mut found: BTreeMap<String, f64> = BTreeMap::new();
    for line in stdout.lines().filter(|l| l.trim_start().starts_with('{')) {
        let v: serde_json::Value =
            serde_json::from_str(line).map_err(|e| format!("bad gungraun summary: {e}"))?;
        let Some(func) = v["function_name"].as_str() else {
            continue;
        };
        let Some((_, metric)) = METRICS.iter().find(|(f, _)| *f == func) else {
            continue;
        };
        let ir = v["profiles"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|p| p["tool"] == "Callgrind")
            .and_then(|p| p["data"]["total"]["metrics"]["Ir"]["values"]["new"].as_f64())
            .ok_or_else(|| format!("no Callgrind Ir in summary for {func}"))?;
        if ir <= 0.0 {
            return Err(format!(
                "Callgrind Ir for {func} is {ir}: nothing was measured"
            ));
        }
        found.insert((*metric).to_string(), ir);
    }
    for (_, metric) in METRICS {
        if !found.contains_key(*metric) {
            return Err(format!("benchmark output has no result for {metric}"));
        }
    }
    Ok(Results {
        suite: Suite::Instr,
        metrics: found
            .into_iter()
            .map(|(k, value)| {
                (
                    k,
                    Measurement {
                        value,
                        unit: "instructions".into(),
                    },
                )
            })
            .collect(),
    })
}

pub fn run() -> Result<Results, String> {
    if !cfg!(target_os = "linux") {
        return Err("instr suite requires Linux + valgrind".into());
    }
    let out = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["bench", "-p", "fleet-perf", "--bench", "instr", "--"])
        .arg("--output-format=json")
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("spawning cargo bench: {e}"))?;
    if !out.status.success() {
        return Err(format!("cargo bench failed: {}", out.status));
    }
    parse_summaries(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = r#"{"function_name":"fleetd_startup","profiles":[{"data":{"total":{"metrics":{"Ir":{"values":{"new":7243}}}}},"tool":"Callgrind"}]}"#;

    /// `LINE` plus a summary for every other benchmark, so the output is complete.
    fn all_lines(startup: &str) -> String {
        let mut out = format!("noise\n{startup}\n");
        for (func, _) in &METRICS[1..] {
            out.push_str(&LINE.replace("fleetd_startup", func).replace("7243", "100"));
            out.push('\n');
        }
        out
    }

    #[test]
    fn maps_ir_to_metric() {
        let r = parse_summaries(&all_lines(LINE)).unwrap();
        assert_eq!(r.suite, Suite::Instr);
        let m = &r.metrics["fleetd_startup_instructions"];
        assert_eq!((m.value, m.unit.as_str()), (7243.0, "instructions"));
        let names: Vec<_> = r.metrics.keys().map(String::as_str).collect();
        let mut expected = METRIC_NAMES.to_vec();
        expected.sort_unstable();
        assert_eq!(names, expected);
    }

    #[test]
    fn zero_ir_is_an_error() {
        let zero = LINE.replace("7243", "0");
        assert!(parse_summaries(&all_lines(&zero)).is_err());
    }

    #[test]
    fn one_missing_benchmark_is_an_error() {
        assert!(parse_summaries(&format!("{LINE}\n")).is_err());
    }

    #[test]
    fn missing_benchmark_is_an_error() {
        assert!(parse_summaries("").is_err());
    }
}
