use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::Path,
};

use anyhow::{Context, Result, bail};

use crate::config::validate_file;

pub(crate) const DEFAULT_GROUP_NAME: &str = "Project";

#[derive(Debug)]
struct WizardProject {
    name: String,
    shell: String,
    log_dir: Option<String>,
    groups: Vec<WizardGroup>,
}

#[derive(Debug)]
struct WizardGroup {
    name: String,
    commands: Vec<WizardCommand>,
}

#[derive(Debug)]
struct WizardCommand {
    name: String,
    run: String,
    cwd: String,
    pre: Vec<String>,
    autostart: bool,
}

pub fn initialize(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        let overwrite = prompt_bool(
            &format!("{} already exists. Overwrite it?", path.display()),
            false,
        )?;
        if !overwrite {
            bail!("initialization cancelled; the existing file was left unchanged");
        }
    }

    println!("Blade project initializer (Ctrl-C to cancel)\n");
    let directory_name = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .unwrap_or("project");
    let name = prompt("Project name", Some(directory_name), false)?;
    let default_shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned());
    let shell = prompt("User shell", Some(&default_shell), false)?;
    let log_dir = prompt("Log directory (blank disables file logging)", None, true)?;

    let mut groups = Vec::new();
    loop {
        let (hint, default, allow_empty) = if groups.is_empty() {
            ("Group name", Some(DEFAULT_GROUP_NAME), false)
        } else {
            ("Another group name (blank to finish)", None, true)
        };
        let group_name = prompt(hint, default, allow_empty)?;
        if group_name.is_empty() {
            break;
        }
        let mut commands = Vec::new();
        loop {
            let hint = if commands.is_empty() {
                "  Command name"
            } else {
                "  Another command name (blank to finish this group)"
            };
            let command_name = prompt(hint, None, !commands.is_empty())?;
            if command_name.is_empty() {
                break;
            }
            let run = prompt("  Run", None, false)?;
            let cwd = prompt("  Working directory", Some("."), false)?;
            let autostart = prompt_bool("  Start automatically", false)?;
            let mut pre = Vec::new();
            loop {
                let step = prompt("  Pre-step (blank to finish)", None, true)?;
                if step.is_empty() {
                    break;
                }
                pre.push(step);
            }
            commands.push(WizardCommand {
                name: command_name,
                run,
                cwd,
                pre,
                autostart,
            });
        }
        groups.push(WizardGroup {
            name: group_name,
            commands,
        });
    }

    let project = WizardProject {
        name,
        shell,
        log_dir: (!log_dir.is_empty()).then_some(log_dir),
        groups,
    };
    let contents = render_project(&project);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    let mut options = OpenOptions::new();
    options.write(true).truncate(true);
    if path.exists() {
        options.create(false);
    } else {
        options.create_new(true);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("could not create {}", path.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("could not write {}", path.display()))?;

    let report = validate_file(path);
    if !report.is_valid() {
        bail!(
            "created {}, but validation unexpectedly failed",
            path.display()
        );
    }
    println!("\nCreated {}", path.display());
    if project.groups.iter().all(|group| group.commands.is_empty()) {
        println!("Add at least one command, then run `blade validate`.");
    } else {
        println!("Run `blade` from this directory to open the runner.");
    }
    Ok(())
}

pub(crate) fn prompt(label: &str, default: Option<&str>, allow_empty: bool) -> Result<String> {
    loop {
        match default {
            Some(default) => print!("{label} [{default}]: "),
            None => print!("{label}: "),
        }
        io::stdout().flush()?;
        let mut value = String::new();
        if io::stdin().read_line(&mut value)? == 0 {
            bail!("input closed");
        }
        let value = value.trim().to_owned();
        if value.is_empty() {
            if let Some(default) = default {
                return Ok(default.to_owned());
            }
            if allow_empty {
                return Ok(value);
            }
            println!("A value is required.");
            continue;
        }
        return Ok(value);
    }
}

pub(crate) fn prompt_bool(label: &str, default: bool) -> Result<bool> {
    let suffix = if default { "Y/n" } else { "y/N" };
    loop {
        print!("{label} [{suffix}]: ");
        io::stdout().flush()?;
        let mut value = String::new();
        if io::stdin().read_line(&mut value)? == 0 {
            bail!("input closed");
        }
        match value.trim().to_ascii_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => println!("Please answer yes or no."),
        }
    }
}

fn render_project(project: &WizardProject) -> String {
    let mut output = format!(
        "version = 1\nname = {}\nshell = {}\nmax_log_lines = 100000\nstop_timeout = 10\n",
        toml_string(&project.name),
        toml_string(&project.shell),
    );
    if let Some(log_dir) = &project.log_dir {
        output.push_str(&format!("log_dir = {}\n", toml_string(log_dir)));
    }
    for group in &project.groups {
        output.push_str(&format!(
            "\n[[groups]]\nname = {}\n",
            toml_string(&group.name)
        ));
        for command in &group.commands {
            output.push_str(&format!(
                "\n[[groups.commands]]\nname = {}\nrun = {}\ncwd = {}\nautostart = {}\n",
                toml_string(&command.name),
                toml_string(&command.run),
                toml_string(&command.cwd),
                command.autostart,
            ));
            if !command.pre.is_empty() {
                let steps = command
                    .pre
                    .iter()
                    .map(|step| toml_string(step))
                    .collect::<Vec<_>>()
                    .join(", ");
                output.push_str(&format!("pre = [{steps}]\n"));
            }
        }
    }
    output
}

pub(crate) fn toml_string(value: &str) -> String {
    toml::Value::String(value.to_owned()).to_string()
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_GROUP_NAME, WizardCommand, WizardGroup, WizardProject, render_project};

    #[test]
    fn rendered_project_is_valid_toml() {
        let source = render_project(&WizardProject {
            name: "Blade \"demo\"".to_owned(),
            shell: "/bin/zsh".to_owned(),
            log_dir: Some(".blade-logs".to_owned()),
            groups: vec![WizardGroup {
                name: DEFAULT_GROUP_NAME.to_owned(),
                commands: vec![WizardCommand {
                    name: "API".to_owned(),
                    run: "echo ok".to_owned(),
                    cwd: ".".to_owned(),
                    pre: vec!["echo prepare".to_owned()],
                    autostart: true,
                }],
            }],
        });
        let parsed: toml::Table = source.parse().unwrap();
        assert_eq!(parsed["name"].as_str(), Some("Blade \"demo\""));
        assert_eq!(
            parsed["groups"][0]["name"].as_str(),
            Some(DEFAULT_GROUP_NAME)
        );
    }
}
