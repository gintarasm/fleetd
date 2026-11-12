//! Baselines file model, the gate comparator, and comment-preserving writes.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, Item, Table, Value};

use crate::results::{Results, Suite};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
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
pub struct Gate {
    pub approvers: Vec<String>,
    pub tolerance: Tolerance,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricSpec {
    pub suite: Suite,
    pub unit: String,
    #[serde(default)]
    pub budget: Option<f64>,
    pub baseline: f64,
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
            Status::Invalid => {
                Some("measurement produced an invalid value (non-finite or <= 0): fix the suite")
            }
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

pub fn compare(b: &Baselines, r: &Results) -> Report {
    let tol = b.gate.tolerance.for_suite(r.suite);
    let mut rows = Vec::new();

    for (name, spec) in b.metrics.iter().filter(|(_, s)| s.suite == r.suite) {
        let Some(m) = r.metrics.get(name) else {
            rows.push(Row {
                name: name.clone(),
                baseline: spec.baseline,
                value: f64::NAN,
                delta_pct: 0.0,
                status: Status::Missing,
                detail: None,
            });
            continue;
        };
        let value = m.value;
        let mut detail = None;
        let status = if !value.is_finite() || value <= 0.0 {
            Status::Invalid
        } else if m.unit != spec.unit {
            detail = Some(format!(
                "{name}: results unit `{}` differs from baseline unit `{}`",
                m.unit, spec.unit
            ));
            Status::UnitMismatch
        } else if spec.budget.is_some_and(|budget| value > budget) {
            Status::OverBudget
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
            });
        }
    }

    let failed = rows.iter().any(|row| row.status.is_failure());
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
                r.status.label()
            )?;
        }
        let mut seen = Vec::new();
        for r in &self.rows {
            if let Some(h) = r.status.hint()
                && !seen.contains(&r.status.label())
            {
                seen.push(r.status.label());
                writeln!(f, "\n{}: {h}", r.status.label())?;
            }
        }
        for r in &self.rows {
            if let Some(d) = &r.detail {
                writeln!(f, "\n{}: {d}", r.status.label())?;
            }
        }
        writeln!(f, "\n{}", if self.failed { "FAIL" } else { "PASS" })
    }
}

/// Lower baselines only: records unrecorded (0) baselines and locks in
/// improvements beyond tolerance (`Status::Improved`). Never raises, and
/// leaves within-tolerance noise alone so baselines do not ratchet down on
/// lucky runs. Only metrics of the results' suite are touched.
pub fn tighten(b: &mut Baselines, r: &Results) {
    let report = compare(b, r);
    for row in report.rows {
        if matches!(row.status, Status::Improved | Status::Unrecorded)
            && let Some(spec) = b.metrics.get_mut(&row.name)
        {
            spec.baseline = row.value;
        }
    }
}

/// Adds metrics present in the results but not in the baselines, with
/// `baseline = value` and no budget. Returns the names added.
pub fn baseline_add(b: &mut Baselines, r: &Results) -> Vec<String> {
    let mut added = Vec::new();
    for (name, m) in &r.metrics {
        if !b.metrics.contains_key(name) {
            b.metrics.insert(
                name.clone(),
                MetricSpec {
                    suite: r.suite,
                    unit: m.unit.clone(),
                    budget: None,
                    baseline: m.value,
                },
            );
            added.push(name.clone());
        }
    }
    added
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
        let added = baseline_add(
            &mut b,
            &res(Suite::Wall, &[("m_wall", 1.0), ("new_m", 7.0)]),
        );
        assert_eq!(added, vec!["new_m"]);
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
