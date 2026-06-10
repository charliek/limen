//! The flag value type shared by every provider.
//!
//! The [`FlagProvider`] trait and the static / file / Redis implementations are
//! introduced in Phase 5; this module defines the value type now because the
//! configuration model parses static flag values at load time.

use serde::{Deserialize, Serialize};

/// A feature-flag value. Flags are loosely typed across providers (a rollout
/// percentage is a number, a `shadow_enabled` switch is a bool), so the value
/// is a small tagged-by-shape union.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum FlagValue {
    /// A boolean flag.
    Bool(bool),
    /// A numeric flag (e.g. a rollout percentage).
    Number(f64),
    /// A string flag.
    String(String),
}

impl FlagValue {
    /// The value as a number, if it is one.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            FlagValue::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// The value as a boolean, if it is one.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            FlagValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The value as a string slice, if it is one.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            FlagValue::String(s) => Some(s),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untagged_parsing_picks_the_right_shape() {
        assert_eq!(
            serde_yaml::from_str::<FlagValue>("true").unwrap(),
            FlagValue::Bool(true)
        );
        assert_eq!(
            serde_yaml::from_str::<FlagValue>("25").unwrap(),
            FlagValue::Number(25.0)
        );
        assert_eq!(
            serde_yaml::from_str::<FlagValue>("0.5").unwrap(),
            FlagValue::Number(0.5)
        );
        assert_eq!(
            serde_yaml::from_str::<FlagValue>("\"legacy_only\"").unwrap(),
            FlagValue::String("legacy_only".into())
        );
    }

    #[test]
    fn typed_accessors() {
        assert_eq!(FlagValue::Number(5.0).as_f64(), Some(5.0));
        assert_eq!(FlagValue::Number(5.0).as_bool(), None);
        assert_eq!(FlagValue::Bool(true).as_bool(), Some(true));
        assert_eq!(FlagValue::String("x".into()).as_str(), Some("x"));
    }
}
