//! Layered configuration loading: defaults < file < env < CLI (spec §5.1).
//!
//! `serde` supplies the built-in defaults, the file is parsed on top, and then
//! a merged [`ConfigOverrides`] layer (environment overlaid by CLI flags) is
//! applied last. Only the documented `LIMEN_*` knobs are overridable; everything
//! else is file-only.

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::config::model::{Config, FailSafeMode, FlagProviderKind};
use crate::config::redact;

/// Errors from loading configuration.
#[derive(Debug, Error)]
pub enum ConfigLoadError {
    /// The config file could not be read.
    #[error("cannot read config file {path}: {source}")]
    Io {
        /// The path that failed to read.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The file did not parse against the config schema.
    #[error("invalid config {path}: {message}")]
    Parse {
        /// The path that failed to parse.
        path: PathBuf,
        /// A field-pathed parse error message.
        message: String,
    },
    /// An environment or CLI override value was invalid.
    #[error("invalid override for {var}: {message}")]
    BadOverride {
        /// The variable / flag name.
        var: String,
        /// What was wrong with the value.
        message: String,
    },
}

/// Optional overrides contributed by one layer (environment or CLI). Fields set
/// to `Some` win over lower layers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfigOverrides {
    /// Overrides `server.listen_addr` (`LIMEN_LISTEN_ADDR`).
    pub listen_addr: Option<String>,
    /// Overrides `metrics.listen_addr` (`LIMEN_METRICS_ADDR`).
    pub metrics_addr: Option<String>,
    /// Overrides `flags.provider` (`LIMEN_FLAGS_PROVIDER`).
    pub flags_provider: Option<FlagProviderKind>,
    /// Overrides `flags.redis.url` (`LIMEN_REDIS_URL`).
    pub redis_url: Option<String>,
    /// Overrides `flags.fail_safe_mode` (`LIMEN_FAIL_SAFE_MODE`).
    pub fail_safe_mode: Option<FailSafeMode>,
}

impl ConfigOverrides {
    /// Read the documented `LIMEN_*` environment overrides.
    pub fn from_env() -> Result<Self, ConfigLoadError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Build overrides from already-typed string values (the CLI layer). This
    /// is the canonical constructor; the `LIMEN_*` environment names live only
    /// in [`from_lookup`](Self::from_lookup), so a new knob is wired in one
    /// place per layer rather than threaded through a key map.
    pub fn from_parts(
        listen_addr: Option<String>,
        metrics_addr: Option<String>,
        flags_provider: Option<String>,
        redis_url: Option<String>,
        fail_safe_mode: Option<String>,
    ) -> Result<Self, ConfigLoadError> {
        Ok(Self {
            listen_addr,
            metrics_addr,
            redis_url,
            flags_provider: flags_provider.as_deref().map(parse_provider).transpose()?,
            fail_safe_mode: fail_safe_mode.as_deref().map(parse_fail_safe).transpose()?,
        })
    }

    /// Build overrides from an arbitrary key lookup over the documented
    /// `LIMEN_*` names (used by `from_env` and by tests, which must not mutate
    /// the process environment).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigLoadError> {
        Self::from_parts(
            get("LIMEN_LISTEN_ADDR"),
            get("LIMEN_METRICS_ADDR"),
            get("LIMEN_FLAGS_PROVIDER"),
            get("LIMEN_REDIS_URL"),
            get("LIMEN_FAIL_SAFE_MODE"),
        )
    }

    /// Overlay a higher-precedence layer on top of `self` (the higher layer
    /// wins per field). Used to combine env (lower) with CLI (higher).
    #[must_use]
    pub fn overlay(self, higher: ConfigOverrides) -> ConfigOverrides {
        ConfigOverrides {
            listen_addr: higher.listen_addr.or(self.listen_addr),
            metrics_addr: higher.metrics_addr.or(self.metrics_addr),
            flags_provider: higher.flags_provider.or(self.flags_provider),
            redis_url: higher.redis_url.or(self.redis_url),
            fail_safe_mode: higher.fail_safe_mode.or(self.fail_safe_mode),
        }
    }

    /// Apply the overrides to a parsed config in place.
    pub fn apply(&self, config: &mut Config) {
        if let Some(v) = &self.listen_addr {
            config.server.listen_addr = v.clone();
        }
        if let Some(v) = &self.metrics_addr {
            config.metrics.listen_addr = v.clone();
        }
        if let Some(v) = self.flags_provider {
            config.flags.provider = v;
        }
        if let Some(v) = &self.redis_url {
            config.flags.redis.url = v.clone();
        }
        if let Some(v) = self.fail_safe_mode {
            config.flags.fail_safe_mode = v;
        }
    }
}

fn parse_provider(v: &str) -> Result<FlagProviderKind, ConfigLoadError> {
    match v {
        "static" => Ok(FlagProviderKind::Static),
        "file" => Ok(FlagProviderKind::File),
        "redis" => Ok(FlagProviderKind::Redis),
        // Not echoed: an override arrives from the environment or a flag, where
        // a URL pasted into the wrong variable is the realistic mistake.
        _ => Err(ConfigLoadError::BadOverride {
            var: "LIMEN_FLAGS_PROVIDER".to_string(),
            message: "unknown provider (expected static|file|redis; value redacted)".to_string(),
        }),
    }
}

fn parse_fail_safe(v: &str) -> Result<FailSafeMode, ConfigLoadError> {
    match v {
        "legacy_only" => Ok(FailSafeMode::LegacyOnly),
        _ => Err(ConfigLoadError::BadOverride {
            var: "LIMEN_FAIL_SAFE_MODE".to_string(),
            message: "unknown mode (expected legacy_only; value redacted)".to_string(),
        }),
    }
}

/// A parsed config plus the directory it was loaded from — needed to resolve
/// relative contract references.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    /// The fully layered configuration.
    pub config: Config,
    /// The directory containing the config file.
    pub base_dir: PathBuf,
}

/// Load configuration from `path` and apply the (already-merged) `overrides`.
pub fn load(path: &Path, overrides: &ConfigOverrides) -> Result<LoadedConfig, ConfigLoadError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigLoadError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let de = yaml_serde::Deserializer::from_str(&text);
    let mut config: Config =
        serde_path_to_error::deserialize(de).map_err(|e| ConfigLoadError::Parse {
            path: path.to_path_buf(),
            // serde quotes the value it choked on, and this is raised before
            // any validator has looked at it (invariant 5).
            message: redact::serde_message(&e.to_string()),
        })?;
    overrides.apply(&mut config);
    let base_dir = path
        .parent()
        .map(Path::to_path_buf)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("."));
    Ok(LoadedConfig { config, base_dir })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn env_overrides_parse_and_reject() {
        let env: HashMap<&str, &str> = [
            ("LIMEN_LISTEN_ADDR", "127.0.0.1:1"),
            ("LIMEN_FLAGS_PROVIDER", "redis"),
            ("LIMEN_REDIS_URL", "redis://r:6379"),
            ("LIMEN_FAIL_SAFE_MODE", "legacy_only"),
        ]
        .into_iter()
        .collect();
        let o = ConfigOverrides::from_lookup(|k| env.get(k).map(|s| s.to_string())).unwrap();
        assert_eq!(o.listen_addr.as_deref(), Some("127.0.0.1:1"));
        assert_eq!(o.flags_provider, Some(FlagProviderKind::Redis));

        let bad = ConfigOverrides::from_lookup(|k| {
            (k == "LIMEN_FLAGS_PROVIDER").then(|| "nope".to_string())
        });
        assert!(bad.is_err());
    }

    #[test]
    fn cli_overrides_win_over_env() {
        let env = ConfigOverrides {
            listen_addr: Some("0.0.0.0:1".into()),
            metrics_addr: Some("0.0.0.0:2".into()),
            ..Default::default()
        };
        let cli = ConfigOverrides {
            listen_addr: Some("0.0.0.0:9".into()),
            ..Default::default()
        };
        let merged = env.overlay(cli);
        assert_eq!(merged.listen_addr.as_deref(), Some("0.0.0.0:9")); // cli wins
        assert_eq!(merged.metrics_addr.as_deref(), Some("0.0.0.0:2")); // env retained
    }

    #[test]
    fn load_applies_overrides_and_records_base_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("limen.config.yaml");
        std::fs::write(&path, "server:\n  listen_addr: \"0.0.0.0:8080\"\n  graceful_shutdown_timeout_ms: 1\n  request_body_limit_bytes: 1\n").unwrap();
        let overrides = ConfigOverrides {
            listen_addr: Some("0.0.0.0:7777".into()),
            ..Default::default()
        };
        let loaded = load(&path, &overrides).unwrap();
        assert_eq!(loaded.config.server.listen_addr, "0.0.0.0:7777");
        assert_eq!(loaded.base_dir, dir.path());
    }

    #[test]
    fn parse_error_reports_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(&path, "server: 123\n").unwrap();
        let err = load(&path, &ConfigOverrides::default()).unwrap_err();
        assert!(matches!(err, ConfigLoadError::Parse { .. }));
    }

    /// Write `text` as `file` (the extension is only a name: the loader reads
    /// both formats with one parser) and return what loading it reports. Also
    /// proves the premise: serde's own message for the same text carries the
    /// canary, so the redaction is what removed it.
    fn load_error(file: &str, text: &str) -> String {
        let raw =
            serde_path_to_error::deserialize::<_, Config>(yaml_serde::Deserializer::from_str(text))
                .unwrap_err()
                .to_string();
        assert!(
            raw.contains("CANARY"),
            "premise: serde quotes the value: {raw}"
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(file);
        std::fs::write(&path, text).unwrap();
        let err = load(&path, &ConfigOverrides::default()).unwrap_err();
        assert!(matches!(err, ConfigLoadError::Parse { .. }), "{err}");
        err.to_string()
    }

    /// Invariant 5, before any validator runs: serde's type errors quote the
    /// value they choked on, so a URL written one level too shallow came back
    /// out in full. The field path, the expected type and the position stay.
    #[test]
    fn a_nesting_mistake_does_not_echo_the_url_in_yaml_or_json() {
        for (file, text) in [
            (
                "limen.config.yaml",
                "flags:\n  provider: redis\n  redis: \"redis://:CANARY@host\"\n",
            ),
            (
                "limen.config.json",
                r#"{"flags": {"provider": "redis", "redis": "redis://:CANARY@host"}}"#,
            ),
        ] {
            let message = load_error(file, text);
            assert!(!message.contains("CANARY"), "{file}: {message}");
            assert!(message.contains("flags.redis"), "{file}: {message}");
            assert!(
                message.contains("invalid type: string (value redacted), expected"),
                "{file}: {message}"
            );
            assert!(message.contains("line"), "position kept: {message}");
        }
    }

    #[test]
    fn a_plain_wrong_type_does_not_echo_the_value() {
        let message = load_error(
            "limen.config.yaml",
            "server:\n  graceful_shutdown_timeout_ms: \"CANARY\"\n",
        );
        assert!(!message.contains("CANARY"), "{message}");
        assert!(
            message.contains("server.graceful_shutdown_timeout_ms"),
            "{message}"
        );
        assert!(
            message.contains("invalid type: string (value redacted), expected u64"),
            "{message}"
        );
    }

    /// An enum field holding something else (a URL pasted into `provider`)
    /// names its variants, not the value.
    #[test]
    fn an_unknown_variant_does_not_echo_the_value_but_lists_the_variants() {
        let message = load_error(
            "limen.config.yaml",
            "flags:\n  provider: \"redis://:CANARY@host\"\n",
        );
        assert!(!message.contains("CANARY"), "{message}");
        assert!(
            message.contains("unknown variant (value redacted), expected one of `static`"),
            "{message}"
        );
    }

    /// A misspelt *key* is still named: that is schema vocabulary, and how the
    /// typo is found.
    #[test]
    fn an_unknown_field_is_still_named() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("limen.config.yaml");
        std::fs::write(&path, "flags:\n  providr: file\n").unwrap();
        let message = load(&path, &ConfigOverrides::default())
            .unwrap_err()
            .to_string();
        assert!(message.contains("unknown field `providr`"), "{message}");
    }

    /// Overrides don't go through serde; they are matched by hand, and the
    /// mismatch used to be reported with the value.
    #[test]
    fn a_bad_override_names_the_variable_not_the_value() {
        for var in ["LIMEN_FLAGS_PROVIDER", "LIMEN_FAIL_SAFE_MODE"] {
            let err = ConfigOverrides::from_lookup(|k| {
                (k == var).then(|| "redis://:CANARY@host".to_string())
            })
            .unwrap_err();
            let message = err.to_string();
            assert!(!message.contains("CANARY"), "{message}");
            assert!(message.contains(var), "{message}");
            assert!(message.contains("value redacted"), "{message}");
        }
    }
}
