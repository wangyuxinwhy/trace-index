//! `.sql` query template parser and line-granular conditional renderer.
//!
//! Template files are valid `SQL` with a leading `/* trace-index-template ... */`
//! TOML metadata comment block and optional line-level `-- #if <param>`,
//! `-- #if !<param>`, `-- #if <param>=<value>`, `-- #else`, and `-- #endif`
//! directives. Values are never text-interpolated; every parameter binds
//! through `SQLite` native `:param`.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::domain::SEMANTIC_ROLES;

const HEADER_OPEN: &str = "/* trace-index-template";
const DEFAULT_MIN_FORMAT: u32 = 2;
const DEFAULT_TEMPLATE_LIMIT: usize = 100;
const MAX_TEMPLATE_LIMIT: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ParamType {
    String,
    Int,
    Bool,
    Enum,
    List,
    Time,
    SessionRef,
    ItemRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ListItemType {
    String,
    SemanticRole,
    SearchableSemanticRole,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ParamSpec {
    #[serde(rename = "type")]
    pub(crate) kind: ParamType,
    #[serde(default)]
    pub(crate) required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) default: Option<toml::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) min: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) values: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) item_type: Option<ListItemType>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TemplateHeader {
    pub(crate) name: String,
    pub(crate) description: String,
    #[serde(default = "default_min_format")]
    pub(crate) min_format: u32,
    #[serde(default = "default_template_limit")]
    pub(crate) default_limit: usize,
    #[serde(default)]
    pub(crate) params: BTreeMap<String, ParamSpec>,
}

const fn default_min_format() -> u32 {
    DEFAULT_MIN_FORMAT
}

const fn default_template_limit() -> usize {
    DEFAULT_TEMPLATE_LIMIT
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConditionalExpr {
    Present { param: String, negated: bool },
    Equals { param: String, value: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TemplateNode {
    Line(String),
    Conditional {
        condition: ConditionalExpr,
        then_lines: Vec<String>,
        else_lines: Vec<String>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct ParsedTemplate {
    pub(crate) header: TemplateHeader,
    pub(crate) sql_body: String,
    pub(crate) nodes: Vec<TemplateNode>,
}

/// Parses and validates one `.sql` template file.
///
/// # Errors
///
/// Returns an error with file-relative line numbers when the metadata block,
/// parameter specifications, or `-- #if` directives are malformed.
pub(crate) fn parse_template_source(source: &str, origin_label: &str) -> Result<ParsedTemplate> {
    let trimmed_source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let (header_toml, sql_body, body_start_line) =
        split_header_and_body(trimmed_source, origin_label)?;
    let header: TemplateHeader = toml::from_str(header_toml)
        .with_context(|| format!("{origin_label}: invalid TOML in template header"))?;
    validate_header(&header, origin_label)?;
    let nodes = parse_body_directives(sql_body, body_start_line, &header, origin_label)?;
    Ok(ParsedTemplate {
        header,
        sql_body: sql_body.to_owned(),
        nodes,
    })
}

/// Renders template nodes into executable SQL given the active presence/bool
/// flags and bound values for each declared parameter.
pub(crate) fn render_nodes(
    nodes: &[TemplateNode],
    active_flags: &BTreeMap<String, bool>,
    bound: &BTreeMap<String, crate::interface::query::SqlParamValue>,
) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for node in nodes {
        match node {
            TemplateNode::Line(line) => lines.push(line.as_str()),
            TemplateNode::Conditional {
                condition,
                then_lines,
                else_lines,
            } => {
                let take_then = match condition {
                    ConditionalExpr::Present { param, negated } => {
                        let active = active_flags.get(param).copied().unwrap_or(false);
                        if *negated { !active } else { active }
                    }
                    ConditionalExpr::Equals { param, value } => {
                        matches!(
                            bound.get(param),
                            Some(crate::interface::query::SqlParamValue::Text(actual))
                                if actual == value
                        )
                    }
                };
                let branch = if take_then { then_lines } else { else_lines };
                for line in branch {
                    lines.push(line.as_str());
                }
            }
        }
    }
    let mut rendered = lines.join("\n");
    rendered = rendered.trim().to_owned();
    rendered.push('\n');
    rendered
}

fn split_header_and_body<'a>(
    source: &'a str,
    origin_label: &str,
) -> Result<(&'a str, &'a str, usize)> {
    let mut leading_lines = 1_usize;
    let mut rest = source;
    while let Some(line_end) = rest.find('\n') {
        let line = &rest[..line_end];
        if line.trim().is_empty() {
            leading_lines += 1;
            rest = &rest[line_end + 1..];
        } else {
            break;
        }
    }
    let Some(after_open) = rest.strip_prefix(HEADER_OPEN) else {
        bail!(
            "{origin_label}:{leading_lines}: template must start with a `/* trace-index-template` metadata block"
        );
    };
    let after_open = after_open
        .strip_prefix("\r\n")
        .or_else(|| after_open.strip_prefix('\n'))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{origin_label}:{leading_lines}: `/* trace-index-template` must be followed by a newline"
            )
        })?;
    let Some(close_offset) = after_open.find("*/") else {
        bail!("{origin_label}:{leading_lines}: unterminated `/* trace-index-template` block");
    };
    let header_toml = &after_open[..close_offset];
    let after_close = &after_open[close_offset + 2..];
    let header_newlines = header_toml.bytes().filter(|byte| *byte == b'\n').count();
    let body_start_line = leading_lines + 1 + header_newlines;
    let body = after_close
        .strip_prefix("\r\n")
        .or_else(|| after_close.strip_prefix('\n'))
        .unwrap_or(after_close);
    let body_start_line = if body.len() < after_close.len() {
        body_start_line + 1
    } else {
        body_start_line
    };
    if body.trim().is_empty() {
        bail!("{origin_label}:{body_start_line}: template SQL body must not be empty");
    }
    Ok((header_toml, body, body_start_line))
}

fn validate_header(header: &TemplateHeader, origin_label: &str) -> Result<()> {
    if !is_valid_template_name(&header.name) {
        bail!(
            "{origin_label}: invalid template name {:?}; use lowercase ASCII letters, digits, '-' or '_'",
            header.name
        );
    }
    if header.description.trim().is_empty() {
        bail!("{origin_label}: template description must not be empty");
    }
    if !(1..=MAX_TEMPLATE_LIMIT).contains(&header.default_limit) {
        bail!(
            "{origin_label}: default_limit must be between 1 and {MAX_TEMPLATE_LIMIT}, got {}",
            header.default_limit
        );
    }
    for (param_name, spec) in &header.params {
        validate_param_spec(param_name, spec, origin_label)?;
    }
    Ok(())
}

fn validate_param_spec(name: &str, spec: &ParamSpec, origin_label: &str) -> Result<()> {
    if !is_valid_param_name(name) {
        bail!(
            "{origin_label}: invalid parameter name {name:?}; use lowercase ASCII letters, digits, and '_'"
        );
    }
    if spec.kind == ParamType::Bool {
        match &spec.default {
            Some(toml::Value::Boolean(_)) => {}
            Some(_) => {
                bail!("{origin_label}: bool parameter {name:?} default must be a boolean");
            }
            None => {
                bail!(
                    "{origin_label}: bool parameter {name:?} must declare an explicit boolean default (true or false)"
                );
            }
        }
    }
    if let (Some(min), Some(max)) = (spec.min, spec.max)
        && min > max
    {
        bail!("{origin_label}: parameter {name:?} has min ({min}) > max ({max})");
    }
    if spec.kind == ParamType::Enum {
        if matches!(
            spec.item_type,
            Some(ListItemType::SemanticRole | ListItemType::SearchableSemanticRole)
        ) {
            if !spec.values.is_empty() {
                bail!(
                    "{origin_label}: enum parameter {name:?} with semantic role item_type must not duplicate values"
                );
            }
        } else if spec.values.is_empty() {
            bail!(
                "{origin_label}: enum parameter {name:?} must specify non-empty values or a semantic role item_type"
            );
        }
    }
    if let Some(default) = &spec.default {
        validate_default_value(name, spec, default, origin_label)?;
    }
    Ok(())
}

fn validate_default_value(
    name: &str,
    spec: &ParamSpec,
    default: &toml::Value,
    origin_label: &str,
) -> Result<()> {
    match spec.kind {
        ParamType::Int | ParamType::ItemRef => {
            let Some(value) = default.as_integer() else {
                bail!("{origin_label}: int parameter {name:?} default must be an integer");
            };
            if let Some(min) = spec.min
                && value < min
            {
                bail!("{origin_label}: int parameter {name:?} default ({value}) is < min ({min})");
            }
            if let Some(max) = spec.max
                && value > max
            {
                bail!("{origin_label}: int parameter {name:?} default ({value}) is > max ({max})");
            }
        }
        ParamType::Bool => {
            if !default.is_bool() {
                bail!("{origin_label}: bool parameter {name:?} default must be a boolean");
            }
        }
        ParamType::String | ParamType::Time | ParamType::SessionRef => {
            if !default.is_str() {
                bail!("{origin_label}: parameter {name:?} default must be a string");
            }
        }
        ParamType::Enum => {
            let Some(value) = default.as_str() else {
                bail!("{origin_label}: enum parameter {name:?} default must be a string");
            };
            if spec.item_type == Some(ListItemType::SemanticRole) {
                if !SEMANTIC_ROLES.iter().any(|role| role.as_str() == value) {
                    bail!(
                        "{origin_label}: enum parameter {name:?} default {value:?} is not a valid semantic role"
                    );
                }
            } else if !spec.values.iter().any(|candidate| candidate == value) {
                bail!(
                    "{origin_label}: enum parameter {name:?} default {value:?} is not in {:?}",
                    spec.values
                );
            }
        }
        ParamType::List => {
            if !default.is_str() && !default.is_array() {
                bail!(
                    "{origin_label}: list parameter {name:?} default must be a comma-separated string or array of strings"
                );
            }
        }
    }
    Ok(())
}

struct OpenConditional {
    condition: ConditionalExpr,
    start_line: usize,
    in_else: bool,
    then_lines: Vec<String>,
    else_lines: Vec<String>,
}

fn parse_body_directives(
    body: &str,
    body_start_line: usize,
    header: &TemplateHeader,
    origin_label: &str,
) -> Result<Vec<TemplateNode>> {
    let mut nodes = Vec::new();
    let mut open: Option<OpenConditional> = None;

    for (offset, raw_line) in body.lines().enumerate() {
        let line_no = body_start_line + offset;
        let trimmed = raw_line.trim();
        if let Some(directive) = trimmed.strip_prefix("-- #") {
            handle_directive(
                directive.trim(),
                line_no,
                header,
                &mut open,
                &mut nodes,
                origin_label,
            )?;
            continue;
        }
        if let Some(current) = open.as_mut() {
            if current.in_else {
                current.else_lines.push(raw_line.to_owned());
            } else {
                current.then_lines.push(raw_line.to_owned());
            }
        } else {
            nodes.push(TemplateNode::Line(raw_line.to_owned()));
        }
    }

    if let Some(unclosed) = open {
        let param = match &unclosed.condition {
            ConditionalExpr::Present { param, .. } | ConditionalExpr::Equals { param, .. } => {
                param.as_str()
            }
        };
        bail!(
            "{origin_label}:{}: unclosed `-- #if` for parameter {param:?}",
            unclosed.start_line,
        );
    }
    Ok(nodes)
}

fn handle_directive(
    directive: &str,
    line_no: usize,
    header: &TemplateHeader,
    open: &mut Option<OpenConditional>,
    nodes: &mut Vec<TemplateNode>,
    origin_label: &str,
) -> Result<()> {
    if let Some(rest) = directive.strip_prefix("if ") {
        if let Some(existing) = open.as_ref() {
            bail!(
                "{origin_label}:{line_no}: nested `-- #if` is not supported (outer `-- #if` opened at line {})",
                existing.start_line
            );
        }
        let condition = parse_conditional_expr(rest.trim(), line_no, header, origin_label)?;
        *open = Some(OpenConditional {
            condition,
            start_line: line_no,
            in_else: false,
            then_lines: Vec::new(),
            else_lines: Vec::new(),
        });
        return Ok(());
    }
    if directive == "else" {
        let Some(current) = open.as_mut() else {
            bail!("{origin_label}:{line_no}: unexpected `-- #else` without matching `-- #if`");
        };
        if current.in_else {
            bail!("{origin_label}:{line_no}: duplicate `-- #else` in the same `-- #if` block");
        }
        current.in_else = true;
        return Ok(());
    }
    if directive == "endif" {
        let Some(finished) = open.take() else {
            bail!("{origin_label}:{line_no}: unexpected `-- #endif` without matching `-- #if`");
        };
        nodes.push(TemplateNode::Conditional {
            condition: finished.condition,
            then_lines: finished.then_lines,
            else_lines: finished.else_lines,
        });
        return Ok(());
    }
    bail!(
        "{origin_label}:{line_no}: unknown template directive `-- #{directive}`; supported directives are `-- #if <param>`, `-- #if !<param>`, `-- #if <param>=<value>`, `-- #else`, and `-- #endif`"
    );
}

fn parse_conditional_expr(
    expr: &str,
    line_no: usize,
    header: &TemplateHeader,
    origin_label: &str,
) -> Result<ConditionalExpr> {
    if let Some((raw_param, raw_value)) = expr.split_once('=') {
        let param = raw_param.trim();
        let value = raw_value.trim();
        if !is_valid_param_name(param) || value.is_empty() || value.contains(char::is_whitespace) {
            bail!(
                "{origin_label}:{line_no}: invalid `-- #if` equality expression {expr:?}; use `-- #if <param>=<value>`"
            );
        }
        let Some(spec) = header.params.get(param) else {
            bail!("{origin_label}:{line_no}: `-- #if` references undeclared parameter {param:?}");
        };
        if !matches!(spec.kind, ParamType::Enum | ParamType::String) {
            bail!(
                "{origin_label}:{line_no}: `-- #if {param}={value}` is only supported for enum or string parameters"
            );
        }
        if spec.kind == ParamType::Enum
            && !spec.values.is_empty()
            && !spec.values.iter().any(|candidate| candidate == value)
        {
            bail!(
                "{origin_label}:{line_no}: `-- #if {param}={value}` value is not in {:?}",
                spec.values
            );
        }
        return Ok(ConditionalExpr::Equals {
            param: param.to_owned(),
            value: value.to_owned(),
        });
    }

    let (negated, param) = if let Some(inner) = expr.strip_prefix('!') {
        (true, inner.trim())
    } else {
        (false, expr)
    };
    if !is_valid_param_name(param) {
        bail!(
            "{origin_label}:{line_no}: invalid `-- #if` target {expr:?}; use `-- #if <param>`, `-- #if !<param>`, or `-- #if <param>=<value>`"
        );
    }
    if !header.params.contains_key(param) {
        bail!("{origin_label}:{line_no}: `-- #if` references undeclared parameter {param:?}");
    }
    Ok(ConditionalExpr::Present {
        param: param.to_owned(),
        negated,
    })
}

pub(crate) fn is_valid_template_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

fn is_valid_param_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && (name.as_bytes()[0].is_ascii_lowercase() || name.as_bytes()[0] == b'_')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}
