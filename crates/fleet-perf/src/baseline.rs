//! Baselines file model, the gate comparator, and comment-preserving writes.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, Item, Table, Value};

use crate::results::{Results, Suite};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tolerance {
    pub instr: f64,
    pub wall: f64,
}

impl Tolerance {
    pub fn for_suite(&self, suite: Suite) -> f64 {
        match suite {
            Suite::Wall => self.wall,
            Suite::Instr => self.instr,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gate {
    pub approvers: Vec<String>,
    pub tolerance: Tolerance,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricSpec {
    pub suite: Suite,
    pub unit: String,
    #[serde(default)]
    pub budget: Option<f64>,
    pub baseline: f64,
    /// Per-metric override of the gate's suite tolerance. Setting or
    /// raising it is a loosening that needs user approval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tolerance: Option<f64>,
    /// Printed and tightened, but a regression or budget breach does not
    /// fail the gate. Turning it on needs user approval; turning it off is free.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub report_only: bool,
}

impl Gate {
    /// Tolerance that applies to `spec`: its own override, else its suite's.
    pub fn tolerance_for(&self, spec: &MetricSpec) -> f64 {
        spec.tolerance
            .unwrap_or_else(|| self.tolerance.for_suite(spec.suite))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Baselines {
    pub gate: Gate,
    pub metrics: BTreeMap<String, MetricSpec>,
}

impl Baselines {
    /// Parses and validates: baselines, budgets and tolerances must be finite
    /// and non-negative (NaN would make every comparison silently pass).
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        let b: Self = toml::from_str(text)?;
        b.validate()
            .map_err(<toml::de::Error as serde::de::Error>::custom)?;
        Ok(b)
    }

    fn validate(&self) -> Result<(), String> {
        let ok = |v: f64| v.is_finite() && v >= 0.0;
        let t = self.gate.tolerance;
        if !ok(t.instr) || !ok(t.wall) {
            return Err("gate.tolerance values must be finite and >= 0".into());
        }
        for (name, m) in &self.metrics {
            if !ok(m.baseline) {
                return Err(format!("metrics.{name}.baseline must be finite and >= 0"));
            }
            if m.budget.is_some_and(|b| !ok(b)) {
                return Err(format!("metrics.{name}.budget must be finite and >= 0"));
            }
            if m.tolerance.is_some_and(|t| !ok(t)) {
                return Err(format!("metrics.{name}.tolerance must be finite and >= 0"));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Improved,
    Regressed,
    OverBudget,
    Missing,
    Unknown,
    Unrecorded,
    Invalid,
    UnitMismatch,
}

impl Status {
    pub fn is_failure(self) -> bool {
        !matches!(self, Status::Ok | Status::Improved)
    }

    fn label(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Improved => "improved",
            Status::Regressed => "REGRESSED",
            Status::OverBudget => "OVER BUDGET",
            Status::Missing => "MISSING",
            Status::Unknown => "UNKNOWN",
            Status::Unrecorded => "UNRECORDED",
            Status::Invalid => "INVALID",
            Status::UnitMismatch => "UNIT MISMATCH",
        }
    }

    fn hint(self) -> Option<&'static str> {
        match self {
            Status::Ok => None,
            Status::Improved => Some("run `fleet-perf tighten` to lock in the improvement"),
            Status::Regressed => Some(
                "regressed beyond tolerance: fix the regression, or ask the user to approve it \
                 (Perf-Approved trailer, updated baseline, docs/perf-log.md entry)",
            ),
            Status::OverBudget => {
                Some("value exceeds the hard budget: the budget cannot be raised by an agent")
            }
            Status::Missing => {
                Some("metric is in baselines but the results lack it: the suite did not measure it")
            }
            Status::Unknown => Some("metric has no baseline entry: run `fleet-perf baseline-add`"),
            Status::Unrecorded => Some("baseline not yet recorded: run `fleet-perf tighten`"),
            Status::Invalid => Some(
                "measurement produced an invalid value (non-finite, or <= 0; a count may be 0): \
                     fix the suite",
            ),
            // Per-row: the row's `detail` names both units.
            Status::UnitMismatch => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub name: String,
    pub baseline: f64,
    /// NaN when the metric is Missing.
    pub value: f64,
    pub delta_pct: f64,
    pub status: Status,
    /// Row-specific explanation printed under the table (e.g. both units of
    /// a unit mismatch).
    pub detail: Option<String>,
    pub report_only: bool,
}

impl Row {
    /// Whether this row fails the gate. Report-only metrics are exempt from
    /// regression failures only: they must still be measured, sane, in the
    /// right unit and within their budget (the budget is the hard limit).
    pub fn fails(&self) -> bool {
        self.status.is_failure() && !(self.report_only && self.status == Status::Regressed)
    }

    fn label(&self) -> String {
        if self.report_only {
            format!("{} (report-only)", self.status.label())
        } else {
            self.status.label().to_string()
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub rows: Vec<Row>,
    pub failed: bool,
}

fn delta_pct(baseline: f64, value: f64) -> f64 {
    if baseline > 0.0 {
        (value - baseline) / baseline * 100.0
    } else {
        0.0
    }
}

/// Unit of metrics that count occurrences (e.g. lagged subscriptions).
pub const COUNT_UNIT: &str = "count";

/// A measurement must be finite and positive; a count may also be 0 (none
/// happened), when both the results and the baseline say `count`.
fn valid_value(value: f64, unit: &str, baseline_unit: &str) -> bool {
    let zero_ok = unit == COUNT_UNIT && baseline_unit == COUNT_UNIT;
    value.is_finite() && (value > 0.0 || (zero_ok && value == 0.0))
}

pub fn compare(b: &Baselines, r: &Results) -> Report {
    let mut rows = Vec::new();

    for (name, spec) in b.metrics.iter().filter(|(_, s)| s.suite == r.suite) {
        let tol = b.gate.tolerance_for(spec);
        let Some(m) = r.metrics.get(name) else {
            rows.push(Row {
                name: name.clone(),
                baseline: spec.baseline,
                value: f64::NAN,
                delta_pct: 0.0,
                status: Status::Missing,
                detail: None,
                report_only: spec.report_only,
            });
            continue;
        };
        let value = m.value;
        let mut detail = None;
        let status = if !valid_value(value, &m.unit, &spec.unit) {
            Status::Invalid
        } else if m.unit != spec.unit {
            detail = Some(format!(
                "{name}: results unit `{}` differs from baseline unit `{}`",
                m.unit, spec.unit
            ));
            Status::UnitMismatch
        } else if spec.budget.is_some_and(|budget| value > budget) {
            Status::OverBudget
        } else if spec.baseline == 0.0 && spec.unit == COUNT_UNIT && spec.budget.is_some() {
            // A budgeted count with baseline 0 is gated by its budget alone
            // (checked above); 0 cannot be recorded as a count baseline.
            Status::Ok
        } else if spec.baseline == 0.0 {
            Status::Unrecorded
        } else if value > spec.baseline * (1.0 + tol) {
            Status::Regressed
        } else if value < spec.baseline * (1.0 - tol) {
            Status::Improved
        } else {
            Status::Ok
        };
        rows.push(Row {
            name: name.clone(),
            baseline: spec.baseline,
            value,
            delta_pct: delta_pct(spec.baseline, value),
            status,
            detail,
            report_only: spec.report_only,
        });
    }

    for (name, m) in &r.metrics {
        let known = b.metrics.get(name).is_some_and(|s| s.suite == r.suite);
        if !known {
            rows.push(Row {
                name: name.clone(),
                baseline: 0.0,
                value: m.value,
                delta_pct: 0.0,
                status: Status::Unknown,
                detail: None,
                report_only: false,
            });
        }
    }

    let failed = rows.iter().any(Row::fails);
    Report { rows, failed }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn num(v: f64) -> String {
            if v.is_nan() {
                "-".to_string()
            } else {
                format!("{v:.1}")
            }
        }
        let w = self
            .rows
            .iter()
            .map(|r| r.name.len())
            .max()
            .unwrap_or(0)
            .max(6);
        writeln!(
            f,
            "{:<w$}  {:>18}  {:>18}  {:>8}  status",
            "metric", "baseline", "value", "delta%"
        )?;
        for r in &self.rows {
            writeln!(
                f,
                "{:<w$}  {:>18}  {:>18}  {:>+8.2}  {}",
                r.name,
                num(r.baseline),
                num(r.value),
                r.delta_pct,
                r.label()
            )?;
        }
        let mut seen = Vec::new();
        for r in &self.rows {
            // A report-only regression is informational: no "fix it" hint.
            let exempt = r.status.is_failure() && !r.fails();
            if let Some(h) = r.status.hint()
                && !exempt
                && !seen.contains(&r.status.label())
            {
                seen.push(r.status.label());
                writeln!(f, "\n{}: {h}", r.status.label())?;
            }
        }
        for r in &self.rows {
            if let Some(d) = &r.detail {
                writeln!(f, "\n{}: {d}", r.label())?;
            }
        }
        writeln!(f, "\n{}", if self.failed { "FAIL" } else { "PASS" })
    }
}

/// Lower baselines only: records unrecorded (0) baselines and locks in
/// improvements beyond tolerance (`Status::Improved`). Never raises, and
/// leaves within-tolerance noise alone so baselines do not ratchet down on
/// lucky runs. Only metrics of the results' suite are touched.
/// Returns notes on baselines it left alone: a `count` metric is never
/// lowered to 0, since a baseline of 0 means "not yet recorded".
pub fn tighten(b: &mut Baselines, r: &Results) -> Vec<String> {
    let report = compare(b, r);
    let mut notes = Vec::new();
    for row in report.rows {
        if matches!(row.status, Status::Improved | Status::Unrecorded)
            && let Some(spec) = b.metrics.get_mut(&row.name)
        {
            if zero_count(&spec.unit, row.value) {
                notes.push(zero_count_note(&row.name, spec.baseline));
                continue;
            }
            spec.baseline = row.value;
        }
    }
    notes
}

fn zero_count(unit: &str, value: f64) -> bool {
    unit == COUNT_UNIT && value == 0.0
}

fn zero_count_note(name: &str, kept: f64) -> String {
    format!(
        "{name}: count of 0 not written as its baseline (0 means unrecorded); \
         baseline stays {kept}"
    )
}

/// Adds metrics present in the results but not in the baselines, with
/// `baseline = value` and no budget. Returns the names added, and notes on
/// those skipped: a `count` of 0 is not added (a baseline of 0 means "not
/// yet recorded"); add it by hand with a non-zero baseline.
pub fn baseline_add(b: &mut Baselines, r: &Results) -> (Vec<String>, Vec<String>) {
    let mut added = Vec::new();
    let mut notes = Vec::new();
    for (name, m) in &r.metrics {
        if !b.metrics.contains_key(name) {
            if zero_count(&m.unit, m.value) {
                notes.push(format!(
                    "{name}: count of 0 not added (0 means unrecorded); add it with a non-zero \
                     baseline"
                ));
                continue;
            }
            b.metrics.insert(
                name.clone(),
                MetricSpec {
                    suite: r.suite,
                    unit: m.unit.clone(),
                    budget: None,
                    baseline: m.value,
                    tolerance: None,
                    report_only: false,
                },
            );
            added.push(name.clone());
        }
    }
    (added, notes)
}

fn number(v: f64) -> Value {
    if v.fract() == 0.0 && v.abs() < 9_007_199_254_740_992.0 {
        Value::from(v as i64)
    } else {
        Value::from(v)
    }
}

/// Writes `b`'s baselines into the original TOML text, editing in place so
/// comments, ordering and formatting survive. New metrics are appended.
pub fn write_into(text: &str, b: &Baselines) -> Result<String, toml_edit::TomlError> {
    let mut doc: DocumentMut = text.parse()?;
    let metrics = doc
        .entry("metrics")
        .or_insert_with(|| {
            let mut t = Table::new();
            t.set_implicit(true);
            Item::Table(t)
        })
        .as_table_mut()
        .expect("`metrics` is a table");
    for (name, spec) in &b.metrics {
        match metrics.get_mut(name).and_then(Item::as_table_mut) {
            Some(t) => {
                let decor = t
                    .get("baseline")
                    .and_then(|i| i.as_value())
                    .map(|v| v.decor().clone());
                let mut v = number(spec.baseline);
                if let Some(d) = decor {
                    *v.decor_mut() = d;
                }
                t.insert("baseline", Item::Value(v));
            }
            None => {
                let mut t = Table::new();
                t.insert("suite", toml_edit::value(suite_str(spec.suite)));
                t.insert("unit", toml_edit::value(spec.unit.as_str()));
                if let Some(budget) = spec.budget {
                    t.insert("budget", Item::Value(number(budget)));
                }
                if let Some(tol) = spec.tolerance {
                    t.insert("tolerance", Item::Value(Value::from(tol)));
                }
                if spec.report_only {
                    t.insert("report_only", toml_edit::value(true));
                }
                t.insert("baseline", Item::Value(number(spec.baseline)));
                metrics.insert(name, Item::Table(t));
            }
        }
    }
    Ok(doc.to_string())
}

fn suite_str(s: Suite) -> &'static str {
    match s {
        Suite::Wall => "wall",
        Suite::Instr => "instr",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::results::Measurement;

    const TOML: &str = r#"
# top comment
[gate]
approvers = ["gintarasm"]
tolerance = { instr = 0.02, wall = 0.05 }

[metrics.m_wall]
suite = "wall"
unit = "ns"
budget = 1000
baseline = 100   # keep this note

[metrics.m_instr]
suite = "instr"
unit = "instructions"
baseline = 100
"#;

    fn base() -> Baselines {
        Baselines::parse(TOML).unwrap()
    }

    fn res(suite: Suite, items: &[(&str, f64)]) -> Results {
        let unit = match suite {
            Suite::Wall => "ns",
            Suite::Instr => "instructions",
        };
        Results {
            suite,
            metrics: items
                .iter()
                .map(|(n, v)| {
                    (
                        n.to_string(),
                        Measurement {
                            value: *v,
                            unit: unit.into(),
                        },
                    )
                })
                .collect(),
        }
    }

    fn status(b: &Baselines, r: &Results, name: &str) -> (Status, bool) {
        let rep = compare(b, r);
        let row = rep.rows.iter().find(|x| x.name == name).unwrap();
        (row.status, rep.failed)
    }

    #[test]
    fn parse_rejects_nan_inf_negative() {
        for (from, to) in [
            ("baseline = 100   #", "baseline = nan   #"),
            ("baseline = 100   #", "baseline = inf   #"),
            ("baseline = 100   #", "baseline = -1   #"),
            ("budget = 1000", "budget = nan"),
            ("budget = 1000", "budget = -5"),
            ("wall = 0.05", "wall = nan"),
            ("instr = 0.02", "instr = inf"),
            ("instr = 0.02", "instr = -0.02"),
        ] {
            assert!(TOML.contains(from), "{from}");
            let t = TOML.replace(from, to);
            assert!(Baselines::parse(&t).is_err(), "{to} accepted");
        }
    }

    #[test]
    fn per_metric_tolerance_overrides_suite_tolerance() {
        let t = TOML.replace("budget = 1000", "budget = 1000\ntolerance = 0.10");
        let b = Baselines::parse(&t).unwrap();
        assert_eq!(b.metrics["m_wall"].tolerance, Some(0.10));
        let r = res(Suite::Wall, &[("m_wall", 109.0)]);
        assert_eq!(status(&b, &r, "m_wall"), (Status::Ok, false));
        let r = res(Suite::Wall, &[("m_wall", 110.5)]);
        assert_eq!(status(&b, &r, "m_wall"), (Status::Regressed, true));
        // Without the override the suite's 5% applies.
        let r = res(Suite::Wall, &[("m_wall", 109.0)]);
        assert_eq!(status(&base(), &r, "m_wall"), (Status::Regressed, true));
    }

    #[test]
    fn parse_rejects_bad_per_metric_tolerance() {
        for v in ["nan", "inf", "-0.01"] {
            let t = TOML.replace("budget = 1000", &format!("budget = 1000\ntolerance = {v}"));
            assert!(Baselines::parse(&t).is_err(), "tolerance = {v} accepted");
        }
    }

    #[test]
    fn parse_rejects_unknown_fields() {
        for (from, to) in [
            ("budget = 1000", "budget = 1000\ntolerence = 0.1"),
            ("approvers = [", "approver = [\"x\"]\napprovers = ["),
            ("wall = 0.05 }", "wall = 0.05, wal = 0.5 }"),
        ] {
            assert!(TOML.contains(from), "{from}");
            assert!(Baselines::parse(&TOML.replace(from, to)).is_err(), "{to}");
        }
    }

    #[test]
    fn shipped_baselines_cover_exactly_the_emitted_metrics() {
        let text = include_str!("../../../bench/baselines.toml");
        let b = Baselines::parse(text).unwrap();
        let shipped: std::collections::BTreeSet<&str> =
            b.metrics.keys().map(String::as_str).collect();
        let emitted: std::collections::BTreeSet<&str> = crate::wall::METRIC_NAMES
            .iter()
            .chain(crate::instr::METRIC_NAMES)
            .copied()
            .collect();
        // Emitted, but with no baseline yet: the user records them (see
        // `bench/README.md`); a shipped baseline needs no edit here.
        const UNRECORDED: &[&str] = &[
            "exec_true_roundtrip_ns",
            "gui_diff_open_ns",
            "gui_diff_scroll_frame_ns",
        ];
        assert!(shipped.is_subset(&emitted), "{shipped:?} vs {emitted:?}");
        let missing: std::collections::BTreeSet<&str> =
            emitted.difference(&shipped).copied().collect();
        let unrecorded: std::collections::BTreeSet<&str> = UNRECORDED.iter().copied().collect();
        assert!(
            missing.is_subset(&unrecorded),
            "no baseline for {missing:?}"
        );
    }

    fn report_only() -> Baselines {
        let t = TOML.replace("budget = 1000", "budget = 1000\nreport_only = true");
        let b = Baselines::parse(&t).unwrap();
        assert!(b.metrics["m_wall"].report_only);
        b
    }

    #[test]
    fn report_only_regression_does_not_fail_but_over_budget_does() {
        let b = report_only();
        let r = res(Suite::Wall, &[("m_wall", 150.0)]);
        assert_eq!(status(&b, &r, "m_wall"), (Status::Regressed, false));
        let s = compare(&b, &r).to_string();
        assert!(
            s.contains("REGRESSED (report-only)") && s.contains("PASS"),
            "{s}"
        );
        let r = res(Suite::Wall, &[("m_wall", 1001.0)]);
        assert_eq!(status(&b, &r, "m_wall"), (Status::OverBudget, true));
        assert!(compare(&b, &r).failed);
        let r = res(Suite::Wall, &[("m_wall", 90.0)]);
        assert_eq!(status(&b, &r, "m_wall"), (Status::Improved, false));
    }

    #[test]
    fn report_only_still_fails_when_missing_unknown_invalid_or_mismatched() {
        let b = report_only();
        let r = res(Suite::Wall, &[]);
        assert_eq!(status(&b, &r, "m_wall"), (Status::Missing, true));
        let r = res(Suite::Wall, &[("m_wall", 0.0)]);
        assert_eq!(status(&b, &r, "m_wall"), (Status::Invalid, true));
        let mut r = res(Suite::Wall, &[("m_wall", 100.0)]);
        r.metrics.get_mut("m_wall").unwrap().unit = "ms".into();
        assert_eq!(status(&b, &r, "m_wall"), (Status::UnitMismatch, true));
    }

    /// `base()` plus a count metric `m_count` (baseline 5).
    fn with_count() -> Baselines {
        let mut b = base();
        let mut spec = b.metrics["m_wall"].clone();
        spec.unit = COUNT_UNIT.into();
        spec.budget = None;
        spec.baseline = 5.0;
        b.metrics.insert("m_count".into(), spec);
        b
    }

    fn count_res(value: f64) -> Results {
        let mut r = res(Suite::Wall, &[("m_wall", 100.0), ("m_count", value)]);
        r.metrics.get_mut("m_count").unwrap().unit = COUNT_UNIT.into();
        r
    }

    #[test]
    fn zero_is_a_valid_count() {
        let b = with_count();
        assert_eq!(
            status(&b, &count_res(0.0), "m_count"),
            (Status::Improved, false)
        );
        assert_eq!(status(&b, &count_res(5.0), "m_count"), (Status::Ok, false));
        assert_eq!(
            status(&b, &count_res(-1.0), "m_count"),
            (Status::Invalid, true)
        );
        assert_eq!(
            status(&b, &count_res(f64::NAN), "m_count"),
            (Status::Invalid, true)
        );
    }

    fn zero_baseline_count(budget: Option<f64>) -> Baselines {
        let mut b = with_count();
        let spec = b.metrics.get_mut("m_count").unwrap();
        spec.baseline = 0.0;
        spec.budget = budget;
        b
    }

    #[test]
    fn count_with_budget_and_zero_baseline_is_gated_by_budget() {
        let b = zero_baseline_count(Some(0.0));
        assert_eq!(status(&b, &count_res(0.0), "m_count"), (Status::Ok, false));
        assert_eq!(
            status(&b, &count_res(1.0), "m_count"),
            (Status::OverBudget, true)
        );
        // Within a non-zero budget it passes; beyond it fails.
        let b = zero_baseline_count(Some(2.0));
        assert_eq!(status(&b, &count_res(2.0), "m_count"), (Status::Ok, false));
        assert_eq!(
            status(&b, &count_res(3.0), "m_count"),
            (Status::OverBudget, true)
        );
    }

    #[test]
    fn count_without_budget_zero_baseline_is_still_unrecorded() {
        let b = zero_baseline_count(None);
        assert_eq!(
            status(&b, &count_res(0.0), "m_count"),
            (Status::Unrecorded, true)
        );
        assert_eq!(
            status(&b, &count_res(3.0), "m_count"),
            (Status::Unrecorded, true)
        );
    }

    #[test]
    fn tighten_never_writes_a_zero_count_baseline() {
        let mut b = with_count();
        let notes = tighten(&mut b, &count_res(0.0));
        assert_eq!(b.metrics["m_count"].baseline, 5.0);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].starts_with("m_count:"), "{notes:?}");
        // Still compares as an improvement, not unrecorded.
        assert_eq!(
            status(&b, &count_res(0.0), "m_count"),
            (Status::Improved, false)
        );
        // A non-zero improvement is tightened as before.
        assert!(tighten(&mut b, &count_res(2.0)).is_empty());
        assert_eq!(b.metrics["m_count"].baseline, 2.0);
    }

    #[test]
    fn baseline_add_skips_a_zero_count() {
        let mut b = base();
        let mut r = res(
            Suite::Wall,
            &[("zero_c", 0.0), ("one_c", 1.0), ("zero_ns", 0.0)],
        );
        for name in ["zero_c", "one_c"] {
            r.metrics.get_mut(name).unwrap().unit = COUNT_UNIT.into();
        }
        let (added, notes) = baseline_add(&mut b, &r);
        assert_eq!(added, vec!["one_c", "zero_ns"]);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].starts_with("zero_c:"), "{notes:?}");
        assert!(!b.metrics.contains_key("zero_c"));
        assert_eq!(b.metrics["one_c"].baseline, 1.0);
    }

    #[test]
    fn zero_is_invalid_for_other_units() {
        let b = with_count();
        let r = res(Suite::Wall, &[("m_wall", 0.0)]);
        assert_eq!(status(&b, &r, "m_wall"), (Status::Invalid, true));
        // A zero count reported against a non-count baseline is still invalid.
        let mut r = res(Suite::Wall, &[("m_wall", 0.0)]);
        r.metrics.get_mut("m_wall").unwrap().unit = COUNT_UNIT.into();
        assert_eq!(status(&b, &r, "m_wall"), (Status::Invalid, true));
    }

    #[test]
    fn report_only_is_tightened_like_any_metric() {
        let mut b = report_only();
        tighten(&mut b, &res(Suite::Wall, &[("m_wall", 90.0)]));
        assert_eq!(b.metrics["m_wall"].baseline, 90.0);
    }

    #[test]
    fn within_tolerance_ok() {
        let r = res(Suite::Wall, &[("m_wall", 104.9)]);
        assert_eq!(status(&base(), &r, "m_wall"), (Status::Ok, false));
    }

    #[test]
    fn regression_fails() {
        let r = res(Suite::Wall, &[("m_wall", 105.1)]);
        assert_eq!(status(&base(), &r, "m_wall"), (Status::Regressed, true));
    }

    #[test]
    fn instr_uses_2pct() {
        let r = res(Suite::Instr, &[("m_instr", 102.1)]);
        assert_eq!(status(&base(), &r, "m_instr"), (Status::Regressed, true));
        let r = res(Suite::Instr, &[("m_instr", 101.9)]);
        assert_eq!(status(&base(), &r, "m_instr"), (Status::Ok, false));
    }

    #[test]
    fn over_budget_fails_even_if_baseline_higher() {
        let mut b = base();
        b.metrics.get_mut("m_wall").unwrap().baseline = 5000.0;
        let r = res(Suite::Wall, &[("m_wall", 1001.0)]);
        assert_eq!(status(&b, &r, "m_wall"), (Status::OverBudget, true));
    }

    #[test]
    fn improvement_passes() {
        let r = res(Suite::Wall, &[("m_wall", 94.0)]);
        assert_eq!(status(&base(), &r, "m_wall"), (Status::Improved, false));
    }

    #[test]
    fn missing_metric_fails() {
        let r = res(Suite::Wall, &[]);
        assert_eq!(status(&base(), &r, "m_wall"), (Status::Missing, true));
    }

    #[test]
    fn unknown_metric_fails() {
        let r = res(Suite::Wall, &[("m_wall", 100.0), ("brand_new", 1.0)]);
        assert_eq!(status(&base(), &r, "brand_new"), (Status::Unknown, true));
    }

    #[test]
    fn unrecorded_baseline_fails() {
        let mut b = base();
        b.metrics.get_mut("m_wall").unwrap().baseline = 0.0;
        let r = res(Suite::Wall, &[("m_wall", 100.0)]);
        assert_eq!(status(&b, &r, "m_wall"), (Status::Unrecorded, true));
    }

    #[test]
    fn invalid_value_fails() {
        for v in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let r = res(Suite::Wall, &[("m_wall", v)]);
            assert_eq!(
                status(&base(), &r, "m_wall"),
                (Status::Invalid, true),
                "{v}"
            );
        }
        let s = compare(&base(), &res(Suite::Wall, &[("m_wall", 0.0)])).to_string();
        assert!(s.contains("measurement produced an invalid value"), "{s}");
    }

    #[test]
    fn unit_mismatch_fails() {
        let mut r = res(Suite::Wall, &[("m_wall", 100.0)]);
        r.metrics.get_mut("m_wall").unwrap().unit = "ms".into();
        assert_eq!(status(&base(), &r, "m_wall"), (Status::UnitMismatch, true));
        let s = compare(&base(), &r).to_string();
        assert!(s.contains("`ms`") && s.contains("`ns`"), "{s}");
    }

    #[test]
    fn other_suite_ignored() {
        let r = res(Suite::Wall, &[("m_wall", 100.0)]);
        let rep = compare(&base(), &r);
        assert!(!rep.failed);
        assert!(rep.rows.iter().all(|x| x.name != "m_instr"));
    }

    #[test]
    fn tighten_never_raises() {
        let mut b = base();
        tighten(&mut b, &res(Suite::Wall, &[("m_wall", 120.0)]));
        assert_eq!(b.metrics["m_wall"].baseline, 100.0);
        tighten(&mut b, &res(Suite::Wall, &[("m_wall", 104.0)]));
        assert_eq!(b.metrics["m_wall"].baseline, 100.0);
        tighten(&mut b, &res(Suite::Wall, &[("m_wall", 90.0)]));
        assert_eq!(b.metrics["m_wall"].baseline, 90.0);
    }

    #[test]
    fn tighten_ignores_within_tolerance_improvement() {
        let mut b = base();
        tighten(&mut b, &res(Suite::Wall, &[("m_wall", 96.0)]));
        assert_eq!(b.metrics["m_wall"].baseline, 100.0);
        tighten(&mut b, &res(Suite::Instr, &[("m_instr", 98.5)]));
        assert_eq!(b.metrics["m_instr"].baseline, 100.0);
    }

    #[test]
    fn tighten_skips_invalid_values() {
        let mut b = base();
        b.metrics.get_mut("m_wall").unwrap().baseline = 0.0;
        tighten(&mut b, &res(Suite::Wall, &[("m_wall", f64::NAN)]));
        assert_eq!(b.metrics["m_wall"].baseline, 0.0);
    }

    #[test]
    fn improved_hint_suggests_tighten() {
        let s = compare(&base(), &res(Suite::Wall, &[("m_wall", 90.0)])).to_string();
        assert!(
            s.contains("run `fleet-perf tighten` to lock in the improvement"),
            "{s}"
        );
    }

    #[test]
    fn tighten_records_unrecorded() {
        let mut b = base();
        b.metrics.get_mut("m_wall").unwrap().baseline = 0.0;
        tighten(&mut b, &res(Suite::Wall, &[("m_wall", 120.0)]));
        assert_eq!(b.metrics["m_wall"].baseline, 120.0);
    }

    #[test]
    fn baseline_add_adds_unknown_only() {
        let mut b = base();
        let (added, notes) = baseline_add(
            &mut b,
            &res(Suite::Wall, &[("m_wall", 1.0), ("new_m", 7.0)]),
        );
        assert_eq!(added, vec!["new_m"]);
        assert!(notes.is_empty());
        assert_eq!(b.metrics["new_m"].baseline, 7.0);
        assert_eq!(b.metrics["new_m"].budget, None);
        assert_eq!(b.metrics["m_wall"].baseline, 100.0);
    }

    #[test]
    fn toml_roundtrip_preserves_comments() {
        let mut b = base();
        tighten(&mut b, &res(Suite::Wall, &[("m_wall", 90.0)]));
        baseline_add(&mut b, &res(Suite::Wall, &[("new_m", 7.0)]));
        let out = write_into(TOML, &b).unwrap();
        assert!(out.contains("# top comment"));
        assert!(out.contains("baseline = 90   # keep this note"), "{out}");
        assert_eq!(Baselines::parse(&out).unwrap(), b);
    }

    #[test]
    fn report_prints_table_and_hints() {
        let r = res(Suite::Wall, &[("m_wall", 105.1)]);
        let s = compare(&base(), &r).to_string();
        assert!(s.contains("m_wall") && s.contains("REGRESSED") && s.contains("FAIL"));
    }
}
