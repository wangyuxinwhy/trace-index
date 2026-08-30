//! CLI configuration parsing, resolution, initialization, and validation.

use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

const CONFIG_ENV: &str = "TRACE_INDEX_CONFIG";
const DATABASE_ENV: &str = "TRACE_INDEX_DB";
const CONFIG_SCHEMA_VERSION: u32 = 1;

pub const DEFAULT_MAX_INDEXED_RECORD_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_MAX_PUBLISHED_TEXT_BYTES: usize = 64 * 1024;

const TEMPLATE_HEADER: &str = r"# Trace Index configuration.
# Relative paths in this file are resolved against this file's directory.
# Omit `database` to use the platform data directory shown by `config show`.
# `[indexing]` initializes a new database; an indexed database owns its policy.
# Add `[[roots]]` entries with a `path`, or pass paths directly to `index sync`.
# Runtime adapters are detected from Source contents; roots do not select an adapter.
";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    #[serde(default = "current_schema_version")]
    schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    database: Option<PathBuf>,
    #[serde(default)]
    indexing: FileIndexingConfig,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    roots: Vec<FileRootConfig>,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            database: None,
            indexing: FileIndexingConfig::default(),
            roots: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FileIndexingConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_indexed_record_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_published_text_bytes: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FileRootConfig {
    #[serde(default, alias = "name", skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OriginKind {
    Cli,
    Environment,
    File,
    Default,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ConfigOrigin {
    pub kind: OriginKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigFileSelection {
    pub path: PathBuf,
    pub origin: ConfigOrigin,
    pub loaded: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedPath {
    pub path: PathBuf,
    pub origin: ConfigOrigin,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedUsize {
    pub value: usize,
    pub origin: ConfigOrigin,
}

#[derive(Debug, Clone, Serialize)]
pub struct EffectiveIndexingConfig {
    pub max_indexed_record_bytes: ResolvedUsize,
    pub max_published_text_bytes: ResolvedUsize,
}

#[derive(Debug, Clone, Serialize)]
pub struct EffectiveRootConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct EffectiveConfig {
    pub schema_version: u32,
    pub config_file: ConfigFileSelection,
    pub database: ResolvedPath,
    pub indexing: EffectiveIndexingConfig,
    pub roots: Vec<EffectiveRootConfig>,
}

#[derive(Debug)]
pub struct LoadedConfig {
    pub effective: EffectiveConfig,
}

#[derive(Debug, Serialize)]
pub struct ConfigInitReport {
    pub created: bool,
    pub config: EffectiveConfig,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigCheckLevel {
    Warning,
    Error,
}

#[derive(Debug, Serialize)]
pub struct ConfigCheckIssue {
    pub level: ConfigCheckLevel,
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct ConfigCheckReport {
    pub valid: bool,
    pub configured_sync_ready: bool,
    pub config: EffectiveConfig,
    pub issues: Vec<ConfigCheckIssue>,
}

#[derive(Debug)]
struct ResolutionContext {
    cwd: PathBuf,
    home: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
    xdg_data_home: Option<PathBuf>,
    config_env: Option<PathBuf>,
    database_env: Option<PathBuf>,
}

#[derive(Debug)]
struct SelectedConfigPath {
    path: PathBuf,
    origin: ConfigOrigin,
    explicit: bool,
}

/// Loads, validates, and resolves the effective configuration.
///
/// # Errors
///
/// Returns an error for missing explicit files, invalid TOML, unsupported
/// values, duplicate roots, invalid environment paths, or unresolved paths.
pub fn load(cli_config: Option<&Path>, cli_database: Option<&Path>) -> Result<LoadedConfig> {
    load_with_context(
        cli_config,
        cli_database,
        &ResolutionContext::from_environment()?,
    )
}

fn load_with_context(
    cli_config: Option<&Path>,
    cli_database: Option<&Path>,
    context: &ResolutionContext,
) -> Result<LoadedConfig> {
    let selected = selected_path(cli_config, context)?;
    let config_loaded = selected.path.exists();
    if selected.explicit && !config_loaded {
        bail!(
            "configuration file does not exist: {}",
            selected.path.display()
        );
    }
    let config = if config_loaded {
        let contents = fs::read_to_string(&selected.path)
            .with_context(|| format!("failed to read configuration {}", selected.path.display()))?;
        toml::from_str::<FileConfig>(&contents)
            .with_context(|| format!("failed to parse configuration {}", selected.path.display()))?
    } else {
        FileConfig::default()
    };
    validate_file_config(&config)?;
    resolve_config(config, selected, config_loaded, cli_database, context)
}

fn resolve_config(
    config: FileConfig,
    selected: SelectedConfigPath,
    config_loaded: bool,
    cli_database: Option<&Path>,
    context: &ResolutionContext,
) -> Result<LoadedConfig> {
    let base = selected
        .path
        .parent()
        .expect("absolute configuration paths have a parent");
    let database = if let Some(path) = cli_database {
        ResolvedPath {
            path: resolve_cli_path(path, context)?,
            origin: cli_origin(),
        }
    } else if let Some(path) = &context.database_env {
        validate_explicit_override(path, DATABASE_ENV)?;
        ResolvedPath {
            path: resolve_cli_path(path, context)?,
            origin: environment_origin(DATABASE_ENV),
        }
    } else if let Some(path) = config.database.as_deref() {
        ResolvedPath {
            path: resolve_file_path(path, base, context)?,
            origin: file_origin(),
        }
    } else {
        ResolvedPath {
            path: default_database_path(context)?,
            origin: default_origin(),
        }
    };

    let max_indexed_record_bytes = resolve_usize(
        config.indexing.max_indexed_record_bytes,
        DEFAULT_MAX_INDEXED_RECORD_BYTES,
    );
    let max_published_text_bytes = resolve_usize(
        config.indexing.max_published_text_bytes,
        DEFAULT_MAX_PUBLISHED_TEXT_BYTES,
    );
    let roots = config
        .roots
        .into_iter()
        .map(|root| {
            Ok(EffectiveRootConfig {
                label: root.label,
                path: resolve_file_path(&root.path, base, context)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    validate_resolved_roots(&roots)?;

    Ok(LoadedConfig {
        effective: EffectiveConfig {
            schema_version: config.schema_version,
            config_file: ConfigFileSelection {
                path: selected.path,
                origin: selected.origin,
                loaded: config_loaded,
            },
            database,
            indexing: EffectiveIndexingConfig {
                max_indexed_record_bytes,
                max_published_text_bytes,
            },
            roots,
        },
    })
}

/// Creates a validated configuration without overwriting an existing file.
///
/// `cli_roots` are explicit opt-ins. `discover` adds only standard Runtime
/// roots that currently exist. The command does not create a database or index
/// any traces.
///
/// # Errors
///
/// Returns an error when inputs are invalid, the target exists, or the file
/// cannot be created and written.
pub fn initialize(
    cli_config: Option<&Path>,
    cli_database: Option<&Path>,
    cli_roots: &[PathBuf],
    discover: bool,
) -> Result<ConfigInitReport> {
    let context = ResolutionContext::from_environment()?;
    let selected = selected_path(cli_config, &context)?;
    if selected.path.exists() {
        bail!(
            "configuration {} already exists; refusing to overwrite it",
            selected.path.display()
        );
    }

    let mut roots = cli_roots
        .iter()
        .map(|path| {
            Ok(FileRootConfig {
                label: None,
                path: resolve_cli_path(path, &context)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if discover {
        roots.extend(discover_standard_roots(&context)?);
    }
    deduplicate_file_roots(&mut roots, &context)?;

    let config = FileConfig {
        schema_version: CONFIG_SCHEMA_VERSION,
        database: cli_database
            .map(|path| resolve_cli_path(path, &context))
            .transpose()?,
        indexing: FileIndexingConfig {
            max_indexed_record_bytes: Some(DEFAULT_MAX_INDEXED_RECORD_BYTES),
            max_published_text_bytes: Some(DEFAULT_MAX_PUBLISHED_TEXT_BYTES),
        },
        roots,
    };
    validate_file_config(&config)?;
    let body = format!(
        "{TEMPLATE_HEADER}\n{}",
        toml::to_string_pretty(&config).context("failed to render configuration")?
    );
    toml::from_str::<FileConfig>(&body).context("generated configuration did not parse")?;

    if let Some(parent) = selected.path.parent() {
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
        .open(&selected.path)
        .with_context(|| {
            format!(
                "failed to create configuration {}; refusing to overwrite an existing file",
                selected.path.display()
            )
        })?;
    file.write_all(body.as_bytes())
        .with_context(|| format!("failed to write configuration {}", selected.path.display()))?;

    let mut loaded = load_with_context(Some(&selected.path), cli_database, &context)?;
    loaded.effective.config_file.origin = selected.origin;
    Ok(ConfigInitReport {
        created: true,
        config: loaded.effective,
    })
}

/// Checks whether the effective configuration is ready for a configured sync.
///
/// This is read-only: it does not create the database or modify roots.
///
/// # Errors
///
/// Returns an error when configuration loading itself fails.
pub fn check(cli_config: Option<&Path>, cli_database: Option<&Path>) -> Result<ConfigCheckReport> {
    let loaded = load(cli_config, cli_database)?;
    let mut issues = Vec::new();
    if !loaded.effective.config_file.loaded {
        issues.push(ConfigCheckIssue {
            level: ConfigCheckLevel::Warning,
            code: "configuration_not_found",
            message: "the default configuration file does not exist; built-in defaults are active"
                .to_owned(),
            path: None,
        });
    }
    if loaded.effective.roots.is_empty() {
        issues.push(ConfigCheckIssue {
            level: ConfigCheckLevel::Warning,
            code: "no_configured_roots",
            message: "index sync needs explicit PATHS because no roots are configured".to_owned(),
            path: None,
        });
    }

    let mut canonical_roots = HashMap::new();
    for root in &loaded.effective.roots {
        match fs::metadata(&root.path) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(metadata)
                if metadata.is_file()
                    && root
                        .path
                        .extension()
                        .is_some_and(|extension| extension == "jsonl") => {}
            Ok(_) => issues.push(ConfigCheckIssue {
                level: ConfigCheckLevel::Error,
                code: "unsupported_root_type",
                message: "a root must be a directory or a .jsonl file".to_owned(),
                path: Some(root.path.clone()),
            }),
            Err(error) => {
                issues.push(ConfigCheckIssue {
                    level: ConfigCheckLevel::Error,
                    code: "root_unavailable",
                    message: format!("failed to inspect configured root: {error}"),
                    path: Some(root.path.clone()),
                });
                continue;
            }
        }
        match root.path.canonicalize() {
            Ok(canonical) => {
                if let Some(previous) = canonical_roots.insert(canonical, root.path.clone()) {
                    issues.push(ConfigCheckIssue {
                        level: ConfigCheckLevel::Error,
                        code: "duplicate_canonical_root",
                        message: format!(
                            "this root resolves to the same location as {}",
                            previous.display()
                        ),
                        path: Some(root.path.clone()),
                    });
                }
            }
            Err(error) => issues.push(ConfigCheckIssue {
                level: ConfigCheckLevel::Error,
                code: "root_canonicalization_failed",
                message: format!("failed to canonicalize configured root: {error}"),
                path: Some(root.path.clone()),
            }),
        }
    }
    check_database_path(&loaded.effective.database.path, &mut issues);
    let valid = !issues
        .iter()
        .any(|issue| matches!(issue.level, ConfigCheckLevel::Error));
    let configured_sync_ready = valid && !loaded.effective.roots.is_empty();
    Ok(ConfigCheckReport {
        valid,
        configured_sync_ready,
        config: loaded.effective,
        issues,
    })
}

fn check_database_path(path: &Path, issues: &mut Vec<ConfigCheckIssue>) {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => return,
        Ok(_) => {
            issues.push(ConfigCheckIssue {
                level: ConfigCheckLevel::Error,
                code: "unsupported_database_type",
                message: "the database path exists but is not a file".to_owned(),
                path: Some(path.to_path_buf()),
            });
            return;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            issues.push(ConfigCheckIssue {
                level: ConfigCheckLevel::Error,
                code: "database_path_unavailable",
                message: format!("failed to inspect database path: {error}"),
                path: Some(path.to_path_buf()),
            });
            return;
        }
    }

    let Some(existing_ancestor) = path.ancestors().skip(1).find(|ancestor| ancestor.exists())
    else {
        issues.push(ConfigCheckIssue {
            level: ConfigCheckLevel::Error,
            code: "database_parent_unavailable",
            message: "no existing ancestor is available for the database directory".to_owned(),
            path: path.parent().map(Path::to_path_buf),
        });
        return;
    };
    if !existing_ancestor.is_dir() {
        issues.push(ConfigCheckIssue {
            level: ConfigCheckLevel::Error,
            code: "database_parent_not_directory",
            message: "an existing database path ancestor is not a directory".to_owned(),
            path: Some(existing_ancestor.to_path_buf()),
        });
    }
}

fn validate_file_config(config: &FileConfig) -> Result<()> {
    if config.schema_version != CONFIG_SCHEMA_VERSION {
        bail!(
            "configuration schema_version {} is not supported; this binary supports schema_version {CONFIG_SCHEMA_VERSION}",
            config.schema_version
        );
    }
    if config.indexing.max_indexed_record_bytes == Some(0) {
        bail!("indexing.max_indexed_record_bytes must be greater than zero");
    }
    if config.indexing.max_published_text_bytes == Some(0) {
        bail!("indexing.max_published_text_bytes must be greater than zero");
    }
    if config
        .database
        .as_deref()
        .is_some_and(|path| path.as_os_str().is_empty())
    {
        bail!("database must not be empty when supplied");
    }
    for (index, root) in config.roots.iter().enumerate() {
        if root.path.as_os_str().is_empty() {
            bail!("roots[{index}].path must not be empty");
        }
        if root
            .label
            .as_deref()
            .is_some_and(|label| label.trim().is_empty())
        {
            bail!("roots[{index}].label must not be empty when supplied");
        }
    }
    Ok(())
}

fn validate_resolved_roots(roots: &[EffectiveRootConfig]) -> Result<()> {
    let mut paths = HashSet::new();
    for (index, root) in roots.iter().enumerate() {
        if !paths.insert(root.path.clone()) {
            bail!(
                "roots[{index}].path duplicates another configured root: {}",
                root.path.display()
            );
        }
    }
    Ok(())
}

fn deduplicate_file_roots(
    roots: &mut Vec<FileRootConfig>,
    context: &ResolutionContext,
) -> Result<()> {
    let mut paths = HashSet::new();
    let mut deduplicated = Vec::with_capacity(roots.len());
    for root in roots.drain(..) {
        let path = resolve_cli_path(&root.path, context)?;
        let identity = path.canonicalize().unwrap_or_else(|_| path.clone());
        if paths.insert(identity) {
            deduplicated.push(FileRootConfig { path, ..root });
        }
    }
    *roots = deduplicated;
    Ok(())
}

fn discover_standard_roots(context: &ResolutionContext) -> Result<Vec<FileRootConfig>> {
    let home = required_home(context).context(
        "--discover requires HOME, a platform home directory, or explicit --root paths instead",
    )?;
    Ok([
        ("codex", home.join(".codex/sessions")),
        ("pi", home.join(".pi/agent/sessions")),
        ("claude", home.join(".claude/projects")),
    ]
    .into_iter()
    .filter(|(_, path)| path.is_dir())
    .map(|(label, path)| FileRootConfig {
        label: Some(label.to_owned()),
        path,
    })
    .collect())
}

fn selected_path(
    cli_path: Option<&Path>,
    context: &ResolutionContext,
) -> Result<SelectedConfigPath> {
    if let Some(path) = cli_path {
        return Ok(SelectedConfigPath {
            path: resolve_cli_path(path, context)?,
            origin: cli_origin(),
            explicit: true,
        });
    }
    if let Some(path) = &context.config_env {
        validate_explicit_override(path, CONFIG_ENV)?;
        return Ok(SelectedConfigPath {
            path: resolve_cli_path(path, context)?,
            origin: environment_origin(CONFIG_ENV),
            explicit: true,
        });
    }
    Ok(SelectedConfigPath {
        path: default_config_path(context)?,
        origin: default_origin(),
        explicit: false,
    })
}

fn default_config_path(context: &ResolutionContext) -> Result<PathBuf> {
    let base = if let Some(base) = &context.xdg_config_home {
        base.clone()
    } else {
        required_home(context)?.join(".config")
    };
    absolute_standard_directory(base, "XDG_CONFIG_HOME")
        .map(|path| path.join("trace-index/config.toml"))
}

fn default_database_path(context: &ResolutionContext) -> Result<PathBuf> {
    let base = if let Some(base) = &context.xdg_data_home {
        base.clone()
    } else {
        required_home(context)?.join(".local/share")
    };
    absolute_standard_directory(base, "XDG_DATA_HOME")
        .map(|path| path.join("trace-index/index.sqlite"))
}

fn absolute_standard_directory(path: PathBuf, name: &str) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        bail!(
            "{name} must be an absolute path, got {}; unset it or use an absolute path",
            path.display()
        )
    }
}

fn resolve_cli_path(path: &Path, context: &ResolutionContext) -> Result<PathBuf> {
    make_absolute(expand_tilde(path, context.home.as_deref())?, &context.cwd)
}

fn resolve_file_path(path: &Path, base: &Path, context: &ResolutionContext) -> Result<PathBuf> {
    make_absolute(expand_tilde(path, context.home.as_deref())?, base)
}

fn make_absolute(path: PathBuf, base: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path
    } else {
        base.join(path)
    };
    if absolute.is_absolute() {
        Ok(absolute)
    } else {
        bail!(
            "failed to resolve an absolute path from {}",
            absolute.display()
        )
    }
}

fn expand_tilde(path: &Path, home: Option<&Path>) -> Result<PathBuf> {
    let text = path.to_string_lossy();
    if text == "~" {
        return Ok(required_expansion_home(home)?.to_path_buf());
    }
    if let Some(remainder) = text.strip_prefix("~/") {
        return Ok(required_expansion_home(home)?.join(remainder));
    }
    if text.starts_with('~') {
        bail!("only '~' and '~/' home expansion are supported: {text:?}");
    }
    Ok(path.to_path_buf())
}

fn required_expansion_home(home: Option<&Path>) -> Result<&Path> {
    let home =
        home.context("home directory is unavailable; use an absolute path instead of '~'")?;
    validate_home(home)?;
    Ok(home)
}

fn required_home(context: &ResolutionContext) -> Result<&Path> {
    let home = context
        .home
        .as_deref()
        .context("home directory is unavailable; set the relevant XDG directory or use an explicit absolute path")?;
    validate_home(home)?;
    Ok(home)
}

fn validate_home(home: &Path) -> Result<()> {
    if home.is_absolute() {
        Ok(())
    } else {
        bail!("HOME must be an absolute path, got {}", home.display())
    }
}

fn validate_explicit_override(path: &Path, name: &str) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("{name} is set but empty");
    }
    Ok(())
}

fn resolve_usize(value: Option<usize>, default: usize) -> ResolvedUsize {
    match value {
        Some(value) => ResolvedUsize {
            value,
            origin: file_origin(),
        },
        None => ResolvedUsize {
            value: default,
            origin: default_origin(),
        },
    }
}

fn current_schema_version() -> u32 {
    CONFIG_SCHEMA_VERSION
}

fn cli_origin() -> ConfigOrigin {
    ConfigOrigin {
        kind: OriginKind::Cli,
        detail: None,
    }
}

fn environment_origin(name: &str) -> ConfigOrigin {
    ConfigOrigin {
        kind: OriginKind::Environment,
        detail: Some(name.to_owned()),
    }
}

fn file_origin() -> ConfigOrigin {
    ConfigOrigin {
        kind: OriginKind::File,
        detail: None,
    }
}

fn default_origin() -> ConfigOrigin {
    ConfigOrigin {
        kind: OriginKind::Default,
        detail: None,
    }
}

impl ResolutionContext {
    fn from_environment() -> Result<Self> {
        let cwd = env::current_dir().context("failed to read current working directory")?;
        let home = home_directory();
        Ok(Self {
            cwd,
            home,
            xdg_config_home: optional_standard_directory("XDG_CONFIG_HOME"),
            xdg_data_home: optional_standard_directory("XDG_DATA_HOME"),
            config_env: environment_override(CONFIG_ENV),
            database_env: environment_override(DATABASE_ENV),
        })
    }
}

fn optional_standard_directory(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn environment_override(name: &str) -> Option<PathBuf> {
    env::var_os(name).map(PathBuf::from)
}

fn home_directory() -> Option<PathBuf> {
    if let Some(home) = non_empty_environment("HOME") {
        return Some(PathBuf::from(home));
    }
    #[cfg(windows)]
    if let Some(profile) = non_empty_environment("USERPROFILE") {
        return Some(PathBuf::from(profile));
    }
    #[cfg(windows)]
    if let (Some(drive), Some(path)) = (
        non_empty_environment("HOMEDRIVE"),
        non_empty_environment("HOMEPATH"),
    ) {
        return Some(PathBuf::from(drive).join(path));
    }
    None
}

fn non_empty_environment(name: &str) -> Option<OsString> {
    env::var_os(name).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::tempdir;

    use super::{CONFIG_SCHEMA_VERSION, OriginKind, ResolutionContext, load_with_context};

    fn context(directory: &Path) -> ResolutionContext {
        ResolutionContext {
            cwd: directory.to_path_buf(),
            home: Some(directory.join("home")),
            xdg_config_home: Some(directory.join("config")),
            xdg_data_home: Some(directory.join("data")),
            config_env: None,
            database_env: None,
        }
    }

    #[test]
    fn resolves_every_effective_path_to_an_absolute_path() {
        let directory = tempdir().expect("temporary directory");
        let config = directory.path().join("config.toml");
        fs::write(
            &config,
            r#"database = "data/index.sqlite"
[[roots]]
label = "traces"
path = "sessions"
"#,
        )
        .expect("write configuration");

        let loaded = load_with_context(
            Some(Path::new("config.toml")),
            None,
            &context(directory.path()),
        )
        .expect("load configuration");
        assert_eq!(
            loaded.effective.database.path,
            directory.path().join("data/index.sqlite")
        );
        assert_eq!(
            loaded.effective.roots[0].path,
            directory.path().join("sessions")
        );
        assert_eq!(loaded.effective.config_file.path, config);
    }

    #[test]
    fn applies_database_precedence_and_reports_its_origin() {
        let directory = tempdir().expect("temporary directory");
        let config = directory.path().join("config.toml");
        fs::write(&config, "database = 'file.sqlite'\n").expect("write configuration");
        let mut context = context(directory.path());
        context.database_env = Some(PathBuf::from("environment.sqlite"));

        let from_environment =
            load_with_context(Some(&config), None, &context).expect("load environment override");
        assert_eq!(
            from_environment.effective.database.path,
            directory.path().join("environment.sqlite")
        );
        assert_eq!(
            from_environment.effective.database.origin.kind,
            OriginKind::Environment
        );

        let from_cli = load_with_context(Some(&config), Some(Path::new("cli.sqlite")), &context)
            .expect("load CLI override");
        assert_eq!(
            from_cli.effective.database.path,
            directory.path().join("cli.sqlite")
        );
        assert_eq!(from_cli.effective.database.origin.kind, OriginKind::Cli);
    }

    #[test]
    fn uses_xdg_data_home_for_the_default_database() {
        let directory = tempdir().expect("temporary directory");
        let loaded = load_with_context(None, None, &context(directory.path()))
            .expect("load built-in configuration");
        assert_eq!(
            loaded.effective.database.path,
            directory.path().join("data/trace-index/index.sqlite")
        );
        assert_eq!(loaded.effective.database.origin.kind, OriginKind::Default);
    }

    #[test]
    fn rejects_an_unknown_configuration_version() {
        let directory = tempdir().expect("temporary directory");
        let config = directory.path().join("config.toml");
        fs::write(
            &config,
            format!("schema_version = {}\n", CONFIG_SCHEMA_VERSION + 1),
        )
        .expect("write configuration");
        let error = load_with_context(Some(&config), None, &context(directory.path()))
            .expect_err("unknown schema must fail");
        assert!(error.to_string().contains("schema_version"));
    }

    #[test]
    fn rejects_an_empty_database_path_in_the_file() {
        let directory = tempdir().expect("temporary directory");
        let config = directory.path().join("config.toml");
        fs::write(&config, "database = ''\n").expect("write configuration");
        let error = load_with_context(Some(&config), None, &context(directory.path()))
            .expect_err("empty database path must fail during loading");
        assert!(error.to_string().contains("database must not be empty"));
    }

    #[test]
    fn reads_the_legacy_root_name_as_a_diagnostic_label() {
        let directory = tempdir().expect("temporary directory");
        let config = directory.path().join("config.toml");
        fs::write(
            &config,
            "[[roots]]\nname = 'legacy-codex'\npath = 'sessions'\n",
        )
        .expect("write legacy configuration");

        let loaded = load_with_context(Some(&config), None, &context(directory.path()))
            .expect("load legacy root name");
        assert_eq!(
            loaded.effective.roots[0].label.as_deref(),
            Some("legacy-codex")
        );
    }

    #[test]
    fn explicit_missing_configuration_is_an_error() {
        let directory = tempdir().expect("temporary directory");
        let missing = directory.path().join("missing.toml");
        let error = load_with_context(Some(&missing), None, &context(directory.path()))
            .expect_err("missing config must fail");
        assert!(error.to_string().contains("does not exist"));
    }

    #[test]
    fn absolute_overrides_do_not_require_a_home_directory() {
        let directory = tempdir().expect("temporary directory");
        let config = directory.path().join("config.toml");
        let database = directory.path().join("index.sqlite");
        fs::write(&config, "schema_version = 1\n").expect("write configuration");
        let context = ResolutionContext {
            cwd: directory.path().to_path_buf(),
            home: None,
            xdg_config_home: None,
            xdg_data_home: None,
            config_env: None,
            database_env: None,
        };

        let loaded = load_with_context(Some(&config), Some(&database), &context)
            .expect("absolute overrides do not need HOME");
        assert_eq!(loaded.effective.database.path, database);
    }

    #[test]
    fn selected_cli_values_do_not_validate_lower_priority_environment_values() {
        let directory = tempdir().expect("temporary directory");
        let config = directory.path().join("config.toml");
        let database = directory.path().join("index.sqlite");
        fs::write(&config, "schema_version = 1\n").expect("write configuration");
        let context = ResolutionContext {
            cwd: directory.path().to_path_buf(),
            home: Some(PathBuf::from("relative-home")),
            xdg_config_home: Some(PathBuf::from("relative-config")),
            xdg_data_home: Some(PathBuf::from("relative-data")),
            config_env: Some(PathBuf::new()),
            database_env: Some(PathBuf::new()),
        };

        let loaded = load_with_context(Some(&config), Some(&database), &context)
            .expect("CLI values must suppress invalid lower-priority values");
        assert_eq!(loaded.effective.config_file.origin.kind, OriginKind::Cli);
        assert_eq!(loaded.effective.database.origin.kind, OriginKind::Cli);
    }
}
