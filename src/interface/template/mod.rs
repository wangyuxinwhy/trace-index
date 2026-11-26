//! Named parameterized read-only SQL templates.
//!
//! Built-in templates are compiled into the binary (`include_str!`). Optional
//! user templates are loaded strictly from `<config_dir>/templates/*.sql`
//! (never implicitly from the current working directory) and cannot shadow
//! built-in template names.

pub(crate) mod params;
pub(crate) mod parser;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use self::params::{ParamResolution, resolve_template_params};
use self::parser::{ParamSpec, ParsedTemplate, parse_template_source, render_nodes};
use crate::interface::output::SqlQueryReport;
use crate::interface::query::{QueryBudget, SqlParamValue, query_sql_with_params};
use crate::storage::db::Store;

const BUILTIN_SOURCES: &[(&str, &str)] = &[
    (
        "search-sessions",
        include_str!("builtin/search-sessions.sql"),
    ),
    ("search-items", include_str!("builtin/search-items.sql")),
    (
        "session-transcript",
        include_str!("builtin/session-transcript.sql"),
    ),
    ("item-context", include_str!("builtin/item-context.sql")),
    ("shell-calls", include_str!("builtin/shell-calls.sql")),
    (
        "recent-sessions",
        include_str!("builtin/recent-sessions.sql"),
    ),
];

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TemplateOrigin {
    Builtin,
    User { path: PathBuf },
}

#[derive(Debug, Clone)]
pub(crate) struct RegisteredTemplate {
    pub(crate) origin: TemplateOrigin,
    pub(crate) parsed: ParsedTemplate,
}

#[derive(Debug, Clone, Serialize)]
pub struct TemplateSummary {
    pub name: String,
    pub description: String,
    pub origin: TemplateOrigin,
    pub min_format: u32,
    pub default_limit: usize,
    pub required_params: Vec<String>,
    pub optional_params: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct TemplateListReport {
    pub templates_dir: PathBuf,
    pub templates: Vec<TemplateSummary>,
}

#[derive(Debug, Serialize)]
pub struct TemplateShowReport {
    pub name: String,
    pub description: String,
    pub origin: TemplateOrigin,
    pub min_format: u32,
    pub default_limit: usize,
    pub params: BTreeMap<String, ParamSpec>,
    pub sql: String,
}

#[derive(Debug, Serialize)]
pub struct TemplateRenderReport {
    pub template: String,
    pub origin: TemplateOrigin,
    pub default_limit: usize,
    pub sql: String,
    pub params: BTreeMap<String, SqlParamValue>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub resolved: BTreeMap<String, ParamResolution>,
}

#[derive(Debug, Serialize)]
pub struct TemplateRunReport {
    pub template: String,
    pub origin: TemplateOrigin,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub resolved: BTreeMap<String, ParamResolution>,
    #[serde(flatten)]
    pub query: SqlQueryReport,
}

/// Validation issue discovered in `<config_dir>/templates/` during `config check`.
#[derive(Debug, Clone)]
pub(crate) struct TemplateValidationIssue {
    pub(crate) path: PathBuf,
    pub(crate) message: String,
}

/// Collection of built-in and optional user SQL templates.
#[derive(Debug, Clone)]
pub(crate) struct TemplateRegistry {
    templates_dir: PathBuf,
    templates: BTreeMap<String, RegisteredTemplate>,
}

impl TemplateRegistry {
    /// Loads built-in templates and any user `.sql` templates under
    /// `<config_dir>/templates/`.
    ///
    /// # Errors
    ///
    /// Returns an error if a user template fails to parse or attempts to shadow
    /// a built-in template name.
    pub(crate) fn load(config_file_path: &Path) -> Result<Self> {
        let templates_dir = templates_dir_for_config(config_file_path);
        let mut templates = load_builtins()?;
        if templates_dir.is_dir() {
            for path in discover_user_template_files(&templates_dir)? {
                let source = fs::read_to_string(&path)
                    .with_context(|| format!("failed to read user template {}", path.display()))?;
                let parsed = parse_template_source(&source, &path.display().to_string())?;
                if templates.contains_key(&parsed.header.name) {
                    bail!(
                        "user template {} cannot shadow existing template name {:?}",
                        path.display(),
                        parsed.header.name
                    );
                }
                templates.insert(
                    parsed.header.name.clone(),
                    RegisteredTemplate {
                        origin: TemplateOrigin::User { path },
                        parsed,
                    },
                );
            }
        }
        Ok(Self {
            templates_dir,
            templates,
        })
    }

    pub(crate) fn list_report(&self) -> TemplateListReport {
        let templates = self
            .templates
            .values()
            .map(|entry| {
                let mut required_params = Vec::new();
                let mut optional_params = Vec::new();
                for (name, spec) in &entry.parsed.header.params {
                    if spec.required {
                        required_params.push(name.clone());
                    } else {
                        optional_params.push(name.clone());
                    }
                }
                TemplateSummary {
                    name: entry.parsed.header.name.clone(),
                    description: entry.parsed.header.description.clone(),
                    origin: entry.origin.clone(),
                    min_format: entry.parsed.header.min_format,
                    default_limit: entry.parsed.header.default_limit,
                    required_params,
                    optional_params,
                }
            })
            .collect();
        TemplateListReport {
            templates_dir: self.templates_dir.clone(),
            templates,
        }
    }

    pub(crate) fn get(&self, name: &str) -> Result<&RegisteredTemplate> {
        self.templates.get(name).ok_or_else(|| {
            let available = self
                .templates
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::anyhow!(
                "unknown query template {name:?}; available templates: {available} (run `trace-index query template list`)"
            )
        })
    }

    pub(crate) fn show_report(&self, name: &str) -> Result<TemplateShowReport> {
        let entry = self.get(name)?;
        Ok(TemplateShowReport {
            name: entry.parsed.header.name.clone(),
            description: entry.parsed.header.description.clone(),
            origin: entry.origin.clone(),
            min_format: entry.parsed.header.min_format,
            default_limit: entry.parsed.header.default_limit,
            params: entry.parsed.header.params.clone(),
            sql: entry.parsed.sql_body.clone(),
        })
    }
}

/// Validates user `.sql` templates under `<config_dir>/templates/` for
/// `config check` without aborting on the first failure.
pub(crate) fn validate_user_templates(config_file_path: &Path) -> Vec<TemplateValidationIssue> {
    let templates_dir = templates_dir_for_config(config_file_path);
    if !templates_dir.exists() {
        return Vec::new();
    }
    if !templates_dir.is_dir() {
        return vec![TemplateValidationIssue {
            path: templates_dir,
            message: "templates path exists but is not a directory".to_owned(),
        }];
    }
    let builtins = match load_builtins() {
        Ok(map) => map,
        Err(error) => {
            return vec![TemplateValidationIssue {
                path: templates_dir,
                message: format!("built-in template registry failed to load: {error}"),
            }];
        }
    };
    let files = match discover_user_template_files(&templates_dir) {
        Ok(files) => files,
        Err(error) => {
            return vec![TemplateValidationIssue {
                path: templates_dir,
                message: format!("failed to scan user templates directory: {error}"),
            }];
        }
    };
    let mut seen_names: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut issues = Vec::new();
    for path in files {
        let source = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                issues.push(TemplateValidationIssue {
                    path,
                    message: format!("failed to read template: {error}"),
                });
                continue;
            }
        };
        let parsed = match parse_template_source(&source, &path.display().to_string()) {
            Ok(parsed) => parsed,
            Err(error) => {
                issues.push(TemplateValidationIssue {
                    path,
                    message: format!("{error:#}"),
                });
                continue;
            }
        };
        let name = &parsed.header.name;
        if builtins.contains_key(name) {
            issues.push(TemplateValidationIssue {
                path,
                message: format!("user template cannot shadow built-in template {name:?}"),
            });
            continue;
        }
        if let Some(previous) = seen_names.insert(name.clone(), path.clone()) {
            issues.push(TemplateValidationIssue {
                path,
                message: format!(
                    "duplicate user template name {name:?} (already declared in {})",
                    previous.display()
                ),
            });
        }
    }
    issues
}

/// Renders one template with resolved parameters against the opened `Store`
/// without executing the SQL statement.
///
/// # Errors
///
/// Returns an error if the database storage format is below `min_format` or if
/// parameter parsing/resolution fails.
pub(crate) fn render_template(
    entry: &RegisteredTemplate,
    store: &Store,
    raw_params: &[String],
) -> Result<TemplateRenderReport> {
    let storage_format = store.storage_format()?;
    if storage_format < entry.parsed.header.min_format {
        bail!(
            "template {:?} requires Storage Format {} or newer (this database is Format {storage_format})",
            entry.parsed.header.name,
            entry.parsed.header.min_format
        );
    }
    let resolved = resolve_template_params(&entry.parsed.header, raw_params, store)?;
    let sql = render_nodes(&entry.parsed.nodes, &resolved.active_flags);
    let used_params = collect_referenced_params(store, &sql, &resolved.bound)?;
    Ok(TemplateRenderReport {
        template: entry.parsed.header.name.clone(),
        origin: entry.origin.clone(),
        default_limit: entry.parsed.header.default_limit,
        sql,
        params: used_params,
        resolved: resolved.resolutions,
    })
}

fn collect_referenced_params(
    store: &Store,
    sql: &str,
    bound: &BTreeMap<String, SqlParamValue>,
) -> Result<BTreeMap<String, SqlParamValue>> {
    let statement = store
        .connection()
        .prepare(sql)
        .context("failed to prepare rendered template SQL")?;
    let mut used = BTreeMap::new();
    for index in 1..=statement.parameter_count() {
        if let Some(raw_name) = statement.parameter_name(index)
            && let Some(name) = raw_name.strip_prefix([':', '@', '$'])
            && let Some(value) = bound.get(name)
        {
            used.insert(name.to_owned(), value.clone());
        }
    }
    Ok(used)
}

/// Executes one template through the bounded read-only SQL query plane.
///
/// # Errors
///
/// Returns an error if rendering, parameter binding, or SQL execution fails.
pub(crate) fn execute_template(
    entry: &RegisteredTemplate,
    store: &Store,
    raw_params: &[String],
    limit_override: Option<usize>,
    base_budget: QueryBudget,
) -> Result<TemplateRunReport> {
    let rendered = render_template(entry, store, raw_params)?;
    let budget = QueryBudget {
        limit: limit_override.unwrap_or(rendered.default_limit),
        ..base_budget
    };
    let query = query_sql_with_params(store, &rendered.sql, &rendered.params, budget)?;
    Ok(TemplateRunReport {
        template: rendered.template,
        origin: rendered.origin,
        resolved: rendered.resolved,
        query,
    })
}

pub(crate) fn templates_dir_for_config(config_file_path: &Path) -> PathBuf {
    config_file_path
        .parent()
        .map_or_else(|| PathBuf::from("templates"), |dir| dir.join("templates"))
}

fn load_builtins() -> Result<BTreeMap<String, RegisteredTemplate>> {
    let mut map = BTreeMap::new();
    for &(expected_name, source) in BUILTIN_SOURCES {
        let parsed = parse_template_source(source, &format!("builtin/{expected_name}.sql"))?;
        if parsed.header.name != expected_name {
            bail!(
                "built-in template {expected_name:?} declared mismatched name {:?}",
                parsed.header.name
            );
        }
        map.insert(
            parsed.header.name.clone(),
            RegisteredTemplate {
                origin: TemplateOrigin::Builtin,
                parsed,
            },
        );
    }
    Ok(map)
}

fn discover_user_template_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() && path.extension().is_some_and(|ext| ext == "sql") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests;
