use std::{
    collections::HashSet,
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{
    config::{DEFAULT_FILE_NAME, RawTheme, Severity, THEME_KEYS, build_theme, valid_theme_name},
    theme::{Theme, ThemeCatalog, ThemeOverrides},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectEntry {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct GlobalTheme {
    pub theme: Theme,
    pub preset: String,
    pub overrides: ThemeOverrides,
}

#[derive(Debug, Clone)]
pub struct ProjectList {
    pub projects: Vec<ProjectEntry>,
    pub theme: Option<GlobalTheme>,
    pub theme_catalog: ThemeCatalog,
}

#[derive(Debug, Clone, Default)]
pub struct GlobalSettings {
    pub theme: Option<GlobalTheme>,
    pub theme_catalog: ThemeCatalog,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProjectList {
    version: Option<u32>,
    theme: Option<RawTheme>,
    #[serde(default)]
    themes: toml::Table,
    projects: Vec<RawProjectEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProjectEntry {
    name: String,
    path: PathBuf,
}

pub fn home_directory() -> Result<PathBuf> {
    env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .context("HOME is not set; cannot locate Blade's global project list")
}

pub fn candidate_paths(home: &Path) -> [PathBuf; 3] {
    [
        home.join(DEFAULT_FILE_NAME),
        home.join(".config").join("blade.config"),
        home.join(".config").join("blade.conf"),
    ]
}

pub fn find_file(home: &Path) -> Option<PathBuf> {
    candidate_paths(home)
        .into_iter()
        .find(|candidate| candidate.is_file())
}

pub fn load(path: &Path, home: &Path) -> Result<Vec<ProjectEntry>> {
    Ok(load_config(path, home)?.projects)
}

pub fn load_settings(path: &Path, home: &Path) -> Result<GlobalSettings> {
    let raw = read_raw(path)?;
    let version = raw.version.unwrap_or(1);
    if version != 1 {
        bail!("unsupported project list version {version}; expected 1");
    }
    let theme_catalog = resolve_custom_themes(raw.themes, path, home)?;
    let theme = resolve_theme(raw.theme, &theme_catalog)?;
    Ok(GlobalSettings {
        theme,
        theme_catalog,
    })
}

pub fn load_config(path: &Path, home: &Path) -> Result<ProjectList> {
    let raw = read_raw(path)?;
    let version = raw.version.unwrap_or(1);
    if version != 1 {
        bail!("unsupported project list version {version}; expected 1");
    }
    if raw.projects.is_empty() {
        bail!("project list {} contains no projects", path.display());
    }
    let theme_catalog = resolve_custom_themes(raw.themes, path, home)?;
    let theme = resolve_theme(raw.theme, &theme_catalog)?;
    let projects = resolve_projects(raw.projects, path, home)?;
    Ok(ProjectList {
        projects,
        theme,
        theme_catalog,
    })
}

fn read_raw(path: &Path) -> Result<RawProjectList> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("could not read project list {}", path.display()))?;
    let document = toml::from_str::<toml::Table>(&source)
        .with_context(|| format!("invalid project list {}", path.display()))?;
    if let Some(theme) = document.get("theme").and_then(toml::Value::as_table)
        && let Some(key) = theme
            .keys()
            .find(|key| key.as_str() == "file" || !THEME_KEYS.contains(&key.as_str()))
    {
        bail!("theme contains unknown key {key:?}");
    }
    let raw: RawProjectList = toml::from_str(&source)
        .with_context(|| format!("invalid project list {}", path.display()))?;
    Ok(raw)
}

fn resolve_theme(raw: Option<RawTheme>, catalog: &ThemeCatalog) -> Result<Option<GlobalTheme>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let mut issues = Vec::new();
    let (theme, preset, overrides) = build_theme(raw, &mut issues, catalog);
    let errors = issues
        .iter()
        .filter(|issue| issue.severity == Severity::Error)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if !errors.is_empty() {
        bail!("invalid global theme: {}", errors.join("; "));
    }
    Ok(Some(GlobalTheme {
        theme,
        preset,
        overrides,
    }))
}

fn resolve_custom_themes(
    raw_themes: toml::Table,
    config_path: &Path,
    home: &Path,
) -> Result<ThemeCatalog> {
    let base = config_path.parent().unwrap_or_else(|| Path::new("."));
    let mut catalog = ThemeCatalog::default();
    for (raw_name, value) in raw_themes {
        let name = raw_name.trim().to_ascii_lowercase();
        if !valid_theme_name(&name) {
            bail!("invalid custom theme name {raw_name:?}; use letters, numbers, '-' or '_'");
        }
        let (mut table, source_path) = match value {
            toml::Value::String(path) => {
                let path = resolve_path(Path::new(&path), base, home);
                let source = fs::read_to_string(&path).with_context(|| {
                    format!(
                        "could not read custom theme {name:?} from {}",
                        path.display()
                    )
                })?;
                let mut document = toml::from_str::<toml::Table>(&source).with_context(|| {
                    format!("invalid custom theme {name:?} in {}", path.display())
                })?;
                let table = if document.len() == 1
                    && let Some(theme) = document.remove("theme")
                {
                    theme.as_table().cloned().with_context(|| {
                        format!("custom theme {name:?} [theme] value must be a table")
                    })?
                } else {
                    document
                };
                let source_path = fs::canonicalize(&path).unwrap_or(path);
                (table, Some(source_path))
            }
            toml::Value::Table(table) => (table, None),
            _ => bail!("custom theme {name:?} must be an inline table or a theme-file path string"),
        };
        let description = match table.remove("description") {
            Some(toml::Value::String(description)) => {
                let description = description.trim().to_owned();
                (!description.is_empty()).then_some(description)
            }
            Some(_) => bail!("custom theme {name:?} description must be a string"),
            None => None,
        };
        if let Some(key) = table
            .keys()
            .find(|key| key.as_str() == "file" || !THEME_KEYS.contains(&key.as_str()))
        {
            bail!("custom theme {name:?} contains unknown key {key:?}");
        }
        let raw = toml::Value::Table(table)
            .try_into::<RawTheme>()
            .with_context(|| format!("invalid custom theme {name:?}"))?;
        let mut issues = Vec::new();
        let (theme, _, _) = build_theme(raw, &mut issues, &ThemeCatalog::default());
        let errors = issues
            .iter()
            .filter(|issue| issue.severity == Severity::Error)
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        if !errors.is_empty() {
            bail!("invalid custom theme {name:?}: {}", errors.join("; "));
        }
        if !catalog.insert_with_metadata(name.clone(), theme, description, source_path) {
            bail!("custom theme name {name:?} duplicates a built-in or custom theme");
        }
    }
    Ok(catalog)
}

fn resolve_projects(
    raw_projects: Vec<RawProjectEntry>,
    path: &Path,
    home: &Path,
) -> Result<Vec<ProjectEntry>> {
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let mut names = HashSet::new();
    let mut paths = HashSet::new();
    let mut projects = Vec::with_capacity(raw_projects.len());
    for (index, project) in raw_projects.into_iter().enumerate() {
        let location = format!("projects[{index}]");
        let name = project.name.trim().to_owned();
        if name.is_empty() {
            bail!("{location}.name must not be empty");
        }
        if !names.insert(name.clone()) {
            bail!("{location}.name duplicates project {name:?}");
        }

        let mut project_path = resolve_path(&project.path, base, home);
        if project_path.is_dir() {
            project_path.push(DEFAULT_FILE_NAME);
        }
        let project_path = fs::canonicalize(&project_path).with_context(|| {
            format!(
                "{location}.path does not point to a project file: {}",
                project_path.display()
            )
        })?;
        if !project_path.is_file() {
            bail!(
                "{location}.path does not point to a file: {}",
                project_path.display()
            );
        }
        if !paths.insert(project_path.clone()) {
            bail!(
                "{location}.path duplicates another project: {}",
                project_path.display()
            );
        }
        projects.push(ProjectEntry {
            name,
            path: project_path,
        });
    }
    Ok(projects)
}

fn resolve_path(path: &Path, base: &Path, home: &Path) -> PathBuf {
    if path == Path::new("~") {
        return home.to_path_buf();
    }
    if let Ok(relative) = path.strip_prefix("~/") {
        return home.join(relative);
    }
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use ratatui::style::Color;
    use tempfile::tempdir;

    use crate::config::set_theme_preset;

    use super::{candidate_paths, find_file, load, load_config};

    #[test]
    fn prefers_the_home_project_list() {
        let directory = tempdir().unwrap();
        let home = directory.path();
        fs::create_dir(home.join(".config")).unwrap();
        fs::write(home.join(".blade"), "").unwrap();
        fs::write(home.join(".config/blade.config"), "").unwrap();

        assert_eq!(find_file(home).unwrap(), candidate_paths(home)[0]);
    }

    #[test]
    fn supports_blade_conf_as_the_final_global_fallback() {
        let directory = tempdir().unwrap();
        let home = directory.path();
        fs::create_dir(home.join(".config")).unwrap();
        let path = home.join(".config/blade.conf");
        fs::write(&path, "").unwrap();

        assert_eq!(find_file(home).unwrap(), path);
    }

    #[test]
    fn loads_absolute_relative_directory_and_home_paths() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let config_dir = home.join(".config");
        let first = directory.path().join("first");
        let second = config_dir.join("second");
        let third = home.join("third");
        for project in [&first, &second, &third] {
            fs::create_dir_all(project).unwrap();
            fs::write(project.join(".blade"), "name = 'test'").unwrap();
        }
        fs::create_dir_all(&config_dir).unwrap();
        let path = config_dir.join("blade.config");
        fs::write(
            &path,
            format!(
                r#"
version = 1

[[projects]]
name = "First"
path = "{}"

[[projects]]
name = "Second"
path = "second"

[[projects]]
name = "Third"
path = "~/third"
"#,
                first.display()
            ),
        )
        .unwrap();

        let projects = load(&path, &home).unwrap();
        assert_eq!(projects.len(), 3);
        assert_eq!(projects[0].path, first.join(".blade"));
        assert_eq!(projects[1].path, second.join(".blade"));
        assert_eq!(projects[2].path, third.join(".blade"));
    }

    #[test]
    fn rejects_duplicate_names_and_unknown_fields() {
        let directory = tempdir().unwrap();
        let project = directory.path().join("project.blade");
        fs::write(&project, "").unwrap();
        let duplicate = directory.path().join("duplicate.config");
        fs::write(
            &duplicate,
            format!(
                r#"
[[projects]]
name = "Same"
path = "{}"
[[projects]]
name = "Same"
path = "{}"
"#,
                project.display(),
                project.display()
            ),
        )
        .unwrap();
        assert!(load(&duplicate, directory.path()).is_err());

        let unknown = directory.path().join("unknown.config");
        fs::write(
            &unknown,
            format!(
                r#"
extra = true
[[projects]]
name = "Project"
path = "{}"
"#,
                project.display()
            ),
        )
        .unwrap();
        assert!(load(&unknown, directory.path()).is_err());
    }

    #[test]
    fn loads_and_validates_a_global_theme() {
        let directory = tempdir().unwrap();
        let project = directory.path().join("project.blade");
        fs::write(&project, "name = 'test'").unwrap();
        let path = directory.path().join("projects.config");
        fs::write(
            &path,
            format!(
                r##"
version = 1

[theme]
preset = "matrix-alt"
accent = "#010203"

[[projects]]
name = "Project"
path = "{}"
"##,
                project.display()
            ),
        )
        .unwrap();

        let config = load_config(&path, directory.path()).unwrap();
        let theme = config.theme.unwrap();
        assert_eq!(theme.preset, "matrix-alt");
        assert_eq!(theme.theme.text, Color::White);
        assert_eq!(theme.theme.accent, Color::Rgb(1, 2, 3));
        assert_eq!(theme.overrides.accent, Some(Color::Rgb(1, 2, 3)));

        let invalid = fs::read_to_string(&path)
            .unwrap()
            .replace("accent = \"#010203\"", "unknown = \"value\"");
        fs::write(&path, invalid).unwrap();
        assert!(load_config(&path, directory.path()).is_err());
    }

    #[test]
    fn theme_picker_can_insert_a_theme_into_the_global_list() {
        let directory = tempdir().unwrap();
        let project = directory.path().join("project.blade");
        fs::write(&project, "name = 'test'").unwrap();
        let path = directory.path().join("projects.config");
        fs::write(
            &path,
            format!(
                r#"
version = 1

[[projects]]
name = "Project"
path = "{}"
"#,
                project.display()
            ),
        )
        .unwrap();

        set_theme_preset(&path, "sand").unwrap();

        let config = load_config(&path, directory.path()).unwrap();
        assert_eq!(config.theme.unwrap().preset, "sand");
    }

    #[test]
    fn loads_inline_and_file_referenced_custom_themes() {
        let directory = tempdir().unwrap();
        let project = directory.path().join("project.blade");
        fs::write(&project, "name = 'test'").unwrap();
        fs::write(
            directory.path().join("ocean.toml"),
            r#"
[theme]
preset = "blue"
text = "white"
description = "Ocean blues with white output"
"#,
        )
        .unwrap();
        let path = directory.path().join("projects.config");
        fs::write(
            &path,
            format!(
                r#"
version = 1

[themes]
ocean = "ocean.toml"

[themes.terminal-green]
preset = "matrix"
text = "white"
description = "Green controls with white output"

[theme]
preset = "terminal-green"

[[projects]]
name = "Project"
path = "{}"
"#,
                project.display()
            ),
        )
        .unwrap();

        let config = load_config(&path, directory.path()).unwrap();
        assert_eq!(config.theme.unwrap().preset, "terminal-green");
        assert_eq!(
            config.theme_catalog.resolve("terminal-green").unwrap().text,
            Color::White
        );
        assert_eq!(
            config.theme_catalog.resolve("ocean").unwrap().text,
            Color::White
        );
        assert_eq!(
            config.theme_catalog.description("terminal-green"),
            Some("Green controls with white output")
        );
        assert_eq!(
            config.theme_catalog.description("ocean"),
            Some("Ocean blues with white output")
        );
        let ocean_path = fs::canonicalize(directory.path().join("ocean.toml")).unwrap();
        assert_eq!(
            config.theme_catalog.source("ocean"),
            Some(ocean_path.as_path())
        );
        assert!(config.theme_catalog.names().any(|name| name == "ocean"));
    }
}
