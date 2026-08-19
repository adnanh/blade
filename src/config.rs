use std::{
    collections::{HashMap, HashSet},
    env, fmt, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::theme::{Theme, ThemeCatalog, ThemeOverrides, parse_color};

pub const DEFAULT_FILE_NAME: &str = ".blade";
pub(crate) const THEME_KEYS: &[&str] = &[
    "file",
    "preset",
    "accent",
    "accent_text",
    "muted",
    "text",
    "footer",
    "search",
    "waiting",
    "running",
    "stopping",
    "completed",
    "failed",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Warning,
    Error,
}

impl fmt::Display for Severity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Warning => "warning",
            Self::Error => "error",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub severity: Severity,
    pub location: String,
    pub message: String,
}

impl Issue {
    fn error(location: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            location: location.into(),
            message: message.into(),
        }
    }

    fn warning(location: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            location: location.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for Issue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.location.is_empty() {
            write!(formatter, "{}: {}", self.severity, self.message)
        } else {
            write!(
                formatter,
                "{}: {}: {}",
                self.severity, self.location, self.message
            )
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProjectConfig {
    pub name: String,
    pub root: PathBuf,
    pub path: PathBuf,
    pub max_log_lines: usize,
    pub theme: Theme,
    pub theme_preset: String,
    pub theme_overrides: ThemeOverrides,
    pub theme_configured: bool,
    pub theme_preset_configured: bool,
    pub theme_catalog: ThemeCatalog,
    pub theme_file: Option<PathBuf>,
    pub theme_file_is_global: bool,
    pub groups: Vec<GroupConfig>,
}

impl ProjectConfig {
    pub fn commands(&self) -> impl Iterator<Item = &CommandConfig> {
        self.groups.iter().flat_map(|group| group.commands.iter())
    }

    pub fn command(&self, name: &str) -> Option<&CommandConfig> {
        self.commands().find(|command| command.id == name)
    }
}

pub fn action_id(parent_id: &str, action_name: &str) -> String {
    format!("{parent_id}::action::{action_name}")
}

#[derive(Debug, Clone)]
pub struct GroupConfig {
    pub project: Option<String>,
    pub name: String,
    pub project_file: PathBuf,
    pub project_root: PathBuf,
    pub commands: Vec<CommandConfig>,
}

#[derive(Debug, Clone)]
pub struct CommandConfig {
    pub id: String,
    pub name: String,
    pub shell: PathBuf,
    pub project_root: PathBuf,
    pub project_file: PathBuf,
    pub max_log_lines: usize,
    pub run: String,
    pub cwd: PathBuf,
    pub shell_setup: Vec<String>,
    pub pre: Vec<String>,
    pub wait_for: Vec<WaitCondition>,
    pub autostart: bool,
    pub log_dir: Option<PathBuf>,
    pub log_file: Option<PathBuf>,
    pub log_rotate_bytes: Option<u64>,
    pub log_rotate_keep: usize,
    pub stop_timeout: f64,
    pub actions: Vec<ActionConfig>,
}

#[derive(Debug, Clone)]
pub struct ActionConfig {
    pub id: String,
    pub parent_id: String,
    pub name: String,
    pub shell: PathBuf,
    pub project_root: PathBuf,
    pub project_file: PathBuf,
    pub max_log_lines: usize,
    pub run: String,
    pub cwd: PathBuf,
    pub shell_setup: Vec<String>,
    pub pre: Vec<String>,
    pub log_dir: Option<PathBuf>,
    pub log_file: Option<PathBuf>,
    pub log_rotate_bytes: Option<u64>,
    pub log_rotate_keep: usize,
    pub stop_timeout: f64,
    pub requires_stopped: bool,
    pub restart_after: RestartAfter,
}

impl ActionConfig {
    pub fn runtime_config(&self) -> CommandConfig {
        CommandConfig {
            id: self.id.clone(),
            // Keep inherited log directories collision-free when multiple
            // parents define an action with the same display name.
            name: format!("{} - {}", self.parent_id, self.name),
            shell: self.shell.clone(),
            project_root: self.project_root.clone(),
            project_file: self.project_file.clone(),
            max_log_lines: self.max_log_lines,
            run: self.run.clone(),
            cwd: self.cwd.clone(),
            shell_setup: self.shell_setup.clone(),
            pre: self.pre.clone(),
            wait_for: Vec::new(),
            autostart: false,
            log_dir: self.log_dir.clone(),
            log_file: self.log_file.clone(),
            log_rotate_bytes: self.log_rotate_bytes,
            log_rotate_keep: self.log_rotate_keep,
            stop_timeout: self.stop_timeout,
            actions: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartAfter {
    Never,
    IfRunning,
    Always,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WaitCondition {
    pub command: String,
    pub readiness: Readiness,
    pub timeout: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Readiness {
    Keyword { value: String, case_sensitive: bool },
    Idle { seconds: f64 },
    Delay { seconds: f64 },
}

#[derive(Debug)]
pub struct ValidationReport {
    pub project: Option<ProjectConfig>,
    pub issues: Vec<Issue>,
}

impl ValidationReport {
    pub fn is_valid(&self) -> bool {
        self.project.is_some()
            && !self
                .issues
                .iter()
                .any(|issue| issue.severity == Severity::Error)
    }

    pub fn error_count(&self) -> usize {
        self.issues
            .iter()
            .filter(|issue| issue.severity == Severity::Error)
            .count()
    }

    pub fn warning_count(&self) -> usize {
        self.issues
            .iter()
            .filter(|issue| issue.severity == Severity::Warning)
            .count()
    }
}

#[derive(Debug, Deserialize)]
struct RawProject {
    version: Option<u32>,
    name: Option<String>,
    shell: Option<PathBuf>,
    log_dir: Option<PathBuf>,
    log_rotate_bytes: Option<u64>,
    log_rotate_keep: Option<usize>,
    max_log_lines: Option<usize>,
    stop_timeout: Option<f64>,
    theme: Option<RawTheme>,
    groups: Option<Vec<RawGroup>>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawTheme {
    pub(crate) file: Option<PathBuf>,
    pub(crate) preset: Option<String>,
    accent: Option<String>,
    accent_text: Option<String>,
    muted: Option<String>,
    text: Option<String>,
    footer: Option<String>,
    search: Option<String>,
    waiting: Option<String>,
    running: Option<String>,
    stopping: Option<String>,
    completed: Option<String>,
    failed: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawGroup {
    name: Option<String>,
    #[serde(default)]
    commands: Vec<RawCommand>,
}

#[derive(Debug, Deserialize)]
struct RawCommand {
    name: Option<String>,
    run: Option<String>,
    cwd: Option<PathBuf>,
    #[serde(default)]
    shell_setup: Vec<String>,
    #[serde(default)]
    pre: Vec<String>,
    #[serde(default)]
    wait_for: Vec<RawWaitCondition>,
    #[serde(default)]
    autostart: bool,
    log_file: Option<PathBuf>,
    log_rotate_bytes: Option<u64>,
    log_rotate_keep: Option<usize>,
    stop_timeout: Option<f64>,
    #[serde(default)]
    actions: Vec<RawAction>,
}

#[derive(Debug, Deserialize)]
struct RawAction {
    name: Option<String>,
    run: Option<String>,
    cwd: Option<PathBuf>,
    #[serde(default)]
    pre: Vec<String>,
    log_file: Option<PathBuf>,
    log_rotate_bytes: Option<u64>,
    log_rotate_keep: Option<usize>,
    stop_timeout: Option<f64>,
    #[serde(default)]
    requires_stopped: bool,
    restart_after: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawWaitCondition {
    command: Option<String>,
    kind: Option<String>,
    value: Option<String>,
    seconds: Option<f64>,
    timeout: Option<f64>,
    #[serde(default = "default_true")]
    case_sensitive: bool,
}

fn default_true() -> bool {
    true
}

pub fn find_project_file(start: &Path) -> Option<PathBuf> {
    if start.is_file() {
        return Some(start.to_path_buf());
    }
    let mut current = absolutize(start);
    loop {
        let candidate = current.join(DEFAULT_FILE_NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

pub fn validate_file(path: &Path) -> ValidationReport {
    validate_file_with_catalog(path, &ThemeCatalog::default())
}

#[cfg(test)]
pub fn validate_file_for_combined(path: &Path) -> ValidationReport {
    validate_file_for_combined_with_catalog(path, &ThemeCatalog::default())
}

pub fn validate_file_with_catalog(path: &Path, catalog: &ThemeCatalog) -> ValidationReport {
    validate_file_with_external_dependencies(path, false, catalog)
}

pub fn validate_file_for_combined_with_catalog(
    path: &Path,
    catalog: &ThemeCatalog,
) -> ValidationReport {
    validate_file_with_external_dependencies(path, true, catalog)
}

fn validate_file_with_external_dependencies(
    path: &Path,
    allow_external_dependencies: bool,
    catalog: &ThemeCatalog,
) -> ValidationReport {
    let path = absolutize(path);
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) => {
            return ValidationReport {
                project: None,
                issues: vec![Issue::error(path.display().to_string(), error.to_string())],
            };
        }
    };

    let value = match source.parse::<toml::Table>() {
        Ok(table) => toml::Value::Table(table),
        Err(error) => {
            return ValidationReport {
                project: None,
                issues: vec![Issue::error(
                    path.display().to_string(),
                    format!("invalid TOML: {error}"),
                )],
            };
        }
    };

    let mut issues = Vec::new();
    collect_unknown_keys(&value, &mut issues);
    let raw = match toml::from_str::<RawProject>(&source) {
        Ok(raw) => raw,
        Err(error) => {
            issues.push(Issue::error(
                path.display().to_string(),
                format!("invalid configuration: {error}"),
            ));
            return ValidationReport {
                project: None,
                issues,
            };
        }
    };

    let project = build_project(raw, path, &mut issues, allow_external_dependencies, catalog);
    ValidationReport {
        project: Some(project),
        issues,
    }
}

pub fn combine_projects(
    path: PathBuf,
    projects: Vec<(String, ProjectConfig)>,
) -> Result<ProjectConfig> {
    if projects.is_empty() {
        bail!("cannot create a combined session without projects");
    }

    let root = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let max_log_lines = projects
        .iter()
        .map(|(_, project)| project.max_log_lines)
        .max()
        .unwrap_or(100_000);
    let mut project_names = HashSet::new();
    let mut command_ids = HashMap::new();
    for (project_index, (project_name, project)) in projects.iter().enumerate() {
        let project_name = project_name.trim();
        if project_name.is_empty() {
            bail!("combined project names must not be empty");
        }
        if !project_names.insert(project_name.to_owned()) {
            bail!("duplicate combined project name {project_name:?}");
        }
        for command in project.commands() {
            command_ids.insert(
                (project_name.to_owned(), command.name.clone()),
                format!("{project_index}::{}", command.id),
            );
        }
    }

    let mut groups = Vec::new();
    for (project_index, (project_name, project)) in projects.into_iter().enumerate() {
        let project_name = project_name.trim().to_owned();
        for mut group in project.groups {
            group.project = Some(project_name.clone());
            for command in &mut group.commands {
                command.id = format!("{project_index}::{}", command.id);
                for action in &mut command.actions {
                    action.parent_id = command.id.clone();
                    action.id = action_id(&command.id, &action.name);
                }
                for wait in &mut command.wait_for {
                    let target = if let Some(target) =
                        command_ids.get(&(project_name.clone(), wait.command.clone()))
                    {
                        target
                    } else {
                        let (target_project, target_command) =
                            qualified_command_reference(&wait.command).with_context(|| {
                                format!(
                                    "command {:?} in project {:?} has unresolved dependency {:?}",
                                    command.name, project_name, wait.command
                                )
                            })?;
                        command_ids
                            .get(&(target_project.to_owned(), target_command.to_owned()))
                            .with_context(|| {
                                format!(
                                    "command {:?} in project {:?} references missing cross-project command {:?}",
                                    command.name, project_name, wait.command
                                )
                            })?
                    };
                    wait.command = target.clone();
                }
            }
            groups.push(group);
        }
    }

    let combined = ProjectConfig {
        name: "All projects".to_owned(),
        root,
        path,
        max_log_lines,
        theme: Theme::default(),
        theme_preset: "default".to_owned(),
        theme_overrides: ThemeOverrides::default(),
        theme_configured: false,
        theme_preset_configured: false,
        theme_catalog: ThemeCatalog::default(),
        theme_file: None,
        theme_file_is_global: false,
        groups,
    };
    let mut dependency_issues = Vec::new();
    validate_dependencies(&combined, &mut dependency_issues, false);
    if !dependency_issues.is_empty() {
        bail!(
            "combined project dependencies are invalid: {}",
            dependency_issues
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    Ok(combined)
}

pub fn set_theme_preset(path: &Path, preset: &str) -> Result<()> {
    if !valid_theme_name(preset) {
        bail!("invalid theme name {preset:?}");
    }
    let source =
        fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    let updated = update_theme_value_source(
        &source,
        "preset",
        &toml::Value::String(preset.to_owned()).to_string(),
    );
    updated
        .parse::<toml::Table>()
        .context("theme update would produce invalid TOML")?;
    fs::write(path, updated).with_context(|| format!("could not update {}", path.display()))?;
    Ok(())
}

pub fn set_project_theme_preset(path: &Path, preset: &str) -> Result<()> {
    if !valid_theme_name(preset) {
        bail!("invalid theme name {preset:?}");
    }
    replace_theme_table(
        path,
        "preset",
        &toml::Value::String(preset.to_owned()).to_string(),
    )
}

pub fn set_project_theme_file(path: &Path, theme_path: &Path) -> Result<()> {
    let portable_path = env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|home| {
            theme_path
                .strip_prefix(home)
                .ok()
                .map(|relative| Path::new("~").join(relative))
        })
        .unwrap_or_else(|| theme_path.to_path_buf());
    replace_theme_table(
        path,
        "file",
        &toml::Value::String(portable_path.to_string_lossy().into_owned()).to_string(),
    )
}

fn replace_theme_table(path: &Path, key: &str, value: &str) -> Result<()> {
    let source =
        fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    let newline = preferred_newline(&source);
    let table = format!("[theme]{newline}{key} = {value}{newline}{newline}");
    let lines = source_lines(&source);
    let updated = if let Some(header_index) = lines
        .iter()
        .position(|(_, _, line)| toml_code(line) == "[theme]")
    {
        let start = lines[header_index].0;
        let end = lines
            .iter()
            .skip(header_index + 1)
            .find(|(_, _, line)| toml_code(line).starts_with('['))
            .map(|(start, _, _)| *start)
            .unwrap_or(source.len());
        let mut updated = source.clone();
        updated.replace_range(start..end, &table);
        updated
    } else {
        let insertion_at = lines
            .iter()
            .find(|(_, _, line)| toml_code(line) == "[[groups]]")
            .map(|(start, _, _)| *start)
            .unwrap_or(source.len());
        let separator = if insertion_at > 0 && !source[..insertion_at].ends_with('\n') {
            newline
        } else {
            ""
        };
        let mut updated = source.clone();
        updated.insert_str(insertion_at, &format!("{separator}{table}"));
        updated
    };
    updated
        .parse::<toml::Table>()
        .context("theme update would produce invalid TOML")?;
    fs::write(path, updated).with_context(|| format!("could not update {}", path.display()))?;
    Ok(())
}

fn update_theme_value_source(source: &str, key: &str, value: &str) -> String {
    let lines = source_lines(source);
    let theme_header = lines
        .iter()
        .position(|(_, _, line)| toml_code(line) == "[theme]");

    if let Some(header_index) = theme_header {
        let (_, header_end, _) = lines[header_index];
        let table_end_index = lines
            .iter()
            .enumerate()
            .skip(header_index + 1)
            .find(|(_, (_, _, line))| toml_code(line).starts_with('['))
            .map(|(index, _)| index)
            .unwrap_or(lines.len());
        if let Some((start, end, line)) = lines[header_index + 1..table_end_index]
            .iter()
            .find(|(_, _, line)| toml_key(line) == Some(key))
            .copied()
        {
            let indentation = &line[..line.len() - line.trim_start().len()];
            let ending = line_ending(line);
            let mut updated = source.to_owned();
            updated.replace_range(start..end, &format!("{indentation}{key} = {value}{ending}"));
            return updated;
        }

        let newline = preferred_newline(source);
        let separator = if source[..header_end].ends_with('\n') {
            ""
        } else {
            newline
        };
        let mut updated = source.to_owned();
        updated.insert_str(header_end, &format!("{separator}{key} = {value}{newline}"));
        return updated;
    }

    let insertion_at = lines
        .iter()
        .find(|(_, _, line)| {
            toml_code(line)
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect::<String>()
                == "[[groups]]"
        })
        .map(|(start, _, _)| *start)
        .unwrap_or(source.len());
    let newline = preferred_newline(source);
    let separator = if insertion_at > 0 && !source[..insertion_at].ends_with('\n') {
        newline
    } else {
        ""
    };
    let mut updated = source.to_owned();
    updated.insert_str(
        insertion_at,
        &format!("{separator}[theme]{newline}{key} = {value}{newline}{newline}"),
    );
    updated
}

fn source_lines(source: &str) -> Vec<(usize, usize, &str)> {
    let mut offset = 0;
    source
        .split_inclusive('\n')
        .map(|line| {
            let start = offset;
            offset += line.len();
            (start, offset, line)
        })
        .collect()
}

fn toml_code(line: &str) -> &str {
    line.trim().split('#').next().unwrap_or_default().trim()
}

fn toml_key(line: &str) -> Option<&str> {
    toml_code(line).split_once('=').map(|(key, _)| key.trim())
}

fn line_ending(line: &str) -> &'static str {
    if line.ends_with("\r\n") {
        "\r\n"
    } else if line.ends_with('\n') {
        "\n"
    } else {
        ""
    }
}

fn preferred_newline(source: &str) -> &'static str {
    if source.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

fn build_project(
    raw: RawProject,
    path: PathBuf,
    issues: &mut Vec<Issue>,
    allow_external_dependencies: bool,
    catalog: &ThemeCatalog,
) -> ProjectConfig {
    let root = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    if raw.version.unwrap_or(1) != 1 {
        issues.push(Issue::error(
            "version",
            format!(
                "unsupported version {}; expected 1",
                raw.version.unwrap_or_default()
            ),
        ));
    }

    let name = non_empty(raw.name.as_deref()).unwrap_or_else(|| {
        issues.push(Issue::warning(
            "name",
            "missing project name; using the directory name",
        ));
        root.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Blade project")
            .to_owned()
    });

    let raw_shell = raw
        .shell
        .or_else(|| env::var_os("SHELL").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/bin/sh"));
    let shell = find_executable(&raw_shell).unwrap_or_else(|| {
        issues.push(Issue::error(
            "shell",
            format!("{} was not found or is not executable", raw_shell.display()),
        ));
        raw_shell
    });

    let max_log_lines = raw.max_log_lines.unwrap_or(100_000);
    if max_log_lines < 100 {
        issues.push(Issue::error("max_log_lines", "must be at least 100"));
    }

    let project_stop_timeout = raw.stop_timeout.unwrap_or(10.0);
    if !project_stop_timeout.is_finite() || project_stop_timeout <= 0.0 {
        issues.push(Issue::error("stop_timeout", "must be a positive number"));
    }
    let project_stop_timeout = if project_stop_timeout.is_finite() && project_stop_timeout > 0.0 {
        project_stop_timeout
    } else {
        10.0
    };

    let theme_configured = raw.theme.is_some();
    let raw_theme = raw.theme.unwrap_or_default();
    let theme_preset_configured =
        non_empty(raw_theme.preset.as_deref()).is_some() || raw_theme.file.is_some();
    let (theme, theme_preset, theme_overrides) = if raw_theme.file.is_some() {
        build_theme_reference(raw_theme, &root, issues, catalog)
    } else {
        build_theme(raw_theme, issues, catalog)
    };

    let log_dir = raw.log_dir.map(|path| resolve_path(&root, &path));
    let project_log_rotate_bytes = match raw.log_rotate_bytes {
        Some(0) => {
            issues.push(Issue::error(
                "log_rotate_bytes",
                "must be a positive integer",
            ));
            None
        }
        value => value,
    };
    let project_log_rotate_keep = match raw.log_rotate_keep.unwrap_or(5) {
        0 => {
            issues.push(Issue::error(
                "log_rotate_keep",
                "must be a positive integer",
            ));
            5
        }
        value => value,
    };
    let raw_groups = raw.groups.unwrap_or_default();
    if raw_groups.is_empty() {
        issues.push(Issue::warning(
            "groups",
            "project has no groups or commands",
        ));
    }

    let mut group_names = HashSet::new();
    let mut command_names = HashSet::new();
    let mut groups = Vec::new();
    for (group_index, raw_group) in raw_groups.into_iter().enumerate() {
        let group_location = format!("groups[{group_index}]");
        let group_name = non_empty(raw_group.name.as_deref()).unwrap_or_else(|| {
            issues.push(Issue::error(
                format!("{group_location}.name"),
                "group name is required",
            ));
            format!("group-{}", group_index + 1)
        });
        if !group_names.insert(group_name.clone()) {
            issues.push(Issue::error(
                format!("{group_location}.name"),
                format!("duplicate group name {group_name:?}"),
            ));
        }
        if raw_group.commands.is_empty() {
            issues.push(Issue::warning(&group_location, "group has no commands"));
        }

        let mut commands = Vec::new();
        for (command_index, raw_command) in raw_group.commands.into_iter().enumerate() {
            let location = format!("{group_location}.commands[{command_index}]");
            let command_name = non_empty(raw_command.name.as_deref()).unwrap_or_else(|| {
                issues.push(Issue::error(
                    format!("{location}.name"),
                    "command name is required",
                ));
                format!("command-{}-{}", group_index + 1, command_index + 1)
            });
            if !command_names.insert(command_name.clone()) {
                issues.push(Issue::error(
                    format!("{location}.name"),
                    format!("duplicate command name {command_name:?}"),
                ));
            }
            let run = non_empty(raw_command.run.as_deref()).unwrap_or_else(|| {
                issues.push(Issue::error(
                    format!("{location}.run"),
                    "run command is required",
                ));
                String::new()
            });
            let cwd = resolve_path(&root, raw_command.cwd.as_deref().unwrap_or(Path::new(".")));
            if !cwd.is_dir() {
                issues.push(Issue::warning(
                    format!("{location}.cwd"),
                    format!("directory does not exist: {}", cwd.display()),
                ));
            }
            let stop_timeout = raw_command.stop_timeout.unwrap_or(project_stop_timeout);
            if !stop_timeout.is_finite() || stop_timeout <= 0.0 {
                issues.push(Issue::error(
                    format!("{location}.stop_timeout"),
                    "must be a positive number",
                ));
            }
            let stop_timeout = if stop_timeout > 0.0 && stop_timeout.is_finite() {
                stop_timeout
            } else {
                5.0
            };
            let wait_for = raw_command
                .wait_for
                .into_iter()
                .enumerate()
                .filter_map(|(index, wait)| {
                    build_wait(wait, &format!("{location}.wait_for[{index}]"), issues)
                })
                .collect();
            let log_rotate_bytes = match raw_command.log_rotate_bytes {
                Some(0) => {
                    issues.push(Issue::error(
                        format!("{location}.log_rotate_bytes"),
                        "must be a positive integer",
                    ));
                    project_log_rotate_bytes
                }
                Some(value) => Some(value),
                None => project_log_rotate_bytes,
            };
            let log_rotate_keep = match raw_command
                .log_rotate_keep
                .unwrap_or(project_log_rotate_keep)
            {
                0 => {
                    issues.push(Issue::error(
                        format!("{location}.log_rotate_keep"),
                        "must be a positive integer",
                    ));
                    project_log_rotate_keep
                }
                value => value,
            };
            let shell_setup = raw_command
                .shell_setup
                .into_iter()
                .filter(|command| !command.trim().is_empty())
                .collect::<Vec<_>>();
            let pre = raw_command
                .pre
                .into_iter()
                .filter(|command| !command.trim().is_empty())
                .collect::<Vec<_>>();
            let mut action_names = HashSet::new();
            let actions = raw_command
                .actions
                .into_iter()
                .enumerate()
                .map(|(action_index, raw_action)| {
                    let action_location = format!("{location}.actions[{action_index}]");
                    let action_name = non_empty(raw_action.name.as_deref()).unwrap_or_else(|| {
                        issues.push(Issue::error(
                            format!("{action_location}.name"),
                            "action name is required",
                        ));
                        format!("action-{}", action_index + 1)
                    });
                    if !action_names.insert(action_name.clone()) {
                        issues.push(Issue::error(
                            format!("{action_location}.name"),
                            format!("duplicate action name {action_name:?}"),
                        ));
                    }
                    let action_run = non_empty(raw_action.run.as_deref()).unwrap_or_else(|| {
                        issues.push(Issue::error(
                            format!("{action_location}.run"),
                            "run command is required",
                        ));
                        String::new()
                    });
                    let action_cwd = raw_action
                        .cwd
                        .as_deref()
                        .map(|path| resolve_path(&root, path))
                        .unwrap_or_else(|| cwd.clone());
                    if !action_cwd.is_dir() {
                        issues.push(Issue::warning(
                            format!("{action_location}.cwd"),
                            format!("directory does not exist: {}", action_cwd.display()),
                        ));
                    }
                    let action_stop_timeout =
                        raw_action.stop_timeout.unwrap_or(stop_timeout);
                    if !action_stop_timeout.is_finite() || action_stop_timeout <= 0.0 {
                        issues.push(Issue::error(
                            format!("{action_location}.stop_timeout"),
                            "must be a positive number",
                        ));
                    }
                    let action_stop_timeout = if action_stop_timeout.is_finite()
                        && action_stop_timeout > 0.0
                    {
                        action_stop_timeout
                    } else {
                        stop_timeout
                    };
                    let action_log_rotate_bytes = match raw_action.log_rotate_bytes {
                        Some(0) => {
                            issues.push(Issue::error(
                                format!("{action_location}.log_rotate_bytes"),
                                "must be a positive integer",
                            ));
                            log_rotate_bytes
                        }
                        Some(value) => Some(value),
                        None => log_rotate_bytes,
                    };
                    let action_log_rotate_keep =
                        match raw_action.log_rotate_keep.unwrap_or(log_rotate_keep) {
                            0 => {
                                issues.push(Issue::error(
                                    format!("{action_location}.log_rotate_keep"),
                                    "must be a positive integer",
                                ));
                                log_rotate_keep
                            }
                            value => value,
                        };
                    let restart_after = match raw_action.restart_after.as_deref().unwrap_or("never") {
                        "never" => RestartAfter::Never,
                        "if-running" => RestartAfter::IfRunning,
                        "always" => RestartAfter::Always,
                        value => {
                            issues.push(Issue::error(
                                format!("{action_location}.restart_after"),
                                format!(
                                    "unknown restart policy {value:?}; expected never, if-running, or always"
                                ),
                            ));
                            RestartAfter::Never
                        }
                    };
                    ActionConfig {
                        id: action_id(&command_name, &action_name),
                        parent_id: command_name.clone(),
                        name: action_name,
                        shell: shell.clone(),
                        project_root: root.clone(),
                        project_file: path.clone(),
                        max_log_lines: max_log_lines.max(100),
                        run: action_run,
                        cwd: action_cwd,
                        shell_setup: shell_setup.clone(),
                        pre: raw_action
                            .pre
                            .into_iter()
                            .filter(|command| !command.trim().is_empty())
                            .collect(),
                        log_dir: log_dir.clone(),
                        log_file: raw_action
                            .log_file
                            .map(|path| resolve_path(&root, &path)),
                        log_rotate_bytes: action_log_rotate_bytes,
                        log_rotate_keep: action_log_rotate_keep,
                        stop_timeout: action_stop_timeout,
                        requires_stopped: raw_action.requires_stopped,
                        restart_after,
                    }
                })
                .collect();
            commands.push(CommandConfig {
                id: command_name.clone(),
                name: command_name,
                shell: shell.clone(),
                project_root: root.clone(),
                project_file: path.clone(),
                max_log_lines: max_log_lines.max(100),
                run,
                cwd,
                shell_setup,
                pre,
                wait_for,
                autostart: raw_command.autostart,
                log_dir: log_dir.clone(),
                log_file: raw_command.log_file.map(|path| resolve_path(&root, &path)),
                log_rotate_bytes,
                log_rotate_keep,
                stop_timeout,
                actions,
            });
        }
        groups.push(GroupConfig {
            project: None,
            name: group_name,
            project_file: path.clone(),
            project_root: root.clone(),
            commands,
        });
    }

    let project = ProjectConfig {
        name,
        root,
        path: path.clone(),
        max_log_lines: max_log_lines.max(100),
        theme,
        theme_preset,
        theme_overrides,
        theme_configured,
        theme_preset_configured,
        theme_catalog: catalog.clone(),
        theme_file: Some(path.clone()),
        theme_file_is_global: false,
        groups,
    };
    validate_dependencies(&project, issues, allow_external_dependencies);
    project
}

pub(crate) fn build_theme(
    raw: RawTheme,
    issues: &mut Vec<Issue>,
    catalog: &ThemeCatalog,
) -> (Theme, String, ThemeOverrides) {
    let preset = non_empty(raw.preset.as_deref())
        .unwrap_or_else(|| "default".to_owned())
        .to_ascii_lowercase();
    let preset = if preset == "blade" {
        "default".to_owned()
    } else {
        preset
    };
    let (mut theme, preset) = match catalog.resolve(&preset) {
        Some(theme) => (theme, preset),
        None => {
            issues.push(Issue::error(
                "theme.preset",
                format!(
                    "unknown preset {preset:?}; expected one of: {}",
                    catalog.names().collect::<Vec<_>>().join(", ")
                ),
            ));
            (Theme::default(), "default".to_owned())
        }
    };
    let mut overrides = ThemeOverrides::default();

    macro_rules! apply_color {
        ($field:ident) => {
            if let Some(value) = raw.$field {
                match parse_color(&value) {
                    Some(color) => {
                        theme.$field = color;
                        overrides.$field = Some(color);
                    }
                    None => issues.push(Issue::error(
                        concat!("theme.", stringify!($field)),
                        format!(
                            "invalid color {value:?}; use a named color, #RRGGBB, or ansi:0-255"
                        ),
                    )),
                }
            }
        };
    }

    apply_color!(accent);
    apply_color!(accent_text);
    apply_color!(muted);
    apply_color!(text);
    apply_color!(footer);
    apply_color!(search);
    apply_color!(waiting);
    apply_color!(running);
    apply_color!(stopping);
    apply_color!(completed);
    apply_color!(failed);
    (theme, preset, overrides)
}

fn build_theme_reference(
    mut raw: RawTheme,
    root: &Path,
    issues: &mut Vec<Issue>,
    catalog: &ThemeCatalog,
) -> (Theme, String, ThemeOverrides) {
    let reference = raw.file.take().expect("theme file was checked by caller");
    if raw.preset.is_some()
        || raw.accent.is_some()
        || raw.accent_text.is_some()
        || raw.muted.is_some()
        || raw.text.is_some()
        || raw.footer.is_some()
        || raw.search.is_some()
        || raw.waiting.is_some()
        || raw.running.is_some()
        || raw.stopping.is_some()
        || raw.completed.is_some()
        || raw.failed.is_some()
    {
        issues.push(Issue::error(
            "theme",
            "file cannot be combined with preset or color fields",
        ));
    }

    let path = resolve_path(root, &reference);
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) => {
            issues.push(Issue::error(
                "theme.file",
                format!("could not read {}: {error}", path.display()),
            ));
            return (
                Theme::default(),
                "default".to_owned(),
                ThemeOverrides::default(),
            );
        }
    };
    let mut document = match source.parse::<toml::Table>() {
        Ok(document) => document,
        Err(error) => {
            issues.push(Issue::error(
                "theme.file",
                format!("invalid theme file {}: {error}", path.display()),
            ));
            return (
                Theme::default(),
                "default".to_owned(),
                ThemeOverrides::default(),
            );
        }
    };
    let mut table = if document.len() == 1
        && let Some(theme) = document.remove("theme")
    {
        match theme.as_table().cloned() {
            Some(table) => table,
            None => {
                issues.push(Issue::error(
                    "theme.file",
                    format!("{} [theme] value must be a table", path.display()),
                ));
                return (
                    Theme::default(),
                    "default".to_owned(),
                    ThemeOverrides::default(),
                );
            }
        }
    } else {
        document
    };
    table.remove("description");
    if let Some(key) = table
        .keys()
        .find(|key| key.as_str() == "file" || !THEME_KEYS.contains(&key.as_str()))
    {
        issues.push(Issue::error(
            "theme.file",
            format!("{} contains unknown key {key:?}", path.display()),
        ));
        return (
            Theme::default(),
            "default".to_owned(),
            ThemeOverrides::default(),
        );
    }
    let file_theme = match toml::Value::Table(table).try_into::<RawTheme>() {
        Ok(theme) => theme,
        Err(error) => {
            issues.push(Issue::error(
                "theme.file",
                format!("invalid theme file {}: {error}", path.display()),
            ));
            return (
                Theme::default(),
                "default".to_owned(),
                ThemeOverrides::default(),
            );
        }
    };
    let (theme, file_preset, _) = build_theme(file_theme, issues, catalog);
    let canonical_path = fs::canonicalize(&path).unwrap_or(path);
    let preset = catalog
        .custom_name_for_source(&canonical_path)
        .unwrap_or(&file_preset)
        .to_owned();
    (theme, preset, ThemeOverrides::default())
}

pub(crate) fn valid_theme_name(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

fn build_wait(
    raw: RawWaitCondition,
    location: &str,
    issues: &mut Vec<Issue>,
) -> Option<WaitCondition> {
    let command = non_empty(raw.command.as_deref()).unwrap_or_else(|| {
        issues.push(Issue::error(
            format!("{location}.command"),
            "dependency command is required",
        ));
        String::new()
    });
    let kind = non_empty(raw.kind.as_deref()).unwrap_or_else(|| {
        issues.push(Issue::error(
            format!("{location}.kind"),
            "readiness kind is required",
        ));
        String::new()
    });

    let readiness = match kind.as_str() {
        "keyword" => {
            let value = non_empty(raw.value.as_deref()).unwrap_or_else(|| {
                issues.push(Issue::error(
                    format!("{location}.value"),
                    "keyword value is required",
                ));
                String::new()
            });
            Readiness::Keyword {
                value,
                case_sensitive: raw.case_sensitive,
            }
        }
        "idle" | "delay" => {
            let seconds = raw.seconds.unwrap_or(0.0);
            if !seconds.is_finite() || seconds <= 0.0 {
                issues.push(Issue::error(
                    format!("{location}.seconds"),
                    "must be a positive number",
                ));
            }
            if kind == "idle" {
                Readiness::Idle {
                    seconds: seconds.max(0.0),
                }
            } else {
                Readiness::Delay {
                    seconds: seconds.max(0.0),
                }
            }
        }
        _ => {
            issues.push(Issue::error(
                format!("{location}.kind"),
                "must be one of: keyword, idle, delay",
            ));
            return None;
        }
    };

    let timeout = match raw.timeout.unwrap_or(60.0) {
        0.0 => None,
        timeout if timeout.is_finite() && timeout > 0.0 => Some(timeout),
        _ => {
            issues.push(Issue::error(
                format!("{location}.timeout"),
                "must be zero (no timeout) or a positive number",
            ));
            Some(60.0)
        }
    };
    Some(WaitCondition {
        command,
        readiness,
        timeout,
    })
}

fn validate_dependencies(
    project: &ProjectConfig,
    issues: &mut Vec<Issue>,
    allow_external_dependencies: bool,
) {
    let commands: HashMap<_, _> = project
        .commands()
        .map(|command| (command.id.as_str(), command))
        .collect();
    let mut graph: HashMap<&str, Vec<&str>> = HashMap::new();
    for command in project.commands() {
        let mut dependencies = Vec::new();
        for (index, wait) in command.wait_for.iter().enumerate() {
            let location = format!("command {:?}.wait_for[{index}]", command.name);
            if wait.command == command.id {
                issues.push(Issue::error(location, "command cannot wait for itself"));
            } else if commands.contains_key(wait.command.as_str()) {
                dependencies.push(wait.command.as_str());
            } else if wait.command.contains("::") {
                if qualified_command_reference(&wait.command).is_none() {
                    issues.push(Issue::error(
                        location,
                        "invalid cross-project command reference; expected Project::Command",
                    ));
                } else if !allow_external_dependencies {
                    issues.push(Issue::error(
                        location,
                        format!(
                            "cross-project dependency {:?} requires launching All projects from a global project list",
                            wait.command
                        ),
                    ));
                }
            } else {
                issues.push(Issue::error(
                    location,
                    format!("references missing command {:?}", wait.command),
                ));
            }
        }
        graph.insert(command.id.as_str(), dependencies);
    }

    let mut visiting = Vec::new();
    let mut visited = HashSet::new();
    let mut cycles = HashSet::new();
    for command in project.commands() {
        visit_dependency(
            command.id.as_str(),
            &graph,
            &mut visiting,
            &mut visited,
            &mut cycles,
            issues,
        );
    }
}

fn qualified_command_reference(reference: &str) -> Option<(&str, &str)> {
    let (project, command) = reference.split_once("::")?;
    if project.trim().is_empty() || command.trim().is_empty() || command.contains("::") {
        return None;
    }
    Some((project.trim(), command.trim()))
}

fn visit_dependency<'a>(
    name: &'a str,
    graph: &HashMap<&'a str, Vec<&'a str>>,
    visiting: &mut Vec<&'a str>,
    visited: &mut HashSet<&'a str>,
    cycles: &mut HashSet<String>,
    issues: &mut Vec<Issue>,
) {
    if let Some(start) = visiting.iter().position(|candidate| *candidate == name) {
        let mut cycle = visiting[start..].to_vec();
        cycle.push(name);
        let description = cycle.join(" -> ");
        if cycles.insert(description.clone()) {
            issues.push(Issue::error(
                "wait_for",
                format!("dependency deadlock: {description}"),
            ));
        }
        return;
    }
    if visited.contains(name) {
        return;
    }
    visiting.push(name);
    if let Some(dependencies) = graph.get(name) {
        for dependency in dependencies {
            visit_dependency(dependency, graph, visiting, visited, cycles, issues);
        }
    }
    visiting.pop();
    visited.insert(name);
}

fn collect_unknown_keys(value: &toml::Value, issues: &mut Vec<Issue>) {
    const PROJECT: &[&str] = &[
        "version",
        "name",
        "shell",
        "log_dir",
        "log_rotate_bytes",
        "log_rotate_keep",
        "max_log_lines",
        "stop_timeout",
        "theme",
        "groups",
    ];
    const GROUP: &[&str] = &["name", "commands"];
    const COMMAND: &[&str] = &[
        "name",
        "run",
        "cwd",
        "shell_setup",
        "pre",
        "wait_for",
        "actions",
        "autostart",
        "log_file",
        "log_rotate_bytes",
        "log_rotate_keep",
        "stop_timeout",
    ];
    const ACTION: &[&str] = &[
        "name",
        "run",
        "cwd",
        "pre",
        "log_file",
        "log_rotate_bytes",
        "log_rotate_keep",
        "stop_timeout",
        "requires_stopped",
        "restart_after",
    ];
    const WAIT: &[&str] = &[
        "command",
        "kind",
        "value",
        "seconds",
        "timeout",
        "case_sensitive",
    ];

    let Some(project) = value.as_table() else {
        return;
    };
    warn_unknown(project, PROJECT, "project", issues);
    if let Some(theme) = project.get("theme").and_then(toml::Value::as_table) {
        warn_unknown(theme, THEME_KEYS, "theme", issues);
    }
    let Some(groups) = project.get("groups").and_then(toml::Value::as_array) else {
        return;
    };
    for (group_index, group) in groups.iter().filter_map(toml::Value::as_table).enumerate() {
        let group_location = format!("groups[{group_index}]");
        warn_unknown(group, GROUP, &group_location, issues);
        let Some(commands) = group.get("commands").and_then(toml::Value::as_array) else {
            continue;
        };
        for (command_index, command) in commands
            .iter()
            .filter_map(toml::Value::as_table)
            .enumerate()
        {
            let command_location = format!("{group_location}.commands[{command_index}]");
            warn_unknown(command, COMMAND, &command_location, issues);
            if let Some(actions) = command.get("actions").and_then(toml::Value::as_array) {
                for (action_index, action) in
                    actions.iter().filter_map(toml::Value::as_table).enumerate()
                {
                    warn_unknown(
                        action,
                        ACTION,
                        &format!("{command_location}.actions[{action_index}]"),
                        issues,
                    );
                }
            }
            let Some(waits) = command.get("wait_for").and_then(toml::Value::as_array) else {
                continue;
            };
            for (wait_index, wait) in waits.iter().filter_map(toml::Value::as_table).enumerate() {
                warn_unknown(
                    wait,
                    WAIT,
                    &format!("{command_location}.wait_for[{wait_index}]"),
                    issues,
                );
            }
        }
    }
}

fn warn_unknown(table: &toml::Table, allowed: &[&str], location: &str, issues: &mut Vec<Issue>) {
    for key in table.keys().filter(|key| !allowed.contains(&key.as_str())) {
        issues.push(Issue::warning(location, format!("unknown key {key:?}")));
    }
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn resolve_path(root: &Path, path: &Path) -> PathBuf {
    let path = expand_home(path);
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

fn expand_home(path: &Path) -> PathBuf {
    let Some(value) = path.to_str() else {
        return path.to_path_buf();
    };
    if value == "~" {
        return env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    }
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    path.to_path_buf()
}

fn find_executable(shell: &Path) -> Option<PathBuf> {
    if shell.components().count() > 1 {
        return is_executable(shell).then(|| shell.to_path_buf());
    }
    env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .find_map(|part| {
            let candidate = Path::new(part).join(shell);
            is_executable(&candidate).then_some(candidate)
        })
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use ratatui::style::Color;
    use tempfile::tempdir;

    use super::{
        Readiness, RestartAfter, Severity, combine_projects, set_project_theme_file,
        set_theme_preset, validate_file, validate_file_for_combined,
    };

    #[test]
    fn parses_groups_commands_and_wait_conditions() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
version = 1
name = "Example"
shell = "/bin/sh"
log_rotate_bytes = 100
log_rotate_keep = 4

[[groups]]
name = "Backend"

[[groups.commands]]
name = "api"
run = "run-api"
pre = ["install-api"]
autostart = true

[[groups.commands]]
name = "worker"
run = "run-worker"
log_rotate_bytes = 200
log_rotate_keep = 2

[[groups.commands.wait_for]]
command = "api"
kind = "keyword"
value = "ready"
case_sensitive = false
"#,
        )
        .unwrap();

        let report = validate_file(&path);
        assert!(report.is_valid(), "{:?}", report.issues);
        let project = report.project.unwrap();
        assert_eq!(project.groups[0].commands.len(), 2);
        assert_eq!(project.command("api").unwrap().log_rotate_bytes, Some(100));
        assert_eq!(project.command("api").unwrap().log_rotate_keep, 4);
        assert_eq!(
            project.command("worker").unwrap().log_rotate_bytes,
            Some(200)
        );
        assert_eq!(project.command("worker").unwrap().log_rotate_keep, 2);
        assert_eq!(
            project.command("worker").unwrap().wait_for[0].readiness,
            Readiness::Keyword {
                value: "ready".to_owned(),
                case_sensitive: false
            }
        );
    }

    #[test]
    fn parses_nested_actions_with_inherited_command_context() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
name = "Example"
shell = "/bin/sh"
[[groups]]
name = "Frontend"
[[groups.commands]]
name = "dashboard"
run = "yarn start"
cwd = "."
shell_setup = ["export TOOLCHAIN=stable"]

[[groups.commands.actions]]
name = "Install dependencies"
run = "yarn install"
requires_stopped = true
restart_after = "if-running"
"#,
        )
        .unwrap();

        let report = validate_file(&path);
        assert!(report.is_valid(), "{:?}", report.issues);
        let project = report.project.unwrap();
        let command = project.command("dashboard").unwrap();
        let action = &command.actions[0];
        assert_eq!(action.id, "dashboard::action::Install dependencies");
        assert_eq!(action.parent_id, "dashboard");
        assert_eq!(action.cwd, command.cwd);
        assert_eq!(action.shell_setup, command.shell_setup);
        assert!(action.requires_stopped);
        assert_eq!(action.restart_after, RestartAfter::IfRunning);
    }

    #[test]
    fn reports_missing_dependencies_and_cycles() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
shell = "/bin/sh"
[[groups]]
name = "all"
[[groups.commands]]
name = "a"
run = "true"
[[groups.commands.wait_for]]
command = "b"
kind = "delay"
seconds = 1
[[groups.commands]]
name = "b"
run = "true"
[[groups.commands.wait_for]]
command = "a"
kind = "idle"
seconds = 1
[[groups.commands]]
name = "c"
run = "true"
[[groups.commands.wait_for]]
command = "missing"
kind = "keyword"
value = "ready"
"#,
        )
        .unwrap();

        let report = validate_file(&path);
        assert!(!report.is_valid());
        let errors: Vec<_> = report
            .issues
            .iter()
            .filter(|issue| issue.severity == Severity::Error)
            .map(|issue| issue.message.as_str())
            .collect();
        assert!(errors.iter().any(|message| message.contains("deadlock")));
        assert!(
            errors
                .iter()
                .any(|message| message.contains("missing command"))
        );
    }

    #[test]
    fn malformed_toml_is_an_error_instead_of_a_panic() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(&path, "[[groups]\n").unwrap();
        let report = validate_file(&path);
        assert!(!report.is_valid());
        assert!(report.project.is_none());
    }

    #[test]
    fn commands_inherit_project_stop_timeout_and_can_override_it() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
shell = "/bin/sh"
stop_timeout = 15
[[groups]]
name = "all"
[[groups.commands]]
name = "inherited"
run = "true"
[[groups.commands]]
name = "overridden"
run = "true"
stop_timeout = 30
"#,
        )
        .unwrap();

        let report = validate_file(&path);
        assert!(report.is_valid(), "{:?}", report.issues);
        let project = report.project.unwrap();
        assert_eq!(project.command("inherited").unwrap().stop_timeout, 15.0);
        assert_eq!(project.command("overridden").unwrap().stop_timeout, 30.0);
    }

    #[test]
    fn applies_theme_presets_and_color_overrides() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r##"
shell = "/bin/sh"

[theme]
preset = "nord"
accent = "#010203"
muted = "ansi:8"

[[groups]]
name = "all"

[[groups.commands]]
name = "example"
run = "true"
"##,
        )
        .unwrap();

        let report = validate_file(&path);
        assert!(report.is_valid(), "{:?}", report.issues);
        let project = report.project.unwrap();
        assert_eq!(project.theme_preset, "nord");
        assert_eq!(project.theme.accent, Color::Rgb(1, 2, 3));
        assert_eq!(project.theme.muted, Color::Indexed(8));
        assert_eq!(project.theme.running, Color::Rgb(163, 190, 140));
        assert_eq!(project.theme_overrides.accent, Some(Color::Rgb(1, 2, 3)));
    }

    #[test]
    fn rejects_invalid_theme_values() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
shell = "/bin/sh"

[theme]
preset = "missing"
accent = "ultraviolet"

[[groups]]
name = "all"

[[groups.commands]]
name = "example"
run = "true"
"#,
        )
        .unwrap();

        let report = validate_file(&path);
        assert!(!report.is_valid());
        assert_eq!(report.error_count(), 2);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.location == "theme.preset")
        );
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.location == "theme.accent")
        );
    }

    #[test]
    fn theme_picker_update_preserves_color_overrides() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r##"shell = "/bin/sh"

[theme]
preset = "default"
accent = "#010203"

[[groups]]
name = "all"
[[groups.commands]]
name = "example"
run = "true"
"##,
        )
        .unwrap();

        set_theme_preset(&path, "nord").unwrap();

        let source = fs::read_to_string(&path).unwrap();
        assert!(source.contains("preset = \"nord\""));
        assert!(source.contains("accent = \"#010203\""));
        let project = validate_file(&path).project.unwrap();
        assert_eq!(project.theme_preset, "nord");
        assert_eq!(project.theme.accent, Color::Rgb(1, 2, 3));
    }

    #[test]
    fn theme_picker_inserts_a_theme_table_before_groups() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"shell = "/bin/sh"

[[groups]]
name = "all"
[[groups.commands]]
name = "example"
run = "true"
"#,
        )
        .unwrap();

        set_theme_preset(&path, "gruvbox").unwrap();

        let source = fs::read_to_string(&path).unwrap();
        assert!(source.find("[theme]").unwrap() < source.find("[[groups]]").unwrap());
        let report = validate_file(&path);
        assert!(report.is_valid(), "{:?}", report.issues);
        assert_eq!(report.project.unwrap().theme_preset, "gruvbox");
    }

    #[test]
    fn project_theme_can_reference_a_theme_file() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        let theme_path = directory.path().join("custom.toml");
        fs::write(
            &path,
            r#"
shell = "/bin/sh"
[[groups]]
name = "all"
[[groups.commands]]
name = "example"
run = "true"
"#,
        )
        .unwrap();
        fs::write(
            &theme_path,
            "preset = \"matrix\"\ndescription = \"Green and sharp\"\n",
        )
        .unwrap();
        let theme = crate::theme::Theme::preset("matrix").unwrap();

        set_project_theme_file(&path, &theme_path).unwrap();

        let project = validate_file(&path).project.unwrap();
        assert_eq!(project.theme_preset, "matrix");
        assert_eq!(project.theme, theme);
        assert_eq!(
            project.theme_overrides,
            crate::theme::ThemeOverrides::default()
        );
        let source = fs::read_to_string(path).unwrap();
        assert!(source.contains("file ="));
        assert!(!source.contains("accent ="));
    }

    #[test]
    fn combines_projects_with_isolated_command_and_dependency_ids() {
        let directory = tempdir().unwrap();
        let first_path = directory.path().join("first.blade");
        let second_path = directory.path().join("second.blade");
        fs::write(
            &first_path,
            r#"
shell = "/bin/sh"
[[groups]]
name = "Backend"
[[groups.commands]]
name = "api"
run = "true"
"#,
        )
        .unwrap();
        fs::write(
            &second_path,
            r#"
shell = "/bin/sh"
[[groups]]
name = "Backend"
[[groups.commands]]
name = "api"
run = "true"
[[groups.commands]]
name = "worker"
run = "true"
[[groups.commands.wait_for]]
command = "First::api"
kind = "delay"
seconds = 1
"#,
        )
        .unwrap();

        let first = validate_file(&first_path).project.unwrap();
        let standalone_second = validate_file(&second_path);
        assert!(!standalone_second.is_valid());
        assert!(
            standalone_second
                .issues
                .iter()
                .any(|issue| issue.message.contains("requires launching All projects"))
        );
        let second_report = validate_file_for_combined(&second_path);
        assert!(second_report.is_valid(), "{:?}", second_report.issues);
        let second = second_report.project.unwrap();
        let combined = combine_projects(
            directory.path().join("projects.config"),
            vec![("First".to_owned(), first), ("Second".to_owned(), second)],
        )
        .unwrap();

        assert_eq!(combined.name, "All projects");
        assert_eq!(combined.groups[0].project.as_deref(), Some("First"));
        assert_eq!(combined.groups[1].project.as_deref(), Some("Second"));
        let first_api = &combined.groups[0].commands[0];
        let second_api = &combined.groups[1].commands[0];
        let second_worker = &combined.groups[1].commands[1];
        assert_eq!(first_api.name, "api");
        assert_eq!(second_api.name, "api");
        assert_ne!(first_api.id, second_api.id);
        assert_eq!(second_worker.wait_for[0].command, first_api.id);
        assert!(combined.command(&first_api.id).is_some());
        assert!(combined.command(&second_api.id).is_some());
        assert!(combined.theme_file.is_none());
    }

    #[test]
    fn rejects_cross_project_dependency_deadlocks() {
        let directory = tempdir().unwrap();
        let first_path = directory.path().join("first.blade");
        let second_path = directory.path().join("second.blade");
        fs::write(
            &first_path,
            r#"
shell = "/bin/sh"
[[groups]]
name = "all"
[[groups.commands]]
name = "first"
run = "true"
[[groups.commands.wait_for]]
command = "Second::second"
kind = "delay"
seconds = 1
"#,
        )
        .unwrap();
        fs::write(
            &second_path,
            r#"
shell = "/bin/sh"
[[groups]]
name = "all"
[[groups.commands]]
name = "second"
run = "true"
[[groups.commands.wait_for]]
command = "First::first"
kind = "delay"
seconds = 1
"#,
        )
        .unwrap();
        let first = validate_file_for_combined(&first_path).project.unwrap();
        let second = validate_file_for_combined(&second_path).project.unwrap();

        let error = combine_projects(
            directory.path().join("projects.config"),
            vec![("First".to_owned(), first), ("Second".to_owned(), second)],
        )
        .unwrap_err();

        assert!(error.to_string().contains("dependency deadlock"));
    }
}
