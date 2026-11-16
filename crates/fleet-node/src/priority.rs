//! Agents' CPU priority (ADR 0066): the node setting and how a spawn's
//! [`AgentPriority`] resolves against it.

use fleet_proto::AgentPriority;
use std::ffi::OsStr;

/// The daemon's environment variable holding the node setting.
pub const AGENT_NICE_VAR: &str = "FLEETD_AGENT_NICE";

/// How much an `AGENT_PRIORITY_LOW` agent is lowered while the setting is off.
pub const LOW_NICE: u8 = 10;

/// Parses the node setting: unset, empty, `off` or `0` is off (0);
/// `1`..=`19` is the nice increment agents get.
pub fn parse_setting(value: Option<&OsStr>) -> Result<u8, String> {
    let Some(value) = value else { return Ok(0) };
    let text = value.to_string_lossy();
    let text = text.trim();
    if text.is_empty() || text == "off" {
        return Ok(0);
    }
    match text.parse::<u8>() {
        Ok(n) if n <= 19 => Ok(n),
        _ => Err(format!(
            "{AGENT_NICE_VAR}={text:?} is not off or a nice value from 1 to 19"
        )),
    }
}

/// The nice increment an agent spawned with `priority` gets under the node
/// setting `setting`.
pub fn agent_nice(setting: u8, priority: AgentPriority) -> u8 {
    match priority {
        AgentPriority::Default => setting,
        AgentPriority::Normal => 0,
        AgentPriority::Low if setting == 0 => LOW_NICE,
        AgentPriority::Low => setting,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<u8, String> {
        parse_setting(Some(OsStr::new(s)))
    }

    #[test]
    fn setting_is_off_unless_given_a_nice_value() {
        assert_eq!(parse_setting(None), Ok(0));
        for off in ["", "off", "0", " 0 "] {
            assert_eq!(parse(off), Ok(0), "{off:?}");
        }
        assert_eq!(parse("1"), Ok(1));
        assert_eq!(parse("10"), Ok(10));
        assert_eq!(parse(" 19\n"), Ok(19));
    }

    #[test]
    fn setting_refuses_values_outside_1_to_19() {
        for bad in ["20", "-1", "on", "5x", "999999999999"] {
            let err = parse(bad).unwrap_err();
            assert!(err.contains(AGENT_NICE_VAR), "{bad:?}: {err}");
        }
    }

    #[test]
    fn default_follows_the_setting() {
        assert_eq!(agent_nice(0, AgentPriority::Default), 0);
        assert_eq!(agent_nice(7, AgentPriority::Default), 7);
    }

    #[test]
    fn normal_opts_out_of_the_setting() {
        assert_eq!(agent_nice(0, AgentPriority::Normal), 0);
        assert_eq!(agent_nice(7, AgentPriority::Normal), 0);
    }

    #[test]
    fn low_is_lowered_even_with_the_setting_off() {
        assert_eq!(agent_nice(0, AgentPriority::Low), LOW_NICE);
        assert_eq!(agent_nice(4, AgentPriority::Low), 4);
    }
}
