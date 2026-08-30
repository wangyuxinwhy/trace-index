//! CLI configuration loading and resolution.

use std::collections::HashSet;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

const CONFIG_ENV: &str = "TRACE_INDEX_CONFIG";
const DATABASE_ENV: &str = "TRACE_INDEX_DB";
const DEFAULT_TEMPLATE: &str = r#"database = "~/.local/share/trace-index/index.sqlite"

# Add one or more trace roots. `index sync` reads these when no paths are given.
# [[roots]]
# name = "codex"
# path = "~/.codex/sessions"
#
# [[roots]]
# name = "pi"
# path = "~/.pi/agent/sessions"
#
# [[roots]]
# name = "claude"
# path = "~/.claude/projects"
"#;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub database: Option<PathBuf>,
    #[serde(default)]
    pub roots: Vec<RootConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RootConfig {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct EffectiveConfig {
    pub config_file: PathBuf,
    pub config_loaded: bool,
    pub database: PathBuf,
    pub roots: Vec<RootConfig>,
}

#[derive(Debug)]
pub struct LoadedConfig {
    pub effective: EffectiveConfig,
}

#[derive(Debug, Serialize)]
pub struct ConfigInitReport {
    pub config_file: PathBuf,
    pub created: bool,
}

/// Resolves the selected configuration path and whether it was explicit.
///
/// # Errors
///
/// Returns an error when home-directory expansion is required but unavailable.
pub fn selected_path(cli_path: Option<&Path>) -> Result<(PathBuf, bool)> {
    if let Some(path) = cli_path {
        return Ok((expand_tilde(path)?, true));
    }
    if let Some(path) = env::var_os(CONFIG_ENV) {
        return Ok((expand_tilde(Path::new(&path))?, true));
    }
    Ok((default_config_path()?, false))
}

/// Loads, validates, and resolves the effective configuration.
///
/// # Errors
///
/// Returns an error for missing explicit files, invalid TOML, unsupported
/// values, duplicate roots, or unresolved paths.
pub fn load(cli_config: Option<&Path>, cli_database: Option<&Path>) -> Result<LoadedConfig> {
    let (config_file, explicit) = selected_path(cli_config)?;
    let config_loaded = config_file.exists();
    if explicit && !config_loaded {
        bail!(
            "configuration file does not exist: {}",
            config_file.display()
        );
    }
    let mut config = if config_loaded {
        let contents = fs::read_to_string(&config_file)
            .with_context(|| format!("failed to read configuration {}", config_file.display()))?;
        toml::from_str::<Config>(&contents)
            .with_context(|| format!("failed to parse configuration {}", config_file.display()))?
    } else {
        Config::default()
    };
    validate(&config)?;

    let base = config_file.parent().unwrap_or_else(|| Path::new("."));
    config.database = config
        .database
        .as_deref()
        .map(|path| resolve_config_path(path, base))
        .transpose()?;
    for root in &mut config.roots {
        root.path = resolve_config_path(&root.path, base)?;
    }
    let database = if let Some(path) = cli_database {
        expand_tilde(path)?
    } else if let Some(path) = env::var_os(DATABASE_ENV) {
        expand_tilde(Path::new(&path))?
    } else {
        config
            .database
            .clone()
            .map_or_else(default_database_path, Ok)?
    };
    Ok(LoadedConfig {
        effective: EffectiveConfig {
            config_file,
            config_loaded,
            database,
            roots: config.roots,
        },
    })
}

/// Creates a new configuration template without overwriting an existing file.
///
/// # Errors
///
/// Returns an error when the target exists or its directory or file cannot be created.
pub fn initialize(cli_config: Option<&Path>) -> Result<ConfigInitReport> {
    let (config_file, _) = selected_path(cli_config)?;
    if let Some(parent) = config_file.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create configuration directory {}",
                parent.display()
            )
        })?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&config_file)
        .with_context(|| {
            format!(
                "failed to create configuration {}; refusing to overwrite an existing file",
                config_file.display()
            )
        })?;
    file.write_all(DEFAULT_TEMPLATE.as_bytes())
        .with_context(|| format!("failed to write configuration {}", config_file.display()))?;
    Ok(ConfigInitReport {
        config_file,
        created: true,
    })
}

fn validate(config: &Config) -> Result<()> {
    let mut names = HashSet::new();
    for root in &config.roots {
        if root.name.trim().is_empty() {
            bail!("root name must not be empty");
        }
        if root.path.as_os_str().is_empty() {
            bail!("root path must not be empty");
        }
        if !names.insert(root.name.as_str()) {
            bail!("duplicate root name: {:?}", root.name);
        }
    }
    Ok(())
}

fn default_config_path() -> Result<PathBuf> {
    if let Some(directory) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(directory).join("trace-index/config.toml"));
    }
    let home = home_directory()?;
    Ok(home.join(".config/trace-index/config.toml"))
}

fn default_database_path() -> Result<PathBuf> {
    Ok(home_directory()?.join(".local/share/trace-index/index.sqlite"))
}

fn resolve_config_path(path: &Path, base: &Path) -> Result<PathBuf> {
    let expanded = expand_tilde(path)?;
    Ok(if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    })
}

fn expand_tilde(path: &Path) -> Result<PathBuf> {
    let text = path.to_string_lossy();
    if text == "~" {
        return home_directory();
    }
    if let Some(remainder) = text.strip_prefix("~/") {
        return Ok(home_directory()?.join(remainder));
    }
    if text.starts_with('~') {
        bail!("only '~' and '~/' home expansion are supported: {text:?}");
    }
    Ok(path.to_path_buf())
}

fn home_directory() -> Result<PathBuf> {
    if let Some(home) = env::var_os("HOME") {
        return Ok(PathBuf::from(home));
    }
    #[cfg(windows)]
    if let Some(profile) = env::var_os("USERPROFILE") {
        return Ok(PathBuf::from(profile));
    }
    #[cfg(windows)]
    if let (Some(drive), Some(path)) = (env::var_os("HOMEDRIVE"), env::var_os("HOMEPATH")) {
        return Ok(PathBuf::from(drive).join(path));
    }
    bail!("home directory is unavailable; use an absolute path")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{load, selected_path};

    #[test]
    fn resolves_relative_paths_against_the_config_file() {
        let directory = tempdir().expect("temporary directory");
        let config = directory.path().join("config.toml");
        fs::write(
            &config,
            r#"database = "data/index.sqlite"
[[roots]]
name = "traces"
path = "sessions"
"#,
        )
        .expect("write configuration");

        let loaded = load(Some(&config), None).expect("load configuration");
        assert_eq!(
            loaded.effective.database,
            directory.path().join("data/index.sqlite")
        );
        assert_eq!(
            loaded.effective.roots[0].path,
            directory.path().join("sessions")
        );
    }

    #[test]
    fn explicit_missing_configuration_is_an_error() {
        let directory = tempdir().expect("temporary directory");
        let missing = directory.path().join("missing.toml");
        let error = load(Some(&missing), None).expect_err("missing config must fail");
        assert!(error.to_string().contains("does not exist"));
        assert!(selected_path(Some(&missing)).is_ok());
    }
}
