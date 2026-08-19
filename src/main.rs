mod command_edit;
mod config;
mod init;
mod log_buffer;
mod project_list;
mod runner;
mod theme;
mod tui;

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::{
    config::{
        DEFAULT_FILE_NAME, ProjectConfig, Severity, ValidationReport, combine_projects,
        find_project_file, validate_file_for_combined_with_catalog, validate_file_with_catalog,
    },
    project_list::{
        GlobalSettings, GlobalTheme, ProjectEntry, candidate_paths, find_file as find_project_list,
        home_directory,
    },
    runner::Runner,
    theme::{ThemeCatalog, ThemeOverrides},
    tui::StartupMode,
};

#[derive(Debug, Parser)]
#[command(
    name = "blade",
    version,
    about = "Blade — the terminal project command runner",
    long_about = None
)]
struct Cli {
    /// Use a specific project file instead of searching parent directories.
    #[arg(short, long, global = true, value_name = "PATH")]
    file: Option<PathBuf>,

    #[command(flatten)]
    startup: StartupOptions,

    #[command(subcommand)]
    command: Option<Action>,
}

#[derive(Debug, Default, Args)]
struct StartupOptions {
    /// Skip local project discovery and open the global project picker.
    #[arg(long, conflicts_with = "file")]
    global: bool,

    /// Start every configured command, ignoring individual autostart settings.
    #[arg(long, visible_alias = "start-all", conflicts_with = "no_autostart")]
    all: bool,

    /// Do not start commands marked autostart.
    #[arg(long, conflicts_with = "all")]
    no_autostart: bool,
}

impl StartupOptions {
    fn is_set(&self) -> bool {
        self.global || self.all || self.no_autostart
    }

    fn merge(self, other: Self) -> Result<Self> {
        let merged = Self {
            global: self.global || other.global,
            all: self.all || other.all,
            no_autostart: self.no_autostart || other.no_autostart,
        };
        if merged.all && merged.no_autostart {
            bail!("--all and --no-autostart cannot be used together");
        }
        Ok(merged)
    }
}

#[derive(Debug, Subcommand)]
enum Action {
    /// Open the interactive runner (the default action).
    Run {
        #[command(flatten)]
        startup: StartupOptions,
    },
    /// Interactively create a .blade project file.
    Init {
        /// File or project directory to initialize.
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,

        /// Replace an existing file without the confirmation prompt.
        #[arg(long)]
        force: bool,
    },
    /// Parse and validate a project file without opening the TUI.
    Validate {
        /// Project file, project directory, or global project list to validate.
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,
    },
    /// Manage project commands or register the project globally.
    Command {
        #[command(subcommand)]
        action: CommandAction,
    },
}

#[derive(Debug, Subcommand)]
enum CommandAction {
    /// Display a project or global configuration in a readable format.
    List {
        /// Project alias to show when targeting a global project list.
        #[arg(long, value_name = "PROJECT")]
        project: Option<String>,
    },
    /// Append a command, creating the group when needed.
    Add {
        /// Unique command name. Omit to prompt interactively.
        #[arg(value_name = "NAME")]
        name: Option<String>,

        /// Project alias when targeting a global project list.
        #[arg(long, value_name = "PROJECT")]
        project: Option<String>,

        /// Existing or new group name.
        #[arg(long, value_name = "GROUP")]
        group: Option<String>,

        /// Shell command to execute.
        #[arg(long, value_name = "SHELL_COMMAND")]
        run: Option<String>,

        /// Working directory, relative to the project file by default.
        #[arg(long, value_name = "PATH")]
        cwd: Option<String>,

        /// Pre-step to run before the command. May be repeated.
        #[arg(long, value_name = "SHELL_COMMAND")]
        pre: Vec<String>,

        /// Start this command automatically.
        #[arg(long)]
        autostart: bool,
    },
    /// Modify selected command fields, or open the interactive editor.
    Edit {
        /// Existing command name. Omit to select interactively.
        #[arg(value_name = "NAME")]
        name: Option<String>,

        /// Project alias when targeting a global project list.
        #[arg(long, value_name = "PROJECT")]
        project: Option<String>,

        /// Rename the command and update local dependency references.
        #[arg(long, value_name = "NEW_NAME")]
        rename: Option<String>,

        /// Replace the shell command.
        #[arg(long, value_name = "SHELL_COMMAND")]
        run: Option<String>,

        /// Replace the working directory.
        #[arg(long, value_name = "PATH")]
        cwd: Option<String>,

        /// Replace pre-steps. May be repeated.
        #[arg(long, value_name = "SHELL_COMMAND", conflicts_with = "clear_pre")]
        pre: Vec<String>,

        /// Remove all pre-steps.
        #[arg(long)]
        clear_pre: bool,

        /// Set whether the command starts automatically.
        #[arg(long, value_name = "BOOL")]
        autostart: Option<bool>,
    },
    /// Delete a command after confirmation.
    Delete {
        /// Existing command name. Omit to select interactively.
        #[arg(value_name = "NAME")]
        name: Option<String>,

        /// Project alias when targeting a global project list.
        #[arg(long, value_name = "PROJECT")]
        project: Option<String>,

        /// Delete without prompting for confirmation.
        #[arg(short, long)]
        yes: bool,
    },
    /// Register this project in Blade's global project list.
    Register {
        /// Project alias shown in the global picker. Defaults to the project name.
        #[arg(long, value_name = "NAME")]
        name: Option<String>,

        /// Global list to update. Defaults to the active list or ~/.blade.
        #[arg(long, value_name = "PATH")]
        registry: Option<PathBuf>,
    },
    /// Remove this project from Blade's global project list.
    Deregister {
        /// Project alias when targeting a global project list.
        #[arg(long, value_name = "PROJECT")]
        project: Option<String>,

        /// Global list to update. Defaults to the active list.
        #[arg(long, value_name = "PATH")]
        registry: Option<PathBuf>,

        /// Deregister without prompting for confirmation.
        #[arg(short, long)]
        yes: bool,
    },
}

enum RunTarget {
    Project(PathBuf),
    Combined {
        project_list: PathBuf,
        projects: Vec<ProjectEntry>,
    },
}

fn main() -> ExitCode {
    match execute(Cli::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("blade: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn execute(cli: Cli) -> Result<u8> {
    let root_startup = cli.startup;
    let action = cli.command.unwrap_or(Action::Run {
        startup: StartupOptions::default(),
    });
    if !matches!(&action, Action::Run { .. }) && root_startup.is_set() {
        bail!("--all, --no-autostart, and --global may only be used when running Blade");
    }
    match action {
        Action::Init { path, force } => {
            let path = project_path(path.or(cli.file), false)?;
            init::initialize(&path, force)?;
            Ok(0)
        }
        Action::Validate { path } => {
            let requested = path.or(cli.file);
            if let Some(path) = requested {
                let path = project_path(Some(path), true)?;
                if let Ok(home) = home_directory()
                    && candidate_paths(&home).contains(&path)
                {
                    return validate_project_list(&path, &home);
                }
                let settings = active_global_settings();
                let report = validate_file_with_catalog(&path, &settings.theme_catalog);
                print_report(&path, &report);
                return Ok(if report.is_valid() { 0 } else { 2 });
            }

            let current =
                env::current_dir().context("could not determine the current directory")?;
            let home = home_directory()?;
            if let Some(path) = find_local_project_file(&current, &home) {
                let settings = active_global_settings();
                let report = validate_file_with_catalog(&path, &settings.theme_catalog);
                print_report(&path, &report);
                return Ok(if report.is_valid() { 0 } else { 2 });
            }
            let candidates = candidate_paths(&home);
            let path = find_project_list(&home).with_context(|| {
                format!(
                    "no {DEFAULT_FILE_NAME} project found in {} or its parents, and no global project list found at {}, {}, or {}",
                    current.display(),
                    candidates[0].display(),
                    candidates[1].display(),
                    candidates[2].display()
                )
            })?;
            validate_project_list(&path, &home)
        }
        Action::Command { action } => {
            match action {
                CommandAction::List { project } => {
                    let path = command_source_path(cli.file)?;
                    command_edit::list(&path, project.as_deref())?;
                }
                CommandAction::Add {
                    name,
                    project,
                    group,
                    run,
                    cwd,
                    pre,
                    autostart,
                } => {
                    let target = command_project_target(cli.file, project.as_deref())?;
                    command_edit::add_with_catalog(
                        &target.path,
                        command_edit::AddOptions {
                            group,
                            name,
                            run,
                            cwd,
                            pre,
                            autostart,
                        },
                        &target.catalog,
                    )?;
                }
                CommandAction::Edit {
                    name,
                    project,
                    rename,
                    run,
                    cwd,
                    pre,
                    clear_pre,
                    autostart,
                } => {
                    let target = command_project_target(cli.file, project.as_deref())?;
                    command_edit::edit_with_catalog(
                        &target.path,
                        command_edit::EditOptions {
                            target: name,
                            new_name: rename,
                            run,
                            cwd,
                            pre: if clear_pre {
                                Some(Vec::new())
                            } else if pre.is_empty() {
                                None
                            } else {
                                Some(pre)
                            },
                            autostart,
                        },
                        &target.catalog,
                    )?;
                }
                CommandAction::Delete { name, project, yes } => {
                    let target = command_project_target(cli.file, project.as_deref())?;
                    command_edit::delete_with_catalog(&target.path, name, yes, &target.catalog)?;
                }
                CommandAction::Register { name, registry } => {
                    let path = canonical_project_path(cli.file)?;
                    command_edit::register(&path, name, registry)?
                }
                CommandAction::Deregister {
                    project,
                    registry,
                    yes,
                } => {
                    let path = command_source_path(cli.file)?;
                    if is_project_list_file(&path) {
                        if registry.is_some() {
                            bail!(
                                "--registry is unnecessary when --file already names a global project list"
                            );
                        }
                        command_edit::deregister_from_registry(&path, project.as_deref(), yes)?;
                    } else {
                        if project.is_some() {
                            bail!("--project may only be used with a global project list");
                        }
                        command_edit::deregister(&path, registry, yes)?;
                    }
                }
            }
            Ok(0)
        }
        Action::Run { startup } => {
            let startup = root_startup.merge(startup)?;
            if startup.global && cli.file.is_some() {
                bail!("--global cannot be used with --file");
            }
            let Some(target) = run_target(cli.file, startup.global)? else {
                return Ok(0);
            };
            let project = match target {
                RunTarget::Project(path) => {
                    let settings = active_global_settings();
                    let mut project = load_project(&path, false, &settings.theme_catalog)?;
                    if !project.theme_preset_configured
                        && let Some(theme) = settings.theme
                    {
                        apply_global_theme(&mut project, &theme, false);
                    }
                    project
                }
                RunTarget::Combined {
                    project_list,
                    projects,
                } => {
                    let settings = project_list::load_settings(&project_list, &home_directory()?)?;
                    let mut loaded = Vec::with_capacity(projects.len());
                    for project in projects {
                        loaded.push((
                            project.name,
                            load_project(&project.path, true, &settings.theme_catalog)?,
                        ));
                    }
                    let mut project = combine_projects(project_list.clone(), loaded)?;
                    if let Some(theme) = &settings.theme {
                        apply_global_theme(&mut project, theme, true);
                    }
                    project.theme_catalog = settings.theme_catalog;
                    project.theme_file = Some(project_list);
                    project.theme_file_is_global = true;
                    project.theme_configured = settings.theme.is_some();
                    project.theme_preset_configured = settings.theme.is_some();
                    project
                }
            };
            let startup = if startup.all {
                StartupMode::All
            } else if startup.no_autostart {
                StartupMode::None
            } else {
                StartupMode::Configured
            };
            tui::run(Runner::new(project), startup)?;
            Ok(0)
        }
    }
}

fn validate_project_list(path: &Path, home: &Path) -> Result<u8> {
    let project_list = match project_list::load_config(path, home) {
        Ok(project_list) => project_list,
        Err(error) => {
            eprintln!("error: {}: {error:#}", path.display());
            eprintln!("{} is invalid (1 error(s), 0 warning(s))", path.display());
            return Ok(2);
        }
    };
    let project_list::ProjectList {
        projects,
        theme: _global_theme,
        theme_catalog,
    } = project_list;
    let mut loaded = Vec::with_capacity(projects.len());
    let mut command_count = 0;
    let mut error_count = 0;
    let mut warning_count = 0;
    for project in &projects {
        let report = validate_file_for_combined_with_catalog(&project.path, &theme_catalog);
        for issue in &report.issues {
            eprintln!("{}: {issue}", project.name);
        }
        error_count += report.error_count();
        warning_count += report.warning_count();
        if let Some(config) = report.project {
            command_count += config.commands().count();
            loaded.push((project.name.clone(), config));
        }
    }
    if error_count == 0
        && let Err(error) = combine_projects(path.to_path_buf(), loaded)
    {
        eprintln!("error: combined projects: {error:#}");
        error_count += 1;
    }

    if error_count == 0 {
        println!(
            "{} is valid ({} project(s), {command_count} command(s), {warning_count} warning(s))",
            path.display(),
            projects.len()
        );
        Ok(0)
    } else {
        eprintln!(
            "{} is invalid ({error_count} error(s), {warning_count} warning(s))",
            path.display()
        );
        Ok(2)
    }
}

fn active_global_settings() -> GlobalSettings {
    let Ok(home) = home_directory() else {
        return GlobalSettings::default();
    };
    let Some(path) = find_project_list(&home) else {
        return GlobalSettings::default();
    };
    match project_list::load_settings(&path, &home) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!(
                "warning: could not load global theme from {}: {error:#}",
                path.display()
            );
            GlobalSettings::default()
        }
    }
}

fn apply_global_theme(project: &mut ProjectConfig, global: &GlobalTheme, force: bool) {
    if force || !project.theme_configured {
        project.theme = global.theme.clone();
        project.theme_preset = global.preset.clone();
        project.theme_overrides = if force {
            global.overrides.clone()
        } else {
            ThemeOverrides::default()
        };
        return;
    }
    if !project.theme_preset_configured {
        let mut theme = global.theme.clone();
        project.theme_overrides.apply(&mut theme);
        project.theme = theme;
        project.theme_preset = global.preset.clone();
    }
}

fn load_project(
    path: &Path,
    allow_external_dependencies: bool,
    catalog: &crate::theme::ThemeCatalog,
) -> Result<ProjectConfig> {
    let report = if allow_external_dependencies {
        validate_file_for_combined_with_catalog(path, catalog)
    } else {
        validate_file_with_catalog(path, catalog)
    };
    if !report.is_valid() {
        print_report(path, &report);
        bail!(
            "project configuration {} is invalid; fix the errors above and try again",
            path.display()
        );
    }
    for issue in report
        .issues
        .iter()
        .filter(|issue| issue.severity == Severity::Warning)
    {
        eprintln!("{issue}");
    }
    report.project.context("validated project was missing")
}

fn run_target(path: Option<PathBuf>, global: bool) -> Result<Option<RunTarget>> {
    if let Some(path) = path {
        return project_path(Some(path), true)
            .map(RunTarget::Project)
            .map(Some);
    }

    let home = home_directory();
    let current = if global {
        None
    } else {
        Some(env::current_dir().context("could not determine the current directory")?)
    };
    if let Some(current) = &current {
        let local = match &home {
            Ok(home) => find_local_project_file(current, home),
            Err(_) => find_project_file(current),
        };
        if let Some(path) = local {
            return Ok(Some(RunTarget::Project(path)));
        }
    }

    let home = home?;
    let candidates = candidate_paths(&home);
    let config_path = find_project_list(&home).with_context(|| {
        if let Some(current) = &current {
            format!(
                "no {DEFAULT_FILE_NAME} file found in {} or its project parents, and no global project list found at {}, {}, or {}; run `blade init` or create a project list",
                current.display(),
                candidates[0].display(),
                candidates[1].display(),
                candidates[2].display()
            )
        } else {
            format!(
                "no global project list found at {}, {}, or {}; create one or run `blade command register` from a project",
                candidates[0].display(),
                candidates[1].display(),
                candidates[2].display()
            )
        }
    })?;
    let projects = project_list::load(&config_path, &home)?;
    let Some(mut selected) = tui::select_projects(&config_path, &projects)? else {
        return Ok(None);
    };
    if selected.len() == 1 {
        return Ok(Some(RunTarget::Project(selected.remove(0).path)));
    }
    Ok(Some(RunTarget::Combined {
        project_list: config_path,
        projects: selected,
    }))
}

fn find_local_project_file(current: &Path, home: &Path) -> Option<PathBuf> {
    let global_project_list = home.join(DEFAULT_FILE_NAME);
    find_project_file(current).filter(|path| path != &global_project_list)
}

fn command_source_path(path: Option<PathBuf>) -> Result<PathBuf> {
    let path = if let Some(path) = path {
        if path.is_dir() {
            path.join(DEFAULT_FILE_NAME)
        } else {
            path
        }
    } else {
        let current = env::current_dir().context("could not determine the current directory")?;
        let home = home_directory()?;
        find_local_project_file(&current, &home)
            .or_else(|| find_project_list(&home))
            .with_context(|| {
                let candidates = candidate_paths(&home);
                format!(
                    "no project file found in {} or its parents, and no global project list found at {}, {}, or {}",
                    current.display(),
                    candidates[0].display(),
                    candidates[1].display(),
                    candidates[2].display()
                )
            })?
    };
    path.canonicalize()
        .with_context(|| format!("could not resolve {}", path.display()))
}

struct CommandProjectTarget {
    path: PathBuf,
    catalog: ThemeCatalog,
}

fn command_project_target(
    path: Option<PathBuf>,
    project: Option<&str>,
) -> Result<CommandProjectTarget> {
    let source = if path.is_none() && project.is_some() {
        let home = home_directory()?;
        find_project_list(&home).context("no global project list exists")?
    } else {
        command_source_path(path)?
    };
    let source = source
        .canonicalize()
        .with_context(|| format!("could not resolve {}", source.display()))?;
    let is_project_list = is_project_list_file(&source);
    if !is_project_list {
        if project.is_some() {
            bail!("--project may only be used with a global project list");
        }
        return Ok(CommandProjectTarget {
            path: source,
            catalog: active_global_settings().theme_catalog,
        });
    }

    let home = home_directory()?;
    let config = project_list::load_config(&source, &home)?;
    if let Some(project) = project {
        let path = config
            .projects
            .iter()
            .find(|entry| entry.name == project)
            .map(|entry| entry.path.clone())
            .with_context(|| {
                format!(
                    "project {project:?} is not registered in {}; available projects: {}",
                    source.display(),
                    config
                        .projects
                        .iter()
                        .map(|entry| entry.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
        return Ok(CommandProjectTarget {
            path,
            catalog: config.theme_catalog,
        });
    }
    if config.projects.len() == 1 {
        return Ok(CommandProjectTarget {
            path: config.projects[0].path.clone(),
            catalog: config.theme_catalog,
        });
    }
    bail!(
        "{} contains multiple projects; select one with --project <NAME> (available: {})",
        source.display(),
        config
            .projects
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn is_project_list_file(path: &Path) -> bool {
    fs::read_to_string(path)
        .ok()
        .and_then(|source| source.parse::<toml::Table>().ok())
        .is_some_and(|document| document.contains_key("projects"))
}

#[cfg(test)]
fn command_project_path(path: Option<PathBuf>, project: Option<&str>) -> Result<PathBuf> {
    command_project_target(path, project).map(|target| target.path)
}

fn canonical_project_path(path: Option<PathBuf>) -> Result<PathBuf> {
    let path = project_path(path, true)?;
    path.canonicalize()
        .with_context(|| format!("could not resolve {}", path.display()))
}

fn project_path(path: Option<PathBuf>, search_parents: bool) -> Result<PathBuf> {
    if let Some(path) = path {
        return Ok(if path.is_dir() {
            path.join(DEFAULT_FILE_NAME)
        } else {
            path
        });
    }
    let current = env::current_dir().context("could not determine the current directory")?;
    if search_parents {
        let project = match home_directory() {
            Ok(home) => find_local_project_file(&current, &home),
            Err(_) => find_project_file(&current),
        };
        project.with_context(|| {
            format!(
                "no {DEFAULT_FILE_NAME} file found in {} or its parents; run `blade init`",
                current.display()
            )
        })
    } else {
        Ok(current.join(DEFAULT_FILE_NAME))
    }
}

fn print_report(path: &Path, report: &ValidationReport) {
    for issue in &report.issues {
        eprintln!("{issue}");
    }
    if report.is_valid() {
        println!(
            "{} is valid ({} command(s), {} warning(s))",
            path.display(),
            report
                .project
                .as_ref()
                .map(|project| project.commands().count())
                .unwrap_or_default(),
            report.warning_count()
        );
    } else {
        eprintln!(
            "{} is invalid ({} error(s), {} warning(s))",
            path.display(),
            report.error_count(),
            report.warning_count()
        );
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use clap::Parser;
    use ratatui::style::Color;
    use tempfile::tempdir;

    use crate::{
        config::validate_file,
        project_list::GlobalTheme,
        theme::{Theme, ThemeOverrides},
    };

    use super::{
        Action, Cli, CommandAction, StartupOptions, apply_global_theme, command_project_path,
        execute, find_local_project_file, validate_project_list,
    };

    #[test]
    fn all_flag_works_with_the_default_and_explicit_run_actions() {
        let default_run = Cli::try_parse_from(["blade", "--all"]).unwrap();
        assert!(default_run.startup.all);
        assert!(default_run.command.is_none());

        let explicit_run = Cli::try_parse_from(["blade", "run", "--start-all"]).unwrap();
        assert!(matches!(
            explicit_run.command,
            Some(Action::Run {
                startup: StartupOptions { all: true, .. }
            })
        ));
    }

    #[test]
    fn all_and_no_autostart_are_mutually_exclusive() {
        assert!(Cli::try_parse_from(["blade", "--all", "--no-autostart"]).is_err());
    }

    #[test]
    fn global_flag_is_available_for_run_and_conflicts_with_file() {
        let default_run = Cli::try_parse_from(["blade", "--global"]).unwrap();
        assert!(default_run.startup.global);

        let explicit_run = Cli::try_parse_from(["blade", "run", "--global"]).unwrap();
        assert!(matches!(
            explicit_run.command,
            Some(Action::Run {
                startup: StartupOptions { global: true, .. }
            })
        ));

        assert!(Cli::try_parse_from(["blade", "--global", "--file", "project.blade"]).is_err());
    }

    #[test]
    fn command_edit_help_omits_runner_only_options() {
        let help = Cli::try_parse_from(["blade", "command", "edit", "--help"])
            .unwrap_err()
            .to_string();

        assert!(help.contains("--file"));
        assert!(!help.contains("--all"));
        assert!(!help.contains("--no-autostart"));
        assert!(!help.contains("--global"));
    }

    #[test]
    fn parses_scriptable_command_mutations() {
        let add = Cli::try_parse_from([
            "blade",
            "command",
            "add",
            "api",
            "--group",
            "Backend",
            "--run",
            "./manage.py runserver",
            "--pre",
            "poetry install",
            "--autostart",
        ])
        .unwrap();
        assert!(matches!(
            add.command,
            Some(Action::Command {
                action: CommandAction::Add {
                    autostart: true,
                    ..
                }
            })
        ));

        let edit = Cli::try_parse_from([
            "blade",
            "command",
            "edit",
            "api",
            "--autostart",
            "false",
            "--clear-pre",
        ])
        .unwrap();
        assert!(matches!(
            edit.command,
            Some(Action::Command {
                action: CommandAction::Edit {
                    autostart: Some(false),
                    clear_pre: true,
                    ..
                }
            })
        ));

        let register = Cli::try_parse_from([
            "blade",
            "command",
            "register",
            "--name",
            "Blade Development",
        ])
        .unwrap();
        assert!(matches!(
            register.command,
            Some(Action::Command {
                action: CommandAction::Register { .. }
            })
        ));

        let deregister = Cli::try_parse_from(["blade", "command", "deregister", "--yes"]).unwrap();
        assert!(matches!(
            deregister.command,
            Some(Action::Command {
                action: CommandAction::Deregister { yes: true, .. }
            })
        ));

        let list =
            Cli::try_parse_from(["blade", "command", "list", "--project", "ACME Development"])
                .unwrap();
        assert!(matches!(
            list.command,
            Some(Action::Command {
                action: CommandAction::List {
                    project: Some(project)
                }
            }) if project == "ACME Development"
        ));
    }

    #[test]
    fn command_mutations_can_target_a_project_through_a_global_list() {
        let directory = tempdir().unwrap();
        let first = directory.path().join("first.blade");
        let second = directory.path().join("second.blade");
        for (path, name) in [(&first, "First"), (&second, "Second")] {
            fs::write(
                path,
                format!(
                    r#"name = "{name}"
shell = "/bin/sh"
[[groups]]
name = "Project"
"#
                ),
            )
            .unwrap();
        }
        let registry = directory.path().join("blade.conf");
        fs::write(
            &registry,
            format!(
                r#"version = 1
[[projects]]
name = "First"
path = "{}"
[[projects]]
name = "Second"
path = "{}"
"#,
                first.display(),
                second.display()
            ),
        )
        .unwrap();

        assert_eq!(
            command_project_path(Some(registry.clone()), Some("Second")).unwrap(),
            fs::canonicalize(&second).unwrap()
        );
        assert!(command_project_path(Some(registry.clone()), None).is_err());

        let cli = Cli::try_parse_from([
            "blade",
            "--file",
            registry.to_str().unwrap(),
            "command",
            "add",
            "worker",
            "--project",
            "Second",
            "--group",
            "Project",
            "--run",
            "echo worker",
        ])
        .unwrap();
        assert_eq!(execute(cli).unwrap(), 0);
        assert!(
            crate::config::validate_file(&second)
                .project
                .unwrap()
                .command("worker")
                .is_some()
        );
        assert!(!fs::read_to_string(first).unwrap().contains("worker"));

        let cli = Cli::try_parse_from([
            "blade",
            "--file",
            registry.to_str().unwrap(),
            "command",
            "deregister",
            "--project",
            "First",
            "--yes",
        ])
        .unwrap();
        assert_eq!(execute(cli).unwrap(), 0);
        let entries = crate::project_list::load(&registry, directory.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "Second");
    }

    #[test]
    fn the_home_blade_file_is_reserved_for_the_global_project_list() {
        let directory = tempdir().unwrap();
        let home = directory.path();
        let nested = home.join("work/project/src");
        fs::create_dir_all(&nested).unwrap();
        fs::write(home.join(".blade"), "global project list").unwrap();

        assert!(find_local_project_file(&nested, home).is_none());

        let project_file = home.join("work/project/.blade");
        fs::write(&project_file, "local project").unwrap();
        assert_eq!(
            find_local_project_file(&nested, home).unwrap(),
            project_file
        );
    }

    #[test]
    fn validates_a_global_project_list_and_its_references() {
        let directory = tempdir().unwrap();
        let home = directory.path();
        let project = home.join("project.blade");
        fs::write(
            &project,
            r#"
shell = "/bin/sh"
[[groups]]
name = "all"
[[groups.commands]]
name = "command"
run = "true"
"#,
        )
        .unwrap();
        let project_list = home.join(".blade");
        fs::write(
            &project_list,
            format!(
                r#"
[[projects]]
name = "Project"
path = "{}"
"#,
                project.display()
            ),
        )
        .unwrap();

        assert_eq!(validate_project_list(&project_list, home).unwrap(), 0);
    }

    #[test]
    fn project_theme_precedence_and_combined_override_are_explicit() {
        let directory = tempdir().unwrap();
        let global = GlobalTheme {
            theme: Theme::preset("matrix-alt").unwrap(),
            preset: "matrix-alt".to_owned(),
            overrides: ThemeOverrides::default(),
        };
        let load = |name: &str, theme: &str| {
            let path = directory.path().join(name);
            fs::write(
                &path,
                format!(
                    r##"
shell = "/bin/sh"
{theme}
[[groups]]
name = "Project"
[[groups.commands]]
name = "command"
run = "true"
"##
                ),
            )
            .unwrap();
            validate_file(&path).project.unwrap()
        };

        let mut inherited = load("inherited.blade", "");
        apply_global_theme(&mut inherited, &global, false);
        assert_eq!(inherited.theme_preset, "matrix-alt");
        assert_eq!(inherited.theme.text, Color::White);

        let mut local_override = load("override.blade", "[theme]\naccent = \"#010203\"\n");
        apply_global_theme(&mut local_override, &global, false);
        assert_eq!(local_override.theme_preset, "matrix-alt");
        assert_eq!(local_override.theme.text, Color::White);
        assert_eq!(local_override.theme.accent, Color::Rgb(1, 2, 3));

        let mut project_preset = load("preset.blade", "[theme]\npreset = \"red\"\n");
        let project_accent = project_preset.theme.accent;
        apply_global_theme(&mut project_preset, &global, false);
        assert_eq!(project_preset.theme_preset, "red");
        assert_eq!(project_preset.theme.accent, project_accent);

        apply_global_theme(&mut project_preset, &global, true);
        assert_eq!(project_preset.theme_preset, "matrix-alt");
        assert_eq!(project_preset.theme.text, Color::White);
    }
}
