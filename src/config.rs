//! Configuration loading and operator-URL resolution.
//!
//! The operator base URL is resolved with the precedence:
//! `--operator` flag  >  `~/.akcli/config.toml`  >  error.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// On-disk configuration, read from `~/.akcli/config.toml`.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
pub struct Config {
    /// Operator base URL, e.g. `http://localhost:7071`.
    pub operator: Option<String>,
}

/// Errors produced while loading or resolving configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("no operator URL: pass --operator <url> or set operator in {0}")]
    MissingOperator(String),
    #[error("failed to parse config at {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("failed to read config at {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// Default config file path: `~/.akcli/config.toml`.
pub fn config_path() -> PathBuf {
    directories::BaseDirs::new()
        .map(|d| d.home_dir().join(".akcli").join("config.toml"))
        .unwrap_or_else(|| PathBuf::from(".akcli/config.toml"))
}

/// Parse config from TOML text.
pub fn parse_config(contents: &str) -> Result<Config, toml::de::Error> {
    toml::from_str(contents)
}

/// Load config from `path`. A missing file yields an empty default config
/// (the operator can still come from the flag); any other IO or parse failure
/// is an error.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(contents) => parse_config(&contents).map_err(|source| ConfigError::Parse {
            path: path.display().to_string(),
            source,
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(source) => Err(ConfigError::Read {
            path: path.display().to_string(),
            source,
        }),
    }
}

/// Normalize an operator URL by trimming a single trailing slash.
fn normalize(url: &str) -> String {
    url.trim_end_matches('/').to_string()
}

/// Resolve the operator URL: flag wins over the config file; neither is an error.
pub fn resolve_operator(
    flag: Option<&str>,
    config: &Config,
    config_display_path: &str,
) -> Result<String, ConfigError> {
    flag.map(str::to_string)
        .or_else(|| config.operator.clone())
        .map(|u| normalize(&u))
        .filter(|u| !u.is_empty())
        .ok_or_else(|| ConfigError::MissingOperator(config_display_path.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(operator: Option<&str>) -> Config {
        Config {
            operator: operator.map(str::to_string),
        }
    }

    #[test]
    fn parses_operator_from_toml() {
        let c = parse_config("operator = \"http://localhost:7071\"").unwrap();
        assert_eq!(c.operator.as_deref(), Some("http://localhost:7071"));
    }

    #[test]
    fn parses_empty_toml_as_no_operator() {
        assert_eq!(parse_config("").unwrap(), Config::default());
    }

    #[test]
    fn flag_overrides_config_file() {
        let c = cfg(Some("http://from-file:1111"));
        let got = resolve_operator(Some("http://from-flag:2222"), &c, "p").unwrap();
        assert_eq!(got, "http://from-flag:2222");
    }

    #[test]
    fn uses_config_file_when_no_flag() {
        let c = cfg(Some("http://from-file:1111"));
        assert_eq!(resolve_operator(None, &c, "p").unwrap(), "http://from-file:1111");
    }

    #[test]
    fn errors_when_neither_flag_nor_file() {
        let err = resolve_operator(None, &cfg(None), "p").unwrap_err();
        assert!(matches!(err, ConfigError::MissingOperator(_)));
    }

    #[test]
    fn empty_values_are_treated_as_missing() {
        let err = resolve_operator(Some(""), &cfg(Some("")), "p").unwrap_err();
        assert!(matches!(err, ConfigError::MissingOperator(_)));
    }

    #[test]
    fn trims_trailing_slash() {
        let got = resolve_operator(Some("http://localhost:7071/"), &cfg(None), "p").unwrap();
        assert_eq!(got, "http://localhost:7071");
    }

    #[test]
    fn missing_file_is_default_config() {
        let dir = std::env::temp_dir().join("akcli-test-nonexistent-xyz");
        let path = dir.join("config.toml");
        assert_eq!(load(&path).unwrap(), Config::default());
    }
}
