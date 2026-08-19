use std::{
    fs::{self, OpenOptions},
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use crate::{
    config::{
        ActionConfig, ProjectConfig, Readiness, RestartAfter, combine_projects,
        validate_file_for_combined_with_catalog,
    },
    init::{DEFAULT_GROUP_NAME, prompt, prompt_bool, toml_string},
    project_list::{self, candidate_paths, find_file as find_project_list, home_directory},
    theme::{PRESETS, ThemeCatalog},
};

#[derive(Debug)]
pub struct AddOptions {
    pub group: Option<String>,
    pub name: Option<String>,
    pub run: Option<String>,
    pub cwd: Option<String>,
    pub pre: Vec<String>,
    pub autostart: bool,
}

#[derive(Debug)]
pub struct EditOptions {
    pub target: Option<String>,
    pub new_name: Option<String>,
    pub run: Option<String>,
    pub cwd: Option<String>,
    pub pre: Option<Vec<String>>,
    pub autostart: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct ActionOptions {
    pub name: String,
    pub run: String,
    pub cwd: Option<String>,
    pub pre: Vec<String>,
    pub requires_stopped: bool,
    pub restart_after: RestartAfter,
}

pub fn list(path: &Path, project_alias: Option<&str>) -> Result<()> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("could not read configuration {}", path.display()))?;
    let is_project_list = source
        .parse::<toml::Table>()
        .map(|document| document.contains_key("projects"))
        .unwrap_or(false);
    if !is_project_list {
        if project_alias.is_some() {
            bail!("--project may only be used with a global project list");
        }
        let project = load_project(path)?;
        print_project(&project, None, "");
        return Ok(());
    }

    let home = home_directory()?;
    let config = project_list::load_config(path, &home)?;
    let entries = if let Some(alias) = project_alias {
        vec![
            config
                .projects
                .iter()
                .find(|entry| entry.name == alias)
                .with_context(|| {
                    format!(
                        "project {alias:?} is not registered in {}; available projects: {}",
                        path.display(),
                        config
                            .projects
                            .iter()
                            .map(|entry| entry.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?,
        ]
    } else {
        config.projects.iter().collect()
    };

    println!("Blade global configuration");
    println!("  File: {}", path.display());
    println!(
        "  Theme: {}",
        config
            .theme
            .as_ref()
            .map(|theme| theme.preset.as_str())
            .unwrap_or("default")
    );
    let custom_themes = config
        .theme_catalog
        .names()
        .skip(PRESETS.len())
        .collect::<Vec<_>>();
    if !custom_themes.is_empty() {
        println!("  Custom themes ({}):", custom_themes.len());
        for name in custom_themes {
            match config.theme_catalog.description(name) {
                Some(description) => println!("    - {name}: {description}"),
                None => println!("    - {name}"),
            }
        }
    }
    println!("  Projects: {}", config.projects.len());
    for entry in entries {
        let project = load_project_with_catalog(&entry.path, &config.theme_catalog)?;
        println!();
        print_project(&project, Some(&entry.name), "  ");
    }
    Ok(())
}

fn print_project(project: &ProjectConfig, alias: Option<&str>, indent: &str) {
    match alias {
        Some(alias) if alias != project.name => {
            println!("{indent}Project: {alias} ({})", project.name)
        }
        Some(alias) => println!("{indent}Project: {alias}"),
        None => println!("{indent}Project: {}", project.name),
    }
    println!("{indent}  File: {}", project.path.display());
    println!("{indent}  Theme: {}", project.theme_preset);
    println!("{indent}  Max log lines: {}", project.max_log_lines);
    println!("{indent}  Groups: {}", project.groups.len());
    for group in &project.groups {
        println!(
            "{indent}    {} ({} command{}):",
            group.name,
            group.commands.len(),
            if group.commands.len() == 1 { "" } else { "s" }
        );
        for command in &group.commands {
            println!(
                "{indent}      - {} [{}]",
                command.name,
                if command.autostart {
                    "autostart"
                } else {
                    "manual"
                }
            );
            print_multiline_field(&format!("{indent}        "), "Run", &command.run);
            let cwd = fs::canonicalize(&command.cwd).unwrap_or_else(|_| command.cwd.clone());
            println!("{indent}        Cwd: {}", cwd.display());
            println!("{indent}        Shell: {}", command.shell.display());
            println!("{indent}        Stop timeout: {}s", command.stop_timeout);
            if !command.shell_setup.is_empty() {
                println!("{indent}        Shell setup:");
                for step in &command.shell_setup {
                    print_multiline_item(&format!("{indent}          "), step);
                }
            }
            if !command.pre.is_empty() {
                println!("{indent}        Pre-steps:");
                for step in &command.pre {
                    print_multiline_item(&format!("{indent}          "), step);
                }
            }
            if !command.wait_for.is_empty() {
                println!("{indent}        Waits for:");
                for wait in &command.wait_for {
                    let condition = match &wait.readiness {
                        Readiness::Keyword {
                            value,
                            case_sensitive,
                        } => format!(
                            "log keyword {value:?} ({})",
                            if *case_sensitive {
                                "case-sensitive"
                            } else {
                                "case-insensitive"
                            }
                        ),
                        Readiness::Idle { seconds } => format!("{seconds}s of idle output"),
                        Readiness::Delay { seconds } => format!("{seconds}s delay"),
                    };
                    let timeout = wait
                        .timeout
                        .map(|seconds| format!(", timeout {seconds}s"))
                        .unwrap_or_default();
                    println!("{indent}          - {}: {condition}{timeout}", wait.command);
                }
            }
            if let Some(log_file) = &command.log_file {
                println!("{indent}        Log file: {}", log_file.display());
            }
            if let Some(bytes) = command.log_rotate_bytes {
                println!(
                    "{indent}        Log rotation: {bytes} bytes, {} backup{}",
                    command.log_rotate_keep,
                    if command.log_rotate_keep == 1 {
                        ""
                    } else {
                        "s"
                    }
                );
            }
            if !command.actions.is_empty() {
                println!("{indent}        Actions:");
                for action in &command.actions {
                    println!("{indent}          - {}", action.name);
                    print_multiline_field(&format!("{indent}            "), "Run", &action.run);
                    if action.cwd == command.cwd {
                        println!("{indent}            Cwd: inherited");
                    } else {
                        println!("{indent}            Cwd: {}", action.cwd.display());
                    }
                    if action.requires_stopped {
                        println!("{indent}            Requires parent stopped: yes");
                    }
                    if action.restart_after != RestartAfter::Never {
                        println!(
                            "{indent}            Restart parent afterward: {}",
                            restart_after_value(action.restart_after)
                        );
                    }
                }
            }
        }
    }
}

fn print_multiline_field(indent: &str, label: &str, value: &str) {
    let mut lines = value.lines();
    println!("{indent}{label}: {}", lines.next().unwrap_or_default());
    let continuation = " ".repeat(label.len() + 2);
    for line in lines {
        println!("{indent}{continuation}{line}");
    }
}

fn print_multiline_item(indent: &str, value: &str) {
    let mut lines = value.lines();
    println!("{indent}- {}", lines.next().unwrap_or_default());
    for line in lines {
        println!("{indent}  {line}");
    }
}

#[cfg(test)]
pub fn add(path: &Path, options: AddOptions) -> Result<()> {
    let catalog = active_theme_catalog();
    add_with_catalog(path, options, &catalog)
}

pub fn add_with_catalog(path: &Path, options: AddOptions, catalog: &ThemeCatalog) -> Result<()> {
    add_with_catalog_impl(path, options, catalog, true)
}

pub(crate) fn add_with_catalog_quiet(
    path: &Path,
    options: AddOptions,
    catalog: &ThemeCatalog,
) -> Result<()> {
    add_with_catalog_impl(path, options, catalog, false)
}

fn add_with_catalog_impl(
    path: &Path,
    mut options: AddOptions,
    catalog: &ThemeCatalog,
    report: bool,
) -> Result<()> {
    let project = load_project_with_catalog(path, catalog)?;
    let interactive = options.group.is_none() || options.name.is_none() || options.run.is_none();
    ensure_interactive_input(interactive)?;

    if options.group.is_none() {
        println!("Available groups:");
        for group in &project.groups {
            println!("  - {}", group.name);
        }
        options.group = Some(prompt(
            "Group (a new name creates it)",
            project
                .groups
                .first()
                .map(|group| group.name.as_str())
                .or(Some(DEFAULT_GROUP_NAME)),
            false,
        )?);
    }
    if options.name.is_none() {
        options.name = Some(prompt("Command name", None, false)?);
    }
    if options.run.is_none() {
        options.run = Some(prompt("Run", None, false)?);
    }
    if interactive && options.cwd.is_none() {
        options.cwd = Some(prompt("Working directory", Some("."), false)?);
    }
    if interactive && !options.autostart {
        options.autostart = prompt_bool("Start automatically", false)?;
    }
    if interactive && options.pre.is_empty() {
        options.pre = prompt_pre_steps()?;
    }

    let group = required(options.group, "group")?;
    let name = required(options.name, "command name")?;
    let run = required(options.run, "run command")?;
    if project.command(&name).is_some() {
        bail!("command {name:?} already exists");
    }
    let cwd = options.cwd.unwrap_or_else(|| ".".to_owned());
    let source =
        fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    let layout = ConfigLayout::parse(&source, &project)?;
    let command = render_command(&name, &run, &cwd, &options.pre, options.autostart);
    let updated = if let Some(group_index) = project
        .groups
        .iter()
        .position(|candidate| candidate.name == group)
    {
        insert_block(&source, layout.groups[group_index].end, &command)
    } else {
        let group_block = format!("[[groups]]\nname = {}\n\n{}", toml_string(&group), command);
        insert_block(&source, source.len(), &group_block)
    };
    commit_validated(path, &updated, catalog)?;
    if report {
        println!(
            "Added command {name:?} to group {group:?} in {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
pub fn edit(path: &Path, options: EditOptions) -> Result<()> {
    let catalog = active_theme_catalog();
    edit_with_catalog(path, options, &catalog)
}

pub fn edit_with_catalog(path: &Path, options: EditOptions, catalog: &ThemeCatalog) -> Result<()> {
    edit_with_catalog_impl(path, options, catalog, true)
}

pub(crate) fn edit_with_catalog_quiet(
    path: &Path,
    options: EditOptions,
    catalog: &ThemeCatalog,
) -> Result<()> {
    edit_with_catalog_impl(path, options, catalog, false)
}

fn edit_with_catalog_impl(
    path: &Path,
    mut options: EditOptions,
    catalog: &ThemeCatalog,
    report: bool,
) -> Result<()> {
    let project = load_project_with_catalog(path, catalog)?;
    if options.target.is_none() {
        ensure_interactive_input(true)?;
        print_commands(&project);
        options.target = Some(prompt("Command to edit", None, false)?);
    }
    let target = required(options.target, "command to edit")?;
    let (group_index, command_index) = find_command(&project, &target)?;
    let command = &project.groups[group_index].commands[command_index];
    let source =
        fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    let layout = ConfigLayout::parse(&source, &project)?;
    let command_layout = &layout.groups[group_index].commands[command_index];
    let current_cwd = find_field_span(
        &source,
        command_layout.body_start,
        command_layout.body_end,
        "cwd",
    )?
    .and_then(|(start, end)| parse_field_string(&source[start..end], "cwd"))
    .unwrap_or_else(|| command.cwd.display().to_string());
    let interactive = options.new_name.is_none()
        && options.run.is_none()
        && options.cwd.is_none()
        && options.pre.is_none()
        && options.autostart.is_none();
    ensure_interactive_input(interactive)?;
    if interactive {
        options.new_name = Some(prompt("Command name", Some(&command.name), false)?);
        options.run = Some(prompt("Run", Some(&command.run), false)?);
        options.cwd = Some(prompt("Working directory", Some(&current_cwd), false)?);
        options.autostart = Some(prompt_bool("Start automatically", command.autostart)?);
        if prompt_bool("Replace pre-steps", false)? {
            options.pre = Some(prompt_pre_steps()?);
        }
    }
    if let Some(new_name) = options.new_name.as_deref()
        && new_name != target
        && project.command(new_name).is_some()
    {
        bail!("command {new_name:?} already exists");
    }

    let mut changes = Vec::new();
    let mut insertions = Vec::new();
    add_field_change(
        &source,
        command_layout,
        "name",
        options.new_name.as_deref().map(toml_string),
        &mut changes,
        &mut insertions,
    )?;
    add_field_change(
        &source,
        command_layout,
        "run",
        options.run.as_deref().map(toml_string),
        &mut changes,
        &mut insertions,
    )?;
    add_field_change(
        &source,
        command_layout,
        "cwd",
        options.cwd.as_deref().map(toml_string),
        &mut changes,
        &mut insertions,
    )?;
    add_field_change(
        &source,
        command_layout,
        "pre",
        options.pre.as_ref().map(|steps| render_string_array(steps)),
        &mut changes,
        &mut insertions,
    )?;
    add_field_change(
        &source,
        command_layout,
        "autostart",
        options.autostart.map(|value| value.to_string()),
        &mut changes,
        &mut insertions,
    )?;
    if !insertions.is_empty() {
        changes.push(Change {
            start: command_layout.body_end,
            end: command_layout.body_end,
            replacement: format!("{}\n", insertions.join("\n")),
        });
    }
    if let Some(new_name) = options.new_name.as_deref()
        && new_name != target
    {
        changes.extend(rename_local_dependencies(&source, &target, new_name)?);
    }
    if changes.is_empty() {
        bail!("no command changes were requested");
    }
    let updated = apply_changes(source, changes)?;
    commit_validated(path, &updated, catalog)?;
    if report {
        println!("Updated command {target:?} in {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
pub fn delete(path: &Path, target: Option<String>, yes: bool) -> Result<()> {
    let catalog = active_theme_catalog();
    delete_with_catalog(path, target, yes, &catalog)
}

pub fn delete_with_catalog(
    path: &Path,
    target: Option<String>,
    yes: bool,
    catalog: &ThemeCatalog,
) -> Result<()> {
    delete_with_catalog_impl(path, target, yes, catalog, true)
}

pub(crate) fn delete_with_catalog_quiet(
    path: &Path,
    target: Option<String>,
    yes: bool,
    catalog: &ThemeCatalog,
) -> Result<()> {
    delete_with_catalog_impl(path, target, yes, catalog, false)
}

fn delete_with_catalog_impl(
    path: &Path,
    target: Option<String>,
    yes: bool,
    catalog: &ThemeCatalog,
    report: bool,
) -> Result<()> {
    let project = load_project_with_catalog(path, catalog)?;
    let target = match target {
        Some(target) => target,
        None => {
            ensure_interactive_input(true)?;
            print_commands(&project);
            prompt("Command to delete", None, false)?
        }
    };
    let (group_index, command_index) = find_command(&project, &target)?;
    if !yes {
        ensure_interactive_input(true)?;
        if !prompt_bool(&format!("Delete command {target:?}?"), false)? {
            bail!("deletion cancelled; {} was left unchanged", path.display());
        }
    }
    let source =
        fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    let layout = ConfigLayout::parse(&source, &project)?;
    let command = &layout.groups[group_index].commands[command_index];
    let group_becomes_empty = project.groups[group_index].commands.len() == 1;
    let mut start = if group_becomes_empty {
        layout.groups[group_index].start
    } else {
        command.start
    };
    let end = if group_becomes_empty {
        layout.groups[group_index].end
    } else {
        command.end
    };
    if start > 0 && source[..start].ends_with("\n\n") {
        start -= 1;
    }
    let mut updated = source;
    updated.replace_range(start..end, "");
    commit_validated(path, &updated, catalog).with_context(|| {
        format!("could not delete {target:?}; it may still be referenced by another command")
    })?;
    if report {
        println!("Deleted command {target:?} from {}", path.display());
    }
    Ok(())
}

pub fn reorder_with_catalog(
    path: &Path,
    target: &str,
    direction: isize,
    catalog: &ThemeCatalog,
) -> Result<()> {
    if !matches!(direction, -1 | 1) {
        bail!("reorder direction must be -1 or 1");
    }
    let project = load_project_with_catalog(path, catalog)?;
    let (group_index, command_index) = find_command(&project, target)?;
    let destination = command_index
        .checked_add_signed(direction)
        .with_context(|| format!("command {target:?} is already at the edge of its group"))?;
    if destination >= project.groups[group_index].commands.len() {
        bail!("command {target:?} is already at the edge of its group");
    }
    let source =
        fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    let layout = ConfigLayout::parse(&source, &project)?;
    let first_index = command_index.min(destination);
    let second_index = command_index.max(destination);
    let first = &layout.groups[group_index].commands[first_index];
    let second = &layout.groups[group_index].commands[second_index];
    let mut updated = source.clone();
    let replacement = format!(
        "{}{}",
        &source[second.start..second.end],
        &source[first.start..first.end]
    );
    updated.replace_range(first.start..second.end, &replacement);
    commit_validated(path, &updated, catalog)
}

pub fn move_with_catalog(
    source_path: &Path,
    target: &str,
    target_path: &Path,
    target_group: &str,
    catalog: &ThemeCatalog,
) -> Result<()> {
    let source_project = load_project_with_catalog(source_path, catalog)?;
    let (source_group_index, command_index) = find_command(&source_project, target)?;
    let command = source_project.groups[source_group_index].commands[command_index].clone();
    let target_project = if source_path == target_path {
        source_project.clone()
    } else {
        load_project_with_catalog(target_path, catalog)?
    };
    let target_group_index = target_project
        .groups
        .iter()
        .position(|group| group.name == target_group);
    if source_path == target_path && target_group_index == Some(source_group_index) {
        return Ok(());
    }

    let source = fs::read_to_string(source_path)
        .with_context(|| format!("could not read {}", source_path.display()))?;
    let source_layout = ConfigLayout::parse(&source, &source_project)?;
    let command_layout = &source_layout.groups[source_group_index].commands[command_index];
    let source_group_is_empty_after_move =
        source_project.groups[source_group_index].commands.len() == 1;
    let removal_start = if source_group_is_empty_after_move {
        source_layout.groups[source_group_index].start
    } else {
        command_layout.start
    };
    let removal_end = if source_group_is_empty_after_move {
        source_layout.groups[source_group_index].end
    } else {
        command_layout.end
    };

    if source_path == target_path {
        let block = source[command_layout.start..command_layout.end]
            .trim_matches('\n')
            .to_owned();
        let mut updated = source.clone();
        updated.replace_range(removal_start..removal_end, "");
        let Some(target_group_index) = target_group_index else {
            let group_block = format!(
                "[[groups]]\nname = {}\n\n{}",
                toml_string(target_group),
                block
            );
            updated = insert_block(&updated, updated.len(), &group_block);
            return commit_validated(source_path, &updated, catalog);
        };
        let original_insertion = source_layout.groups[target_group_index].end;
        let removed = removal_end - removal_start;
        let insertion = if original_insertion > removal_end {
            original_insertion - removed
        } else {
            original_insertion
        };
        updated = insert_block(&updated, insertion, &block);
        return commit_validated(source_path, &updated, catalog);
    }

    if !command.wait_for.is_empty() {
        bail!(
            "command {target:?} has readiness dependencies; remove or qualify them before moving it to another project"
        );
    }
    let mut updated_source = source.clone();
    updated_source.replace_range(removal_start..removal_end, "");
    let target_source = fs::read_to_string(target_path)
        .with_context(|| format!("could not read {}", target_path.display()))?;
    let block = render_full_command(&command);
    let updated_target = if let Some(target_group_index) = target_group_index {
        let target_layout = ConfigLayout::parse(&target_source, &target_project)?;
        insert_block(
            &target_source,
            target_layout.groups[target_group_index].end,
            &block,
        )
    } else {
        insert_block(
            &target_source,
            target_source.len(),
            &format!(
                "[[groups]]\nname = {}\n\n{}",
                toml_string(target_group),
                block
            ),
        )
    };

    validate_source_candidate(source_path, &updated_source, catalog)?;
    validate_source_candidate(target_path, &updated_target, catalog)?;
    commit_validated(target_path, &updated_target, catalog)?;
    if let Err(error) = commit_validated(source_path, &updated_source, catalog) {
        let rollback =
            delete_with_catalog_quiet(target_path, Some(target.to_owned()), true, catalog);
        return Err(error).context(format!(
            "target was updated but source could not be; rollback {}",
            if rollback.is_ok() {
                "succeeded"
            } else {
                "failed"
            }
        ));
    }
    Ok(())
}

pub(crate) fn add_action_with_catalog_quiet(
    path: &Path,
    parent: &str,
    options: &ActionOptions,
    catalog: &ThemeCatalog,
) -> Result<()> {
    let project = load_project_with_catalog(path, catalog)?;
    let (group_index, command_index) = find_command(&project, parent)?;
    let command = &project.groups[group_index].commands[command_index];
    if command
        .actions
        .iter()
        .any(|action| action.name == options.name)
    {
        bail!(
            "action {:?} already exists under command {parent:?}",
            options.name
        );
    }
    let source =
        fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    let layout = ConfigLayout::parse(&source, &project)?;
    let command_layout = &layout.groups[group_index].commands[command_index];
    let updated = insert_block(&source, command_layout.end, &render_action(options));
    commit_validated(path, &updated, catalog)
}

pub(crate) fn edit_action_with_catalog_quiet(
    path: &Path,
    parent: &str,
    target: &str,
    options: &ActionOptions,
    catalog: &ThemeCatalog,
) -> Result<()> {
    let project = load_project_with_catalog(path, catalog)?;
    let (group_index, command_index) = find_command(&project, parent)?;
    let command = &project.groups[group_index].commands[command_index];
    let action_index = find_action(command, target)?;
    if options.name != target
        && command
            .actions
            .iter()
            .any(|action| action.name == options.name)
    {
        bail!(
            "action {:?} already exists under command {parent:?}",
            options.name
        );
    }
    let source =
        fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    let layout = ConfigLayout::parse(&source, &project)?;
    let action = &layout.groups[group_index].commands[command_index].actions[action_index];
    let mut changes = Vec::new();
    let mut insertions = Vec::new();
    set_action_field(
        &source,
        action,
        "name",
        Some(toml_string(&options.name)),
        &mut changes,
        &mut insertions,
    )?;
    set_action_field(
        &source,
        action,
        "run",
        Some(toml_string(&options.run)),
        &mut changes,
        &mut insertions,
    )?;
    set_action_field(
        &source,
        action,
        "cwd",
        options.cwd.as_deref().map(toml_string),
        &mut changes,
        &mut insertions,
    )?;
    set_action_field(
        &source,
        action,
        "pre",
        (!options.pre.is_empty()).then(|| render_string_array(&options.pre)),
        &mut changes,
        &mut insertions,
    )?;
    set_action_field(
        &source,
        action,
        "requires_stopped",
        options.requires_stopped.then_some("true".to_owned()),
        &mut changes,
        &mut insertions,
    )?;
    set_action_field(
        &source,
        action,
        "restart_after",
        (options.restart_after != RestartAfter::Never)
            .then(|| toml_string(restart_after_value(options.restart_after))),
        &mut changes,
        &mut insertions,
    )?;
    if !insertions.is_empty() {
        changes.push(Change {
            start: action.body_end,
            end: action.body_end,
            replacement: format!("{}\n", insertions.join("\n")),
        });
    }
    let updated = apply_changes(source, changes)?;
    commit_validated(path, &updated, catalog)
}

pub(crate) fn delete_action_with_catalog_quiet(
    path: &Path,
    parent: &str,
    target: &str,
    catalog: &ThemeCatalog,
) -> Result<()> {
    let project = load_project_with_catalog(path, catalog)?;
    let (group_index, command_index) = find_command(&project, parent)?;
    let action_index = find_action(&project.groups[group_index].commands[command_index], target)?;
    let source =
        fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    let layout = ConfigLayout::parse(&source, &project)?;
    let action = &layout.groups[group_index].commands[command_index].actions[action_index];
    let mut start = action.start;
    if start > 0 && source[..start].ends_with("\n\n") {
        start -= 1;
    }
    let mut updated = source;
    updated.replace_range(start..action.end, "");
    commit_validated(path, &updated, catalog)
}

pub(crate) fn reorder_action_with_catalog(
    path: &Path,
    parent: &str,
    target: &str,
    direction: isize,
    catalog: &ThemeCatalog,
) -> Result<()> {
    if !matches!(direction, -1 | 1) {
        bail!("reorder direction must be -1 or 1");
    }
    let project = load_project_with_catalog(path, catalog)?;
    let (group_index, command_index) = find_command(&project, parent)?;
    let command = &project.groups[group_index].commands[command_index];
    let action_index = find_action(command, target)?;
    let destination = action_index
        .checked_add_signed(direction)
        .filter(|destination| *destination < command.actions.len())
        .context("action is already at the edge of its command")?;
    let source =
        fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    let layout = ConfigLayout::parse(&source, &project)?;
    let first_index = action_index.min(destination);
    let second_index = action_index.max(destination);
    let first = &layout.groups[group_index].commands[command_index].actions[first_index];
    let second = &layout.groups[group_index].commands[command_index].actions[second_index];
    if first.end != second.start {
        bail!("actions separated by another nested table cannot be reordered");
    }
    let replacement = format!(
        "{}{}",
        &source[second.start..second.end],
        &source[first.start..first.end]
    );
    let mut updated = source.clone();
    updated.replace_range(first.start..second.end, &replacement);
    commit_validated(path, &updated, catalog)
}

pub(crate) fn move_action_with_catalog(
    source_path: &Path,
    source_parent: &str,
    target: &str,
    target_path: &Path,
    target_parent: &str,
    catalog: &ThemeCatalog,
) -> Result<()> {
    if source_path == target_path && source_parent == target_parent {
        return Ok(());
    }
    let source_project = load_project_with_catalog(source_path, catalog)?;
    let (source_group, source_command) = find_command(&source_project, source_parent)?;
    let parent = &source_project.groups[source_group].commands[source_command];
    let action = parent
        .actions
        .iter()
        .find(|action| action.name == target)
        .with_context(|| format!("action {target:?} was not found under {source_parent:?}"))?;
    let options = action_options_for_move(action, parent);
    add_action_with_catalog_quiet(target_path, target_parent, &options, catalog)?;
    if let Err(error) =
        delete_action_with_catalog_quiet(source_path, source_parent, target, catalog)
    {
        let rollback =
            delete_action_with_catalog_quiet(target_path, target_parent, target, catalog);
        return Err(error).context(format!(
            "target was updated but source could not be; rollback {}",
            if rollback.is_ok() {
                "succeeded"
            } else {
                "failed"
            }
        ));
    }
    Ok(())
}

pub fn register(
    project_path: &Path,
    name: Option<String>,
    registry_path: Option<PathBuf>,
) -> Result<()> {
    let project = load_project(project_path)?;
    let name = name.unwrap_or_else(|| project.name.clone());
    let name = name.trim();
    if name.is_empty() {
        bail!("registered project name must not be empty");
    }
    let home = home_directory()?;
    let registry_path = registry_path
        .map(|path| expand_home_path(&path, &home))
        .or_else(|| find_project_list(&home))
        .unwrap_or_else(|| candidate_paths(&home)[0].clone());
    let existing = if registry_path.exists() {
        project_list::load(&registry_path, &home)?
    } else {
        Vec::new()
    };
    let project_path = fs::canonicalize(project_path)
        .with_context(|| format!("could not resolve {}", project_path.display()))?;
    if let Some(entry) = existing.iter().find(|entry| entry.path == project_path) {
        if entry.name == name {
            println!(
                "Project {name:?} is already registered in {}",
                registry_path.display()
            );
            return Ok(());
        }
        bail!(
            "{} is already registered as {:?} in {}; remove or rename that entry first",
            project_path.display(),
            entry.name,
            registry_path.display()
        );
    }
    if let Some(entry) = existing.iter().find(|entry| entry.name == name) {
        bail!(
            "project name {name:?} is already registered for {}; use --name with a unique alias",
            entry.path.display()
        );
    }

    let source = if registry_path.exists() {
        fs::read_to_string(&registry_path)
            .with_context(|| format!("could not read {}", registry_path.display()))?
    } else {
        "version = 1\n".to_owned()
    };
    let block = format!(
        "[[projects]]\nname = {}\npath = {}",
        toml_string(name),
        toml_string(&project_path.display().to_string())
    );
    let updated = insert_block(&source, source.len(), &block);
    commit_project_list(&registry_path, &updated, &home)?;
    println!(
        "Registered project {name:?} as {} in {}",
        project_path.display(),
        registry_path.display()
    );
    Ok(())
}

pub fn deregister(project_path: &Path, registry_path: Option<PathBuf>, yes: bool) -> Result<()> {
    let home = home_directory()?;
    let registry_path = registry_path
        .map(|path| expand_home_path(&path, &home))
        .or_else(|| find_project_list(&home))
        .context("no global project list exists; this project is not registered")?;
    let entries = project_list::load(&registry_path, &home)?;
    let project_path = fs::canonicalize(project_path)
        .with_context(|| format!("could not resolve {}", project_path.display()))?;
    let entry_index = entries
        .iter()
        .position(|entry| entry.path == project_path)
        .with_context(|| {
            format!(
                "{} is not registered in {}",
                project_path.display(),
                registry_path.display()
            )
        })?;
    let entry = &entries[entry_index];
    if !yes {
        ensure_interactive_input(true)?;
        if !prompt_bool(
            &format!(
                "Deregister project {:?} from {}?",
                entry.name,
                registry_path.display()
            ),
            false,
        )? {
            bail!(
                "deregistration cancelled; {} was left unchanged",
                registry_path.display()
            );
        }
    }

    if entries.len() == 1 {
        fs::remove_file(&registry_path)
            .with_context(|| format!("could not remove {}", registry_path.display()))?;
        println!(
            "Deregistered project {:?} and removed empty project list {}",
            entry.name,
            registry_path.display()
        );
        return Ok(());
    }

    let source = fs::read_to_string(&registry_path)
        .with_context(|| format!("could not read {}", registry_path.display()))?;
    let updated = remove_project_entry(&source, entry_index)?;
    commit_project_list(&registry_path, &updated, &home)?;
    println!(
        "Deregistered project {:?} from {}",
        entry.name,
        registry_path.display()
    );
    Ok(())
}

pub fn deregister_from_registry(
    registry_path: &Path,
    project_alias: Option<&str>,
    yes: bool,
) -> Result<()> {
    let home = home_directory()?;
    let entries = project_list::load(registry_path, &home)?;
    let entry = if let Some(alias) = project_alias {
        entries
            .iter()
            .find(|entry| entry.name == alias)
            .with_context(|| {
                format!(
                    "project {alias:?} is not registered in {}; available projects: {}",
                    registry_path.display(),
                    entries
                        .iter()
                        .map(|entry| entry.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?
    } else {
        ensure_interactive_input(true)?;
        println!("Registered projects in {}:", registry_path.display());
        for (index, entry) in entries.iter().enumerate() {
            println!("  {}. {} ({})", index + 1, entry.name, entry.path.display());
        }
        loop {
            let selection = prompt("Project to deregister (name or number)", None, false)?;
            if let Ok(index) = selection.parse::<usize>()
                && let Some(entry) = index.checked_sub(1).and_then(|index| entries.get(index))
            {
                break entry;
            }
            if let Some(entry) = entries.iter().find(|entry| entry.name == selection) {
                break entry;
            }
            eprintln!("No registered project matches {selection:?}; try again.");
        }
    };
    deregister(&entry.path, Some(registry_path.to_path_buf()), yes)
}

fn load_project(path: &Path) -> Result<ProjectConfig> {
    let catalog = active_theme_catalog();
    load_project_with_catalog(path, &catalog)
}

fn load_project_with_catalog(path: &Path, catalog: &ThemeCatalog) -> Result<ProjectConfig> {
    let report = validate_file_for_combined_with_catalog(path, catalog);
    if !report.is_valid() {
        bail!(
            "{} is invalid: {}",
            path.display(),
            report
                .issues
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    report.project.context("validated project was missing")
}

fn ensure_interactive_input(required: bool) -> Result<()> {
    if required && !std::io::stdin().is_terminal() {
        bail!("missing required options and stdin is not an interactive terminal");
    }
    Ok(())
}

fn required(value: Option<String>, label: &str) -> Result<String> {
    let value = value.unwrap_or_default();
    if value.trim().is_empty() {
        bail!("{label} must not be empty");
    }
    Ok(value.trim().to_owned())
}

fn print_commands(project: &ProjectConfig) {
    println!("Available commands:");
    for group in &project.groups {
        for command in &group.commands {
            println!("  - {} / {}", group.name, command.name);
        }
    }
}

fn find_command(project: &ProjectConfig, name: &str) -> Result<(usize, usize)> {
    project
        .groups
        .iter()
        .enumerate()
        .find_map(|(group_index, group)| {
            group
                .commands
                .iter()
                .position(|command| command.name == name)
                .map(|command_index| (group_index, command_index))
        })
        .with_context(|| format!("command {name:?} was not found"))
}

fn find_action(command: &crate::config::CommandConfig, name: &str) -> Result<usize> {
    command
        .actions
        .iter()
        .position(|action| action.name == name)
        .with_context(|| format!("action {name:?} was not found under {:?}", command.name))
}

fn prompt_pre_steps() -> Result<Vec<String>> {
    let mut steps = Vec::new();
    loop {
        let step = prompt("Pre-step (blank to finish)", None, true)?;
        if step.is_empty() {
            return Ok(steps);
        }
        steps.push(step);
    }
}

fn render_command(name: &str, run: &str, cwd: &str, pre: &[String], autostart: bool) -> String {
    let mut output = format!(
        "[[groups.commands]]\nname = {}\nrun = {}\ncwd = {}\nautostart = {}",
        toml_string(name),
        toml_string(run),
        toml_string(cwd),
        autostart
    );
    if !pre.is_empty() {
        output.push_str(&format!("\npre = {}", render_string_array(pre)));
    }
    output
}

fn render_action(options: &ActionOptions) -> String {
    let mut output = format!(
        "[[groups.commands.actions]]\nname = {}\nrun = {}",
        toml_string(&options.name),
        toml_string(&options.run)
    );
    if let Some(cwd) = options.cwd.as_deref().filter(|cwd| !cwd.trim().is_empty()) {
        output.push_str(&format!("\ncwd = {}", toml_string(cwd.trim())));
    }
    if !options.pre.is_empty() {
        output.push_str(&format!("\npre = {}", render_string_array(&options.pre)));
    }
    if options.requires_stopped {
        output.push_str("\nrequires_stopped = true");
    }
    if options.restart_after != RestartAfter::Never {
        output.push_str(&format!(
            "\nrestart_after = {}",
            toml_string(restart_after_value(options.restart_after))
        ));
    }
    output
}

fn restart_after_value(value: RestartAfter) -> &'static str {
    match value {
        RestartAfter::Never => "never",
        RestartAfter::IfRunning => "if-running",
        RestartAfter::Always => "always",
    }
}

fn action_options_for_move(
    action: &ActionConfig,
    parent: &crate::config::CommandConfig,
) -> ActionOptions {
    ActionOptions {
        name: action.name.clone(),
        run: action.run.clone(),
        cwd: (action.cwd != parent.cwd).then(|| action.cwd.display().to_string()),
        pre: action.pre.clone(),
        requires_stopped: action.requires_stopped,
        restart_after: action.restart_after,
    }
}

fn render_full_command(command: &crate::config::CommandConfig) -> String {
    let mut output = render_command(
        &command.name,
        &command.run,
        &command.cwd.display().to_string(),
        &command.pre,
        command.autostart,
    );
    if !command.shell_setup.is_empty() {
        output.push_str(&format!(
            "\nshell_setup = {}",
            render_string_array(&command.shell_setup)
        ));
    }
    if let Some(log_file) = &command.log_file {
        output.push_str(&format!(
            "\nlog_file = {}",
            toml_string(&log_file.display().to_string())
        ));
    }
    if let Some(bytes) = command.log_rotate_bytes {
        output.push_str(&format!("\nlog_rotate_bytes = {bytes}"));
    }
    output.push_str(&format!(
        "\nlog_rotate_keep = {}\nstop_timeout = {}",
        command.log_rotate_keep, command.stop_timeout
    ));
    for action in &command.actions {
        output.push_str("\n\n");
        output.push_str(&render_action(&action_options_for_move(action, command)));
    }
    output
}

fn render_string_array(values: &[String]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(|value| toml_string(value))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn insert_block(source: &str, at: usize, block: &str) -> String {
    let before = &source[..at];
    let after = &source[at..];
    let separator = if before.is_empty() || before.ends_with("\n\n") {
        ""
    } else if before.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    let after_separator = if after.is_empty() { "\n" } else { "\n\n" };
    format!("{before}{separator}{block}{after_separator}{after}")
}

fn remove_project_entry(source: &str, index: usize) -> Result<String> {
    let headers = source_lines(source)
        .into_iter()
        .filter(|line| toml_code(line.text) == "[[projects]]")
        .collect::<Vec<_>>();
    let header = headers
        .get(index)
        .with_context(|| format!("could not find projects[{index}] in the global list"))?;
    let mut start = header.start;
    if start > 0 && source[..start].ends_with("\n\n") {
        start -= 1;
    }
    let end = headers
        .get(index + 1)
        .map(|next| next.start)
        .unwrap_or(source.len());
    let mut updated = source.to_owned();
    updated.replace_range(start..end, "");
    Ok(updated)
}

#[derive(Debug)]
struct ConfigLayout {
    groups: Vec<GroupLayout>,
}

#[derive(Debug)]
struct GroupLayout {
    start: usize,
    end: usize,
    commands: Vec<CommandLayout>,
}

#[derive(Debug)]
struct CommandLayout {
    start: usize,
    end: usize,
    body_start: usize,
    body_end: usize,
    actions: Vec<ActionLayout>,
}

#[derive(Debug)]
struct ActionLayout {
    start: usize,
    end: usize,
    body_start: usize,
    body_end: usize,
}

#[derive(Clone, Copy)]
struct SourceLine<'a> {
    start: usize,
    end: usize,
    text: &'a str,
}

impl ConfigLayout {
    fn parse(source: &str, project: &ProjectConfig) -> Result<Self> {
        let lines = source_lines(source);
        let group_headers = lines
            .iter()
            .filter(|line| toml_code(line.text) == "[[groups]]")
            .copied()
            .collect::<Vec<_>>();
        if group_headers.len() != project.groups.len() {
            bail!("could not map [[groups]] tables in the source file");
        }
        let mut groups = Vec::with_capacity(group_headers.len());
        for (group_index, header) in group_headers.iter().enumerate() {
            let end = group_headers
                .get(group_index + 1)
                .map(|next| next.start)
                .unwrap_or(source.len());
            let command_headers = lines
                .iter()
                .filter(|line| {
                    line.start > header.start
                        && line.start < end
                        && toml_code(line.text) == "[[groups.commands]]"
                })
                .copied()
                .collect::<Vec<_>>();
            if command_headers.len() != project.groups[group_index].commands.len() {
                bail!(
                    "could not map command tables for group {:?}",
                    project.groups[group_index].name
                );
            }
            let commands = command_headers
                .iter()
                .enumerate()
                .map(|(command_index, command_header)| {
                    let command_end = command_headers
                        .get(command_index + 1)
                        .map(|next| next.start)
                        .unwrap_or(end);
                    let body_end = lines
                        .iter()
                        .find(|line| {
                            line.start >= command_header.end
                                && line.start < command_end
                                && toml_code(line.text).starts_with('[')
                        })
                        .map(|line| line.start)
                        .unwrap_or(command_end);
                    let action_headers = lines
                        .iter()
                        .filter(|line| {
                            line.start > command_header.start
                                && line.start < command_end
                                && toml_code(line.text) == "[[groups.commands.actions]]"
                        })
                        .copied()
                        .collect::<Vec<_>>();
                    let actions = action_headers
                        .iter()
                        .map(|action_header| {
                            let end = lines
                                .iter()
                                .find(|line| {
                                    line.start >= action_header.end
                                        && line.start < command_end
                                        && toml_code(line.text).starts_with('[')
                                })
                                .map(|line| line.start)
                                .unwrap_or(command_end);
                            ActionLayout {
                                start: action_header.start,
                                end,
                                body_start: action_header.end,
                                body_end: end,
                            }
                        })
                        .collect();
                    CommandLayout {
                        start: command_header.start,
                        end: command_end,
                        body_start: command_header.end,
                        body_end,
                        actions,
                    }
                })
                .collect();
            groups.push(GroupLayout {
                start: header.start,
                end,
                commands,
            });
        }
        Ok(Self { groups })
    }
}

#[derive(Debug)]
struct Change {
    start: usize,
    end: usize,
    replacement: String,
}

fn add_field_change(
    source: &str,
    command: &CommandLayout,
    key: &str,
    value: Option<String>,
    changes: &mut Vec<Change>,
    insertions: &mut Vec<String>,
) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    if let Some((start, end)) = find_field_span(source, command.body_start, command.body_end, key)?
    {
        let newline = line_ending(&source[start..end]);
        changes.push(Change {
            start,
            end,
            replacement: format!("{key} = {value}{newline}"),
        });
    } else {
        insertions.push(format!("{key} = {value}"));
    }
    Ok(())
}

fn set_action_field(
    source: &str,
    action: &ActionLayout,
    key: &str,
    value: Option<String>,
    changes: &mut Vec<Change>,
    insertions: &mut Vec<String>,
) -> Result<()> {
    let existing = find_field_span(source, action.body_start, action.body_end, key)?;
    match (existing, value) {
        (Some((start, end)), Some(value)) => {
            let newline = line_ending(&source[start..end]);
            changes.push(Change {
                start,
                end,
                replacement: format!("{key} = {value}{newline}"),
            });
        }
        (Some((start, end)), None) => changes.push(Change {
            start,
            end,
            replacement: String::new(),
        }),
        (None, Some(value)) => insertions.push(format!("{key} = {value}")),
        (None, None) => {}
    }
    Ok(())
}

fn rename_local_dependencies(source: &str, old: &str, new: &str) -> Result<Vec<Change>> {
    let lines = source_lines(source);
    let headers = lines
        .iter()
        .filter(|line| toml_code(line.text).starts_with('['))
        .copied()
        .collect::<Vec<_>>();
    let mut changes = Vec::new();
    for (index, header) in headers.iter().enumerate() {
        if toml_code(header.text) != "[[groups.commands.wait_for]]" {
            continue;
        }
        let end = headers
            .get(index + 1)
            .map(|next| next.start)
            .unwrap_or(source.len());
        let Some((start, field_end)) = find_field_span(source, header.end, end, "command")? else {
            continue;
        };
        let Some(value) = parse_field_string(&source[start..field_end], "command") else {
            continue;
        };
        if value == old {
            let newline = line_ending(&source[start..field_end]);
            changes.push(Change {
                start,
                end: field_end,
                replacement: format!("command = {}{newline}", toml_string(new)),
            });
        }
    }
    Ok(changes)
}

fn find_field_span(
    source: &str,
    start: usize,
    end: usize,
    key: &str,
) -> Result<Option<(usize, usize)>> {
    let lines = source_lines(source);
    let Some((line_index, line)) = lines.iter().enumerate().find(|(_, line)| {
        line.start >= start && line.start < end && assignment_key(line.text) == Some(key)
    }) else {
        return Ok(None);
    };
    let equals = line
        .text
        .find('=')
        .context("configuration assignment had no equals sign")?;
    let value_start = line.start + equals + 1;
    for candidate in lines.iter().skip(line_index) {
        if candidate.end > end {
            break;
        }
        let value = &source[value_start..candidate.end];
        if format!("value = {value}").parse::<toml::Table>().is_ok() {
            return Ok(Some((line.start, candidate.end)));
        }
    }
    bail!("could not parse value for field {key:?}")
}

fn parse_field_string(source: &str, key: &str) -> Option<String> {
    source
        .parse::<toml::Table>()
        .ok()?
        .get(key)?
        .as_str()
        .map(str::to_owned)
}

fn apply_changes(mut source: String, mut changes: Vec<Change>) -> Result<String> {
    changes.sort_by_key(|change| std::cmp::Reverse(change.start));
    let mut previous_start = source.len();
    for change in changes {
        if change.end > previous_start || change.start > change.end || change.end > source.len() {
            bail!("overlapping or invalid configuration edits");
        }
        source.replace_range(change.start..change.end, &change.replacement);
        previous_start = change.start;
    }
    Ok(source)
}

fn source_lines(source: &str) -> Vec<SourceLine<'_>> {
    let mut offset = 0;
    source
        .split_inclusive('\n')
        .map(|text| {
            let start = offset;
            offset += text.len();
            SourceLine {
                start,
                end: offset,
                text,
            }
        })
        .collect()
}

fn toml_code(line: &str) -> &str {
    line.trim().split('#').next().unwrap_or_default().trim()
}

fn assignment_key(line: &str) -> Option<&str> {
    let code = toml_code(line);
    if code.starts_with('[') {
        return None;
    }
    code.split_once('=').map(|(key, _)| key.trim())
}

fn line_ending(value: &str) -> &'static str {
    if value.ends_with("\r\n") {
        "\r\n"
    } else if value.ends_with('\n') {
        "\n"
    } else {
        ""
    }
}

fn commit_validated(path: &Path, source: &str, catalog: &ThemeCatalog) -> Result<()> {
    commit_atomic(path, source, |candidate| {
        let report = validate_file_for_combined_with_catalog(candidate, catalog);
        if !report.is_valid() {
            bail!(
                "the requested change would make the project invalid: {}",
                report
                    .issues
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            );
        }
        Ok(())
    })
}

fn validate_source_candidate(path: &Path, source: &str, catalog: &ThemeCatalog) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let candidate = (0..100)
        .map(|attempt| {
            parent.join(format!(
                ".blade-validate-{}-{attempt}.tmp",
                std::process::id()
            ))
        })
        .find(|candidate| !candidate.exists())
        .context("could not allocate a validation file")?;
    fs::write(&candidate, source)
        .with_context(|| format!("could not write temporary file {}", candidate.display()))?;
    let report = validate_file_for_combined_with_catalog(&candidate, catalog);
    let _ = fs::remove_file(&candidate);
    if !report.is_valid() {
        bail!(
            "the requested change would make {} invalid: {}",
            path.display(),
            report
                .issues
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    Ok(())
}

fn commit_project_list(path: &Path, source: &str, home: &Path) -> Result<()> {
    commit_atomic(path, source, |candidate| {
        let config = project_list::load_config(candidate, home)?;
        let mut projects = Vec::with_capacity(config.projects.len());
        for entry in config.projects {
            let report =
                validate_file_for_combined_with_catalog(&entry.path, &config.theme_catalog);
            if !report.is_valid() {
                bail!(
                    "referenced project {:?} is invalid: {}",
                    entry.name,
                    report
                        .issues
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("; ")
                );
            }
            projects.push((
                entry.name,
                report.project.context("validated project was missing")?,
            ));
        }
        combine_projects(candidate.to_path_buf(), projects)?;
        Ok(())
    })
}

fn active_theme_catalog() -> crate::theme::ThemeCatalog {
    let Ok(home) = home_directory() else {
        return crate::theme::ThemeCatalog::default();
    };
    let Some(path) = find_project_list(&home) else {
        return crate::theme::ThemeCatalog::default();
    };
    project_list::load_settings(&path, &home)
        .map(|settings| settings.theme_catalog)
        .unwrap_or_default()
}

fn commit_atomic<F>(path: &Path, source: &str, validate: F) -> Result<()>
where
    F: FnOnce(&Path) -> Result<()>,
{
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))?;
    let mut pending = None;
    for attempt in 0..100 {
        let candidate = parent.join(format!(".blade-edit-{}-{attempt}.tmp", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                pending = Some(PendingFile {
                    path: candidate,
                    file: Some(file),
                    committed: false,
                });
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("could not create temporary config file"),
        }
    }
    let mut pending = pending.context("could not allocate a temporary config file")?;
    let file = pending.file.as_mut().context("temporary file was closed")?;
    file.write_all(source.as_bytes())?;
    file.sync_all()?;
    pending.file.take();

    validate(&pending.path)?;
    if path.exists() {
        let permissions = fs::metadata(path)
            .with_context(|| format!("could not inspect {}", path.display()))?
            .permissions();
        fs::set_permissions(&pending.path, permissions)?;
    }
    fs::rename(&pending.path, path)
        .with_context(|| format!("could not replace {}", path.display()))?;
    pending.committed = true;
    Ok(())
}

fn expand_home_path(path: &Path, home: &Path) -> PathBuf {
    if path == Path::new("~") {
        return home.to_path_buf();
    }
    if let Ok(relative) = path.strip_prefix("~/") {
        return home.join(relative);
    }
    path.to_path_buf()
}

struct PendingFile {
    path: PathBuf,
    file: Option<fs::File>,
    committed: bool,
}

impl Drop for PendingFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use crate::config::{RestartAfter, validate_file};

    use super::{
        ActionOptions, AddOptions, EditOptions, add, add_action_with_catalog_quiet, delete,
        delete_action_with_catalog_quiet, deregister, edit, edit_action_with_catalog_quiet,
        move_action_with_catalog, move_with_catalog, register, reorder_action_with_catalog,
        reorder_with_catalog,
    };

    fn project_file() -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"version = 1
name = "Test"
shell = "/bin/sh"

# Keep this comment.
[[groups]]
name = "Backend"

[[groups.commands]]
name = "api"
run = "echo api"
cwd = "."
autostart = false

[[groups.commands]]
name = "worker"
run = "echo worker"
cwd = "."
pre = [
  "echo prepare",
]

[[groups.commands.wait_for]]
command = "api"
kind = "delay"
seconds = 1
"#,
        )
        .unwrap();
        (directory, path)
    }

    #[test]
    fn adds_to_an_existing_or_new_group_without_losing_comments() {
        let (_directory, path) = project_file();
        add(
            &path,
            AddOptions {
                group: Some("Backend".to_owned()),
                name: Some("scheduler".to_owned()),
                run: Some("echo scheduler".to_owned()),
                cwd: Some(".".to_owned()),
                pre: vec!["echo install".to_owned()],
                autostart: true,
            },
        )
        .unwrap();
        add(
            &path,
            AddOptions {
                group: Some("Frontend".to_owned()),
                name: Some("dashboard".to_owned()),
                run: Some("yarn start".to_owned()),
                cwd: Some("frontend".to_owned()),
                pre: Vec::new(),
                autostart: false,
            },
        )
        .unwrap();

        let source = fs::read_to_string(&path).unwrap();
        assert!(source.contains("# Keep this comment."));
        let report = validate_file(&path);
        assert!(report.is_valid(), "{:?}", report.issues);
        let project = report.project.unwrap();
        assert!(project.command("scheduler").is_some());
        assert!(project.command("dashboard").is_some());
    }

    #[test]
    fn adds_edits_reorders_moves_and_deletes_nested_actions() {
        let (_directory, path) = project_file();
        let catalog = crate::theme::ThemeCatalog::default();
        let action = |name: &str, run: &str| ActionOptions {
            name: name.to_owned(),
            run: run.to_owned(),
            cwd: None,
            pre: Vec::new(),
            requires_stopped: false,
            restart_after: RestartAfter::Never,
        };
        add_action_with_catalog_quiet(&path, "api", &action("pull", "git pull"), &catalog).unwrap();
        add_action_with_catalog_quiet(&path, "api", &action("install", "yarn install"), &catalog)
            .unwrap();
        reorder_action_with_catalog(&path, "api", "install", -1, &catalog).unwrap();
        edit_action_with_catalog_quiet(
            &path,
            "api",
            "pull",
            &ActionOptions {
                name: "update".to_owned(),
                run: "git pull --ff-only".to_owned(),
                cwd: Some("frontend".to_owned()),
                pre: vec!["git status --short".to_owned()],
                requires_stopped: true,
                restart_after: RestartAfter::IfRunning,
            },
            &catalog,
        )
        .unwrap();
        move_action_with_catalog(&path, "api", "install", &path, "worker", &catalog).unwrap();
        delete_action_with_catalog_quiet(&path, "api", "update", &catalog).unwrap();

        let report = validate_file(&path);
        assert!(report.is_valid(), "{:?}", report.issues);
        let project = report.project.unwrap();
        assert!(project.command("api").unwrap().actions.is_empty());
        let worker = project.command("worker").unwrap();
        assert_eq!(worker.actions.len(), 1);
        assert_eq!(worker.actions[0].name, "install");
        assert_eq!(worker.actions[0].run, "yarn install");
    }

    #[test]
    fn edits_multiline_pre_steps_and_renames_local_dependencies() {
        let (_directory, path) = project_file();
        edit(
            &path,
            EditOptions {
                target: Some("worker".to_owned()),
                new_name: None,
                run: None,
                cwd: None,
                pre: Some(vec!["echo one".to_owned(), "echo two".to_owned()]),
                autostart: None,
            },
        )
        .unwrap();
        edit(
            &path,
            EditOptions {
                target: Some("api".to_owned()),
                new_name: Some("web".to_owned()),
                run: Some("echo web".to_owned()),
                cwd: None,
                pre: None,
                autostart: Some(true),
            },
        )
        .unwrap();

        let source = fs::read_to_string(&path).unwrap();
        assert!(source.contains("command = \"web\""));
        let report = validate_file(&path);
        assert!(report.is_valid(), "{:?}", report.issues);
        let project = report.project.unwrap();
        let web = project.command("web").unwrap();
        assert_eq!(web.run, "echo web");
        assert!(web.autostart);
        assert_eq!(
            project.command("worker").unwrap().pre,
            ["echo one", "echo two"]
        );
    }

    #[test]
    fn refuses_to_delete_a_referenced_command_and_preserves_the_file() {
        let (_directory, path) = project_file();
        let original = fs::read_to_string(&path).unwrap();

        assert!(delete(&path, Some("api".to_owned()), true).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);

        delete(&path, Some("worker".to_owned()), true).unwrap();
        assert!(validate_file(&path).is_valid());
        assert!(
            !fs::read_to_string(&path)
                .unwrap()
                .contains("name = \"worker\"")
        );
    }

    #[test]
    fn deleting_the_final_command_removes_its_empty_group() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"name = "Test"
shell = "/bin/sh"
[[groups]]
name = "Remove"
[[groups.commands]]
name = "task"
run = "true"
[[groups]]
name = "Keep"
[[groups.commands]]
name = "server"
run = "true"
"#,
        )
        .unwrap();

        delete(&path, Some("task".to_owned()), true).unwrap();

        let project = validate_file(&path).project.unwrap();
        assert!(project.groups.iter().all(|group| group.name != "Remove"));
        assert!(project.command("server").is_some());
    }

    #[test]
    fn reorders_and_moves_commands_between_groups_without_losing_configuration() {
        let (_directory, path) = project_file();
        let catalog = crate::theme::ThemeCatalog::default();

        reorder_with_catalog(&path, "worker", -1, &catalog).unwrap();
        let project = validate_file(&path).project.unwrap();
        assert_eq!(project.groups[0].commands[0].name, "worker");
        assert_eq!(project.groups[0].commands[1].name, "api");
        assert_eq!(project.command("worker").unwrap().pre, ["echo prepare"]);

        move_with_catalog(&path, "worker", &path, "Workers", &catalog).unwrap();
        let project = validate_file(&path).project.unwrap();
        let workers = project
            .groups
            .iter()
            .find(|group| group.name == "Workers")
            .unwrap();
        assert_eq!(workers.commands.len(), 1);
        assert_eq!(workers.commands[0].name, "worker");
        assert_eq!(workers.commands[0].wait_for[0].command, "api");
    }

    #[test]
    fn moves_a_dependency_free_command_between_project_files() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source.blade");
        let target = directory.path().join("target.blade");
        fs::write(
            &source,
            r#"name = "Source"
shell = "/bin/sh"
[[groups]]
name = "Project"
[[groups.commands]]
name = "task"
run = "echo task"
cwd = "."
pre = ["echo prepare"]
"#,
        )
        .unwrap();
        fs::write(
            &target,
            r#"name = "Target"
shell = "/bin/sh"
[[groups]]
name = "Project"
"#,
        )
        .unwrap();
        let catalog = crate::theme::ThemeCatalog::default();

        move_with_catalog(&source, "task", &target, "Imported", &catalog).unwrap();

        assert!(validate_file(&source).project.unwrap().groups.is_empty());
        let moved = validate_file(&target)
            .project
            .unwrap()
            .command("task")
            .unwrap()
            .clone();
        assert_eq!(moved.run, "echo task");
        assert_eq!(moved.pre, ["echo prepare"]);
    }

    #[test]
    fn moving_the_final_command_removes_its_empty_group() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"name = "Test"
shell = "/bin/sh"
[[groups]]
name = "Source"
[[groups.commands]]
name = "task"
run = "echo task"
[[groups]]
name = "Destination"
[[groups.commands]]
name = "keep"
run = "echo keep"
"#,
        )
        .unwrap();
        let catalog = crate::theme::ThemeCatalog::default();

        move_with_catalog(&path, "task", &path, "Destination", &catalog).unwrap();

        let project = validate_file(&path).project.unwrap();
        assert!(project.groups.iter().all(|group| group.name != "Source"));
        let destination = project
            .groups
            .iter()
            .find(|group| group.name == "Destination")
            .unwrap();
        assert_eq!(
            destination
                .commands
                .iter()
                .map(|command| command.name.as_str())
                .collect::<Vec<_>>(),
            ["keep", "task"]
        );
    }

    #[test]
    fn registers_the_project_without_duplicating_it() {
        let (_directory, path) = project_file();
        let registry = path.parent().unwrap().join("projects.config");

        register(
            &path,
            Some("Registered Test".to_owned()),
            Some(registry.clone()),
        )
        .unwrap();
        let first = fs::read_to_string(&registry).unwrap();
        register(
            &path,
            Some("Registered Test".to_owned()),
            Some(registry.clone()),
        )
        .unwrap();

        assert_eq!(fs::read_to_string(&registry).unwrap(), first);
        let entries = crate::project_list::load(&registry, path.parent().unwrap()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "Registered Test");
        assert_eq!(entries[0].path, fs::canonicalize(&path).unwrap());

        deregister(&path, Some(registry.clone()), true).unwrap();
        assert!(!registry.exists());
    }
}
