//! Results JSON emitted by the wall and instruction suites.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Suite {
    Wall,
    Instr,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Measurement {
    pub value: f64,
    pub unit: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Results {
    pub suite: Suite,
    pub metrics: BTreeMap<String, Measurement>,
}

impl Results {
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_documented_shape() {
        let r = Results::from_json(
            r#"{"suite":"wall","metrics":{"fleetd_idle_rss_bytes":{"value":12345.0,"unit":"bytes"}}}"#,
        )
        .unwrap();
        assert_eq!(r.suite, Suite::Wall);
        assert_eq!(r.metrics["fleetd_idle_rss_bytes"].value, 12345.0);
        assert!(
            serde_json::to_string(&r)
                .unwrap()
                .contains(r#""suite":"wall""#)
        );
    }
}
