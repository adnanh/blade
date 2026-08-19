use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Local;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Wrap,
    },
};
use regex::{Regex, RegexBuilder};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    command_edit::{self, AddOptions},
    config::{
        ActionConfig, CommandConfig, GroupConfig, RestartAfter, action_id, set_project_theme_file,
        set_project_theme_preset, set_theme_preset, validate_file_for_combined_with_catalog,
    },
    log_buffer::{LogKind, LogLine},
    project_list::ProjectEntry,
    runner::{CommandState, Runner},
    theme::{PRESETS, Theme, ThemeOverrides},
};

const TICK_RATE: Duration = Duration::from_millis(75);
const CLIPBOARD_TTL: Duration = Duration::from_secs(2 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Commands,
    Logs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandSelection {
    EphemeralGroup,
    Ephemeral(usize),
    Project {
        first_group: usize,
    },
    Group(usize),
    Command {
        group: usize,
        command: usize,
    },
    Action {
        group: usize,
        command: usize,
        action: usize,
    },
}

#[derive(Debug)]
enum InputMode {
    Normal,
    Ephemeral(String),
    Search { buffer: String, kind: SearchKind },
    Dump(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchKind {
    Exact,
    InsensitiveRegex,
}

struct ThemePicker {
    selected: usize,
    original: Theme,
}

struct CommandPicker {
    query: String,
    selected: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PersistField {
    Name,
    Project,
    Group,
}

struct PersistDialog {
    ephemeral_index: usize,
    name: String,
    target: usize,
    group: String,
    field: PersistField,
    cursor: usize,
}

#[derive(Debug, Clone)]
struct ProjectTarget {
    label: String,
    file: PathBuf,
    root: PathBuf,
    group_names: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManageEditMode {
    Add,
    Edit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManageField {
    Name,
    Run,
    Cwd,
    Project,
    Group,
}

struct ManageEditDialog {
    mode: ManageEditMode,
    original: Option<(usize, usize)>,
    name: String,
    run: String,
    cwd: String,
    target: usize,
    group: String,
    field: ManageField,
    cursor: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManageItem {
    Command {
        group: usize,
        command: usize,
    },
    Action {
        group: usize,
        command: usize,
        action: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManageActionField {
    Name,
    Run,
    Cwd,
    Parent,
    RequiresStopped,
    RestartAfter,
}

struct ManageActionEditDialog {
    mode: ManageEditMode,
    original: Option<(usize, usize, usize)>,
    name: String,
    run: String,
    cwd: String,
    parent: usize,
    requires_stopped: bool,
    restart_after: RestartAfter,
    field: ManageActionField,
    cursor: usize,
}

struct ManageState {
    selected: usize,
    edit: Option<ManageEditDialog>,
    action_edit: Option<ManageActionEditDialog>,
    error: Option<String>,
    confirm_delete: bool,
}

#[derive(Debug, Clone, Copy)]
struct CommandMatch {
    item_index: usize,
    score: i64,
}

struct ClipboardLease {
    expires_at: Instant,
    active: Arc<AtomicBool>,
}

#[derive(Debug, Clone)]
struct EphemeralCommand {
    config: CommandConfig,
    project: Option<String>,
    suggested_group: String,
}

struct App {
    runner: Runner,
    groups: Vec<GroupConfig>,
    project_catalog: Vec<ProjectTarget>,
    ephemeral: Vec<EphemeralCommand>,
    next_ephemeral_id: u64,
    theme: Theme,
    theme_preset: String,
    theme_overrides: ThemeOverrides,
    theme_picker: Option<ThemePicker>,
    command_picker: Option<CommandPicker>,
    persist_dialog: Option<PersistDialog>,
    manage: Option<ManageState>,
    command_items: Vec<CommandSelection>,
    selected_item: usize,
    focus: Focus,
    timestamps: bool,
    wrap_logs: bool,
    follow: bool,
    log_cursor: usize,
    log_scroll: usize,
    wrapped_log_end: usize,
    wrapped_cursor_at_top: bool,
    visible_log_range: Option<(usize, usize)>,
    observed_log_count: usize,
    observed_log_tail: Option<u64>,
    horizontal_scroll: u16,
    search: Option<Regex>,
    search_source: String,
    search_kind: SearchKind,
    mode: InputMode,
    message: String,
    shutting_down: bool,
    forced_quit_at: Option<Instant>,
    show_help: bool,
    log_view_height: usize,
    clipboard: Option<ClipboardLease>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupMode {
    Configured,
    All,
    None,
}

pub fn run(runner: Runner, startup: StartupMode) -> Result<()> {
    match startup {
        StartupMode::Configured => runner.autostart(),
        StartupMode::All => runner.start_all(),
        StartupMode::None => {}
    }
    let mut terminal = ratatui::try_init().context("could not initialize the terminal")?;
    let _restore = RestoreTerminal;
    let mut app = App::new(runner.clone());
    let result = app.event_loop(&mut terminal);
    runner.shutdown(true);
    result
}

pub fn select_projects(
    config_path: &Path,
    projects: &[ProjectEntry],
) -> Result<Option<Vec<ProjectEntry>>> {
    if projects.is_empty() {
        return Err(anyhow!("the global project list contains no projects"));
    }
    let mut terminal = ratatui::try_init().context("could not initialize the terminal")?;
    let _restore = RestoreTerminal;
    let mut selected = 0;
    let all_offset = usize::from(projects.len() > 1);
    let item_count = projects.len() + all_offset;
    loop {
        terminal.draw(|frame| draw_project_list(frame, config_path, projects, selected))?;
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                    return Ok(None);
                }
                match key.code {
                    KeyCode::Enter if all_offset == 1 && selected == 0 => {
                        return Ok(Some(projects.to_vec()));
                    }
                    KeyCode::Enter => {
                        return Ok(Some(vec![projects[selected - all_offset].clone()]));
                    }
                    KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
                    KeyCode::Up | KeyCode::Char('k') => {
                        selected = selected.checked_sub(1).unwrap_or(item_count - 1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        selected = (selected + 1) % item_count;
                    }
                    KeyCode::Home => selected = 0,
                    KeyCode::End => selected = item_count - 1,
                    _ => {}
                }
            }
            Event::Resize(_, _) => {}
            _ => {}
        }
    }
}

fn draw_project_list(
    frame: &mut Frame<'_>,
    config_path: &Path,
    projects: &[ProjectEntry],
    selected: usize,
) {
    let area = frame.area();
    if area.width < 32 || area.height < 7 {
        frame.render_widget(
            Paragraph::new(
                "Blade needs a terminal at least 32×7\nResize the terminal to continue.",
            )
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL).title(" Blade ")),
            area,
        );
        return;
    }

    let theme = Theme::default();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " BLADE ",
                Style::default()
                    .fg(theme.accent_text)
                    .bg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  Choose a project  "),
            Span::styled(
                config_path.display().to_string(),
                Style::default().fg(theme.muted),
            ),
        ]))
        .block(Block::default().borders(Borders::BOTTOM)),
        rows[0],
    );

    let width = rows[1].width.saturating_sub(2) as usize;
    let mut project_rows = Vec::with_capacity(projects.len() + 1);
    if projects.len() > 1 {
        project_rows.push((
            "All projects".to_owned(),
            format!("{} projects in one session", projects.len()),
        ));
    }
    project_rows.extend(
        projects
            .iter()
            .map(|project| (project.name.clone(), project.path.display().to_string())),
    );
    let name_width = project_rows
        .iter()
        .map(|(name, _)| UnicodeWidthStr::width(name.as_str()))
        .max()
        .unwrap_or_default()
        + 2;
    let items = project_rows
        .iter()
        .enumerate()
        .map(|(index, (name, path))| {
            let padding = name_width.saturating_sub(UnicodeWidthStr::width(name.as_str()));
            let text = pad_row(format!(" {name}{}{path}", " ".repeat(padding)), width);
            let style = if index == selected {
                Style::default()
                    .fg(theme.accent_text)
                    .bg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };
            ListItem::new(Line::from(Span::styled(text, style)))
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(" Projects "),
        ),
        rows[1],
        &mut state,
    );

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "↑/↓ j/k",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" select  ", Style::default().fg(theme.footer)),
            Span::styled(
                "Enter",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" open  ", Style::default().fg(theme.footer)),
            Span::styled(
                "q/Esc",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" cancel", Style::default().fg(theme.footer)),
        ]))
        .block(Block::default().borders(Borders::TOP)),
        rows[2],
    );
}

struct RestoreTerminal;

impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        let _ = ratatui::try_restore();
    }
}

fn build_command_items(groups: &[GroupConfig], ephemeral_count: usize) -> Vec<CommandSelection> {
    let mut items = Vec::new();
    if ephemeral_count > 0 {
        items.push(CommandSelection::EphemeralGroup);
        items.extend((0..ephemeral_count).map(CommandSelection::Ephemeral));
    }
    let mut previous_project = None;
    for (group_index, group) in groups.iter().enumerate() {
        if group.project.as_deref() != previous_project {
            if group.project.is_some() {
                items.push(CommandSelection::Project {
                    first_group: group_index,
                });
            }
            previous_project = group.project.as_deref();
        }
        items.push(CommandSelection::Group(group_index));
        for (command, command_config) in group.commands.iter().enumerate() {
            items.push(CommandSelection::Command {
                group: group_index,
                command,
            });
            items.extend(
                command_config
                    .actions
                    .iter()
                    .enumerate()
                    .map(|(action, _)| CommandSelection::Action {
                        group: group_index,
                        command,
                        action,
                    }),
            );
        }
    }
    items
}

fn command_id_for_selection<'a>(
    selection: &CommandSelection,
    groups: &'a [GroupConfig],
    ephemeral: &'a [EphemeralCommand],
) -> Option<&'a str> {
    match *selection {
        CommandSelection::Ephemeral(index) => ephemeral
            .get(index)
            .map(|command| command.config.id.as_str()),
        CommandSelection::Command { group, command } => groups
            .get(group)?
            .commands
            .get(command)
            .map(|command| command.id.as_str()),
        CommandSelection::Action {
            group,
            command,
            action,
        } => groups
            .get(group)?
            .commands
            .get(command)?
            .actions
            .get(action)
            .map(|action| action.id.as_str()),
        _ => None,
    }
}

impl App {
    fn new(runner: Runner) -> Self {
        let groups = runner.project().groups.clone();
        let mut project_catalog = Vec::<ProjectTarget>::new();
        for group in &groups {
            let label = group
                .project
                .clone()
                .unwrap_or_else(|| runner.project().name.clone());
            if !project_catalog
                .iter()
                .any(|target| target.file == group.project_file)
            {
                project_catalog.push(ProjectTarget {
                    label,
                    file: group.project_file.clone(),
                    root: group.project_root.clone(),
                    group_names: Vec::new(),
                });
            }
        }
        if project_catalog.is_empty() {
            project_catalog.push(ProjectTarget {
                label: runner.project().name.clone(),
                file: runner.project().path.clone(),
                root: runner.project().root.clone(),
                group_names: Vec::new(),
            });
        }
        let command_items = build_command_items(&groups, 0);
        let selected_item = command_items
            .iter()
            .position(|item| matches!(item, CommandSelection::Command { .. }))
            .unwrap_or(0);
        let theme = runner.project().theme.clone();
        let mut theme_preset = runner.project().theme_preset.clone();
        let mut theme_overrides = runner.project().theme_overrides.clone();
        if theme_overrides.is_complete()
            && let Some(custom_name) = runner.project().theme_catalog.custom_name_for_theme(&theme)
        {
            // Older Blade versions materialized custom themes as eleven project-level
            // overrides. Treat that exact generated palette as the named custom theme so
            // it does not mask every picker preview.
            theme_preset = custom_name.to_owned();
            theme_overrides = ThemeOverrides::default();
        }
        Self {
            theme,
            theme_preset,
            theme_overrides,
            theme_picker: None,
            command_picker: None,
            persist_dialog: None,
            manage: None,
            groups,
            project_catalog,
            ephemeral: Vec::new(),
            next_ephemeral_id: 1,
            runner,
            command_items,
            selected_item,
            focus: Focus::Commands,
            timestamps: true,
            wrap_logs: true,
            follow: true,
            log_cursor: 0,
            log_scroll: 0,
            wrapped_log_end: 0,
            wrapped_cursor_at_top: false,
            visible_log_range: None,
            observed_log_count: 0,
            observed_log_tail: None,
            horizontal_scroll: 0,
            search: None,
            search_source: String::new(),
            search_kind: SearchKind::Exact,
            mode: InputMode::Normal,
            message: "Ctrl-P opens Quick jump • : runs an ephemeral command • ? shows all keys"
                .to_owned(),
            shutting_down: false,
            forced_quit_at: None,
            show_help: false,
            log_view_height: 1,
            clipboard: None,
        }
    }

    fn rebuild_command_items(&mut self, selected_id: Option<&str>) {
        self.command_items = build_command_items(&self.groups, self.ephemeral.len());
        if let Some(selected_id) = selected_id
            && let Some(index) = self.command_items.iter().position(|selection| {
                command_id_for_selection(selection, &self.groups, &self.ephemeral)
                    == Some(selected_id)
            })
        {
            self.selected_item = index;
            return;
        }
        self.selected_item = self
            .selected_item
            .min(self.command_items.len().saturating_sub(1));
    }

    fn event_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        loop {
            self.refresh_clipboard();
            if self.shutting_down && self.runner.active_count() == 0 {
                return Ok(());
            }
            if self
                .forced_quit_at
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                return Ok(());
            }
            terminal.draw(|frame| self.draw(frame))?;
            if !event::poll(TICK_RATE)? {
                continue;
            }
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if self.handle_key(key)? {
                        return Ok(());
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if !matches!(self.mode, InputMode::Normal) {
            return self.handle_input_key(key);
        }
        if self.command_picker.is_some() {
            self.handle_command_picker_key(key);
            return Ok(false);
        }
        if self.theme_picker.is_some() {
            self.handle_theme_picker_key(key);
            return Ok(false);
        }
        if self.manage.is_some() {
            self.handle_manage_key(key);
            return Ok(false);
        }
        if self.persist_dialog.is_some() {
            self.handle_persist_dialog_key(key);
            return Ok(false);
        }
        if self.show_help {
            self.show_help = false;
            return Ok(false);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return self.request_quit();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('p') {
            self.open_command_picker();
            return Ok(false);
        }
        match key.code {
            KeyCode::Char('q') => return self.request_quit(),
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char('T') => self.open_theme_picker(),
            KeyCode::Char(':') => self.mode = InputMode::Ephemeral(String::new()),
            KeyCode::Char('p') => self.open_persist_dialog(),
            KeyCode::Char('M') => self.open_manage(),
            KeyCode::Char('D') => self.forget_ephemeral(),
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Commands => Focus::Logs,
                    Focus::Logs => Focus::Commands,
                }
            }
            KeyCode::Right if self.focus == Focus::Commands => self.focus = Focus::Logs,
            KeyCode::Left if self.focus == Focus::Logs && self.wrap_logs => {
                self.focus = Focus::Commands
            }
            KeyCode::Left if self.focus == Focus::Logs => {
                if self.horizontal_scroll == 0 {
                    self.focus = Focus::Commands;
                } else {
                    self.horizontal_scroll = self.horizontal_scroll.saturating_sub(4);
                }
            }
            KeyCode::Right if self.focus == Focus::Logs && !self.wrap_logs => {
                self.horizontal_scroll = self.horizontal_scroll.saturating_add(4)
            }
            KeyCode::Up | KeyCode::Char('k') if self.focus == Focus::Commands => {
                self.select_previous_command()
            }
            KeyCode::Down | KeyCode::Char('j') if self.focus == Focus::Commands => {
                self.select_next_command()
            }
            KeyCode::Up | KeyCode::Char('k') if self.focus == Focus::Logs => {
                self.move_log_cursor(-1)
            }
            KeyCode::Down | KeyCode::Char('j') if self.focus == Focus::Logs => {
                self.move_log_cursor(1)
            }
            KeyCode::PageUp => self.move_log_cursor(-(self.log_view_height as isize)),
            KeyCode::PageDown => self.move_log_cursor(self.log_view_height as isize),
            KeyCode::Home | KeyCode::Char('g') if self.focus == Focus::Logs => {
                self.follow = false;
                self.log_cursor = 0;
                self.wrapped_cursor_at_top = true;
            }
            KeyCode::End | KeyCode::Char('G') if self.focus == Focus::Logs => {
                self.follow = true;
                self.wrapped_cursor_at_top = false;
            }
            KeyCode::Char('h') if self.focus == Focus::Logs && !self.wrap_logs => {
                self.horizontal_scroll = self.horizontal_scroll.saturating_sub(4)
            }
            KeyCode::Char('l') if self.focus == Focus::Logs && !self.wrap_logs => {
                self.horizontal_scroll = self.horizontal_scroll.saturating_add(4)
            }
            KeyCode::Enter | KeyCode::Char('s') => self.start_selected(),
            KeyCode::Char('f') => self.force_start_selected(),
            KeyCode::Char('x') => self.stop_selected(),
            KeyCode::Char('r') => self.restart_selected(),
            KeyCode::Char('t') => {
                self.timestamps = !self.timestamps;
                self.message = format!(
                    "timestamps {}",
                    if self.timestamps {
                        "enabled"
                    } else {
                        "disabled"
                    }
                );
            }
            KeyCode::Char('w') => {
                self.wrap_logs = !self.wrap_logs;
                self.horizontal_scroll = 0;
                self.wrapped_cursor_at_top = false;
                self.message = format!(
                    "line wrapping {}",
                    if self.wrap_logs {
                        "enabled"
                    } else {
                        "disabled"
                    }
                );
            }
            KeyCode::Char('/') => {
                self.mode = InputMode::Search {
                    buffer: String::new(),
                    kind: SearchKind::Exact,
                }
            }
            KeyCode::Char('\\') => {
                self.mode = InputMode::Search {
                    buffer: String::new(),
                    kind: SearchKind::InsensitiveRegex,
                }
            }
            KeyCode::Char('n') => self.next_search_match(false),
            KeyCode::Char('N') => self.next_search_match(true),
            KeyCode::Char('y') => self.copy_current_line(),
            KeyCode::Char('Y') => self.copy_all_logs(),
            KeyCode::Char('c') => self.clear_selected_logs(),
            KeyCode::Char('d') => {
                let path = self.default_dump_path();
                self.mode = InputMode::Dump(path.display().to_string());
            }
            _ => {}
        }
        Ok(false)
    }

    fn handle_input_key(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Esc => {
                self.mode = InputMode::Normal;
                self.message = "input cancelled".to_owned();
            }
            KeyCode::Enter => {
                let mode = std::mem::replace(&mut self.mode, InputMode::Normal);
                match mode {
                    InputMode::Ephemeral(command) => self.run_ephemeral(command),
                    InputMode::Search { buffer, kind } => self.apply_search(buffer, kind),
                    InputMode::Dump(path) => self.dump_logs(PathBuf::from(path)),
                    InputMode::Normal => {}
                }
            }
            KeyCode::Backspace => match &mut self.mode {
                InputMode::Ephemeral(buffer)
                | InputMode::Search { buffer, .. }
                | InputMode::Dump(buffer) => {
                    buffer.pop();
                }
                InputMode::Normal => {}
            },
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                match &mut self.mode {
                    InputMode::Ephemeral(buffer)
                    | InputMode::Search { buffer, .. }
                    | InputMode::Dump(buffer) => buffer.push(character),
                    InputMode::Normal => {}
                }
            }
            _ => {}
        }
        Ok(false)
    }

    fn request_quit(&mut self) -> Result<bool> {
        let active = self.runner.active_count();
        if active == 0 {
            return Ok(true);
        }
        if self.shutting_down {
            self.runner.shutdown(true);
            self.message = "force-killing all command process groups…".to_owned();
            self.forced_quit_at = Some(Instant::now() + Duration::from_millis(400));
        } else {
            self.shutting_down = true;
            self.runner.shutdown(false);
            self.message =
                format!("gracefully stopping {active} command(s); press q again to force quit");
        }
        Ok(false)
    }

    fn selected_command_name(&self) -> Option<&str> {
        command_id_for_selection(
            self.command_items.get(self.selected_item)?,
            &self.groups,
            &self.ephemeral,
        )
    }

    fn selected_command(&self) -> Option<&CommandConfig> {
        match *self.command_items.get(self.selected_item)? {
            CommandSelection::Ephemeral(index) => {
                self.ephemeral.get(index).map(|command| &command.config)
            }
            CommandSelection::Command { group, command }
            | CommandSelection::Action { group, command, .. } => {
                self.groups.get(group)?.commands.get(command)
            }
            _ => None,
        }
    }

    fn selected_action(&self) -> Option<&ActionConfig> {
        let CommandSelection::Action {
            group,
            command,
            action,
        } = *self.command_items.get(self.selected_item)?
        else {
            return None;
        };
        self.groups
            .get(group)?
            .commands
            .get(command)?
            .actions
            .get(action)
    }

    fn selected_runnable_label(&self) -> Option<&str> {
        self.selected_action()
            .map(|action| action.name.as_str())
            .or_else(|| self.selected_command().map(|command| command.name.as_str()))
    }

    fn selected_group(&self) -> Option<(usize, &str)> {
        let CommandSelection::Group(group) = *self.command_items.get(self.selected_item)? else {
            return None;
        };
        self.groups
            .get(group)
            .map(|config| (group, config.name.as_str()))
    }

    fn selected_project(&self) -> Option<&str> {
        let CommandSelection::Project { first_group } =
            *self.command_items.get(self.selected_item)?
        else {
            return None;
        };
        self.groups.get(first_group)?.project.as_deref()
    }

    fn selected_targets(&self) -> Option<(String, Vec<String>)> {
        if let Some(action) = self.selected_action() {
            let parent = self.selected_command()?;
            return Some((
                format!("action {:?} for {:?}", action.name, parent.name),
                vec![action.id.clone()],
            ));
        }
        if let Some(command) = self.selected_command() {
            return Some((command.name.clone(), vec![command.id.clone()]));
        }
        if matches!(
            self.command_items.get(self.selected_item),
            Some(CommandSelection::EphemeralGroup)
        ) {
            return Some((
                "group Ephemeral".to_owned(),
                self.ephemeral
                    .iter()
                    .map(|command| command.config.id.clone())
                    .collect(),
            ));
        }
        if let Some(project) = self.selected_project() {
            let names = self
                .groups
                .iter()
                .filter(|group| group.project.as_deref() == Some(project))
                .flat_map(|group| group.commands.iter())
                .map(|command| command.id.clone())
                .collect();
            return Some((format!("project {project}"), names));
        }
        let (group_index, group_name) = self.selected_group()?;
        let names = self.groups[group_index]
            .commands
            .iter()
            .map(|command| command.id.clone())
            .collect();
        Some((format!("group {group_name}"), names))
    }

    fn select_previous_command(&mut self) {
        if self.selected_item > 0 {
            self.selected_item -= 1;
            self.reset_log_navigation();
        }
    }

    fn select_next_command(&mut self) {
        if self.selected_item + 1 < self.command_items.len() {
            self.selected_item += 1;
            self.reset_log_navigation();
        }
    }

    fn reset_log_navigation(&mut self) {
        let search_was_active = self.search.take().is_some();
        self.search_source.clear();
        self.follow = true;
        self.log_cursor = 0;
        self.log_scroll = 0;
        self.wrapped_log_end = 0;
        self.wrapped_cursor_at_top = false;
        self.visible_log_range = None;
        self.observed_log_count = 0;
        self.observed_log_tail = None;
        self.horizontal_scroll = 0;
        if search_was_active {
            self.message = "search cleared after changing selection".to_owned();
        }
    }

    fn open_theme_picker(&mut self) {
        let selected = self
            .runner
            .project()
            .theme_catalog
            .names()
            .position(|preset| preset == self.theme_preset)
            .unwrap_or_default();
        self.theme_picker = Some(ThemePicker {
            selected,
            original: self.theme.clone(),
        });
        self.preview_theme(selected);
    }

    fn open_command_picker(&mut self) {
        self.command_picker = Some(CommandPicker {
            query: String::new(),
            selected: 0,
        });
    }

    fn handle_command_picker_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc
            || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('p'))
        {
            self.command_picker = None;
            self.message = "Quick jump closed".to_owned();
            return;
        }

        let Some((query, selected)) = self
            .command_picker
            .as_ref()
            .map(|picker| (picker.query.clone(), picker.selected))
        else {
            return;
        };
        let matches = self.command_matches(&query);
        match key.code {
            KeyCode::Enter => {
                let Some(hit) = matches.get(selected).copied() else {
                    self.message = "no matching command to focus".to_owned();
                    return;
                };
                let label = self
                    .command_picker_label(hit.item_index)
                    .unwrap_or_else(|| "command".to_owned());
                self.selected_item = hit.item_index;
                self.command_picker = None;
                self.focus = Focus::Commands;
                self.reset_log_navigation();
                self.message = format!("focused {label}");
            }
            KeyCode::Up => {
                if !matches.is_empty() {
                    let next = selected
                        .checked_sub(1)
                        .unwrap_or(matches.len().saturating_sub(1));
                    if let Some(picker) = self.command_picker.as_mut() {
                        picker.selected = next;
                    }
                }
            }
            KeyCode::Down => {
                if !matches.is_empty()
                    && let Some(picker) = self.command_picker.as_mut()
                {
                    picker.selected = (selected + 1) % matches.len();
                }
            }
            KeyCode::Home => {
                if let Some(picker) = self.command_picker.as_mut() {
                    picker.selected = 0;
                }
            }
            KeyCode::End => {
                if let Some(picker) = self.command_picker.as_mut() {
                    picker.selected = matches.len().saturating_sub(1);
                }
            }
            KeyCode::Backspace => {
                if let Some(picker) = self.command_picker.as_mut() {
                    picker.query.pop();
                    picker.selected = 0;
                }
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(picker) = self.command_picker.as_mut() {
                    picker.query.push(character);
                    picker.selected = 0;
                }
            }
            _ => {}
        }
    }

    fn command_matches(&self, query: &str) -> Vec<CommandMatch> {
        let mut matches = self
            .command_items
            .iter()
            .enumerate()
            .filter_map(|(item_index, selection)| {
                let (command_name, qualified) = match *selection {
                    CommandSelection::Ephemeral(index) => {
                        let command = &self.ephemeral.get(index)?.config;
                        (command.name.as_str(), format!("Ephemeral {}", command.name))
                    }
                    CommandSelection::Command { group, command } => {
                        let group_config = &self.groups[group];
                        let command_config = &group_config.commands[command];
                        (
                            command_config.name.as_str(),
                            format!(
                                "{} {} {}",
                                group_config.project.as_deref().unwrap_or_default(),
                                group_config.name,
                                command_config.name
                            ),
                        )
                    }
                    CommandSelection::Action {
                        group,
                        command,
                        action,
                    } => {
                        let group_config = &self.groups[group];
                        let command_config = &group_config.commands[command];
                        let action_config = &command_config.actions[action];
                        (
                            action_config.name.as_str(),
                            format!(
                                "{} {} {} {}",
                                group_config.project.as_deref().unwrap_or_default(),
                                group_config.name,
                                command_config.name,
                                action_config.name
                            ),
                        )
                    }
                    _ => return None,
                };
                let score = if query.is_empty() {
                    0
                } else {
                    let command_score =
                        fuzzy_score(command_name, query).map(|score| score.saturating_add(1_000));
                    command_score
                        .into_iter()
                        .chain(fuzzy_score(&qualified, query))
                        .max()?
                };
                Some(CommandMatch { item_index, score })
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| left.item_index.cmp(&right.item_index))
        });
        matches
    }

    fn command_picker_label(&self, item_index: usize) -> Option<String> {
        match *self.command_items.get(item_index)? {
            CommandSelection::Ephemeral(index) => Some(format!(
                "Ephemeral / {}",
                self.ephemeral.get(index)?.config.name
            )),
            CommandSelection::Command { group, command } => {
                let group = self.groups.get(group)?;
                let command = group.commands.get(command)?;
                Some(format!(
                    "{}{} / {}",
                    group
                        .project
                        .as_deref()
                        .map(|project| format!("{project} / "))
                        .unwrap_or_default(),
                    group.name,
                    command.name
                ))
            }
            CommandSelection::Action {
                group,
                command,
                action,
            } => {
                let group = self.groups.get(group)?;
                let command = group.commands.get(command)?;
                let action = command.actions.get(action)?;
                Some(format!(
                    "{}{} / {} › {}",
                    group
                        .project
                        .as_deref()
                        .map(|project| format!("{project} / "))
                        .unwrap_or_default(),
                    group.name,
                    command.name,
                    action.name
                ))
            }
            _ => None,
        }
    }

    fn handle_theme_picker_key(&mut self, key: KeyEvent) {
        let Some(selected) = self.theme_picker.as_ref().map(|picker| picker.selected) else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                if let Some(picker) = self.theme_picker.take() {
                    self.theme = picker.original;
                    self.message = "theme selection cancelled".to_owned();
                }
            }
            KeyCode::Enter => {
                let Some(preset) = self
                    .runner
                    .project()
                    .theme_catalog
                    .name(selected)
                    .map(str::to_owned)
                else {
                    return;
                };
                let Some(theme_file) = self.runner.project().theme_file.as_deref() else {
                    self.theme_preset = preset.clone();
                    self.theme_picker = None;
                    self.message =
                        format!("applied {preset} theme for this combined session (not saved)");
                    return;
                };
                let is_global = self.runner.project().theme_file_is_global;
                let custom_source = self
                    .runner
                    .project()
                    .theme_catalog
                    .source(&preset)
                    .map(Path::to_path_buf);
                let result = if is_global {
                    set_theme_preset(theme_file, &preset)
                } else if let Some(source) = custom_source {
                    set_project_theme_file(theme_file, &source)
                } else {
                    set_project_theme_preset(theme_file, &preset)
                };
                match result {
                    Ok(()) => {
                        self.theme_preset = preset.clone();
                        if !is_global {
                            self.theme_overrides = ThemeOverrides::default();
                        }
                        self.theme_picker = None;
                        self.message =
                            format!("applied {preset} theme to {}", theme_file.display());
                    }
                    Err(error) => self.message = format!("could not apply theme: {error:#}"),
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let count = self.runner.project().theme_catalog.len();
                let next = selected.checked_sub(1).unwrap_or(count.saturating_sub(1));
                if let Some(picker) = self.theme_picker.as_mut() {
                    picker.selected = next;
                }
                self.preview_theme(next);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let count = self.runner.project().theme_catalog.len();
                let next = (selected + 1) % count;
                if let Some(picker) = self.theme_picker.as_mut() {
                    picker.selected = next;
                }
                self.preview_theme(next);
            }
            _ => {}
        }
    }

    fn preview_theme(&mut self, selected: usize) {
        let Some(name) = self.runner.project().theme_catalog.name(selected) else {
            return;
        };
        if let Some(mut theme) = self.runner.project().theme_catalog.resolve(name) {
            self.theme_overrides.apply(&mut theme);
            self.theme = theme;
        }
    }

    fn move_log_cursor(&mut self, amount: isize) {
        let count = self
            .selected_command_name()
            .map(|name| self.runner.logs(name).len())
            .unwrap_or_default();
        if count == 0 {
            return;
        }
        self.wrapped_cursor_at_top = false;
        self.follow = false;
        self.log_cursor = self
            .log_cursor
            .saturating_add_signed(amount)
            .min(count.saturating_sub(1));
        if self.log_cursor + 1 == count {
            self.follow = true;
        }
    }

    fn start_selected(&mut self) {
        let Some((label, names)) = self.selected_targets() else {
            return;
        };
        self.message =
            self.apply_to_targets("start", &label, names, |name| self.runner.start(name));
    }

    fn stop_selected(&mut self) {
        let Some((label, mut names)) = self.selected_targets() else {
            return;
        };
        names.reverse();
        self.message = self.apply_to_targets("stop", &label, names, |name| self.runner.stop(name));
    }

    fn force_start_selected(&mut self) {
        let Some((label, names)) = self.selected_targets() else {
            return;
        };
        let waiting: Vec<_> = names
            .into_iter()
            .filter(|name| {
                self.runner
                    .snapshot(name)
                    .is_some_and(|snapshot| snapshot.state == CommandState::Waiting)
            })
            .collect();
        if waiting.is_empty() {
            self.message = format!("nothing in {label} is waiting for dependencies");
            return;
        }
        self.message = self.apply_to_targets("force start", &label, waiting, |name| {
            self.runner.force_start(name)
        });
    }

    fn restart_selected(&mut self) {
        let Some((label, names)) = self.selected_targets() else {
            return;
        };
        self.message =
            self.apply_to_targets("restart", &label, names, |name| self.runner.restart(name));
    }

    fn apply_to_targets<F>(
        &self,
        action: &str,
        label: &str,
        names: Vec<String>,
        mut apply: F,
    ) -> String
    where
        F: FnMut(&str) -> Result<()>,
    {
        let count = names.len();
        let errors: Vec<_> = names
            .iter()
            .filter_map(|name| apply(name).err().map(|error| format!("{name}: {error}")))
            .collect();
        if errors.is_empty() {
            format!("{action} requested for {label} ({count} command(s))")
        } else {
            format!("{action} for {label} had errors: {}", errors.join("; "))
        }
    }

    fn run_ephemeral(&mut self, command: String) {
        let command = command.trim();
        if command.is_empty() {
            self.message = "ephemeral command cancelled: command was empty".to_owned();
            return;
        }
        let Some((base, project, suggested_group)) = self.ephemeral_context() else {
            self.message =
                "cannot determine a project context for the ephemeral command".to_owned();
            return;
        };
        let id = format!("__ephemeral::{}", self.next_ephemeral_id);
        self.next_ephemeral_id += 1;
        let config = CommandConfig {
            id: id.clone(),
            name: ephemeral_label(command),
            shell: base.shell,
            project_root: base.project_root,
            project_file: base.project_file,
            max_log_lines: base.max_log_lines,
            run: command.to_owned(),
            cwd: base.cwd,
            shell_setup: base.shell_setup,
            pre: Vec::new(),
            wait_for: Vec::new(),
            autostart: false,
            log_dir: None,
            log_file: None,
            log_rotate_bytes: None,
            log_rotate_keep: base.log_rotate_keep,
            stop_timeout: base.stop_timeout,
            actions: Vec::new(),
        };
        if let Err(error) = self.runner.add_command(config.clone()) {
            self.message = format!("could not create ephemeral command: {error:#}");
            return;
        }
        self.ephemeral.push(EphemeralCommand {
            config,
            project,
            suggested_group,
        });
        self.rebuild_command_items(Some(&id));
        self.reset_log_navigation();
        self.focus = Focus::Commands;
        match self.runner.start(&id) {
            Ok(()) => {
                self.message = "ephemeral command started; use normal lifecycle keys".to_owned()
            }
            Err(error) => {
                self.message = format!("ephemeral command created but not started: {error:#}")
            }
        }
    }

    fn project_targets(&self) -> Vec<ProjectTarget> {
        let mut targets = self.project_catalog.clone();
        for target in &mut targets {
            target.group_names.clear();
        }
        for group in &self.groups {
            let label = group
                .project
                .clone()
                .unwrap_or_else(|| self.runner.project().name.clone());
            if let Some(target) = targets
                .iter_mut()
                .find(|target| target.file == group.project_file)
            {
                if !target.group_names.contains(&group.name) {
                    target.group_names.push(group.name.clone());
                }
            } else {
                debug_assert!(false, "all live groups must belong to a known project");
                targets.push(ProjectTarget {
                    label,
                    file: group.project_file.clone(),
                    root: group.project_root.clone(),
                    group_names: vec![group.name.clone()],
                });
            }
        }
        targets
    }

    fn open_persist_dialog(&mut self) {
        let Some(CommandSelection::Ephemeral(ephemeral_index)) =
            self.command_items.get(self.selected_item).copied()
        else {
            self.message = "select an ephemeral command to add it to a project".to_owned();
            return;
        };
        let targets = self.project_targets();
        if targets.is_empty() {
            self.message = "no project is available for persistence".to_owned();
            return;
        }
        let ephemeral = &self.ephemeral[ephemeral_index];
        let target = targets
            .iter()
            .position(|target| {
                target.file == ephemeral.config.project_file
                    || ephemeral.project.as_deref() == Some(target.label.as_str())
            })
            .unwrap_or_default();
        let group = if targets[target]
            .group_names
            .contains(&ephemeral.suggested_group)
        {
            ephemeral.suggested_group.clone()
        } else {
            targets[target]
                .group_names
                .first()
                .cloned()
                .unwrap_or_else(|| "Project".to_owned())
        };
        let existing_names = self
            .groups
            .iter()
            .filter(|group| group.project_file == targets[target].file)
            .flat_map(|group| group.commands.iter().map(|command| command.name.as_str()))
            .collect::<Vec<_>>();
        let name = unique_persisted_name(&ephemeral.config.run, &existing_names);
        let cursor = name.chars().count();
        self.persist_dialog = Some(PersistDialog {
            ephemeral_index,
            name,
            target,
            group,
            field: PersistField::Name,
            cursor,
        });
    }

    fn forget_ephemeral(&mut self) {
        let Some(CommandSelection::Ephemeral(index)) =
            self.command_items.get(self.selected_item).copied()
        else {
            self.message = "select an ephemeral command to remove it".to_owned();
            return;
        };
        let id = self.ephemeral[index].config.id.clone();
        match self.runner.remove_command(&id) {
            Ok(()) => {
                let name = self.ephemeral.remove(index).config.name;
                self.rebuild_command_items(None);
                self.reset_log_navigation();
                self.message = format!("removed ephemeral command {name:?}");
            }
            Err(error) => self.message = format!("could not remove ephemeral command: {error:#}"),
        }
    }

    fn handle_persist_dialog_key(&mut self, key: KeyEvent) {
        let Some(mut dialog) = self.persist_dialog.take() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.message = "persistence cancelled".to_owned();
                return;
            }
            KeyCode::Enter => {
                self.persist_dialog = Some(dialog);
                self.persist_ephemeral();
                return;
            }
            KeyCode::Tab | KeyCode::Down => {
                dialog.field = match dialog.field {
                    PersistField::Name => PersistField::Project,
                    PersistField::Project => PersistField::Group,
                    PersistField::Group => PersistField::Name,
                };
                dialog.cursor = persist_field_text_len(&dialog);
            }
            KeyCode::BackTab | KeyCode::Up => {
                dialog.field = match dialog.field {
                    PersistField::Name => PersistField::Group,
                    PersistField::Project => PersistField::Name,
                    PersistField::Group => PersistField::Project,
                };
                dialog.cursor = persist_field_text_len(&dialog);
            }
            KeyCode::Left | KeyCode::Right if dialog.field == PersistField::Project => {
                let targets = self.project_targets();
                if !targets.is_empty() {
                    let previous_target = dialog.target;
                    dialog.target = if key.code == KeyCode::Left {
                        dialog
                            .target
                            .checked_sub(1)
                            .unwrap_or(targets.len().saturating_sub(1))
                    } else {
                        (dialog.target + 1) % targets.len()
                    };
                    if dialog.target != previous_target {
                        dialog.group = targets[dialog.target]
                            .group_names
                            .first()
                            .cloned()
                            .unwrap_or_else(|| "Project".to_owned());
                    }
                }
            }
            KeyCode::Left | KeyCode::Right
                if dialog.field == PersistField::Group
                    && key.modifiers.contains(KeyModifiers::ALT) =>
            {
                if let Some(target) = self.project_targets().get(dialog.target) {
                    cycle_named_value(
                        &mut dialog.group,
                        &target.group_names,
                        key.code == KeyCode::Left,
                    );
                    dialog.cursor = dialog.group.chars().count();
                }
            }
            KeyCode::Left | KeyCode::Right
                if matches!(dialog.field, PersistField::Name | PersistField::Group)
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let value = match dialog.field {
                    PersistField::Name => &dialog.name,
                    PersistField::Group => &dialog.group,
                    PersistField::Project => unreachable!(),
                };
                move_text_cursor(&mut dialog.cursor, value, key.code == KeyCode::Right);
            }
            KeyCode::Home if matches!(dialog.field, PersistField::Name | PersistField::Group) => {
                dialog.cursor = 0;
            }
            KeyCode::End if matches!(dialog.field, PersistField::Name | PersistField::Group) => {
                dialog.cursor = persist_field_text_len(&dialog);
            }
            KeyCode::Backspace => match dialog.field {
                PersistField::Name => remove_before_cursor(&mut dialog.name, &mut dialog.cursor),
                PersistField::Group => remove_before_cursor(&mut dialog.group, &mut dialog.cursor),
                PersistField::Project => {}
            },
            KeyCode::Delete => match dialog.field {
                PersistField::Name => remove_at_cursor(&mut dialog.name, dialog.cursor),
                PersistField::Group => remove_at_cursor(&mut dialog.group, dialog.cursor),
                PersistField::Project => {}
            },
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                match dialog.field {
                    PersistField::Name => {
                        insert_at_cursor(&mut dialog.name, &mut dialog.cursor, character)
                    }
                    PersistField::Group => {
                        insert_at_cursor(&mut dialog.group, &mut dialog.cursor, character)
                    }
                    PersistField::Project => {}
                }
            }
            _ => {}
        }
        self.persist_dialog = Some(dialog);
    }

    fn persist_ephemeral(&mut self) {
        let Some(dialog) = self.persist_dialog.take() else {
            return;
        };
        let targets = self.project_targets();
        let Some(target) = targets.get(dialog.target).cloned() else {
            self.message = "selected persistence target is no longer available".to_owned();
            return;
        };
        let name = dialog.name.trim();
        let group_name = dialog.group.trim();
        if name.is_empty() || group_name.is_empty() {
            self.message = "command name and group must not be empty".to_owned();
            self.persist_dialog = Some(dialog);
            return;
        }
        let Some(ephemeral) = self.ephemeral.get(dialog.ephemeral_index).cloned() else {
            self.message = "ephemeral command no longer exists".to_owned();
            return;
        };
        let cwd = ephemeral
            .config
            .cwd
            .strip_prefix(&target.root)
            .ok()
            .map(|path| {
                if path.as_os_str().is_empty() {
                    ".".to_owned()
                } else {
                    path.display().to_string()
                }
            })
            .unwrap_or_else(|| ephemeral.config.cwd.display().to_string());
        let options = AddOptions {
            group: Some(group_name.to_owned()),
            name: Some(name.to_owned()),
            run: Some(ephemeral.config.run.clone()),
            cwd: Some(cwd),
            pre: ephemeral.config.pre.clone(),
            autostart: false,
        };
        if let Err(error) = command_edit::add_with_catalog_quiet(
            &target.file,
            options,
            &self.runner.project().theme_catalog,
        ) {
            self.message = format!("could not persist command: {error:#}");
            self.persist_dialog = Some(dialog);
            return;
        }

        let runtime_id = ephemeral.config.id.clone();
        let mut config =
            match load_configured_command(&target.file, name, &self.runner.project().theme_catalog)
            {
                Ok(config) => config,
                Err(error) => {
                    self.message = format!(
                        "saved command, but could not load it into this session: {error:#}"
                    );
                    return;
                }
            };
        config.id = runtime_id.clone();
        if let Some(group) = self
            .groups
            .iter_mut()
            .find(|group| group.project_file == target.file && group.name == group_name)
        {
            group.commands.push(config.clone());
        } else {
            let combined = self.groups.iter().any(|group| group.project.is_some());
            self.groups.push(GroupConfig {
                project: combined.then_some(target.label.clone()),
                name: group_name.to_owned(),
                project_file: target.file.clone(),
                project_root: target.root,
                commands: vec![config.clone()],
            });
        }
        self.ephemeral.remove(dialog.ephemeral_index);
        if let Err(error) = self.runner.update_command(config) {
            self.message = format!("saved command, but could not update this session: {error:#}");
        } else {
            self.message = format!("saved {name:?} to {} / {group_name}", target.label);
        }
        self.rebuild_command_items(Some(&runtime_id));
        self.reset_log_navigation();
    }

    fn managed_commands(&self) -> Vec<(usize, usize)> {
        self.groups
            .iter()
            .enumerate()
            .flat_map(|(group, config)| {
                (0..config.commands.len()).map(move |command| (group, command))
            })
            .collect()
    }

    fn managed_items(&self) -> Vec<ManageItem> {
        let mut items = Vec::new();
        for (group, group_config) in self.groups.iter().enumerate() {
            for (command, command_config) in group_config.commands.iter().enumerate() {
                items.push(ManageItem::Command { group, command });
                items.extend(
                    command_config
                        .actions
                        .iter()
                        .enumerate()
                        .map(|(action, _)| ManageItem::Action {
                            group,
                            command,
                            action,
                        }),
                );
            }
        }
        items
    }

    fn open_manage(&mut self) {
        let items = self.managed_items();
        let selected = match self.command_items.get(self.selected_item).copied() {
            Some(CommandSelection::Command { group, command }) => items
                .iter()
                .position(|candidate| *candidate == ManageItem::Command { group, command })
                .unwrap_or_default(),
            Some(CommandSelection::Action {
                group,
                command,
                action,
            }) => items
                .iter()
                .position(|candidate| {
                    *candidate
                        == ManageItem::Action {
                            group,
                            command,
                            action,
                        }
                })
                .unwrap_or_default(),
            _ => 0,
        };
        self.manage = Some(ManageState {
            selected,
            edit: None,
            action_edit: None,
            error: None,
            confirm_delete: false,
        });
    }

    fn handle_manage_key(&mut self, key: KeyEvent) {
        if self
            .manage
            .as_ref()
            .is_some_and(|manage| manage.error.is_some())
        {
            if matches!(key.code, KeyCode::Enter | KeyCode::Esc)
                && let Some(manage) = self.manage.as_mut()
            {
                manage.error = None;
            }
            return;
        }
        if self
            .manage
            .as_ref()
            .is_some_and(|manage| manage.edit.is_some())
        {
            self.handle_manage_edit_key(key);
            return;
        }
        if self
            .manage
            .as_ref()
            .is_some_and(|manage| manage.action_edit.is_some())
        {
            self.handle_manage_action_edit_key(key);
            return;
        }
        let Some(mut manage) = self.manage.take() else {
            return;
        };
        let items = self.managed_items();
        if manage.confirm_delete {
            match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    manage.confirm_delete = false;
                    if let Err(error) = self.delete_managed_item(manage.selected) {
                        self.message = format!("could not delete item: {error:#}");
                    } else {
                        manage.selected = manage
                            .selected
                            .min(self.managed_items().len().saturating_sub(1));
                    }
                }
                KeyCode::Char('n') | KeyCode::Esc => manage.confirm_delete = false,
                _ => {}
            }
            self.manage = Some(manage);
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.manage = None;
                self.message = "command management closed".to_owned();
                return;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                manage.selected = manage
                    .selected
                    .checked_sub(1)
                    .unwrap_or(items.len().saturating_sub(1));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !items.is_empty() {
                    manage.selected = (manage.selected + 1) % items.len();
                }
            }
            KeyCode::Char('c') => {
                manage.edit = self.new_manage_dialog(ManageEditMode::Add, manage.selected, false);
            }
            KeyCode::Char('a') if items.is_empty() => {
                manage.edit = self.new_manage_dialog(ManageEditMode::Add, manage.selected, false);
            }
            KeyCode::Char('a') => {
                manage.action_edit =
                    self.new_manage_action_dialog(ManageEditMode::Add, manage.selected, false);
            }
            KeyCode::Enter | KeyCode::Char('e') => match items.get(manage.selected) {
                Some(ManageItem::Command { .. }) => {
                    manage.edit =
                        self.new_manage_dialog(ManageEditMode::Edit, manage.selected, false);
                }
                Some(ManageItem::Action { .. }) => {
                    manage.action_edit =
                        self.new_manage_action_dialog(ManageEditMode::Edit, manage.selected, false);
                }
                None => {}
            },
            KeyCode::Char('m') => match items.get(manage.selected) {
                Some(ManageItem::Command { .. }) => {
                    manage.edit =
                        self.new_manage_dialog(ManageEditMode::Edit, manage.selected, true);
                }
                Some(ManageItem::Action { .. }) => {
                    manage.action_edit =
                        self.new_manage_action_dialog(ManageEditMode::Edit, manage.selected, true);
                }
                None => {}
            },
            KeyCode::Char('d') => {
                if items.is_empty() {
                    self.message =
                        "there are no configured commands or actions to delete".to_owned();
                } else {
                    manage.confirm_delete = true;
                }
            }
            KeyCode::Char('K') => {
                if let Err(error) = self.reorder_managed_item(manage.selected, -1) {
                    self.message = format!("could not move item up: {error:#}");
                } else {
                    manage.selected = manage.selected.saturating_sub(1);
                }
            }
            KeyCode::Char('J') => {
                if let Err(error) = self.reorder_managed_item(manage.selected, 1) {
                    self.message = format!("could not move item down: {error:#}");
                } else {
                    manage.selected =
                        (manage.selected + 1).min(self.managed_items().len().saturating_sub(1));
                }
            }
            _ => {}
        }
        self.manage = Some(manage);
    }

    fn new_manage_dialog(
        &self,
        mode: ManageEditMode,
        selected: usize,
        move_first: bool,
    ) -> Option<ManageEditDialog> {
        let targets = self.project_targets();
        if targets.is_empty() {
            return None;
        }
        let selected_command = self.managed_items().get(selected).map(|item| match *item {
            ManageItem::Command { group, command } | ManageItem::Action { group, command, .. } => {
                (group, command)
            }
        });
        let (original, name, run, cwd, target, group_name) = if mode == ManageEditMode::Edit {
            let (group_index, command_index) = selected_command?;
            let group = &self.groups[group_index];
            let command = &self.groups[group_index].commands[command_index];
            let target = targets
                .iter()
                .position(|target| target.file == group.project_file)
                .unwrap_or_default();
            let cwd = command
                .cwd
                .strip_prefix(&command.project_root)
                .ok()
                .map(|path| {
                    if path.as_os_str().is_empty() {
                        ".".to_owned()
                    } else {
                        path.display().to_string()
                    }
                })
                .unwrap_or_else(|| command.cwd.display().to_string());
            (
                Some((group_index, command_index)),
                command.name.clone(),
                command.run.clone(),
                cwd,
                target,
                group.name.clone(),
            )
        } else {
            let selected_group = selected_command
                .map(|(group, _)| group)
                .or_else(|| (!self.groups.is_empty()).then_some(0));
            let target = selected_group
                .and_then(|group| {
                    targets
                        .iter()
                        .position(|target| target.file == self.groups[group].project_file)
                })
                .unwrap_or_default();
            let group_name = selected_group
                .map(|group| self.groups[group].name.clone())
                .or_else(|| targets[target].group_names.first().cloned())
                .unwrap_or_else(|| "Project".to_owned());
            (
                None,
                String::new(),
                String::new(),
                ".".to_owned(),
                target,
                group_name,
            )
        };
        let field = if move_first {
            ManageField::Project
        } else {
            ManageField::Name
        };
        let cursor = if field == ManageField::Name {
            name.chars().count()
        } else {
            0
        };
        Some(ManageEditDialog {
            mode,
            original,
            name,
            run,
            cwd,
            target,
            group: group_name,
            field,
            cursor,
        })
    }

    fn new_manage_action_dialog(
        &self,
        mode: ManageEditMode,
        selected: usize,
        move_first: bool,
    ) -> Option<ManageActionEditDialog> {
        let items = self.managed_items();
        let selected_item = *items.get(selected)?;
        let (group, command) = match selected_item {
            ManageItem::Command { group, command } | ManageItem::Action { group, command, .. } => {
                (group, command)
            }
        };
        let parents = self.managed_commands();
        let parent = parents
            .iter()
            .position(|candidate| *candidate == (group, command))?;
        let parent_command = &self.groups[group].commands[command];
        let (original, name, run, cwd, requires_stopped, restart_after) =
            if mode == ManageEditMode::Edit {
                let ManageItem::Action { action, .. } = selected_item else {
                    return None;
                };
                let action_config = &parent_command.actions[action];
                let cwd = if action_config.cwd == parent_command.cwd {
                    String::new()
                } else {
                    action_config
                        .cwd
                        .strip_prefix(&action_config.project_root)
                        .ok()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| action_config.cwd.display().to_string())
                };
                (
                    Some((group, command, action)),
                    action_config.name.clone(),
                    action_config.run.clone(),
                    cwd,
                    action_config.requires_stopped,
                    action_config.restart_after,
                )
            } else {
                (
                    None,
                    String::new(),
                    String::new(),
                    String::new(),
                    false,
                    RestartAfter::Never,
                )
            };
        let field = if move_first {
            ManageActionField::Parent
        } else {
            ManageActionField::Name
        };
        let cursor = if field == ManageActionField::Name {
            name.chars().count()
        } else {
            0
        };
        Some(ManageActionEditDialog {
            mode,
            original,
            name,
            run,
            cwd,
            parent,
            requires_stopped,
            restart_after,
            field,
            cursor,
        })
    }

    fn handle_manage_action_edit_key(&mut self, key: KeyEvent) {
        let Some(mut manage) = self.manage.take() else {
            return;
        };
        let Some(mut dialog) = manage.action_edit.take() else {
            self.manage = Some(manage);
            return;
        };
        match key.code {
            KeyCode::Esc => self.message = "action edit cancelled".to_owned(),
            KeyCode::Enter => match self.apply_manage_action_edit(&dialog) {
                Ok(id) => {
                    manage.selected = self
                        .managed_items()
                        .iter()
                        .position(|item| match *item {
                            ManageItem::Action {
                                group,
                                command,
                                action,
                            } => self.groups[group].commands[command].actions[action].id == id,
                            ManageItem::Command { .. } => false,
                        })
                        .unwrap_or(manage.selected);
                }
                Err(error) => {
                    if dialog.name.trim().is_empty() {
                        dialog.field = ManageActionField::Name;
                    } else if dialog.run.trim().is_empty() {
                        dialog.field = ManageActionField::Run;
                    }
                    dialog.cursor = manage_action_field_text_len(&dialog);
                    manage.error = Some(format!("{error:#}"));
                    manage.action_edit = Some(dialog);
                }
            },
            KeyCode::Tab | KeyCode::Down => {
                dialog.field = next_manage_action_field(dialog.field, false);
                dialog.cursor = manage_action_field_text_len(&dialog);
                manage.action_edit = Some(dialog);
            }
            KeyCode::BackTab | KeyCode::Up => {
                dialog.field = next_manage_action_field(dialog.field, true);
                dialog.cursor = manage_action_field_text_len(&dialog);
                manage.action_edit = Some(dialog);
            }
            KeyCode::Left | KeyCode::Right if dialog.field == ManageActionField::Parent => {
                let count = self.managed_commands().len();
                if count > 0 {
                    dialog.parent = if key.code == KeyCode::Left {
                        dialog.parent.checked_sub(1).unwrap_or(count - 1)
                    } else {
                        (dialog.parent + 1) % count
                    };
                }
                manage.action_edit = Some(dialog);
            }
            KeyCode::Left | KeyCode::Right
                if dialog.field == ManageActionField::RequiresStopped =>
            {
                dialog.requires_stopped = !dialog.requires_stopped;
                manage.action_edit = Some(dialog);
            }
            KeyCode::Left | KeyCode::Right if dialog.field == ManageActionField::RestartAfter => {
                dialog.restart_after =
                    cycle_restart_after(dialog.restart_after, key.code == KeyCode::Left);
                manage.action_edit = Some(dialog);
            }
            KeyCode::Char(' ') if dialog.field == ManageActionField::RequiresStopped => {
                dialog.requires_stopped = !dialog.requires_stopped;
                manage.action_edit = Some(dialog);
            }
            KeyCode::Left | KeyCode::Right if manage_action_field_is_text(dialog.field) => {
                let length = manage_action_field_text_len(&dialog);
                move_text_cursor_with_len(&mut dialog.cursor, length, key.code == KeyCode::Right);
                manage.action_edit = Some(dialog);
            }
            KeyCode::Home if manage_action_field_is_text(dialog.field) => {
                dialog.cursor = 0;
                manage.action_edit = Some(dialog);
            }
            KeyCode::End if manage_action_field_is_text(dialog.field) => {
                dialog.cursor = manage_action_field_text_len(&dialog);
                manage.action_edit = Some(dialog);
            }
            KeyCode::Backspace => {
                match dialog.field {
                    ManageActionField::Name => {
                        remove_before_cursor(&mut dialog.name, &mut dialog.cursor)
                    }
                    ManageActionField::Run => {
                        remove_before_cursor(&mut dialog.run, &mut dialog.cursor)
                    }
                    ManageActionField::Cwd => {
                        remove_before_cursor(&mut dialog.cwd, &mut dialog.cursor)
                    }
                    _ => {}
                }
                manage.action_edit = Some(dialog);
            }
            KeyCode::Delete => {
                match dialog.field {
                    ManageActionField::Name => remove_at_cursor(&mut dialog.name, dialog.cursor),
                    ManageActionField::Run => remove_at_cursor(&mut dialog.run, dialog.cursor),
                    ManageActionField::Cwd => remove_at_cursor(&mut dialog.cwd, dialog.cursor),
                    _ => {}
                }
                manage.action_edit = Some(dialog);
            }
            KeyCode::Char(character)
                if manage_action_field_is_text(dialog.field)
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                match dialog.field {
                    ManageActionField::Name => {
                        insert_at_cursor(&mut dialog.name, &mut dialog.cursor, character)
                    }
                    ManageActionField::Run => {
                        insert_at_cursor(&mut dialog.run, &mut dialog.cursor, character)
                    }
                    ManageActionField::Cwd => {
                        insert_at_cursor(&mut dialog.cwd, &mut dialog.cursor, character)
                    }
                    _ => {}
                }
                manage.action_edit = Some(dialog);
            }
            _ => manage.action_edit = Some(dialog),
        }
        self.manage = Some(manage);
    }

    fn handle_manage_edit_key(&mut self, key: KeyEvent) {
        let Some(mut manage) = self.manage.take() else {
            return;
        };
        let Some(mut dialog) = manage.edit.take() else {
            self.manage = Some(manage);
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.message = "command edit cancelled".to_owned();
            }
            KeyCode::Enter => match self.apply_manage_edit(&dialog) {
                Ok(id) => {
                    manage.selected = self
                        .managed_items()
                        .iter()
                        .position(|item| match *item {
                            ManageItem::Command { group, command } => {
                                self.groups[group].commands[command].id == id
                            }
                            ManageItem::Action { .. } => false,
                        })
                        .unwrap_or(manage.selected);
                }
                Err(error) => {
                    if let Some(field) = first_missing_manage_field(&dialog) {
                        dialog.field = field;
                        dialog.cursor = manage_field_text_len(&dialog);
                    }
                    manage.error = Some(format!("{error:#}"));
                    manage.edit = Some(dialog);
                }
            },
            KeyCode::Tab | KeyCode::Down => {
                dialog.field = next_manage_field(dialog.field, false);
                dialog.cursor = manage_field_text_len(&dialog);
                manage.edit = Some(dialog);
            }
            KeyCode::BackTab | KeyCode::Up => {
                dialog.field = next_manage_field(dialog.field, true);
                dialog.cursor = manage_field_text_len(&dialog);
                manage.edit = Some(dialog);
            }
            KeyCode::Left | KeyCode::Right if dialog.field == ManageField::Project => {
                let targets = self.project_targets();
                if !targets.is_empty() {
                    let previous_target = dialog.target;
                    dialog.target = if key.code == KeyCode::Left {
                        dialog
                            .target
                            .checked_sub(1)
                            .unwrap_or(targets.len().saturating_sub(1))
                    } else {
                        (dialog.target + 1) % targets.len()
                    };
                    if dialog.target != previous_target {
                        dialog.group = targets[dialog.target]
                            .group_names
                            .first()
                            .cloned()
                            .unwrap_or_else(|| "Project".to_owned());
                    }
                }
                manage.edit = Some(dialog);
            }
            KeyCode::Left | KeyCode::Right
                if dialog.field == ManageField::Group
                    && key.modifiers.contains(KeyModifiers::ALT) =>
            {
                if let Some(target) = self.project_targets().get(dialog.target) {
                    cycle_named_value(
                        &mut dialog.group,
                        &target.group_names,
                        key.code == KeyCode::Left,
                    );
                    dialog.cursor = dialog.group.chars().count();
                }
                manage.edit = Some(dialog);
            }
            KeyCode::Left | KeyCode::Right
                if manage_field_is_text(dialog.field)
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let length = manage_field_text_len(&dialog);
                move_text_cursor_with_len(&mut dialog.cursor, length, key.code == KeyCode::Right);
                manage.edit = Some(dialog);
            }
            KeyCode::Home if manage_field_is_text(dialog.field) => {
                dialog.cursor = 0;
                manage.edit = Some(dialog);
            }
            KeyCode::End if manage_field_is_text(dialog.field) => {
                dialog.cursor = manage_field_text_len(&dialog);
                manage.edit = Some(dialog);
            }
            KeyCode::Backspace => {
                match dialog.field {
                    ManageField::Name => remove_before_cursor(&mut dialog.name, &mut dialog.cursor),
                    ManageField::Run => remove_before_cursor(&mut dialog.run, &mut dialog.cursor),
                    ManageField::Cwd => remove_before_cursor(&mut dialog.cwd, &mut dialog.cursor),
                    ManageField::Group => {
                        remove_before_cursor(&mut dialog.group, &mut dialog.cursor)
                    }
                    ManageField::Project => {}
                }
                manage.edit = Some(dialog);
            }
            KeyCode::Delete => {
                match dialog.field {
                    ManageField::Name => remove_at_cursor(&mut dialog.name, dialog.cursor),
                    ManageField::Run => remove_at_cursor(&mut dialog.run, dialog.cursor),
                    ManageField::Cwd => remove_at_cursor(&mut dialog.cwd, dialog.cursor),
                    ManageField::Group => remove_at_cursor(&mut dialog.group, dialog.cursor),
                    ManageField::Project => {}
                }
                manage.edit = Some(dialog);
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                match dialog.field {
                    ManageField::Name => {
                        insert_at_cursor(&mut dialog.name, &mut dialog.cursor, character)
                    }
                    ManageField::Run => {
                        insert_at_cursor(&mut dialog.run, &mut dialog.cursor, character)
                    }
                    ManageField::Cwd => {
                        insert_at_cursor(&mut dialog.cwd, &mut dialog.cursor, character)
                    }
                    ManageField::Group => {
                        insert_at_cursor(&mut dialog.group, &mut dialog.cursor, character)
                    }
                    ManageField::Project => {}
                }
                manage.edit = Some(dialog);
            }
            _ => manage.edit = Some(dialog),
        }
        self.manage = Some(manage);
    }

    fn apply_manage_edit(&mut self, dialog: &ManageEditDialog) -> Result<String> {
        let targets = self.project_targets();
        let target = targets
            .get(dialog.target)
            .context("selected project is unavailable")?
            .clone();
        let name = dialog.name.trim();
        let run = dialog.run.trim();
        let group_name = dialog.group.trim();
        let missing = [
            (name.is_empty(), "Name"),
            (run.is_empty(), "Run"),
            (group_name.is_empty(), "Group"),
        ]
        .into_iter()
        .filter_map(|(missing, label)| missing.then_some(label))
        .collect::<Vec<_>>();
        if !missing.is_empty() {
            bail!("required fields missing: {}", missing.join(", "));
        }
        let cwd = if dialog.cwd.trim().is_empty() {
            "."
        } else {
            dialog.cwd.trim()
        };
        let catalog = &self.runner.project().theme_catalog;
        if dialog.mode == ManageEditMode::Add {
            command_edit::add_with_catalog_quiet(
                &target.file,
                AddOptions {
                    group: Some(group_name.to_owned()),
                    name: Some(name.to_owned()),
                    run: Some(run.to_owned()),
                    cwd: Some(cwd.to_owned()),
                    pre: Vec::new(),
                    autostart: false,
                },
                catalog,
            )?;
            let mut config = load_configured_command(&target.file, name, catalog)?;
            if self.runner.snapshot(&config.id).is_some() {
                config.id = format!("__managed::{}", self.next_ephemeral_id);
                self.next_ephemeral_id += 1;
            }
            let id = config.id.clone();
            self.runner.add_command(config.clone())?;
            self.insert_managed_command(&target, group_name, config);
            self.rebuild_command_items(Some(&id));
            self.message = format!("added {name:?} to {} / {group_name}", target.label);
            return Ok(id);
        }

        let (source_group, source_command) =
            dialog.original.context("edited command is missing")?;
        let old = self.groups[source_group].commands[source_command].clone();
        let source_file = self.groups[source_group].project_file.clone();
        let source_group_name = self.groups[source_group].name.clone();
        let moved = source_file != target.file || source_group_name != group_name;
        if moved {
            command_edit::move_with_catalog(
                &source_file,
                &old.name,
                &target.file,
                group_name,
                catalog,
            )?;
        }
        command_edit::edit_with_catalog_quiet(
            &target.file,
            command_edit::EditOptions {
                target: Some(old.name.clone()),
                new_name: Some(name.to_owned()),
                run: Some(run.to_owned()),
                cwd: Some(cwd.to_owned()),
                pre: None,
                autostart: None,
            },
            catalog,
        )?;
        let mut config = load_configured_command(&target.file, name, catalog)?;
        config.id = old.id.clone();
        self.runner.update_command(config.clone())?;
        if moved {
            self.groups[source_group].commands.remove(source_command);
            if self.groups[source_group].commands.is_empty() {
                self.groups.remove(source_group);
            }
            self.insert_managed_command(&target, group_name, config);
        } else {
            self.groups[source_group].commands[source_command] = config;
        }
        let id = old.id;
        self.rebuild_command_items(Some(&id));
        self.message = format!("updated {name:?} in {} / {group_name}", target.label);
        Ok(id)
    }

    fn apply_manage_action_edit(&mut self, dialog: &ManageActionEditDialog) -> Result<String> {
        let name = dialog.name.trim();
        let run = dialog.run.trim();
        let missing = [(name.is_empty(), "Name"), (run.is_empty(), "Run")]
            .into_iter()
            .filter_map(|(missing, label)| missing.then_some(label))
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            bail!("required fields missing: {}", missing.join(", "));
        }
        let parents = self.managed_commands();
        let (target_group, target_command) = *parents
            .get(dialog.parent)
            .context("selected parent command is unavailable")?;
        let target_parent = self.groups[target_group].commands[target_command].clone();
        let options = command_edit::ActionOptions {
            name: name.to_owned(),
            run: run.to_owned(),
            cwd: (!dialog.cwd.trim().is_empty()).then(|| dialog.cwd.trim().to_owned()),
            pre: dialog
                .original
                .and_then(|(group, command, action)| {
                    self.groups
                        .get(group)?
                        .commands
                        .get(command)?
                        .actions
                        .get(action)
                        .map(|action| action.pre.clone())
                })
                .unwrap_or_default(),
            requires_stopped: dialog.requires_stopped,
            restart_after: dialog.restart_after,
        };
        let catalog = &self.runner.project().theme_catalog;
        if dialog.mode == ManageEditMode::Add {
            command_edit::add_action_with_catalog_quiet(
                &target_parent.project_file,
                &target_parent.name,
                &options,
                catalog,
            )?;
            self.refresh_managed_command(target_group, target_command)?;
            let id = action_id(&target_parent.id, name);
            self.rebuild_command_items(Some(&id));
            self.message = format!("added action {name:?} under {:?}", target_parent.name);
            return Ok(id);
        }

        let (source_group, source_command, source_action) =
            dialog.original.context("edited action is missing")?;
        let source_parent = self.groups[source_group].commands[source_command].clone();
        let old = source_parent.actions[source_action].clone();
        let moved = source_group != target_group || source_command != target_command;
        let renamed = old.name != name;
        if (moved || renamed)
            && self
                .runner
                .snapshot(&old.id)
                .is_some_and(|snapshot| snapshot.state.is_active())
        {
            bail!("stop action {:?} before renaming or moving it", old.name);
        }
        if moved {
            command_edit::move_action_with_catalog(
                &source_parent.project_file,
                &source_parent.name,
                &old.name,
                &target_parent.project_file,
                &target_parent.name,
                catalog,
            )?;
        }
        command_edit::edit_action_with_catalog_quiet(
            &target_parent.project_file,
            &target_parent.name,
            &old.name,
            &options,
            catalog,
        )?;
        if moved {
            self.refresh_managed_command(source_group, source_command)?;
        }
        self.refresh_managed_command(target_group, target_command)?;
        let id = action_id(&target_parent.id, name);
        self.rebuild_command_items(Some(&id));
        self.message = format!("updated action {name:?} under {:?}", target_parent.name);
        Ok(id)
    }

    fn refresh_managed_command(&mut self, group: usize, command: usize) -> Result<()> {
        let current = self
            .groups
            .get(group)
            .and_then(|group| group.commands.get(command))
            .context("configured command is unavailable")?
            .clone();
        let mut updated = load_configured_command(
            &current.project_file,
            &current.name,
            &self.runner.project().theme_catalog,
        )?;
        updated.id = current.id;
        for action in &mut updated.actions {
            action.parent_id.clone_from(&updated.id);
            action.id = action_id(&updated.id, &action.name);
        }
        self.runner.update_command(updated.clone())?;
        self.groups[group].commands[command] = updated;
        Ok(())
    }

    fn insert_managed_command(
        &mut self,
        target: &ProjectTarget,
        group_name: &str,
        config: CommandConfig,
    ) {
        if let Some(group) = self
            .groups
            .iter_mut()
            .find(|group| group.project_file == target.file && group.name == group_name)
        {
            group.commands.push(config);
            return;
        }
        let combined = self.groups.iter().any(|group| group.project.is_some());
        let insert_at = self
            .groups
            .iter()
            .rposition(|group| group.project_file == target.file)
            .map(|index| index + 1)
            .unwrap_or(self.groups.len());
        self.groups.insert(
            insert_at,
            GroupConfig {
                project: combined.then_some(target.label.clone()),
                name: group_name.to_owned(),
                project_file: target.file.clone(),
                project_root: target.root.clone(),
                commands: vec![config],
            },
        );
    }

    fn delete_managed_item(&mut self, selected: usize) -> Result<()> {
        let item = *self
            .managed_items()
            .get(selected)
            .context("no configured command or action is selected")?;
        let ManageItem::Command { group, command } = item else {
            let ManageItem::Action {
                group,
                command,
                action,
            } = item
            else {
                unreachable!()
            };
            let parent = self.groups[group].commands[command].clone();
            let action_config = parent.actions[action].clone();
            if self
                .runner
                .snapshot(&action_config.id)
                .is_some_and(|snapshot| snapshot.state.is_active())
            {
                bail!("stop action {:?} before deleting it", action_config.name);
            }
            command_edit::delete_action_with_catalog_quiet(
                &parent.project_file,
                &parent.name,
                &action_config.name,
                &self.runner.project().theme_catalog,
            )?;
            self.groups[group].commands[command].actions.remove(action);
            self.runner
                .update_command(self.groups[group].commands[command].clone())?;
            self.rebuild_command_items(None);
            self.message = format!("deleted action {:?}", action_config.name);
            return Ok(());
        };
        let config = self.groups[group].commands[command].clone();
        if self
            .runner
            .snapshot(&config.id)
            .is_some_and(|snapshot| snapshot.state.is_active())
        {
            bail!("stop {:?} before deleting it", config.name);
        }
        if let Some(action) = config.actions.iter().find(|action| {
            self.runner
                .snapshot(&action.id)
                .is_some_and(|snapshot| snapshot.state.is_active())
        }) {
            bail!(
                "stop action {:?} before deleting its parent command",
                action.name
            );
        }
        command_edit::delete_with_catalog_quiet(
            &config.project_file,
            Some(config.name.clone()),
            true,
            &self.runner.project().theme_catalog,
        )?;
        self.runner.remove_command(&config.id)?;
        self.groups[group].commands.remove(command);
        if self.groups[group].commands.is_empty() {
            self.groups.remove(group);
        }
        self.rebuild_command_items(None);
        self.message = format!("deleted configured command {:?}", config.name);
        Ok(())
    }

    fn reorder_managed_item(&mut self, selected: usize, direction: isize) -> Result<()> {
        let item = *self
            .managed_items()
            .get(selected)
            .context("no configured command or action is selected")?;
        if let ManageItem::Action {
            group,
            command,
            action,
        } = item
        {
            let destination = action
                .checked_add_signed(direction)
                .filter(|destination| {
                    *destination < self.groups[group].commands[command].actions.len()
                })
                .context("action is already at the edge of its command")?;
            let parent = self.groups[group].commands[command].clone();
            let action_config = parent.actions[action].clone();
            command_edit::reorder_action_with_catalog(
                &parent.project_file,
                &parent.name,
                &action_config.name,
                direction,
                &self.runner.project().theme_catalog,
            )?;
            self.groups[group].commands[command]
                .actions
                .swap(action, destination);
            self.runner
                .update_command(self.groups[group].commands[command].clone())?;
            self.rebuild_command_items(Some(&action_config.id));
            self.message = format!("reordered action {:?}", action_config.name);
            return Ok(());
        }
        let ManageItem::Command { group, command } = item else {
            unreachable!()
        };
        let destination = command
            .checked_add_signed(direction)
            .filter(|destination| *destination < self.groups[group].commands.len())
            .context("command is already at the edge of its group")?;
        let config = self.groups[group].commands[command].clone();
        command_edit::reorder_with_catalog(
            &config.project_file,
            &config.name,
            direction,
            &self.runner.project().theme_catalog,
        )?;
        self.groups[group].commands.swap(command, destination);
        self.rebuild_command_items(Some(&config.id));
        self.message = format!("reordered {:?}", config.name);
        Ok(())
    }

    fn ephemeral_context(&self) -> Option<(CommandConfig, Option<String>, String)> {
        match *self.command_items.get(self.selected_item)? {
            CommandSelection::Ephemeral(index) => {
                let ephemeral = self.ephemeral.get(index)?;
                Some((
                    ephemeral.config.clone(),
                    ephemeral.project.clone(),
                    ephemeral.suggested_group.clone(),
                ))
            }
            CommandSelection::Command { group, command }
            | CommandSelection::Action { group, command, .. } => {
                let group = self.groups.get(group)?;
                Some((
                    group.commands.get(command)?.clone(),
                    group.project.clone(),
                    group.name.clone(),
                ))
            }
            CommandSelection::Group(group) | CommandSelection::Project { first_group: group } => {
                let group = self.groups.get(group)?;
                let base = group
                    .commands
                    .first()
                    .or_else(|| self.groups.iter().find_map(|group| group.commands.first()))?;
                Some((base.clone(), group.project.clone(), group.name.clone()))
            }
            CommandSelection::EphemeralGroup => self.ephemeral.first().map(|ephemeral| {
                (
                    ephemeral.config.clone(),
                    ephemeral.project.clone(),
                    ephemeral.suggested_group.clone(),
                )
            }),
        }
    }

    fn apply_search(&mut self, query: String, kind: SearchKind) {
        if query.is_empty() {
            self.search = None;
            self.search_source.clear();
            self.message = "search cleared".to_owned();
            return;
        }
        match build_search_regex(&query, kind) {
            Ok(regex) => {
                self.search = Some(regex);
                self.search_source = query;
                self.search_kind = kind;
                self.next_search_match(false);
            }
            Err(error) => self.message = format!("invalid search expression: {error}"),
        }
    }

    fn next_search_match(&mut self, backwards: bool) {
        let Some(regex) = &self.search else {
            self.message = "press / for exact search or \\ for regex search".to_owned();
            return;
        };
        let Some(name) = self.selected_command_name() else {
            self.message = "select a command to search its output".to_owned();
            return;
        };
        let logs = self.runner.logs(name);
        if logs.is_empty() {
            self.message = "no output to search".to_owned();
            return;
        }
        let mut indices: Vec<_> = logs
            .iter()
            .enumerate()
            .filter(|(_, line)| regex.is_match(&line.text))
            .map(|(index, _)| index)
            .collect();
        if backwards {
            indices.reverse();
        }
        let found = if backwards {
            indices
                .iter()
                .copied()
                .find(|index| *index < self.log_cursor)
                .or_else(|| indices.first().copied())
        } else {
            indices
                .iter()
                .copied()
                .find(|index| *index > self.log_cursor)
                .or_else(|| indices.first().copied())
        };
        if let Some(index) = found {
            self.follow = false;
            self.log_cursor = index;
            self.wrapped_cursor_at_top =
                search_needs_top_alignment(self.wrap_logs, self.visible_log_range, index);
            self.message = format!(
                "match for {}",
                search_expression(&self.search_source, self.search_kind)
            );
        } else {
            self.message = format!(
                "no matches for {}",
                search_expression(&self.search_source, self.search_kind)
            );
        }
    }

    fn copy_current_line(&mut self) {
        let Some(name) = self.selected_command_name() else {
            self.message = "select a command to copy its output".to_owned();
            return;
        };
        let logs = self.runner.logs(name);
        let Some(line) = logs.get(self.log_cursor.min(logs.len().saturating_sub(1))) else {
            self.message = "no line to copy".to_owned();
            return;
        };
        match copy_to_clipboard(line.display(self.timestamps)) {
            Ok(lease) => {
                self.clipboard = Some(lease);
                self.message = "copied selected line".to_owned();
            }
            Err(error) => self.message = error.to_string(),
        }
    }

    fn copy_all_logs(&mut self) {
        let Some(name) = self.selected_command_name() else {
            self.message = "select a command to copy its output".to_owned();
            return;
        };
        let text = logs_as_text(&self.runner.logs(name), self.timestamps);
        match copy_to_clipboard(text) {
            Ok(lease) => {
                self.clipboard = Some(lease);
                self.message = "copied all output".to_owned();
            }
            Err(error) => self.message = error.to_string(),
        }
    }

    fn clear_selected_logs(&mut self) {
        let Some(name) = self.selected_command_name().map(str::to_owned) else {
            self.message = "select a command to clear its output".to_owned();
            return;
        };
        let label = self
            .selected_runnable_label()
            .unwrap_or(name.as_str())
            .to_owned();
        match self.runner.clear_logs(&name) {
            Ok(()) => {
                self.reset_log_navigation();
                self.message = format!("cleared output for {label:?}");
            }
            Err(error) => self.message = error.to_string(),
        }
    }

    fn default_dump_path(&self) -> PathBuf {
        let command = self.selected_command();
        let name = self.selected_runnable_label().unwrap_or("output");
        command
            .map(|command| command.project_root.as_path())
            .unwrap_or(&self.runner.project().root)
            .join(".blade-dumps")
            .join(format!(
                "{}-{}.log",
                safe_filename(name),
                Local::now().format("%Y%m%d-%H%M%S-%3f")
            ))
    }

    fn refresh_clipboard(&mut self) {
        let expired = self.clipboard.as_ref().is_some_and(|lease| {
            !lease.active.load(Ordering::Relaxed) || Instant::now() >= lease.expires_at
        });
        if expired {
            self.clipboard = None;
            self.message = "clipboard lease ended".to_owned();
        }
    }

    fn clipboard_countdown(&self) -> Option<String> {
        self.clipboard.as_ref().map(|lease| {
            let seconds = lease
                .expires_at
                .saturating_duration_since(Instant::now())
                .as_secs()
                .saturating_add(1);
            format!("clipboard expires in {}:{:02}", seconds / 60, seconds % 60)
        })
    }

    fn dump_logs(&mut self, mut path: PathBuf) {
        if path.as_os_str().is_empty() {
            self.message = "dump path must not be empty".to_owned();
            return;
        }
        if path.is_relative() {
            let root = self
                .selected_command()
                .map(|command| command.project_root.as_path())
                .unwrap_or(&self.runner.project().root);
            path = root.join(path);
        }
        let Some(name) = self.selected_command_name() else {
            self.message = "select a command to dump its output".to_owned();
            return;
        };
        self.message = match write_dump(
            &path,
            &logs_as_text(&self.runner.logs(name), self.timestamps),
        ) {
            Ok(()) => format!("dumped output to {}", path.display()),
            Err(error) => error.to_string(),
        };
    }

    fn draw(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        let footer_height = if area.width >= 120 { 3 } else { 4 };
        let minimum_height = 5 + footer_height;
        if area.width < 32 || area.height < minimum_height {
            frame.render_widget(
                Paragraph::new(format!(
                    "Blade needs a terminal at least 32×{minimum_height}\nResize the terminal to continue."
                ))
                .alignment(Alignment::Center)
                .block(Block::default().borders(Borders::ALL).title(" Blade ")),
                area,
            );
            return;
        }
        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(3),
                Constraint::Length(footer_height),
            ])
            .split(area);
        self.draw_header(frame, vertical[0]);
        let (commands_area, logs_area) = if area.width >= 100 {
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(32), Constraint::Min(40)])
                .split(vertical[1]);
            (columns[0], columns[1])
        } else {
            let command_height = (vertical[1].height / 3).clamp(5, 10);
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(command_height), Constraint::Min(3)])
                .split(vertical[1]);
            (rows[0], rows[1])
        };
        self.draw_commands(frame, commands_area);
        self.draw_logs(frame, logs_area);
        self.draw_footer(frame, vertical[2]);
        if self.show_help {
            self.draw_help(frame, area);
        }
        if self.theme_picker.is_some() {
            self.draw_theme_picker(frame, area);
        }
        if self.command_picker.is_some() {
            self.draw_command_picker(frame, area);
        }
        if self.persist_dialog.is_some() {
            self.draw_persist_dialog(frame, area);
        }
        if self.manage.is_some() {
            self.draw_manage(frame, area);
        }
    }

    fn draw_header(&self, frame: &mut Frame<'_>, area: Rect) {
        let active = self.runner.active_command_count();
        let total = self.runner.command_count();
        let title = Line::from(vec![
            Span::styled(
                " BLADE ",
                Style::default()
                    .fg(self.theme.accent_text)
                    .bg(self.theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("  {}", self.runner.project().name)),
            Span::styled(
                format!(
                    "  {active}/{total} commands active  •  {}",
                    self.runner.project().path.display()
                ),
                Style::default().fg(self.theme.muted),
            ),
        ]);
        frame.render_widget(
            Paragraph::new(title).block(Block::default().borders(Borders::BOTTOM)),
            area,
        );
    }

    fn draw_commands(&self, frame: &mut Frame<'_>, area: Rect) {
        let mut items = Vec::new();
        let mut previous_project = None;
        let row_width = area.width.saturating_sub(2) as usize;
        let group_highlight_style = Style::default()
            .fg(self.theme.accent_text)
            .bg(self.theme.accent)
            .add_modifier(Modifier::BOLD);
        if !self.ephemeral.is_empty() {
            let style = if items.len() == self.selected_item {
                group_highlight_style
            } else {
                Style::default()
                    .fg(self.theme.accent)
                    .add_modifier(Modifier::BOLD)
            };
            items.push(ListItem::new(Line::from(Span::styled(
                pad_row(" ▾ Ephemeral".to_owned(), row_width),
                style,
            ))));
            for ephemeral in &self.ephemeral {
                let command = &ephemeral.config;
                let snapshot = self.runner.snapshot(&command.id);
                let state = snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.state)
                    .unwrap_or(CommandState::Stopped);
                let color = state_color(state, &self.theme);
                let style = if items.len() == self.selected_item {
                    Style::default()
                        .fg(contrasting_text_color(color))
                        .bg(color)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(color)
                };
                items.push(ListItem::new(Line::from(Span::styled(
                    pad_row(
                        format!("  {} {}", state_marker(state), command.name),
                        row_width,
                    ),
                    style,
                ))));
            }
        }
        for group in &self.groups {
            if group.project.as_deref() != previous_project {
                if let Some(project) = group.project.as_deref() {
                    let project_style = if items.len() == self.selected_item {
                        group_highlight_style
                    } else {
                        Style::default()
                            .fg(self.theme.accent)
                            .add_modifier(Modifier::BOLD)
                    };
                    items.push(ListItem::new(Line::from(Span::styled(
                        pad_row(format!(" ◆ {project}"), row_width),
                        project_style,
                    ))));
                }
                previous_project = group.project.as_deref();
            }
            let group_style = if items.len() == self.selected_item {
                group_highlight_style
            } else {
                Style::default()
                    .fg(self.theme.accent)
                    .add_modifier(Modifier::BOLD)
            };
            items.push(ListItem::new(Line::from(Span::styled(
                pad_row(
                    format!(
                        "{}▾ {}",
                        if group.project.is_some() { "   " } else { " " },
                        group.name
                    ),
                    row_width,
                ),
                group_style,
            ))));
            for command in &group.commands {
                let snapshot = self.runner.snapshot(&command.id);
                let state = snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.state)
                    .unwrap_or(CommandState::Stopped);
                let marker = state_marker(state);
                let color = state_color(state, &self.theme);
                let style = if items.len() == self.selected_item {
                    Style::default()
                        .fg(contrasting_text_color(color))
                        .bg(color)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(color)
                };
                items.push(ListItem::new(Line::from(Span::styled(
                    pad_row(
                        format!(
                            "{}{marker} {}",
                            if group.project.is_some() {
                                "    "
                            } else {
                                "  "
                            },
                            command.name
                        ),
                        row_width,
                    ),
                    style,
                ))));
                for action in &command.actions {
                    let state = self
                        .runner
                        .snapshot(&action.id)
                        .map(|snapshot| snapshot.state)
                        .unwrap_or(CommandState::Stopped);
                    let color = state_color(state, &self.theme);
                    let style = if items.len() == self.selected_item {
                        Style::default()
                            .fg(contrasting_text_color(color))
                            .bg(color)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(color)
                    };
                    items.push(ListItem::new(Line::from(Span::styled(
                        pad_row(
                            format!(
                                "{}↳ {} {}",
                                if group.project.is_some() {
                                    "      "
                                } else {
                                    "    "
                                },
                                state_marker(state),
                                action.name
                            ),
                            row_width,
                        ),
                        style,
                    ))));
                }
            }
        }
        let border = if self.focus == Focus::Commands {
            self.theme.accent
        } else {
            self.theme.muted
        };
        let item_count = items.len();
        let list = List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border))
                .title(" Commands "),
        );
        let mut state = ListState::default().with_selected(Some(self.selected_item));
        frame.render_stateful_widget(list, area, &mut state);
        render_vertical_scrollbar(
            frame,
            area,
            item_count,
            state.offset(),
            area.height.saturating_sub(2) as usize,
            &self.theme,
        );
    }

    fn draw_logs(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let selected_project = self.selected_project().map(str::to_owned);
        let selected_group = if matches!(
            self.command_items.get(self.selected_item),
            Some(CommandSelection::EphemeralGroup)
        ) {
            Some("Ephemeral".to_owned())
        } else {
            self.selected_group().map(|(_, name)| name.to_owned())
        };
        let command_label = self.selected_runnable_label().map(str::to_owned);
        let name = self.selected_command_name().map(str::to_owned);
        let logs = self
            .selected_command_name()
            .map(|name| self.runner.logs(name))
            .unwrap_or_default();
        self.log_view_height = area.height.saturating_sub(2).max(1) as usize;
        let previous_log_count = self.observed_log_count;
        let current_log_tail = logs.last().map(|line| line.sequence);
        let logs_appended = self.observed_log_tail.is_some()
            && current_log_tail.is_some()
            && current_log_tail != self.observed_log_tail;
        let wrapped_was_at_live_edge =
            previous_log_count == 0 || self.wrapped_log_end >= previous_log_count.saturating_sub(1);
        let unwrapped_was_at_live_edge = previous_log_count == 0
            || self.log_scroll.saturating_add(self.log_view_height) >= previous_log_count;
        self.observed_log_count = logs.len();
        self.observed_log_tail = current_log_tail;
        if logs.is_empty() {
            self.log_cursor = 0;
            self.log_scroll = 0;
            self.wrapped_log_end = 0;
            self.wrapped_cursor_at_top = false;
            self.visible_log_range = None;
        } else {
            if self.follow {
                self.log_cursor = logs.len() - 1;
                self.wrapped_log_end = self.log_cursor;
                self.wrapped_cursor_at_top = false;
            } else {
                self.log_cursor = self.log_cursor.min(logs.len() - 1);
                self.wrapped_log_end = self.wrapped_log_end.min(logs.len() - 1);
                if logs_appended && !self.wrap_logs && unwrapped_was_at_live_edge {
                    self.log_scroll = advance_live_scroll(
                        self.log_scroll,
                        self.log_cursor,
                        self.log_view_height,
                        logs.len(),
                    );
                }
            }
            if !self.wrap_logs {
                if self.log_cursor < self.log_scroll {
                    self.log_scroll = self.log_cursor;
                }
                if self.log_cursor >= self.log_scroll + self.log_view_height {
                    self.log_scroll = self.log_cursor + 1 - self.log_view_height;
                }
            }
        }
        let visible_rows = if self.wrap_logs {
            let width = area.width.saturating_sub(2).max(1) as usize;
            if self.wrapped_cursor_at_top {
                let rows = wrapped_log_rows_from(
                    &logs,
                    self.log_cursor,
                    self.log_view_height,
                    width,
                    self.timestamps,
                );
                if let Some((last_index, _)) = rows.last() {
                    self.wrapped_log_end = *last_index;
                }
                rows
            } else {
                wrapped_log_view(
                    &logs,
                    self.log_cursor,
                    &mut self.wrapped_log_end,
                    self.log_view_height,
                    width,
                    self.timestamps,
                    logs_appended && wrapped_was_at_live_edge,
                )
            }
        } else {
            let end = (self.log_scroll + self.log_view_height).min(logs.len());
            logs.iter()
                .enumerate()
                .skip(self.log_scroll)
                .take(end.saturating_sub(self.log_scroll))
                .map(|(index, line)| (index, line.display(self.timestamps)))
                .collect()
        };
        self.visible_log_range = visible_rows
            .first()
            .zip(visible_rows.last())
            .map(|((first, _), (last, _))| (*first, *last));
        let mut visible = visible_rows
            .into_iter()
            .map(|(index, text)| {
                let line = &logs[index];
                let mut style = if line.kind == LogKind::System {
                    Style::default().fg(self.theme.muted)
                } else {
                    Style::default().fg(self.theme.text)
                };
                if self
                    .search
                    .as_ref()
                    .is_some_and(|regex| regex.is_match(&line.text))
                {
                    style = style.fg(self.theme.search);
                }
                if index == self.log_cursor {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                styled_log_line(line, text, self.timestamps, style, self.theme.muted)
            })
            .collect::<Vec<_>>();
        if name.is_none() {
            visible.push(Line::from(Span::styled(
                if selected_project.is_some() {
                    "Project selected: Enter/s starts all, x stops all, r restarts all"
                } else {
                    "Group selected: Enter/s starts all, x stops all, r restarts all"
                },
                Style::default().fg(self.theme.muted),
            )));
        }
        let snapshot = name
            .as_deref()
            .and_then(|command_name| self.runner.snapshot(command_name));
        let state = snapshot
            .as_ref()
            .map(|snapshot| snapshot.state.label())
            .unwrap_or("unknown");
        let mut process_detail = snapshot
            .as_ref()
            .and_then(|snapshot| {
                snapshot
                    .pid
                    .map(|pid| format!(" pid={pid}"))
                    .or_else(|| snapshot.exit_code.map(|code| format!(" exit={code}")))
            })
            .unwrap_or_default();
        if let Some(snapshot) = snapshot.as_ref()
            && let Some(deadline) = snapshot.stop_deadline
        {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let seconds = remaining.as_millis().div_ceil(1000);
            let next_action = if snapshot.stop_level == 1 {
                "terminate"
            } else {
                "kill"
            };
            process_detail.push_str(&format!(" • {next_action} in {seconds}s"));
        }
        let title = if let Some(name) = name {
            format!(
                " Output: {} [{state}{process_detail}] {}/{}{} • wrap:{} ",
                command_label.as_deref().unwrap_or(&name),
                if logs.is_empty() {
                    0
                } else {
                    self.log_cursor + 1
                },
                logs.len(),
                if self.follow { " • following" } else { "" },
                if self.wrap_logs { "on" } else { "off" }
            )
        } else {
            format!(
                " Output: {} selected • wrap:{} ",
                selected_project
                    .as_deref()
                    .map(|project| format!("project {project}"))
                    .unwrap_or_else(|| format!(
                        "group {}",
                        selected_group.as_deref().unwrap_or("unknown")
                    )),
                if self.wrap_logs { "on" } else { "off" }
            )
        };
        let border = if self.focus == Focus::Logs {
            self.theme.accent
        } else {
            self.theme.muted
        };
        frame.render_widget(
            Paragraph::new(visible)
                .scroll((
                    0,
                    if self.wrap_logs {
                        0
                    } else {
                        self.horizontal_scroll
                    },
                ))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(border))
                        .title(title),
                ),
            area,
        );
        let (position, viewport) = self
            .visible_log_range
            .map(|(first, last)| (first, last.saturating_sub(first).saturating_add(1)))
            .unwrap_or_default();
        render_vertical_scrollbar(frame, area, logs.len(), position, viewport, &self.theme);
    }

    fn draw_footer(&self, frame: &mut Frame<'_>, area: Rect) {
        let key_style = Style::default()
            .fg(self.theme.accent)
            .add_modifier(Modifier::BOLD);
        let text_style = Style::default().fg(self.theme.footer);
        let block = Block::default().borders(Borders::TOP);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0)])
            .split(inner);

        match &self.mode {
            InputMode::Normal => {
                let status = Line::from(Span::styled(
                    format!(
                        "{}{}",
                        self.message,
                        self.clipboard_countdown()
                            .map(|countdown| format!(" • {countdown}"))
                            .unwrap_or_default()
                    ),
                    text_style,
                ));
                frame.render_widget(Paragraph::new(status), rows[0]);

                let mut spans = Vec::new();
                for (key, description) in footer_shortcuts(rows[1].width, rows[1].height) {
                    spans.push(Span::styled(key, key_style));
                    spans.push(Span::styled(description, text_style));
                }
                frame.render_widget(
                    Paragraph::new(Line::from(spans)).wrap(Wrap { trim: false }),
                    rows[1],
                );
            }
            InputMode::Ephemeral(buffer) => {
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(
                            format!("Run ephemeral: {buffer}█  "),
                            Style::default().fg(self.theme.search),
                        ),
                        Span::styled("Enter", key_style),
                        Span::styled(" run  ", text_style),
                        Span::styled("Esc", key_style),
                        Span::styled(" cancel", text_style),
                    ])),
                    inner,
                );
            }
            InputMode::Search { buffer, kind } => {
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(
                            format!(
                                "{}: {buffer}█  ",
                                match kind {
                                    SearchKind::Exact => "Exact substring",
                                    SearchKind::InsensitiveRegex => "Case-insensitive regex",
                                }
                            ),
                            Style::default().fg(self.theme.search),
                        ),
                        Span::styled("Enter", key_style),
                        Span::styled(" apply  ", text_style),
                        Span::styled("Esc", key_style),
                        Span::styled(" cancel", text_style),
                    ])),
                    inner,
                );
            }
            InputMode::Dump(buffer) => {
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(
                            format!("Dump path: {buffer}█  "),
                            Style::default().fg(self.theme.search),
                        ),
                        Span::styled("Enter", key_style),
                        Span::styled(" write  ", text_style),
                        Span::styled("Esc", key_style),
                        Span::styled(" cancel", text_style),
                    ])),
                    inner,
                );
            }
        }
    }

    fn draw_help(&self, frame: &mut Frame<'_>, area: Rect) {
        let popup = centered_rect(
            72.min(area.width.saturating_sub(2)),
            24.min(area.height.saturating_sub(2)),
            area,
        );
        frame.render_widget(Clear, popup);
        let help = vec![
            help_line("Tab", "switch command/output focus"),
            help_line("↑/↓ or j/k", "select group, command, or output line"),
            help_line("PgUp/PgDn, g/G", "page output; first/last line"),
            help_line("→ / ←", "focus panes; scroll when wrapping is off"),
            help_line("w", "toggle output wrapping (on by default)"),
            help_line(
                "h/l (output focus)",
                "horizontal scroll when wrapping is off",
            ),
            help_line("Enter or s", "start selected command/group"),
            help_line("f", "force-start commands waiting for dependencies"),
            help_line("x", "stop selected command/group; repeat to force"),
            help_line("r", "restart selected command/group; repeat to force"),
            help_line("/ then n/N", "exact substring; next/previous"),
            help_line("\\ then n/N", "case-insensitive regex; next/previous"),
            help_line("t", "toggle timestamps (on by default)"),
            help_line("y / Y", "copy selected line / all output with xclip"),
            help_line("c", "clear selected output pane (logfile is kept)"),
            help_line("d", "dump all output to a new file"),
            help_line("T", "preview and apply a project theme"),
            help_line(":", "run a new ephemeral command"),
            help_line("p / D", "persist / remove selected ephemeral command"),
            help_line("M", "manage commands and nested actions"),
            help_line("Ctrl-P", "quick jump to a command"),
            help_line("q or Ctrl-C", "graceful quit; repeat to force"),
        ];
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(self.theme.accent))
            .title(format!(" Blade v{} keys ", env!("CARGO_PKG_VERSION")))
            .title_bottom(
                Line::from(" Press any key to close help ")
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(self.theme.accent)),
            );
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0)])
            .split(inner);
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(rows[1]);
        frame.render_widget(Paragraph::new(help), columns[1]);
    }

    fn draw_persist_dialog(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(dialog) = &self.persist_dialog else {
            return;
        };
        let targets = self.project_targets();
        let project = targets
            .get(dialog.target)
            .map(|target| target.label.as_str())
            .unwrap_or("unknown");
        let popup = centered_rect(
            72.min(area.width.saturating_sub(2)),
            11.min(area.height.saturating_sub(2)),
            area,
        );
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(self.theme.accent))
            .title(" Add ephemeral command to project ")
            .title_bottom(
                Line::from(" Tab • ←/→ cursor/choice • Alt-←/→ groups • Enter save • Esc ")
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(self.theme.accent)),
            );
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let field_style = |field| {
            if dialog.field == field {
                Style::default()
                    .fg(self.theme.accent_text)
                    .bg(self.theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(self.theme.text)
            }
        };
        let width = inner.width as usize;
        let mut rows = Vec::new();
        push_form_field(
            &mut rows,
            format!(
                " Name [type]: {}",
                form_text_value(
                    &dialog.name,
                    dialog.field == PersistField::Name,
                    dialog.cursor,
                    "command name"
                )
            ),
            field_style(PersistField::Name),
            width,
        );
        push_form_field(
            &mut rows,
            format!(
                " Project [←/→]: {}",
                form_choice_value(project, dialog.field == PersistField::Project)
            ),
            field_style(PersistField::Project),
            width,
        );
        push_form_field(
            &mut rows,
            format!(
                " Group [type/Alt-←→]: {}",
                form_text_value(
                    &dialog.group,
                    dialog.field == PersistField::Group,
                    dialog.cursor,
                    "group name"
                )
            ),
            field_style(PersistField::Group),
            width,
        );
        rows.pop();
        let scroll = form_scroll(&rows, inner.height as usize);
        let content_length = rows.len();
        frame.render_widget(Paragraph::new(rows).scroll((scroll, 0)), inner);
        render_vertical_scrollbar(
            frame,
            popup,
            content_length,
            scroll as usize,
            inner.height as usize,
            &self.theme,
        );
    }

    fn draw_manage(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(manage) = &self.manage else {
            return;
        };
        let popup = centered_rect(
            area.width.saturating_sub(6).min(100),
            area.height.saturating_sub(4).min(32),
            area,
        );
        frame.render_widget(Clear, popup);
        if let Some(dialog) = &manage.edit {
            self.draw_manage_edit(frame, popup, dialog);
            if let Some(error) = &manage.error {
                self.draw_manage_error(frame, popup, error);
            }
            return;
        }
        if let Some(dialog) = &manage.action_edit {
            self.draw_manage_action_edit(frame, popup, dialog);
            if let Some(error) = &manage.error {
                self.draw_manage_error(frame, popup, error);
            }
            return;
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(self.theme.accent))
            .title(" Manage configured commands and actions ")
            .title_bottom(
                Line::from(" c add command • a add action • e edit • m move • K/J reorder • d delete • Esc ")
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(self.theme.accent)),
            );
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let row_width = inner.width as usize;
        let managed_items = self.managed_items();
        if managed_items.is_empty() {
            frame.render_widget(
                Paragraph::new("No configured commands. Press c or a to add one.")
                    .style(Style::default().fg(self.theme.muted)),
                inner,
            );
            return;
        }
        let mut items = Vec::new();
        let mut selected_row = 0;
        let mut managed_index = 0;
        let mut previous_project = None::<String>;
        for group in &self.groups {
            let project = group
                .project
                .as_deref()
                .unwrap_or(&self.runner.project().name);
            if previous_project.as_deref() != Some(project) {
                if !items.is_empty() {
                    items.push(ListItem::new(Line::from("")));
                }
                items.push(ListItem::new(Line::from(Span::styled(
                    pad_row(format!(" ▾ {project}"), row_width),
                    Style::default()
                        .fg(self.theme.accent)
                        .add_modifier(Modifier::BOLD),
                ))));
                previous_project = Some(project.to_owned());
            }
            items.push(ListItem::new(Line::from(Span::styled(
                pad_row(format!("   ▾ {}", group.name), row_width),
                Style::default()
                    .fg(self.theme.completed)
                    .add_modifier(Modifier::BOLD),
            ))));
            for command in &group.commands {
                let state = self
                    .runner
                    .snapshot(&command.id)
                    .map(|snapshot| snapshot.state)
                    .unwrap_or(CommandState::Stopped);
                let color = state_color(state, &self.theme);
                let text = pad_row(
                    format!(
                        "     {} {}  —  {}",
                        state_marker(state),
                        command.name,
                        command.run.split_whitespace().collect::<Vec<_>>().join(" ")
                    ),
                    row_width,
                );
                let style = if managed_index == manage.selected {
                    selected_row = items.len();
                    Style::default()
                        .fg(contrasting_text_color(color))
                        .bg(color)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(color)
                };
                items.push(ListItem::new(Line::from(Span::styled(text, style))));
                managed_index += 1;
                for action in &command.actions {
                    let state = self
                        .runner
                        .snapshot(&action.id)
                        .map(|snapshot| snapshot.state)
                        .unwrap_or(CommandState::Stopped);
                    let color = state_color(state, &self.theme);
                    let text = pad_row(
                        format!(
                            "       ↳ {} {}  —  {}",
                            state_marker(state),
                            action.name,
                            action.run.split_whitespace().collect::<Vec<_>>().join(" ")
                        ),
                        row_width,
                    );
                    let style = if managed_index == manage.selected {
                        selected_row = items.len();
                        Style::default()
                            .fg(contrasting_text_color(color))
                            .bg(color)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(color)
                    };
                    items.push(ListItem::new(Line::from(Span::styled(text, style))));
                    managed_index += 1;
                }
            }
        }
        let item_count = items.len();
        let mut state = ListState::default().with_selected(Some(selected_row));
        frame.render_stateful_widget(List::new(items), inner, &mut state);
        render_vertical_scrollbar(
            frame,
            popup,
            item_count,
            state.offset(),
            inner.height as usize,
            &self.theme,
        );
        if manage.confirm_delete {
            self.draw_manage_delete_confirmation(frame, popup, manage.selected);
        }
    }

    fn draw_manage_delete_confirmation(&self, frame: &mut Frame<'_>, area: Rect, selected: usize) {
        let Some(item) = self.managed_items().get(selected).copied() else {
            return;
        };
        let (group_index, command_index) = match item {
            ManageItem::Command { group, command } | ManageItem::Action { group, command, .. } => {
                (group, command)
            }
        };
        let group = &self.groups[group_index];
        let command = &group.commands[command_index];
        let project = group
            .project
            .as_deref()
            .unwrap_or(&self.runner.project().name);
        let popup = centered_rect(
            area.width.saturating_sub(8).min(72),
            7.min(area.height.saturating_sub(4)),
            area,
        );
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(self.theme.failed))
            .title(" Delete configured item? ")
            .title_bottom(
                Line::from(" y/Enter delete • n/Esc cancel ")
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(self.theme.failed)),
            );
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let prompt = match item {
            ManageItem::Command { .. } if command.actions.is_empty() => {
                format!("Delete {:?} from {project} / {}?", command.name, group.name)
            }
            ManageItem::Command { .. } => format!(
                "Delete {:?} and its {} action(s) from {project} / {}?",
                command.name,
                command.actions.len(),
                group.name
            ),
            ManageItem::Action { action, .. } => format!(
                "Delete action {:?} from {:?}?",
                command.actions[action].name, command.name
            ),
        };
        frame.render_widget(
            Paragraph::new(prompt)
                .style(Style::default().fg(self.theme.failed))
                .wrap(Wrap { trim: false })
                .alignment(Alignment::Center),
            inner,
        );
    }

    fn draw_manage_edit(&self, frame: &mut Frame<'_>, popup: Rect, dialog: &ManageEditDialog) {
        let targets = self.project_targets();
        let project = targets
            .get(dialog.target)
            .map(|target| target.label.as_str())
            .unwrap_or("unknown");
        let title = match dialog.mode {
            ManageEditMode::Add => " Add configured command ",
            ManageEditMode::Edit => " Edit configured command ",
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(self.theme.accent))
            .title(title)
            .title_bottom(
                Line::from(" Tab fields • ←/→ cursor/choice • Alt-←/→ groups • Enter apply • Esc ")
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(self.theme.accent)),
            );
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let style = |field| {
            if dialog.field == field {
                Style::default()
                    .fg(self.theme.accent_text)
                    .bg(self.theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(self.theme.text)
            }
        };
        let width = inner.width as usize;
        let mut lines = Vec::new();
        push_form_field(
            &mut lines,
            format!(
                " Name [type]: {}",
                form_text_value(
                    &dialog.name,
                    dialog.field == ManageField::Name,
                    dialog.cursor,
                    "command name"
                )
            ),
            style(ManageField::Name),
            width,
        );
        push_form_field(
            &mut lines,
            format!(
                " Run [type]: {}",
                form_text_value(
                    &dialog.run,
                    dialog.field == ManageField::Run,
                    dialog.cursor,
                    "shell command"
                )
            ),
            style(ManageField::Run),
            width,
        );
        push_form_field(
            &mut lines,
            format!(
                " Cwd [type]: {}",
                form_text_value(
                    &dialog.cwd,
                    dialog.field == ManageField::Cwd,
                    dialog.cursor,
                    "working directory"
                )
            ),
            style(ManageField::Cwd),
            width,
        );
        push_form_field(
            &mut lines,
            format!(
                " Project [←/→]: {}",
                form_choice_value(project, dialog.field == ManageField::Project)
            ),
            style(ManageField::Project),
            width,
        );
        push_form_field(
            &mut lines,
            format!(
                " Group [type/Alt-←→]: {}",
                form_text_value(
                    &dialog.group,
                    dialog.field == ManageField::Group,
                    dialog.cursor,
                    "group name"
                )
            ),
            style(ManageField::Group),
            width,
        );
        lines.pop();
        let scroll = form_scroll(&lines, inner.height as usize);
        let content_length = lines.len();
        frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);
        render_vertical_scrollbar(
            frame,
            popup,
            content_length,
            scroll as usize,
            inner.height as usize,
            &self.theme,
        );
    }

    fn draw_manage_action_edit(
        &self,
        frame: &mut Frame<'_>,
        popup: Rect,
        dialog: &ManageActionEditDialog,
    ) {
        let parent = self
            .managed_commands()
            .get(dialog.parent)
            .map(|(group, command)| {
                let group = &self.groups[*group];
                let command = &group.commands[*command];
                format!(
                    "{}{} / {}",
                    group
                        .project
                        .as_deref()
                        .map(|project| format!("{project} / "))
                        .unwrap_or_default(),
                    group.name,
                    command.name
                )
            })
            .unwrap_or_else(|| "unknown".to_owned());
        let title = match dialog.mode {
            ManageEditMode::Add => " Add command action ",
            ManageEditMode::Edit => " Edit command action ",
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(self.theme.accent))
            .title(title)
            .title_bottom(
                Line::from(" Tab fields • ←/→ cursor/choice • Space toggle • Enter apply • Esc ")
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(self.theme.accent)),
            );
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let style = |field| {
            if dialog.field == field {
                Style::default()
                    .fg(self.theme.accent_text)
                    .bg(self.theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(self.theme.text)
            }
        };
        let width = inner.width as usize;
        let mut lines = Vec::new();
        push_form_field(
            &mut lines,
            format!(
                " Name [type]: {}",
                form_text_value(
                    &dialog.name,
                    dialog.field == ManageActionField::Name,
                    dialog.cursor,
                    "action name"
                )
            ),
            style(ManageActionField::Name),
            width,
        );
        push_form_field(
            &mut lines,
            format!(
                " Run [type]: {}",
                form_text_value(
                    &dialog.run,
                    dialog.field == ManageActionField::Run,
                    dialog.cursor,
                    "shell command"
                )
            ),
            style(ManageActionField::Run),
            width,
        );
        push_form_field(
            &mut lines,
            format!(
                " Cwd [type]: {}",
                form_text_value(
                    &dialog.cwd,
                    dialog.field == ManageActionField::Cwd,
                    dialog.cursor,
                    "inherit parent"
                )
            ),
            style(ManageActionField::Cwd),
            width,
        );
        push_form_field(
            &mut lines,
            format!(
                " Parent [←/→]: {}",
                form_choice_value(&parent, dialog.field == ManageActionField::Parent)
            ),
            style(ManageActionField::Parent),
            width,
        );
        push_form_field(
            &mut lines,
            format!(
                " Requires stopped [←/→/Space]: {}",
                form_choice_value(
                    if dialog.requires_stopped { "Yes" } else { "No" },
                    dialog.field == ManageActionField::RequiresStopped
                )
            ),
            style(ManageActionField::RequiresStopped),
            width,
        );
        push_form_field(
            &mut lines,
            format!(
                " Restart afterward [←/→]: {}",
                form_choice_value(
                    restart_after_label(dialog.restart_after),
                    dialog.field == ManageActionField::RestartAfter
                )
            ),
            style(ManageActionField::RestartAfter),
            width,
        );
        lines.pop();
        let scroll = form_scroll(&lines, inner.height as usize);
        let content_length = lines.len();
        frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);
        render_vertical_scrollbar(
            frame,
            popup,
            content_length,
            scroll as usize,
            inner.height as usize,
            &self.theme,
        );
    }

    fn draw_manage_error(&self, frame: &mut Frame<'_>, area: Rect, error: &str) {
        let popup = centered_rect(
            area.width.saturating_sub(8).min(72),
            7.min(area.height.saturating_sub(4)),
            area,
        );
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(self.theme.failed))
            .title(" Cannot apply changes ")
            .title_bottom(
                Line::from(" Enter/Esc return to the form ")
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(self.theme.failed)),
            );
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        frame.render_widget(
            Paragraph::new(error)
                .style(Style::default().fg(self.theme.failed))
                .wrap(Wrap { trim: false })
                .alignment(Alignment::Center),
            inner,
        );
    }

    fn draw_theme_picker(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(picker) = &self.theme_picker else {
            return;
        };
        let catalog = &self.runner.project().theme_catalog;
        let has_custom = catalog.has_custom();
        let display_count = catalog.len() + usize::from(has_custom);
        let popup = centered_rect(
            78.min(area.width.saturating_sub(2)),
            (display_count as u16 + 8).min(area.height.saturating_sub(2)),
            area,
        );
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(self.theme.accent))
            .title(format!(
                " Theme preview: {} ",
                catalog.name(picker.selected).unwrap_or("default")
            ));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(display_count as u16),
                Constraint::Min(0),
                Constraint::Length(2),
            ])
            .split(inner);
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(rows[1]);
        let row_width = columns[1].width as usize;
        let selected_style = Style::default()
            .fg(self.theme.accent_text)
            .bg(self.theme.accent)
            .add_modifier(Modifier::BOLD);
        let preset_column_width = catalog
            .names()
            .map(UnicodeWidthStr::width)
            .max()
            .unwrap_or_default()
            + 2;
        let mut items = Vec::with_capacity(display_count);
        for (index, preset) in catalog.names().enumerate() {
            if has_custom && index == PRESETS.len() {
                items.push(ListItem::new(Line::from(Span::styled(
                    theme_separator_row("Custom themes", row_width),
                    Style::default().fg(self.theme.muted),
                ))));
            }
            let saved = if preset == self.theme_preset {
                "●"
            } else {
                " "
            };
            let description = catalog.description(preset).unwrap_or_else(|| {
                let built_in = theme_description(preset);
                if built_in.is_empty() {
                    "Custom global theme"
                } else {
                    built_in
                }
            });
            let text = theme_picker_row(saved, preset, description, preset_column_width, row_width);
            let style = if index == picker.selected {
                selected_style
            } else {
                Style::default().fg(self.theme.text)
            };
            items.push(ListItem::new(Line::from(Span::styled(text, style))));
        }
        let selected_row =
            picker.selected + usize::from(has_custom && picker.selected >= PRESETS.len());
        let mut state = ListState::default().with_selected(Some(selected_row));
        frame.render_stateful_widget(List::new(items), columns[1], &mut state);

        let note = if self.theme_overrides == ThemeOverrides::default() {
            "Preview updates the entire interface"
        } else {
            "Configured color overrides remain active"
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    if self.runner.project().theme_file.is_some() {
                        "↑/↓ preview • Enter save theme • Esc cancel"
                    } else {
                        "↑/↓ preview • Enter apply for session • Esc cancel"
                    },
                    Style::default().fg(self.theme.accent),
                )),
                Line::from(Span::styled(note, Style::default().fg(self.theme.muted))),
            ])
            .alignment(Alignment::Center),
            rows[3],
        );
    }

    fn draw_command_picker(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(picker) = &self.command_picker else {
            return;
        };
        let matches = self.command_matches(&picker.query);
        let popup = centered_rect(
            70.min(area.width.saturating_sub(2)),
            18.min(area.height.saturating_sub(2)),
            area,
        );
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(self.theme.accent))
            .title(format!(" Quick jump • {} match(es) ", matches.len()))
            .title_bottom(
                Line::from(" ↑/↓ select • Enter focus • Esc/Ctrl-P close ")
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(self.theme.accent)),
            );
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(2), Constraint::Min(0)])
            .split(inner);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" › ", Style::default().fg(self.theme.accent)),
                Span::styled(picker.query.clone(), Style::default().fg(self.theme.search)),
                Span::styled("█", Style::default().fg(self.theme.search)),
            ]))
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(self.theme.muted)),
            ),
            rows[0],
        );
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(rows[1]);
        if matches.is_empty() {
            frame.render_widget(
                Paragraph::new("No commands match this query")
                    .style(Style::default().fg(self.theme.muted)),
                columns[1],
            );
            return;
        }

        let row_width = columns[1].width as usize;
        let items = matches
            .iter()
            .enumerate()
            .map(|(index, hit)| {
                let command_id = command_id_for_selection(
                    &self.command_items[hit.item_index],
                    &self.groups,
                    &self.ephemeral,
                )
                .unwrap_or_default();
                let state = self
                    .runner
                    .snapshot(command_id)
                    .map(|snapshot| snapshot.state)
                    .unwrap_or(CommandState::Stopped);
                let color = state_color(state, &self.theme);
                let label = self
                    .command_picker_label(hit.item_index)
                    .unwrap_or_else(|| "command".to_owned());
                let text = pad_row(format!(" {} {label}", state_marker(state)), row_width);
                let style = if index == picker.selected {
                    Style::default()
                        .fg(contrasting_text_color(color))
                        .bg(color)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(color)
                };
                ListItem::new(Line::from(Span::styled(text, style)))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(Some(picker.selected));
        frame.render_stateful_widget(List::new(items), columns[1], &mut state);
    }
}

fn theme_description(preset: &str) -> &'static str {
    match preset {
        "default" => "Blade's original palette",
        "red" => "Bold crimson accent colors",
        "yellow" => "Bright golden accent colors",
        "orange" => "Warm amber accent colors",
        "matrix" => "Phosphor green terminal colors",
        "matrix-alt" => "Matrix colors with white output",
        "purple" => "Rich violet accent colors",
        "blue" => "Clear ocean blue accent colors",
        "gray" => "Cool neutral gray colors",
        "sand" => "Soft warm desert colors",
        "nord" => "Cool, muted arctic colors",
        "gruvbox" => "Warm, earthy colors",
        "dracula" => "Vivid purple and neon colors",
        "catppuccin" => "Soft Mocha pastel colors",
        "tokyo-night" => "Deep blue nighttime colors",
        "solarized-dark" => "Balanced low-contrast colors",
        "monochrome" => "Minimal color-independent states",
        _ => "",
    }
}

fn ephemeral_label(command: &str) -> String {
    const LIMIT: usize = 48;
    let flattened = command.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.chars().count() <= LIMIT {
        flattened
    } else {
        format!("{}…", flattened.chars().take(LIMIT - 1).collect::<String>())
    }
}

fn unique_persisted_name(command: &str, existing: &[&str]) -> String {
    let executable = command
        .split_whitespace()
        .next()
        .and_then(|word| Path::new(word).file_name())
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("command");
    if !existing.contains(&executable) {
        return executable.to_owned();
    }
    (2..)
        .map(|suffix| format!("{executable}-{suffix}"))
        .find(|name| !existing.contains(&name.as_str()))
        .expect("an unused generated command name must exist")
}

fn next_manage_field(field: ManageField, backwards: bool) -> ManageField {
    match (field, backwards) {
        (ManageField::Name, false) => ManageField::Run,
        (ManageField::Run, false) => ManageField::Cwd,
        (ManageField::Cwd, false) => ManageField::Project,
        (ManageField::Project, false) => ManageField::Group,
        (ManageField::Group, false) => ManageField::Name,
        (ManageField::Name, true) => ManageField::Group,
        (ManageField::Run, true) => ManageField::Name,
        (ManageField::Cwd, true) => ManageField::Run,
        (ManageField::Project, true) => ManageField::Cwd,
        (ManageField::Group, true) => ManageField::Project,
    }
}

fn next_manage_action_field(field: ManageActionField, backwards: bool) -> ManageActionField {
    match (field, backwards) {
        (ManageActionField::Name, false) => ManageActionField::Run,
        (ManageActionField::Run, false) => ManageActionField::Cwd,
        (ManageActionField::Cwd, false) => ManageActionField::Parent,
        (ManageActionField::Parent, false) => ManageActionField::RequiresStopped,
        (ManageActionField::RequiresStopped, false) => ManageActionField::RestartAfter,
        (ManageActionField::RestartAfter, false) => ManageActionField::Name,
        (ManageActionField::Name, true) => ManageActionField::RestartAfter,
        (ManageActionField::Run, true) => ManageActionField::Name,
        (ManageActionField::Cwd, true) => ManageActionField::Run,
        (ManageActionField::Parent, true) => ManageActionField::Cwd,
        (ManageActionField::RequiresStopped, true) => ManageActionField::Parent,
        (ManageActionField::RestartAfter, true) => ManageActionField::RequiresStopped,
    }
}

fn manage_action_field_is_text(field: ManageActionField) -> bool {
    matches!(
        field,
        ManageActionField::Name | ManageActionField::Run | ManageActionField::Cwd
    )
}

fn manage_action_field_text_len(dialog: &ManageActionEditDialog) -> usize {
    match dialog.field {
        ManageActionField::Name => dialog.name.chars().count(),
        ManageActionField::Run => dialog.run.chars().count(),
        ManageActionField::Cwd => dialog.cwd.chars().count(),
        _ => 0,
    }
}

fn cycle_restart_after(value: RestartAfter, backwards: bool) -> RestartAfter {
    match (value, backwards) {
        (RestartAfter::Never, false) => RestartAfter::IfRunning,
        (RestartAfter::IfRunning, false) => RestartAfter::Always,
        (RestartAfter::Always, false) => RestartAfter::Never,
        (RestartAfter::Never, true) => RestartAfter::Always,
        (RestartAfter::IfRunning, true) => RestartAfter::Never,
        (RestartAfter::Always, true) => RestartAfter::IfRunning,
    }
}

fn restart_after_label(value: RestartAfter) -> &'static str {
    match value {
        RestartAfter::Never => "Never",
        RestartAfter::IfRunning => "If previously running",
        RestartAfter::Always => "Always",
    }
}

fn first_missing_manage_field(dialog: &ManageEditDialog) -> Option<ManageField> {
    if dialog.name.trim().is_empty() {
        Some(ManageField::Name)
    } else if dialog.run.trim().is_empty() {
        Some(ManageField::Run)
    } else if dialog.group.trim().is_empty() {
        Some(ManageField::Group)
    } else {
        None
    }
}

fn manage_field_is_text(field: ManageField) -> bool {
    !matches!(field, ManageField::Project)
}

fn manage_field_text(dialog: &ManageEditDialog) -> Option<&str> {
    match dialog.field {
        ManageField::Name => Some(&dialog.name),
        ManageField::Run => Some(&dialog.run),
        ManageField::Cwd => Some(&dialog.cwd),
        ManageField::Group => Some(&dialog.group),
        ManageField::Project => None,
    }
}

fn manage_field_text_len(dialog: &ManageEditDialog) -> usize {
    manage_field_text(dialog)
        .map(|value| value.chars().count())
        .unwrap_or_default()
}

fn persist_field_text_len(dialog: &PersistDialog) -> usize {
    match dialog.field {
        PersistField::Name => dialog.name.chars().count(),
        PersistField::Group => dialog.group.chars().count(),
        PersistField::Project => 0,
    }
}

fn char_cursor_byte(value: &str, cursor: usize) -> usize {
    value
        .char_indices()
        .nth(cursor)
        .map(|(byte, _)| byte)
        .unwrap_or(value.len())
}

fn move_text_cursor(cursor: &mut usize, value: &str, right: bool) {
    move_text_cursor_with_len(cursor, value.chars().count(), right);
}

fn move_text_cursor_with_len(cursor: &mut usize, length: usize, right: bool) {
    *cursor = if right {
        cursor.saturating_add(1).min(length)
    } else {
        cursor.saturating_sub(1)
    };
}

fn insert_at_cursor(value: &mut String, cursor: &mut usize, character: char) {
    let byte = char_cursor_byte(value, *cursor);
    value.insert(byte, character);
    *cursor += 1;
}

fn remove_before_cursor(value: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let start = char_cursor_byte(value, *cursor - 1);
    let end = char_cursor_byte(value, *cursor);
    value.replace_range(start..end, "");
    *cursor -= 1;
}

fn remove_at_cursor(value: &mut String, cursor: usize) {
    let start = char_cursor_byte(value, cursor);
    let end = char_cursor_byte(value, cursor.saturating_add(1));
    if start < end {
        value.replace_range(start..end, "");
    }
}

fn cycle_named_value(current: &mut String, values: &[String], backwards: bool) {
    if values.is_empty() {
        return;
    }
    let index = values.iter().position(|value| value == current);
    let next = match (index, backwards) {
        (Some(0), true) | (None, true) => values.len() - 1,
        (Some(index), true) => index - 1,
        (Some(index), false) => (index + 1) % values.len(),
        (None, false) => 0,
    };
    current.clone_from(&values[next]);
}

fn form_text_value(value: &str, active: bool, cursor: usize, placeholder: &str) -> String {
    if value.is_empty() {
        let placeholder = format!("<{placeholder}>");
        return if active {
            format!("{placeholder}█")
        } else {
            placeholder
        };
    }
    if !active {
        return value.to_owned();
    }
    let byte = char_cursor_byte(value, cursor);
    format!("{}█{}", &value[..byte], &value[byte..])
}

fn form_choice_value(value: &str, active: bool) -> String {
    if active {
        format!("‹ {value} ›")
    } else {
        value.to_owned()
    }
}

fn push_form_field(lines: &mut Vec<Line<'static>>, text: String, style: Style, width: usize) {
    lines.extend(
        wrap_log_text(&text, width)
            .into_iter()
            .map(|row| Line::from(Span::styled(pad_row(row, width), style))),
    );
    lines.push(Line::from(""));
}

fn form_scroll(lines: &[Line<'_>], height: usize) -> u16 {
    let focus = lines
        .iter()
        .position(|line| {
            line.spans
                .iter()
                .any(|span| span.content.contains('█') || span.content.contains('‹'))
        })
        .unwrap_or_default();
    focus
        .saturating_sub(height.saturating_sub(1))
        .min(u16::MAX as usize) as u16
}

fn render_vertical_scrollbar(
    frame: &mut Frame<'_>,
    area: Rect,
    total_content_length: usize,
    first_visible: usize,
    viewport_length: usize,
    theme: &Theme,
) {
    if viewport_length == 0 || total_content_length <= viewport_length || area.height < 4 {
        return;
    }
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .thumb_symbol("┃")
        .thumb_style(Style::default().fg(theme.accent))
        .track_symbol(Some("│"))
        .track_style(Style::default().fg(theme.muted))
        .begin_symbol(Some("▲"))
        .begin_style(Style::default().fg(theme.accent))
        .end_symbol(Some("▼"))
        .end_style(Style::default().fg(theme.accent));
    // Ratatui's content length is the number of possible viewport positions,
    // not the total number of content rows. Supplying the latter makes the
    // thumb stop short of the bottom by roughly one viewport height.
    let max_position = total_content_length.saturating_sub(viewport_length);
    let mut state = ScrollbarState::new(max_position.saturating_add(1))
        .position(first_visible.min(max_position))
        .viewport_content_length(viewport_length);
    frame.render_stateful_widget(
        scrollbar,
        area.inner(Margin {
            vertical: 1,
            horizontal: 0,
        }),
        &mut state,
    );
}

fn load_configured_command(
    path: &Path,
    name: &str,
    catalog: &crate::theme::ThemeCatalog,
) -> Result<CommandConfig> {
    let report = validate_file_for_combined_with_catalog(path, catalog);
    if !report.is_valid() {
        bail!(
            "{} became invalid: {}",
            path.display(),
            report
                .issues
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    report
        .project
        .and_then(|project| {
            project
                .commands()
                .find(|command| command.name == name)
                .cloned()
        })
        .with_context(|| format!("saved command {name:?} was not found in {}", path.display()))
}

fn theme_picker_row(
    saved: &str,
    preset: &str,
    description: &str,
    preset_column_width: usize,
    width: usize,
) -> String {
    let padding = preset_column_width.saturating_sub(UnicodeWidthStr::width(preset));
    pad_row(
        format!("{saved} {preset}{}{description}", " ".repeat(padding)),
        width,
    )
}

fn theme_separator_row(label: &str, width: usize) -> String {
    let prefix = format!("── {label} ");
    let remaining = width.saturating_sub(UnicodeWidthStr::width(prefix.as_str()));
    format!("{prefix}{}", "─".repeat(remaining))
}

fn help_line(key: &str, description: &str) -> Line<'static> {
    const KEY_COLUMN_WIDTH: usize = 22;
    let padding = KEY_COLUMN_WIDTH.saturating_sub(UnicodeWidthStr::width(key));
    Line::from(vec![
        Span::raw(key.to_owned()),
        Span::raw(" ".repeat(padding)),
        Span::raw(description.to_owned()),
    ])
}

fn footer_shortcuts(width: u16, rows: u16) -> Vec<(&'static str, &'static str)> {
    const PRIMARY: [(&str, &str); 12] = [
        ("Ctrl-P", " jump  "),
        ("Enter", " start  "),
        ("f", " force  "),
        ("x", " stop  "),
        ("r", " restart  "),
        ("/", " exact  "),
        ("\\", " regex  "),
        ("T", " theme  "),
        ("t", " timestamps  "),
        ("y/Y", " copy  "),
        ("d", " dump  "),
        ("q", " quit  "),
    ];
    const SECONDARY: [(&str, &str); 11] = [
        ("c", " clear  "),
        (":", " ephemeral  "),
        ("p/D", " save/remove  "),
        ("M", " manage  "),
        ("Tab", " focus  "),
        ("w", " wrap  "),
        ("PgUp/PgDn", " page  "),
        ("g/G", " ends  "),
        ("n/N", " matches  "),
        ("h/l", " h-scroll  "),
        ("←/→", " panes  "),
    ];
    const HELP: (&str, &str) = ("?", " help");

    let description = |full: &'static str| if width >= 60 { full } else { "  " };
    let item_width = |(key, full): (&str, &'static str)| {
        UnicodeWidthStr::width(key) + UnicodeWidthStr::width(description(full))
    };
    let capacity = usize::from(width).saturating_mul(usize::from(rows));
    let mut used = PRIMARY.iter().copied().map(item_width).sum::<usize>() + item_width(HELP);
    let mut selected = PRIMARY
        .iter()
        .map(|&(key, full)| (key, description(full)))
        .collect::<Vec<_>>();

    for shortcut in SECONDARY {
        let shortcut_width = item_width(shortcut);
        if used.saturating_add(shortcut_width) <= capacity {
            selected.push((shortcut.0, description(shortcut.1)));
            used += shortcut_width;
        }
    }
    selected.push((HELP.0, description(HELP.1)));
    selected
}

fn pad_row(mut text: String, width: usize) -> String {
    let padding = width.saturating_sub(UnicodeWidthStr::width(text.as_str()));
    text.push_str(&" ".repeat(padding));
    text
}

fn fuzzy_score(candidate: &str, query: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let candidate = candidate.to_lowercase();
    let query = query.to_lowercase();
    if let Some(position) = candidate.find(&query) {
        return Some(10_000 - position as i64 * 10 - candidate.len() as i64);
    }

    let candidate_chars = candidate.chars().collect::<Vec<_>>();
    let mut next_index = 0;
    let mut previous_match = None;
    let mut score = 0_i64;
    for query_character in query.chars() {
        let relative = candidate_chars[next_index..]
            .iter()
            .position(|candidate_character| *candidate_character == query_character)?;
        let index = next_index + relative;
        score += 100 - index as i64;
        if index == 0 || !candidate_chars[index - 1].is_alphanumeric() {
            score += 40;
        }
        if previous_match.is_some_and(|previous| previous + 1 == index) {
            score += 75;
        }
        previous_match = Some(index);
        next_index = index + 1;
    }
    Some(score - candidate_chars.len() as i64)
}

fn logs_as_text(logs: &[LogLine], timestamps: bool) -> String {
    let mut text = logs
        .iter()
        .map(|line| line.display(timestamps))
        .collect::<Vec<_>>()
        .join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    text
}

fn styled_log_line(
    line: &LogLine,
    text: String,
    timestamps: bool,
    style: Style,
    muted: Color,
) -> Line<'static> {
    if timestamps {
        let timestamp = line.timestamp.format("%H:%M:%S%.3f").to_string();
        let prefix = format!("{timestamp} ");
        if let Some(content) = text.strip_prefix(&prefix) {
            return Line::from(vec![
                Span::styled(timestamp, style.fg(muted)),
                Span::raw(" "),
                Span::styled(content.to_owned(), style),
            ]);
        }
    }
    Line::from(Span::styled(text, style))
}

fn search_needs_top_alignment(
    wrap_logs: bool,
    visible_range: Option<(usize, usize)>,
    index: usize,
) -> bool {
    wrap_logs && visible_range.is_some_and(|(first, _)| index < first)
}

fn build_search_regex(query: &str, kind: SearchKind) -> Result<Regex, regex::Error> {
    match kind {
        SearchKind::Exact => RegexBuilder::new(&regex::escape(query)).build(),
        SearchKind::InsensitiveRegex => RegexBuilder::new(query).case_insensitive(true).build(),
    }
}

fn search_expression(query: &str, kind: SearchKind) -> String {
    match kind {
        SearchKind::Exact => format!("/{query}/"),
        SearchKind::InsensitiveRegex => format!("\\{query}\\"),
    }
}

fn wrapped_log_rows(
    logs: &[LogLine],
    end: usize,
    height: usize,
    width: usize,
    timestamps: bool,
) -> Vec<(usize, String)> {
    if logs.is_empty() || height == 0 {
        return Vec::new();
    }

    let mut rows = Vec::with_capacity(height);
    for index in (0..=end.min(logs.len() - 1)).rev() {
        for text in wrap_log_text(&logs[index].display(timestamps), width)
            .into_iter()
            .rev()
        {
            rows.push((index, text));
            if rows.len() == height {
                rows.reverse();
                return rows;
            }
        }
    }
    rows.reverse();
    rows
}

fn wrapped_log_rows_from(
    logs: &[LogLine],
    start: usize,
    height: usize,
    width: usize,
    timestamps: bool,
) -> Vec<(usize, String)> {
    if logs.is_empty() || height == 0 {
        return Vec::new();
    }

    logs.iter()
        .enumerate()
        .skip(start.min(logs.len() - 1))
        .flat_map(|(index, line)| {
            wrap_log_text(&line.display(timestamps), width)
                .into_iter()
                .map(move |text| (index, text))
        })
        .take(height)
        .collect()
}

fn wrapped_log_view(
    logs: &[LogLine],
    cursor: usize,
    end: &mut usize,
    height: usize,
    width: usize,
    timestamps: bool,
    extend_to_live_edge: bool,
) -> Vec<(usize, String)> {
    if logs.is_empty() {
        *end = 0;
        return Vec::new();
    }

    *end = if extend_to_live_edge {
        logs.len() - 1
    } else {
        (*end).min(logs.len() - 1)
    };
    let mut rows = wrapped_log_rows(logs, *end, height, width, timestamps);
    if !rows.iter().any(|(index, _)| *index == cursor) {
        if rows
            .first()
            .is_some_and(|(first_index, _)| cursor < *first_index)
        {
            rows = wrapped_log_rows_from(logs, cursor, height, width, timestamps);
            if let Some((last_index, _)) = rows.last() {
                *end = *last_index;
            }
        } else {
            *end = cursor.min(logs.len() - 1);
            rows = wrapped_log_rows(logs, *end, height, width, timestamps);
        }
    }
    rows
}

fn advance_live_scroll(
    current_scroll: usize,
    cursor: usize,
    height: usize,
    log_count: usize,
) -> usize {
    log_count
        .saturating_sub(height)
        .min(cursor)
        .max(current_scroll)
}

fn wrap_log_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;

    for character in text.chars() {
        if character == '\t' {
            let spaces = 4 - (current_width % 4);
            for _ in 0..spaces {
                push_wrapped_character(&mut rows, &mut current, &mut current_width, ' ', width);
            }
        } else {
            push_wrapped_character(
                &mut rows,
                &mut current,
                &mut current_width,
                character,
                width,
            );
        }
    }
    if !current.is_empty() || rows.is_empty() {
        rows.push(current);
    }
    rows
}

fn push_wrapped_character(
    rows: &mut Vec<String>,
    current: &mut String,
    current_width: &mut usize,
    character: char,
    width: usize,
) {
    let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
    if character_width > 0 && *current_width > 0 && *current_width + character_width > width {
        rows.push(std::mem::take(current));
        *current_width = 0;
    }
    current.push(character);
    *current_width += character_width;
}

fn copy_to_clipboard(text: String) -> Result<ClipboardLease> {
    // xclip must remain alive while it owns an X11 selection. Running it in
    // foreground mode under `timeout` preserves the clipboard after Blade
    // exits, while guaranteeing eventual cleanup.
    let mut command = Command::new("timeout");
    command.args([
        "--signal=TERM",
        "--kill-after=1",
        "120",
        "xclip",
        "-quiet",
        "-selection",
        "clip",
    ]);
    let active = spawn_clipboard_writer(command, text)?;
    Ok(ClipboardLease {
        expires_at: Instant::now() + CLIPBOARD_TTL,
        active,
    })
}

fn spawn_clipboard_writer(mut command: Command, text: String) -> Result<Arc<AtomicBool>> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("could not start the clipboard helper; xclip and timeout are required")?;
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("could not open xclip input"))?;
    let active = Arc::new(AtomicBool::new(true));
    let writer_active = Arc::clone(&active);
    thread::Builder::new()
        .name("blade-xclip".to_owned())
        .spawn(move || {
            if input.write_all(text.as_bytes()).is_err() {
                let _ = child.kill();
            }
            drop(input);
            // Reap the foreground clipboard owner here, away from the TUI
            // event loop. The timeout wrapper ends it when the lease expires.
            let _ = child.wait();
            writer_active.store(false, Ordering::Relaxed);
        })
        .context("could not start clipboard writer")?;
    Ok(active)
}

fn write_dump(path: &PathBuf, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| {
            format!(
                "could not create {}; choose a new path (existing files are never overwritten)",
                path.display()
            )
        })?;
    file.write_all(text.as_bytes())?;
    Ok(())
}

fn safe_filename(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn state_marker(state: CommandState) -> &'static str {
    match state {
        CommandState::Stopped => "○",
        CommandState::Waiting => "◌",
        CommandState::Preparing => "◐",
        CommandState::Running => "●",
        CommandState::Stopping => "◑",
        CommandState::Completed => "✓",
        CommandState::Failed => "×",
    }
}

fn state_color(state: CommandState, theme: &Theme) -> Color {
    match state {
        CommandState::Stopped => theme.muted,
        CommandState::Waiting | CommandState::Preparing => theme.waiting,
        CommandState::Running => theme.running,
        CommandState::Stopping => theme.stopping,
        CommandState::Completed => theme.completed,
        CommandState::Failed => theme.failed,
    }
}

fn contrasting_text_color(background: Color) -> Color {
    match background {
        Color::Rgb(red, green, blue) => {
            let luminance =
                (u32::from(red) * 299 + u32::from(green) * 587 + u32::from(blue) * 114) / 1000;
            if luminance >= 145 {
                Color::Black
            } else {
                Color::White
            }
        }
        Color::Black
        | Color::Red
        | Color::Blue
        | Color::Magenta
        | Color::DarkGray
        | Color::Indexed(_)
        | Color::Reset => Color::White,
        _ => Color::Black,
    }
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        process::Command,
        thread,
        time::{Duration, Instant},
    };

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{
        Terminal,
        backend::TestBackend,
        layout::Rect,
        style::{Color, Style},
    };
    use tempfile::tempdir;
    use unicode_width::UnicodeWidthStr;

    use crate::{
        config::{RestartAfter, combine_projects, validate_file, validate_file_with_catalog},
        log_buffer::{LogBuffer, LogKind},
        project_list::ProjectEntry,
        runner::Runner,
        theme::{Theme, ThemeOverrides},
    };

    use super::{
        App, CommandSelection, Focus, InputMode, ManageActionEditDialog, ManageActionField,
        ManageEditDialog, ManageEditMode, ManageField, SearchKind, advance_live_scroll,
        build_search_regex, draw_project_list, footer_shortcuts, form_choice_value,
        form_text_value, fuzzy_score, help_line, render_vertical_scrollbar,
        search_needs_top_alignment, spawn_clipboard_writer, styled_log_line, theme_description,
        theme_picker_row, wrap_log_text, wrapped_log_view, write_dump,
    };

    fn wait_until_inactive(app: &App, id: &str) {
        for _ in 0..100 {
            if app
                .runner
                .snapshot(id)
                .is_some_and(|snapshot| !snapshot.state.is_active())
            {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("{id} did not stop in time");
    }

    #[test]
    fn help_shortcuts_share_the_same_description_column() {
        for key in ["Tab", "↑/↓ or j/k", "PgUp/PgDn, g/G"] {
            let line = help_line(key, "description");
            let key_column_width: usize = line.spans[..2]
                .iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum();
            assert_eq!(key_column_width, 22);
        }
    }

    #[test]
    fn form_fields_make_typing_and_choice_controls_visible() {
        assert_eq!(
            form_text_value("", true, 0, "command name"),
            "<command name>█"
        );
        assert_eq!(form_text_value("api", true, 3, "command name"), "api█");
        assert_eq!(form_text_value("api", true, 1, "command name"), "a█pi");
        assert_eq!(form_text_value("api", false, 1, "command name"), "api");
        assert_eq!(form_choice_value("Backend", true), "‹ Backend ›");
        assert_eq!(form_choice_value("Backend", false), "Backend");
    }

    #[test]
    fn vertical_scrollbar_shows_overflow_position_and_direction() {
        let mut terminal = Terminal::new(TestBackend::new(20, 10)).unwrap();
        terminal
            .draw(|frame| {
                render_vertical_scrollbar(frame, frame.area(), 100, 92, 8, &Theme::default())
            })
            .unwrap();
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer[(19, 1)].symbol(), "▲");
        assert_eq!(buffer[(19, 8)].symbol(), "▼");
        assert_eq!(buffer[(19, 7)].symbol(), "┃");
        assert!((2..8).any(|y| buffer[(19, y)].symbol() == "│"));
    }

    #[test]
    fn theme_picker_descriptions_share_the_same_column() {
        let rows = [
            theme_picker_row("●", "nord", "description", 16, 80),
            theme_picker_row(" ", "tokyo-night", "description", 16, 80),
            theme_picker_row(" ", "solarized-dark", "description", 16, 80),
        ];

        let columns = rows.map(|row| {
            let description = row.find("description").unwrap();
            UnicodeWidthStr::width(&row[..description])
        });
        assert_eq!(columns, [18, 18, 18]);
    }

    #[test]
    fn every_theme_preset_has_a_picker_description() {
        for preset in crate::theme::PRESETS {
            assert!(!theme_description(preset).is_empty(), "{preset}");
        }
    }

    #[test]
    fn theme_picker_separates_and_describes_custom_themes() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"shell = "/bin/sh"
[[groups]]
name = "all"
[[groups.commands]]
name = "command"
run = "true"
"#,
        )
        .unwrap();
        let mut project = validate_file(&path).project.unwrap();
        assert!(project.theme_catalog.insert_with_metadata(
            "ocean-neon".to_owned(),
            Theme::preset("blue").unwrap(),
            Some("Electric cyan over deep ocean blues".to_owned()),
            None,
        ));
        let mut app = App::new(Runner::new(project));
        app.open_theme_picker();
        let mut terminal = Terminal::new(TestBackend::new(80, 35)).unwrap();

        terminal
            .draw(|frame| app.draw_theme_picker(frame, frame.area()))
            .unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Custom themes"));
        assert!(rendered.contains("Electric cyan over deep ocean blues"));
    }

    #[test]
    fn project_picker_highlights_the_selected_project_across_the_row() {
        let projects = vec![
            ProjectEntry {
                name: "First".to_owned(),
                path: PathBuf::from("/projects/first/.blade"),
            },
            ProjectEntry {
                name: "ACME Development".to_owned(),
                path: PathBuf::from("/home/user/acme/.blade"),
            },
        ];
        let mut terminal = Terminal::new(TestBackend::new(100, 8)).unwrap();

        terminal
            .draw(|frame| {
                draw_project_list(frame, &PathBuf::from("/home/adnan/.blade"), &projects, 2)
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let selected_row = (1..99).map(|x| buffer[(x, 4)].symbol()).collect::<String>();
        assert!(selected_row.contains("ACME Development"));
        for x in 1..99 {
            assert_eq!(buffer[(x, 4)].bg, Color::Cyan, "column {x}");
        }
    }

    #[test]
    fn combined_session_adds_selectable_project_rows() {
        let directory = tempdir().unwrap();
        let mut loaded = Vec::new();
        for name in ["First", "Second"] {
            let path = directory.path().join(format!("{name}.blade"));
            fs::write(
                &path,
                format!(
                    r#"
shell = "/bin/sh"
[[groups]]
name = "Backend"
[[groups.commands]]
name = "server"
run = "echo {name}"
"#
                ),
            )
            .unwrap();
            loaded.push((name.to_owned(), validate_file(&path).project.unwrap()));
        }
        let project = combine_projects(directory.path().join("projects.config"), loaded).unwrap();
        let mut app = App::new(Runner::new(project));

        app.selected_item = 0;
        let (label, first_targets) = app.selected_targets().unwrap();
        assert_eq!(label, "project First");
        assert_eq!(first_targets.len(), 1);
        app.selected_item = 3;
        let (label, second_targets) = app.selected_targets().unwrap();
        assert_eq!(label, "project Second");
        assert_eq!(second_targets.len(), 1);
        assert_ne!(first_targets, second_targets);

        let mut terminal = Terminal::new(TestBackend::new(40, 8)).unwrap();
        terminal
            .draw(|frame| app.draw_commands(frame, Rect::new(0, 0, 40, 8)))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("First"));
        assert!(rendered.contains("Second"));
    }

    #[test]
    fn ephemeral_commands_join_the_normal_runtime_and_quick_jump() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
shell = "/bin/sh"
[[groups]]
name = "Project"
[[groups.commands]]
name = "base"
run = "true"
"#,
        )
        .unwrap();
        let project = validate_file(&path).project.unwrap();
        let mut app = App::new(Runner::new(project));

        app.run_ephemeral("echo EPHEMERAL_TUI_READY".to_owned());
        let id = app.ephemeral[0].config.id.clone();
        wait_until_inactive(&app, &id);

        assert!(matches!(
            app.command_items.first(),
            Some(CommandSelection::EphemeralGroup)
        ));
        assert!(matches!(
            app.command_items.get(1),
            Some(CommandSelection::Ephemeral(0))
        ));
        assert!(
            app.runner
                .logs(&id)
                .iter()
                .any(|line| line.text == "EPHEMERAL_TUI_READY")
        );
        assert_eq!(app.command_matches("ephemeral").len(), 1);

        app.forget_ephemeral();
        assert!(app.ephemeral.is_empty());
        assert!(app.runner.snapshot(&id).is_none());
    }

    #[test]
    fn nested_actions_join_the_command_tree_quick_jump_and_output_pane() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
name = "Test"
shell = "/bin/sh"
[[groups]]
name = "Frontend"
[[groups.commands]]
name = "Dashboard"
run = "true"
[[groups.commands.actions]]
name = "Install dependencies"
run = "echo installed"
"#,
        )
        .unwrap();
        let project = validate_file(&path).project.unwrap();
        let action_id = project.command("Dashboard").unwrap().actions[0].id.clone();
        let mut app = App::new(Runner::new(project));
        let action_item = app
            .command_items
            .iter()
            .position(|item| matches!(item, CommandSelection::Action { .. }))
            .unwrap();

        assert_eq!(
            app.command_picker_label(action_item).as_deref(),
            Some("Frontend / Dashboard › Install dependencies")
        );
        assert_eq!(app.command_matches("install")[0].item_index, action_item);
        app.selected_item = action_item;
        assert_eq!(
            app.selected_targets(),
            Some((
                "action \"Install dependencies\" for \"Dashboard\"".to_owned(),
                vec![action_id.clone()]
            ))
        );
        app.start_selected();
        wait_until_inactive(&app, &action_id);
        assert!(
            app.runner
                .logs(&action_id)
                .iter()
                .any(|line| line.text == "installed")
        );
    }

    #[test]
    fn command_manager_adds_moves_edits_and_deletes_actions() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
name = "Test"
shell = "/bin/sh"
[[groups]]
name = "Frontend"
[[groups.commands]]
name = "Dashboard"
run = "true"
[[groups.commands]]
name = "Site manager"
run = "true"
"#,
        )
        .unwrap();
        let project = validate_file(&path).project.unwrap();
        let mut app = App::new(Runner::new(project));

        app.apply_manage_action_edit(&ManageActionEditDialog {
            mode: ManageEditMode::Add,
            original: None,
            name: "Install".to_owned(),
            run: "yarn install".to_owned(),
            cwd: String::new(),
            parent: 0,
            requires_stopped: true,
            restart_after: RestartAfter::IfRunning,
            field: ManageActionField::Name,
            cursor: 0,
        })
        .unwrap();
        assert_eq!(app.groups[0].commands[0].actions.len(), 1);

        app.apply_manage_action_edit(&ManageActionEditDialog {
            mode: ManageEditMode::Edit,
            original: Some((0, 0, 0)),
            name: "Install dependencies".to_owned(),
            run: "yarn install --immutable".to_owned(),
            cwd: "frontend".to_owned(),
            parent: 1,
            requires_stopped: false,
            restart_after: RestartAfter::Always,
            field: ManageActionField::Parent,
            cursor: 0,
        })
        .unwrap();
        assert!(app.groups[0].commands[0].actions.is_empty());
        let action = &app.groups[0].commands[1].actions[0];
        assert_eq!(action.name, "Install dependencies");
        assert_eq!(action.run, "yarn install --immutable");
        assert_eq!(action.restart_after, RestartAfter::Always);

        let selected = app
            .managed_items()
            .iter()
            .position(|item| matches!(item, super::ManageItem::Action { .. }))
            .unwrap();
        app.delete_managed_item(selected).unwrap();
        assert!(app.groups[0].commands[1].actions.is_empty());
        assert!(validate_file(&path).is_valid());
    }

    #[test]
    fn an_ephemeral_command_can_be_persisted_without_losing_its_output() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
shell = "/bin/sh"
[[groups]]
name = "Project"
[[groups.commands]]
name = "base"
run = "true"
"#,
        )
        .unwrap();
        let project = validate_file(&path).project.unwrap();
        let mut app = App::new(Runner::new(project));
        app.run_ephemeral("echo KEEP_THIS_OUTPUT".to_owned());
        let id = app.ephemeral[0].config.id.clone();
        wait_until_inactive(&app, &id);

        app.open_persist_dialog();
        let dialog = app.persist_dialog.as_mut().unwrap();
        dialog.name = "saved-task".to_owned();
        dialog.group = "Utilities".to_owned();
        app.persist_ephemeral();

        let saved = validate_file(&path).project.unwrap();
        assert_eq!(
            saved.command("saved-task").unwrap().run,
            "echo KEEP_THIS_OUTPUT"
        );
        assert!(app.ephemeral.is_empty());
        assert!(
            app.runner
                .logs(&id)
                .iter()
                .any(|line| line.text == "KEEP_THIS_OUTPUT")
        );
        assert!(app.groups.iter().any(|group| {
            group.name == "Utilities"
                && group
                    .commands
                    .iter()
                    .any(|command| command.name == "saved-task")
        }));
    }

    #[test]
    fn in_app_management_adds_moves_edits_and_deletes_commands() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
shell = "/bin/sh"
[[groups]]
name = "Project"
[[groups.commands]]
name = "base"
run = "true"
"#,
        )
        .unwrap();
        let project = validate_file(&path).project.unwrap();
        let mut app = App::new(Runner::new(project));

        let added_id = app
            .apply_manage_edit(&ManageEditDialog {
                mode: ManageEditMode::Add,
                original: None,
                name: "task".to_owned(),
                run: "echo original".to_owned(),
                cwd: ".".to_owned(),
                target: 0,
                group: "Project".to_owned(),
                field: ManageField::Name,
                cursor: 0,
            })
            .unwrap();
        let (group, command) = app
            .managed_commands()
            .into_iter()
            .find(|(group, command)| app.groups[*group].commands[*command].id == added_id)
            .unwrap();
        app.apply_manage_edit(&ManageEditDialog {
            mode: ManageEditMode::Edit,
            original: Some((group, command)),
            name: "renamed-task".to_owned(),
            run: "echo changed".to_owned(),
            cwd: ".".to_owned(),
            target: 0,
            group: "Utilities".to_owned(),
            field: ManageField::Name,
            cursor: 0,
        })
        .unwrap();

        let updated = validate_file(&path).project.unwrap();
        assert_eq!(updated.command("renamed-task").unwrap().run, "echo changed");
        assert!(updated.groups.iter().any(|group| {
            group.name == "Utilities"
                && group
                    .commands
                    .iter()
                    .any(|command| command.name == "renamed-task")
        }));

        let selected = app
            .managed_commands()
            .iter()
            .position(|(group, command)| {
                app.groups[*group].commands[*command].name == "renamed-task"
            })
            .unwrap();
        app.delete_managed_item(selected).unwrap();
        assert!(
            validate_file(&path)
                .project
                .unwrap()
                .command("renamed-task")
                .is_none()
        );
        assert!(app.groups.iter().all(|group| group.name != "Utilities"));
        assert!(
            validate_file(&path)
                .project
                .unwrap()
                .groups
                .iter()
                .all(|group| group.name != "Utilities")
        );
        assert!(app.runner.snapshot(&added_id).is_none());
    }

    #[test]
    fn command_editor_wraps_long_values_and_keeps_the_end_visible() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        let run = format!("echo {} COMMAND_END", "long-argument ".repeat(14));
        fs::write(
            &path,
            format!(
                r#"
name = "Test"
shell = "/bin/sh"
[[groups]]
name = "Project"
[[groups.commands]]
name = "long"
run = {run:?}
"#
            ),
        )
        .unwrap();
        let project = validate_file(&path).project.unwrap();
        let mut app = App::new(Runner::new(project));
        app.open_manage();
        app.handle_manage_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        {
            let dialog = app.manage.as_mut().unwrap().edit.as_mut().unwrap();
            dialog.field = ManageField::Run;
            dialog.cursor = dialog.run.chars().count();
        }
        let mut terminal = Terminal::new(TestBackend::new(70, 30)).unwrap();

        terminal
            .draw(|frame| app.draw_manage(frame, frame.area()))
            .unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("COMMAND_END█"));
        let buffer = terminal.backend().buffer();
        let cursor_row = (0..30)
            .find(|y| {
                (0..70)
                    .map(|x| buffer[(x, *y)].symbol())
                    .collect::<String>()
                    .contains("COMMAND_END█")
            })
            .unwrap();
        for x in 4..66 {
            assert_eq!(
                buffer[(x, cursor_row)].bg,
                Color::Cyan,
                "wrapped selected row column {x}"
            );
        }
    }

    #[test]
    fn command_editor_arrows_move_the_cursor_and_alt_arrows_cycle_groups() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
shell = "/bin/sh"
[[groups]]
name = "Backend"
[[groups.commands]]
name = "api"
run = "true"
[[groups]]
name = "Frontend"
[[groups.commands]]
name = "web"
run = "true"
"#,
        )
        .unwrap();
        let project = validate_file(&path).project.unwrap();
        let mut app = App::new(Runner::new(project));
        app.open_manage();
        let mut dialog = app
            .new_manage_dialog(ManageEditMode::Edit, 0, false)
            .unwrap();
        dialog.field = ManageField::Group;
        app.manage.as_mut().unwrap().edit = Some(dialog);

        app.handle_manage_edit_key(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT));
        assert_eq!(
            app.manage.as_ref().unwrap().edit.as_ref().unwrap().group,
            "Frontend"
        );
        app.handle_manage_edit_key(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT));
        assert_eq!(
            app.manage.as_ref().unwrap().edit.as_ref().unwrap().group,
            "Backend"
        );
        app.handle_manage_edit_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(
            app.manage.as_ref().unwrap().edit.as_ref().unwrap().group,
            "Frontend"
        );

        app.manage.as_mut().unwrap().edit.as_mut().unwrap().field = ManageField::Project;
        app.handle_manage_edit_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(
            app.manage.as_ref().unwrap().edit.as_ref().unwrap().group,
            "Frontend",
            "cycling a single project must not reset its selected group"
        );

        let dialog = app.manage.as_mut().unwrap().edit.as_mut().unwrap();
        dialog.field = ManageField::Name;
        dialog.cursor = dialog.name.chars().count();
        app.handle_manage_edit_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        app.handle_manage_edit_key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE));
        assert_eq!(
            app.manage.as_ref().unwrap().edit.as_ref().unwrap().name,
            "apXi"
        );
    }

    #[test]
    fn command_editor_shows_apply_errors_in_a_dismissible_popup() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
name = "Test"
shell = "/bin/sh"
[[groups]]
name = "Project"
[[groups.commands]]
name = "base"
run = "true"
"#,
        )
        .unwrap();
        let project = validate_file(&path).project.unwrap();
        let mut app = App::new(Runner::new(project));
        app.open_manage();
        app.handle_manage_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        app.handle_manage_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let manage = app.manage.as_ref().unwrap();
        assert_eq!(
            manage.error.as_deref(),
            Some("required fields missing: Name, Run")
        );
        assert_eq!(manage.edit.as_ref().unwrap().field, ManageField::Name);

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| app.draw_manage(frame, frame.area()))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Cannot apply changes"));
        assert!(rendered.contains("required fields missing: Name, Run"));

        app.handle_manage_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.manage.as_ref().unwrap().error.is_none());
        assert!(app.manage.as_ref().unwrap().edit.is_some());

        app.handle_manage_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        terminal
            .draw(|frame| app.draw_manage(frame, frame.area()))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("▾ Test"));
        assert!(rendered.contains("▾ Project"));
        assert!(rendered.contains("base  —  true"));

        app.handle_manage_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        terminal
            .draw(|frame| app.draw_manage(frame, frame.area()))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Delete configured item?"));
        assert!(rendered.contains("Delete \"base\" from Test / Project?"));
        app.handle_manage_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(!app.manage.as_ref().unwrap().confirm_delete);
    }

    #[test]
    fn moving_the_last_command_removes_the_live_empty_group_but_keeps_the_project() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
name = "Test"
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
        let project = validate_file(&path).project.unwrap();
        let mut app = App::new(Runner::new(project));
        let (source_group, source_command) = app
            .managed_commands()
            .into_iter()
            .find(|(group, command)| app.groups[*group].commands[*command].name == "task")
            .unwrap();

        app.apply_manage_edit(&ManageEditDialog {
            mode: ManageEditMode::Edit,
            original: Some((source_group, source_command)),
            name: "task".to_owned(),
            run: "echo task".to_owned(),
            cwd: ".".to_owned(),
            target: 0,
            group: "Destination".to_owned(),
            field: ManageField::Group,
            cursor: "Destination".chars().count(),
        })
        .unwrap();

        assert!(app.groups.iter().all(|group| group.name != "Source"));
        assert_eq!(app.project_targets().len(), 1);
        assert_eq!(app.project_targets()[0].label, "Test");
        assert_eq!(
            app.project_targets()[0].group_names,
            ["Destination".to_owned()]
        );
    }

    #[test]
    fn command_manager_can_add_to_a_project_with_no_groups() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
name = "Empty Test"
shell = "/bin/sh"
"#,
        )
        .unwrap();
        let report = validate_file(&path);
        assert!(report.is_valid());
        let mut app = App::new(Runner::new(report.project.unwrap()));
        app.open_manage();
        app.handle_manage_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        {
            let dialog = app.manage.as_mut().unwrap().edit.as_mut().unwrap();
            assert_eq!(dialog.group, "Project");
            dialog.name = "first".to_owned();
            dialog.run = "echo first".to_owned();
        }
        app.handle_manage_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let project = validate_file(&path).project.unwrap();
        assert_eq!(project.groups.len(), 1);
        assert_eq!(project.groups[0].name, "Project");
        assert_eq!(project.groups[0].commands[0].name, "first");
    }

    #[test]
    fn selected_command_uses_the_highlight_color_for_the_full_row() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
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
        let project = validate_file(&path).project.unwrap();
        let mut app = App::new(Runner::new(project));
        let mut terminal = Terminal::new(TestBackend::new(20, 4)).unwrap();

        terminal
            .draw(|frame| app.draw_commands(frame, Rect::new(0, 0, 20, 4)))
            .unwrap();

        let buffer = terminal.backend().buffer();
        for x in 1..19 {
            assert_eq!(buffer[(x, 2)].bg, Color::DarkGray, "column {x}");
            assert_eq!(buffer[(x, 2)].fg, Color::White, "column {x}");
        }

        app.selected_item = 0;
        terminal
            .draw(|frame| app.draw_commands(frame, Rect::new(0, 0, 20, 4)))
            .unwrap();
        let buffer = terminal.backend().buffer();
        for x in 1..19 {
            assert_eq!(buffer[(x, 1)].bg, Color::Cyan, "column {x}");
            assert_eq!(buffer[(x, 1)].fg, Color::Black, "column {x}");
        }
    }

    #[test]
    fn long_status_message_does_not_displace_footer_shortcuts() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
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
        let project = validate_file(&path).project.unwrap();
        let mut app = App::new(Runner::new(project));
        app.message = "status ".repeat(100);
        let mut terminal = Terminal::new(TestBackend::new(80, 4)).unwrap();

        terminal
            .draw(|frame| app.draw_footer(frame, Rect::new(0, 0, 80, 4)))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let shortcut_rows = (2..4)
            .flat_map(|y| (0..80).map(move |x| buffer[(x, y)].symbol()))
            .collect::<String>();
        assert!(shortcut_rows.contains("Ctrl-P"));
        assert!(shortcut_rows.contains("? help"));
    }

    #[test]
    fn footer_adds_secondary_shortcuts_only_when_they_fit() {
        let compact = footer_shortcuts(110, 1);
        assert!(compact.iter().any(|(key, _)| *key == "?"));
        assert!(!compact.iter().any(|(key, _)| *key == "Tab"));

        let wide = footer_shortcuts(340, 1);
        for key in [
            "c",
            ":",
            "p/D",
            "M",
            "Tab",
            "w",
            "PgUp/PgDn",
            "g/G",
            "n/N",
            "h/l",
            "←/→",
        ] {
            assert!(wide.iter().any(|(candidate, _)| *candidate == key));
        }
        assert_eq!(wide.last().map(|(key, _)| *key), Some("?"));
    }

    #[test]
    fn active_command_summary_is_inline_with_the_project_title() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
name = "ACME Development"
shell = "/bin/sh"
[[groups]]
name = "all"
[[groups.commands]]
name = "command"
run = "true"
"#,
        )
        .unwrap();
        let project = validate_file(&path).project.unwrap();
        let app = App::new(Runner::new(project));
        let mut terminal = Terminal::new(TestBackend::new(160, 2)).unwrap();

        terminal
            .draw(|frame| app.draw_header(frame, Rect::new(0, 0, 160, 2)))
            .unwrap();

        let first_row = (0..160)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
            .collect::<String>();
        assert!(first_row.contains("BLADE   ACME Development  0/1 commands active"));
    }

    #[test]
    fn search_shortcuts_select_exact_regex_and_help_modes() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
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
        let project = validate_file(&path).project.unwrap();
        let mut app = App::new(Runner::new(project));

        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))
            .unwrap();
        assert!(matches!(
            app.mode,
            InputMode::Search {
                kind: SearchKind::Exact,
                ..
            }
        ));
        app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();

        app.handle_key(KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::NONE))
            .unwrap();
        assert!(matches!(
            app.mode,
            InputMode::Search {
                kind: SearchKind::InsensitiveRegex,
                ..
            }
        ));
        app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();

        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE))
            .unwrap();
        assert!(app.show_help);
    }

    #[test]
    fn force_start_shortcut_reports_when_selection_is_not_waiting() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
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
        let project = validate_file(&path).project.unwrap();
        let mut app = App::new(Runner::new(project));

        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
            .unwrap();

        assert_eq!(
            app.message,
            "nothing in command is waiting for dependencies"
        );
    }

    #[test]
    fn clear_shortcut_empties_selected_output_and_resets_navigation() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
shell = "/bin/sh"
[[groups]]
name = "all"
[[groups.commands]]
name = "command"
run = "echo clear-me"
"#,
        )
        .unwrap();
        let project = validate_file(&path).project.unwrap();
        let mut app = App::new(Runner::new(project));
        app.start_selected();
        wait_until_inactive(&app, "command");
        assert!(!app.runner.logs("command").is_empty());
        app.apply_search("clear-me".to_owned(), SearchKind::Exact);
        app.log_scroll = 1;
        app.horizontal_scroll = 4;

        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
            .unwrap();

        assert!(app.runner.logs("command").is_empty());
        assert!(app.search.is_none());
        assert!(app.follow);
        assert_eq!(app.log_cursor, 0);
        assert_eq!(app.log_scroll, 0);
        assert_eq!(app.horizontal_scroll, 0);
        assert_eq!(app.message, "cleared output for \"command\"");
    }

    #[test]
    fn theme_picker_previews_cancels_and_persists_a_preset() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
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
        let project = validate_file(&path).project.unwrap();
        let mut app = App::new(Runner::new(project));
        let original = app.theme.clone();

        app.open_theme_picker();
        app.handle_theme_picker_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.theme, Theme::preset("red").unwrap());
        app.handle_theme_picker_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.theme, original);

        app.open_theme_picker();
        app.handle_theme_picker_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_theme_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.theme_picker.is_none());
        assert_eq!(app.theme_preset, "red");
        let project = validate_file(&path).project.unwrap();
        assert_eq!(project.theme_preset, "red");
    }

    #[test]
    fn theme_picker_references_a_custom_theme_file_from_a_project() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        let theme_path = directory.path().join("custom-sand.toml");
        fs::write(
            &path,
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
        fs::write(&theme_path, "preset = \"sand\"\n").unwrap();
        let theme_path = fs::canonicalize(theme_path).unwrap();
        let custom = Theme::preset("sand").unwrap();
        let mut project = validate_file(&path).project.unwrap();
        assert!(project.theme_catalog.insert_with_metadata(
            "custom-sand".to_owned(),
            custom.clone(),
            Some("Warm custom sand".to_owned()),
            Some(theme_path.clone()),
        ));
        let catalog = project.theme_catalog.clone();
        let mut app = App::new(Runner::new(project));

        app.open_theme_picker();
        for _ in 0..crate::theme::PRESETS.len() {
            app.handle_theme_picker_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        assert_eq!(app.theme, custom);
        app.handle_theme_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let source = fs::read_to_string(&path).unwrap();
        assert!(source.contains("file ="));
        assert!(source.contains("custom-sand.toml"));
        assert!(!source.contains("accent ="));
        let saved = validate_file_with_catalog(&path, &catalog).project.unwrap();
        assert_eq!(saved.theme_preset, "custom-sand");
        assert_eq!(saved.theme, custom);
    }

    #[test]
    fn legacy_materialized_custom_theme_does_not_mask_picker_previews() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
shell = "/bin/sh"
[theme]
preset = "default"
accent = "red"
accent_text = "black"
muted = "gray"
text = "white"
footer = "cyan"
search = "yellow"
waiting = "yellow"
running = "green"
stopping = "light-yellow"
completed = "cyan"
failed = "light-red"
[[groups]]
name = "all"
[[groups.commands]]
name = "command"
run = "true"
"#,
        )
        .unwrap();
        let mut project = validate_file(&path).project.unwrap();
        let custom = project.theme.clone();
        assert!(project.theme_overrides.is_complete());
        assert!(project.theme_catalog.insert_with_metadata(
            "legacy-custom".to_owned(),
            custom,
            None,
            None,
        ));
        let mut app = App::new(Runner::new(project));

        assert_eq!(app.theme_preset, "legacy-custom");
        assert_eq!(app.theme_overrides, ThemeOverrides::default());
        app.open_theme_picker();
        app.handle_theme_picker_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

        assert_eq!(app.theme, Theme::preset("monochrome").unwrap());
    }

    #[test]
    fn exact_search_is_a_case_sensitive_literal_substring() {
        let exact = build_search_regex("Blade.", SearchKind::Exact).unwrap();
        assert!(exact.is_match("run Blade. now"));
        assert!(!exact.is_match("run blade. now"));
        assert!(!exact.is_match("run Blade! now"));
    }

    #[test]
    fn insensitive_search_retains_regex_matching() {
        let regex = build_search_regex("bl.de", SearchKind::InsensitiveRegex).unwrap();
        assert!(regex.is_match("BLADE"));
        assert!(regex.is_match("blxde"));
        let exact = build_search_regex("bl.de", SearchKind::Exact).unwrap();
        assert!(exact.is_match("a bl.de value"));
        assert!(!exact.is_match("blxde"));
    }

    #[test]
    fn fuzzy_command_score_matches_ordered_abbreviations() {
        assert!(fuzzy_score("acme-dashboard", "adash").is_some());
        assert!(fuzzy_score("site-manager", "smgr").is_some());
        assert!(fuzzy_score("backend", "dnek").is_none());
        assert!(
            fuzzy_score("dashboard", "dash").unwrap()
                > fuzzy_score("acme-dashboard", "adash").unwrap()
        );
    }

    #[test]
    fn ctrl_p_fuzzy_finds_and_focuses_a_command() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
shell = "/bin/sh"
[[groups]]
name = "Backend"
[[groups.commands]]
name = "api"
run = "true"
[[groups]]
name = "Frontend"
[[groups.commands]]
name = "dashboard"
run = "true"
"#,
        )
        .unwrap();
        let project = validate_file(&path).project.unwrap();
        let mut app = App::new(Runner::new(project));
        app.focus = Focus::Logs;

        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .unwrap();
        assert!(app.command_picker.is_some());
        for character in "dsh".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .unwrap();
        }
        assert_eq!(app.command_matches("dsh").len(), 1);

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert!(app.command_picker.is_none());
        assert_eq!(app.selected_command_name(), Some("dashboard"));
        assert_eq!(app.focus, Focus::Commands);
    }

    #[test]
    fn search_only_repositions_an_offscreen_wrapped_match() {
        assert!(!search_needs_top_alignment(true, Some((4, 8)), 6));
        assert!(search_needs_top_alignment(true, Some((4, 8)), 2));
        assert!(!search_needs_top_alignment(true, Some((4, 8)), 10));
        assert!(!search_needs_top_alignment(true, None, 10));
        assert!(!search_needs_top_alignment(false, Some((4, 8)), 2));
    }

    #[test]
    fn final_wrapped_search_match_is_bottom_aligned() {
        let mut buffer = LogBuffer::new(10);
        for line in ["zero", "one", "two", "three", "final match"] {
            buffer.push(LogKind::Output, line);
        }
        let logs = buffer.snapshot();
        let mut end = 2;

        assert!(!search_needs_top_alignment(true, Some((0, 2)), 4));
        let rows = wrapped_log_view(&logs, 4, &mut end, 3, 20, false, false);

        assert_eq!(
            rows.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
            [2, 3, 4]
        );
    }

    #[test]
    fn timestamps_use_a_muted_style_without_muting_output() {
        let mut buffer = LogBuffer::new(1);
        let line = buffer.push(LogKind::Output, "hello");
        let rendered = styled_log_line(
            &line,
            line.display(true),
            true,
            Style::default().fg(Color::Green),
            Color::DarkGray,
        );

        assert_eq!(rendered.spans.len(), 3);
        assert_eq!(rendered.spans[0].style.fg, Some(Color::DarkGray));
        assert!(!rendered.spans[0].content.ends_with(' '));
        assert_eq!(rendered.spans[1].content.as_ref(), " ");
        assert_eq!(rendered.spans[1].style, Style::default());
        assert_eq!(rendered.spans[2].style.fg, Some(Color::Green));
        assert_eq!(rendered.spans[2].content.as_ref(), "hello");
    }

    #[test]
    fn wraps_log_text_to_the_output_width() {
        assert_eq!(wrap_log_text("abcdef", 3), ["abc", "def"]);
        assert_eq!(wrap_log_text("a界b", 3), ["a界", "b"]);
        assert_eq!(wrap_log_text("a\tb", 4), ["a   ", "b"]);
        assert_eq!(wrap_log_text("", 10), [""]);
    }

    #[test]
    fn moving_up_keeps_subsequent_wrapped_lines_visible() {
        let mut buffer = LogBuffer::new(10);
        for line in ["zero", "one", "two", "three", "four"] {
            buffer.push(LogKind::Output, line);
        }
        let logs = buffer.snapshot();
        let mut end = 4;

        let rows = wrapped_log_view(&logs, 3, &mut end, 3, 20, false, false);
        assert_eq!(
            rows.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
            [2, 3, 4]
        );
        assert_eq!(end, 4);

        let rows = wrapped_log_view(&logs, 1, &mut end, 3, 20, false, false);
        assert_eq!(
            rows.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(end, 3);
    }

    #[test]
    fn incoming_wrapped_lines_fill_space_below_the_selection() {
        let mut buffer = LogBuffer::new(10);
        for line in ["zero", "one", "two", "three"] {
            buffer.push(LogKind::Output, line);
        }
        let mut end = 3;
        buffer.push(LogKind::Output, "four");
        let logs = buffer.snapshot();

        let rows = wrapped_log_view(&logs, 2, &mut end, 5, 20, false, true);

        assert_eq!(
            rows.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4]
        );
        assert_eq!(end, 4);
    }

    #[test]
    fn incoming_unwrapped_lines_scroll_until_the_selection_reaches_the_top() {
        assert_eq!(advance_live_scroll(0, 3, 5, 6), 1);
        assert_eq!(advance_live_scroll(1, 3, 5, 8), 3);
        assert_eq!(advance_live_scroll(3, 3, 5, 9), 3);
    }

    #[test]
    fn dump_never_overwrites_an_existing_file() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("output.log");
        write_dump(&path, "first").unwrap();
        assert!(write_dump(&path, "second").is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "first");
    }

    #[test]
    fn clipboard_copy_does_not_wait_for_the_selection_owner_to_exit() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 1"]);
        let started = Instant::now();
        spawn_clipboard_writer(command, "output".to_owned()).unwrap();
        assert!(started.elapsed() < std::time::Duration::from_millis(200));
    }

    #[test]
    fn horizontal_arrows_move_focus_between_panes() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
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
        let project = validate_file(&path).project.unwrap();
        let mut app = App::new(Runner::new(project));

        assert_eq!(app.selected_command_name(), Some("command"));
        app.apply_search("needle".to_owned(), SearchKind::Exact);
        assert!(app.search.is_some());
        app.select_previous_command();
        assert!(app.search.is_none());
        assert!(app.search_source.is_empty());
        assert_eq!(
            app.selected_targets(),
            Some(("group all".to_owned(), vec!["command".to_owned()]))
        );
        app.select_next_command();

        assert_eq!(app.focus, Focus::Commands);
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.focus, Focus::Logs);
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.focus, Focus::Commands);

        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
            .unwrap();
        assert!(!app.wrap_logs);

        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.horizontal_scroll, 4);
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.horizontal_scroll, 0);
        assert_eq!(app.focus, Focus::Logs);
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.focus, Focus::Commands);
    }
}
