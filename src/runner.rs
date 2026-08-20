use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::fd::AsRawFd,
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, Condvar, Mutex, MutexGuard, mpsc},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use nix::{
    libc,
    pty::{Winsize, openpty},
    sys::signal::{Signal, killpg},
    sys::termios::Termios,
    unistd::Pid,
};

use crate::{
    config::{
        ActionConfig, CommandConfig, ProjectConfig, Readiness, RestartAfter, WaitCondition,
        action_id,
    },
    log_buffer::{LogBuffer, LogKind, LogLine, sanitize_terminal_text},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandState {
    Stopped,
    Waiting,
    Preparing,
    Running,
    Stopping,
    Completed,
    Failed,
}

impl CommandState {
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Waiting | Self::Preparing | Self::Running | Self::Stopping
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Waiting => "waiting",
            Self::Preparing => "preparing",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeSnapshot {
    pub state: CommandState,
    pub pid: Option<i32>,
    pub exit_code: Option<i32>,
    pub stop_level: u8,
    pub stop_deadline: Option<Instant>,
}

#[derive(Clone)]
pub struct Runner {
    shared: Arc<Shared>,
}

struct Shared {
    project: Arc<ProjectConfig>,
    inner: Mutex<Inner>,
    log_io: Mutex<()>,
    changed: Condvar,
}

struct Inner {
    runtimes: HashMap<String, Runtime>,
}

struct Runtime {
    config: Arc<CommandConfig>,
    kind: RuntimeKind,
    state: CommandState,
    pid: Option<i32>,
    exit_code: Option<i32>,
    generation: u64,
    stop_level: u8,
    stop_deadline: Option<Instant>,
    restart_requested: bool,
    force_start_requested: bool,
    running_since: Option<Instant>,
    last_output_at: Option<Instant>,
    keyword_patterns: Vec<KeywordPattern>,
    keyword_hits: HashSet<KeywordPattern>,
    logs: LogBuffer,
    restart_parent_after: Option<String>,
}

#[derive(Debug, Clone)]
enum RuntimeKind {
    Command,
    Action {
        parent_id: String,
        requires_stopped: bool,
        restart_after: RestartAfter,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct KeywordPattern {
    value: String,
    case_sensitive: bool,
}

fn new_runtime(
    config: CommandConfig,
    kind: RuntimeKind,
    keyword_patterns: Vec<KeywordPattern>,
) -> Runtime {
    let max_log_lines = config.max_log_lines;
    Runtime {
        config: Arc::new(config),
        kind,
        state: CommandState::Stopped,
        pid: None,
        exit_code: None,
        generation: 0,
        stop_level: 0,
        stop_deadline: None,
        restart_requested: false,
        force_start_requested: false,
        running_since: None,
        last_output_at: None,
        keyword_patterns,
        keyword_hits: HashSet::new(),
        logs: LogBuffer::new(max_log_lines),
        restart_parent_after: None,
    }
}

fn new_action_runtime(action: &ActionConfig, keyword_patterns: Vec<KeywordPattern>) -> Runtime {
    new_runtime(
        action.runtime_config(),
        RuntimeKind::Action {
            parent_id: action.parent_id.clone(),
            requires_stopped: action.requires_stopped,
            restart_after: action.restart_after,
        },
        keyword_patterns,
    )
}

fn normalize_action_ids(command: &mut CommandConfig) {
    for action in &mut command.actions {
        action.parent_id.clone_from(&command.id);
        action.id = action_id(&command.id, &action.name);
    }
}

impl Runner {
    pub fn new(project: ProjectConfig) -> Self {
        let project = Arc::new(project);
        let mut patterns: HashMap<String, HashSet<KeywordPattern>> = HashMap::new();
        for command in project.commands() {
            for wait in &command.wait_for {
                if let Readiness::Keyword {
                    value,
                    case_sensitive,
                } = &wait.readiness
                {
                    patterns
                        .entry(wait.command.clone())
                        .or_default()
                        .insert(KeywordPattern {
                            value: value.clone(),
                            case_sensitive: *case_sensitive,
                        });
                }
            }
        }

        let mut runtimes = HashMap::new();
        for command in project.commands() {
            let command_patterns = patterns
                .remove(&command.id)
                .unwrap_or_default()
                .into_iter()
                .collect();
            runtimes.insert(
                command.id.clone(),
                new_runtime(command.clone(), RuntimeKind::Command, command_patterns),
            );
            for action in &command.actions {
                runtimes.insert(action.id.clone(), new_action_runtime(action, Vec::new()));
            }
        }
        Self {
            shared: Arc::new(Shared {
                project,
                inner: Mutex::new(Inner { runtimes }),
                log_io: Mutex::new(()),
                changed: Condvar::new(),
            }),
        }
    }

    pub fn project(&self) -> &ProjectConfig {
        &self.shared.project
    }

    pub fn autostart(&self) {
        let names: Vec<_> = {
            let inner = lock_inner(&self.shared);
            inner
                .runtimes
                .iter()
                .filter(|(_, runtime)| runtime.config.autostart)
                .map(|(name, _)| name.clone())
                .collect()
        };
        for name in names {
            let _ = self.start(&name);
        }
    }

    pub fn start_all(&self) {
        let names: Vec<_> = {
            let inner = lock_inner(&self.shared);
            inner
                .runtimes
                .iter()
                .filter(|(_, runtime)| matches!(runtime.kind, RuntimeKind::Command))
                .map(|(name, _)| name.clone())
                .collect()
        };
        for name in names {
            let _ = self.start(&name);
        }
    }

    pub fn start(&self, name: &str) -> Result<()> {
        let kind = lock_inner(&self.shared)
            .runtimes
            .get(name)
            .map(|runtime| runtime.kind.clone())
            .ok_or_else(|| anyhow!("unknown command {name:?}"))?;
        if matches!(kind, RuntimeKind::Action { .. }) {
            return start_action(&self.shared, name, kind);
        }
        let mut visited = HashSet::new();
        start_recursive(&self.shared, name, &mut visited)
    }

    pub fn stop(&self, name: &str) -> Result<()> {
        stop_command(&self.shared, name, false)
    }

    pub fn force_start(&self, name: &str) -> Result<()> {
        {
            let mut inner = lock_inner(&self.shared);
            let runtime = inner
                .runtimes
                .get_mut(name)
                .ok_or_else(|| anyhow!("unknown command {name:?}"))?;
            if runtime.state != CommandState::Waiting {
                return Err(anyhow!(
                    "command {name:?} is {}, not waiting for dependencies",
                    runtime.state.label()
                ));
            }
            runtime.force_start_requested = true;
            self.shared.changed.notify_all();
        }
        emit(
            &self.shared,
            name,
            LogKind::System,
            "force start requested; bypassing remaining readiness conditions".to_owned(),
        );
        Ok(())
    }

    pub fn restart(&self, name: &str) -> Result<()> {
        let mut restart_immediately = false;
        {
            let mut inner = lock_inner(&self.shared);
            let runtime = inner
                .runtimes
                .get_mut(name)
                .ok_or_else(|| anyhow!("unknown command {name:?}"))?;
            if !runtime.state.is_active() {
                restart_immediately = true;
            } else if runtime.pid.is_none() {
                runtime.generation += 1;
                runtime.state = CommandState::Stopped;
                runtime.restart_requested = false;
                runtime.restart_parent_after = None;
                runtime.stop_deadline = None;
                restart_immediately = true;
            } else {
                runtime.restart_requested = true;
            }
            self.shared.changed.notify_all();
        }
        if restart_immediately {
            self.start(name)
        } else {
            self.stop(name)
        }
    }

    pub fn shutdown(&self, force: bool) {
        let active_names: HashSet<_> = {
            let mut inner = lock_inner(&self.shared);
            inner
                .runtimes
                .iter_mut()
                .filter_map(|(name, runtime)| {
                    if runtime.state.is_active() {
                        runtime.restart_requested = false;
                        runtime.restart_parent_after = None;
                        Some(name.clone())
                    } else {
                        None
                    }
                })
                .collect()
        };
        let names: Vec<_> = runtime_dependency_order(&self.shared)
            .into_iter()
            .rev()
            .filter(|name| active_names.contains(name))
            .collect();
        for name in names {
            let _ = stop_command(&self.shared, &name, force);
        }
    }

    pub fn active_count(&self) -> usize {
        let inner = lock_inner(&self.shared);
        inner
            .runtimes
            .values()
            .filter(|runtime| runtime.state.is_active())
            .count()
    }

    pub fn active_command_count(&self) -> usize {
        let inner = lock_inner(&self.shared);
        inner
            .runtimes
            .values()
            .filter(|runtime| {
                matches!(runtime.kind, RuntimeKind::Command) && runtime.state.is_active()
            })
            .count()
    }

    pub fn command_count(&self) -> usize {
        lock_inner(&self.shared)
            .runtimes
            .values()
            .filter(|runtime| matches!(runtime.kind, RuntimeKind::Command))
            .count()
    }

    pub fn add_command(&self, mut config: CommandConfig) -> Result<()> {
        let mut inner = lock_inner(&self.shared);
        if inner.runtimes.contains_key(&config.id) {
            return Err(anyhow!("command id {:?} already exists", config.id));
        }
        normalize_action_ids(&mut config);
        if let Some(action) = config
            .actions
            .iter()
            .find(|action| inner.runtimes.contains_key(&action.id))
        {
            return Err(anyhow!("action id {:?} already exists", action.id));
        }
        let id = config.id.clone();
        for action in &config.actions {
            inner
                .runtimes
                .insert(action.id.clone(), new_action_runtime(action, Vec::new()));
        }
        inner
            .runtimes
            .insert(id, new_runtime(config, RuntimeKind::Command, Vec::new()));
        rebuild_keyword_patterns(&mut inner);
        self.shared.changed.notify_all();
        Ok(())
    }

    pub fn update_command(&self, mut config: CommandConfig) -> Result<()> {
        let mut inner = lock_inner(&self.shared);
        normalize_action_ids(&mut config);
        let previous_action_ids = inner
            .runtimes
            .iter()
            .filter_map(|(id, runtime)| match &runtime.kind {
                RuntimeKind::Action { parent_id, .. } if parent_id == &config.id => {
                    Some(id.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let next_action_ids = config
            .actions
            .iter()
            .map(|action| action.id.as_str())
            .collect::<HashSet<_>>();
        if let Some(active) = previous_action_ids.iter().find(|id| {
            !next_action_ids.contains(id.as_str())
                && inner
                    .runtimes
                    .get(*id)
                    .is_some_and(|runtime| runtime.state.is_active())
        }) {
            return Err(anyhow!("stop active action {active:?} before removing it"));
        }
        for id in previous_action_ids {
            if !next_action_ids.contains(id.as_str()) {
                inner.runtimes.remove(&id);
            }
        }
        for action in &config.actions {
            if let Some(runtime) = inner.runtimes.get_mut(&action.id) {
                runtime.config = Arc::new(action.runtime_config());
                runtime.kind = RuntimeKind::Action {
                    parent_id: action.parent_id.clone(),
                    requires_stopped: action.requires_stopped,
                    restart_after: action.restart_after,
                };
            } else {
                inner
                    .runtimes
                    .insert(action.id.clone(), new_action_runtime(action, Vec::new()));
            }
        }
        let runtime = inner
            .runtimes
            .get_mut(&config.id)
            .ok_or_else(|| anyhow!("unknown command {:?}", config.id))?;
        runtime.config = Arc::new(config);
        rebuild_keyword_patterns(&mut inner);
        self.shared.changed.notify_all();
        Ok(())
    }

    pub fn remove_command(&self, name: &str) -> Result<()> {
        let mut inner = lock_inner(&self.shared);
        let runtime = inner
            .runtimes
            .get(name)
            .ok_or_else(|| anyhow!("unknown command {name:?}"))?;
        if runtime.state.is_active() {
            return Err(anyhow!(
                "command {name:?} is {}; stop it before removing it",
                runtime.state.label()
            ));
        }
        let action_ids = inner
            .runtimes
            .iter()
            .filter_map(|(id, runtime)| match &runtime.kind {
                RuntimeKind::Action { parent_id, .. } if parent_id == name => Some(id.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        if let Some(active) = action_ids.iter().find(|id| {
            inner
                .runtimes
                .get(*id)
                .is_some_and(|runtime| runtime.state.is_active())
        }) {
            return Err(anyhow!(
                "stop active action {active:?} before removing its command"
            ));
        }
        for id in action_ids {
            inner.runtimes.remove(&id);
        }
        inner.runtimes.remove(name);
        rebuild_keyword_patterns(&mut inner);
        self.shared.changed.notify_all();
        Ok(())
    }

    pub fn snapshot(&self, name: &str) -> Option<RuntimeSnapshot> {
        let inner = lock_inner(&self.shared);
        inner.runtimes.get(name).map(|runtime| RuntimeSnapshot {
            state: runtime.state,
            pid: runtime.pid,
            exit_code: runtime.exit_code,
            stop_level: runtime.stop_level,
            stop_deadline: runtime.stop_deadline,
        })
    }

    pub fn logs(&self, name: &str) -> Vec<LogLine> {
        let inner = lock_inner(&self.shared);
        inner
            .runtimes
            .get(name)
            .map(|runtime| runtime.logs.snapshot())
            .unwrap_or_default()
    }

    pub fn clear_logs(&self, name: &str) -> Result<()> {
        let mut inner = lock_inner(&self.shared);
        let runtime = inner
            .runtimes
            .get_mut(name)
            .ok_or_else(|| anyhow!("unknown command {name:?}"))?;
        runtime.logs.clear();
        self.shared.changed.notify_all();
        Ok(())
    }
}

#[cfg(test)]
fn dependency_order(project: &ProjectConfig) -> Vec<String> {
    fn visit(
        project: &ProjectConfig,
        name: &str,
        visited: &mut HashSet<String>,
        ordered: &mut Vec<String>,
    ) {
        if !visited.insert(name.to_owned()) {
            return;
        }
        if let Some(command) = project.command(name) {
            for dependency in &command.wait_for {
                visit(project, &dependency.command, visited, ordered);
            }
        }
        ordered.push(name.to_owned());
    }

    let mut visited = HashSet::new();
    let mut ordered = Vec::new();
    for command in project.commands() {
        visit(project, &command.id, &mut visited, &mut ordered);
    }
    ordered
}

fn runtime_dependency_order(shared: &Arc<Shared>) -> Vec<String> {
    fn visit(
        configs: &HashMap<String, Arc<CommandConfig>>,
        name: &str,
        visited: &mut HashSet<String>,
        ordered: &mut Vec<String>,
    ) {
        if !visited.insert(name.to_owned()) {
            return;
        }
        if let Some(command) = configs.get(name) {
            for dependency in &command.wait_for {
                visit(configs, &dependency.command, visited, ordered);
            }
        }
        ordered.push(name.to_owned());
    }

    let (configs, action_parents) = {
        let inner = lock_inner(shared);
        (
            inner
                .runtimes
                .iter()
                .map(|(name, runtime)| (name.clone(), Arc::clone(&runtime.config)))
                .collect::<HashMap<_, _>>(),
            inner
                .runtimes
                .iter()
                .filter_map(|(name, runtime)| match &runtime.kind {
                    RuntimeKind::Action { parent_id, .. } => {
                        Some((name.clone(), parent_id.clone()))
                    }
                    RuntimeKind::Command => None,
                })
                .collect::<HashMap<_, _>>(),
        )
    };
    let mut visited = HashSet::new();
    let mut ordered = Vec::new();
    for name in configs.keys() {
        if let Some(parent) = action_parents.get(name) {
            visit(&configs, parent, &mut visited, &mut ordered);
        }
        visit(&configs, name, &mut visited, &mut ordered);
    }
    ordered
}

fn rebuild_keyword_patterns(inner: &mut Inner) {
    let mut patterns: HashMap<String, HashSet<KeywordPattern>> = HashMap::new();
    for runtime in inner.runtimes.values() {
        for wait in &runtime.config.wait_for {
            if let Readiness::Keyword {
                value,
                case_sensitive,
            } = &wait.readiness
            {
                patterns
                    .entry(wait.command.clone())
                    .or_default()
                    .insert(KeywordPattern {
                        value: value.clone(),
                        case_sensitive: *case_sensitive,
                    });
            }
        }
    }
    for (name, runtime) in &mut inner.runtimes {
        runtime.keyword_patterns = patterns
            .remove(name)
            .unwrap_or_default()
            .into_iter()
            .collect();
        runtime
            .keyword_hits
            .retain(|pattern| runtime.keyword_patterns.contains(pattern));
    }
}

fn start_action(shared: &Arc<Shared>, name: &str, kind: RuntimeKind) -> Result<()> {
    let RuntimeKind::Action {
        parent_id,
        requires_stopped,
        restart_after,
    } = kind
    else {
        return Err(anyhow!("{name:?} is not an action"));
    };
    let (parent_active, action_active) = {
        let inner = lock_inner(shared);
        let parent = inner
            .runtimes
            .get(&parent_id)
            .ok_or_else(|| anyhow!("action {name:?} has missing parent {parent_id:?}"))?;
        let action = inner
            .runtimes
            .get(name)
            .ok_or_else(|| anyhow!("unknown action {name:?}"))?;
        (parent.state.is_active(), action.state.is_active())
    };
    if action_active {
        return Ok(());
    }
    let restart_parent = match restart_after {
        RestartAfter::Never => false,
        RestartAfter::IfRunning => parent_active,
        RestartAfter::Always => true,
    };
    if requires_stopped && parent_active {
        let generation = {
            let mut inner = lock_inner(shared);
            let action = inner
                .runtimes
                .get_mut(name)
                .ok_or_else(|| anyhow!("unknown action {name:?}"))?;
            action.generation += 1;
            action.state = CommandState::Waiting;
            action.pid = None;
            action.exit_code = None;
            action.stop_level = 0;
            action.stop_deadline = None;
            action.restart_requested = false;
            action.restart_parent_after = restart_parent.then_some(parent_id.clone());
            shared.changed.notify_all();
            action.generation
        };
        emit(
            shared,
            name,
            LogKind::System,
            format!("waiting for parent command {parent_id:?} to stop"),
        );
        stop_command(shared, &parent_id, false)?;
        let shared = Arc::clone(shared);
        let name = name.to_owned();
        thread::Builder::new()
            .name(format!("blade-action-wait-{name}"))
            .spawn(move || {
                let mut inner = lock_inner(&shared);
                loop {
                    let (action_is_waiting, forced) = inner
                        .runtimes
                        .get(&name)
                        .map(|action| {
                            (
                                action.generation == generation
                                    && action.state == CommandState::Waiting,
                                action.force_start_requested,
                            )
                        })
                        .unwrap_or_default();
                    if !action_is_waiting {
                        return;
                    }
                    let parent_inactive = inner
                        .runtimes
                        .get(&parent_id)
                        .is_none_or(|parent| !parent.state.is_active());
                    if parent_inactive || forced {
                        if let Some(action) = inner.runtimes.get_mut(&name) {
                            action.state = CommandState::Stopped;
                        }
                        shared.changed.notify_all();
                        drop(inner);
                        let _ = start_one(&shared, &name);
                        return;
                    }
                    inner = shared
                        .changed
                        .wait_timeout(inner, Duration::from_millis(100))
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .0;
                }
            })
            .context("could not spawn action coordinator thread")?;
        return Ok(());
    }
    {
        let mut inner = lock_inner(shared);
        let action = inner
            .runtimes
            .get_mut(name)
            .ok_or_else(|| anyhow!("unknown action {name:?}"))?;
        action.restart_parent_after = restart_parent.then_some(parent_id);
    }
    start_one(shared, name)
}

fn start_recursive(shared: &Arc<Shared>, name: &str, visited: &mut HashSet<String>) -> Result<()> {
    if !visited.insert(name.to_owned()) {
        return Ok(());
    }
    let dependencies: Vec<_> = {
        let inner = lock_inner(shared);
        inner
            .runtimes
            .get(name)
            .ok_or_else(|| anyhow!("unknown command {name:?}"))?
            .config
            .wait_for
            .iter()
            .map(|wait| wait.command.clone())
            .collect()
    };
    for dependency in dependencies {
        start_recursive(shared, &dependency, visited)?;
    }
    start_one(shared, name)
}

fn start_one(shared: &Arc<Shared>, name: &str) -> Result<()> {
    let generation;
    let wait_count;
    {
        let mut inner = lock_inner(shared);
        let runtime = inner
            .runtimes
            .get_mut(name)
            .ok_or_else(|| anyhow!("unknown command {name:?}"))?;
        if runtime.state.is_active() {
            return Ok(());
        }
        runtime.generation += 1;
        generation = runtime.generation;
        wait_count = runtime.config.wait_for.len();
        runtime.state = if wait_count > 0 {
            CommandState::Waiting
        } else {
            CommandState::Preparing
        };
        runtime.pid = None;
        runtime.exit_code = None;
        runtime.stop_level = 0;
        runtime.stop_deadline = None;
        runtime.restart_requested = false;
        runtime.force_start_requested = false;
        runtime.running_since = None;
        runtime.last_output_at = None;
        runtime.keyword_hits.clear();
        shared.changed.notify_all();
    }
    if wait_count > 0 {
        emit(
            shared,
            name,
            LogKind::System,
            format!("waiting for {wait_count} readiness condition(s)"),
        );
    } else {
        emit(shared, name, LogKind::System, "starting".to_owned());
    }

    let shared = Arc::clone(shared);
    let name = name.to_owned();
    thread::Builder::new()
        .name(format!("blade-{name}"))
        .spawn(move || run_generation(&shared, &name, generation))
        .context("could not spawn command supervisor thread")?;
    Ok(())
}

fn run_generation(shared: &Arc<Shared>, name: &str, generation: u64) {
    let config = {
        let inner = lock_inner(shared);
        let Some(runtime) = inner.runtimes.get(name) else {
            return;
        };
        Arc::clone(&runtime.config)
    };

    for wait in &config.wait_for {
        match wait_for_readiness(shared, name, generation, wait) {
            Ok(ReadinessWait::Ready) => {}
            Ok(ReadinessWait::Forced) => break,
            Err(error) => {
                let mut inner = lock_inner(shared);
                let Some(runtime) = inner.runtimes.get_mut(name) else {
                    return;
                };
                if runtime.generation != generation {
                    return;
                }
                runtime.state = CommandState::Failed;
                runtime.exit_code = None;
                shared.changed.notify_all();
                drop(inner);
                emit(shared, name, LogKind::System, error.to_string());
                return;
            }
        }
    }

    {
        let mut inner = lock_inner(shared);
        let Some(runtime) = inner.runtimes.get_mut(name) else {
            return;
        };
        if runtime.generation != generation || !runtime.state.is_active() {
            return;
        }
        runtime.state = CommandState::Preparing;
        runtime.force_start_requested = false;
        shared.changed.notify_all();
    }

    let sentinel = format!(
        "__BLADE_COMMAND_STARTED_{generation}_{}__",
        std::process::id()
    );
    let script = shell_script(&config, &sentinel);
    let mut command = Command::new(&config.shell);
    command
        .args(["-ilc", script.as_str()])
        .current_dir(&config.cwd);
    let pty_master = match attach_pty(&mut command) {
        Ok(master) => master,
        Err(error) => {
            mark_spawn_failure(
                shared,
                name,
                generation,
                format!("could not create pseudo-terminal: {error}"),
            );
            return;
        }
    };

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            mark_spawn_failure(shared, name, generation, error.to_string());
            return;
        }
    };
    // `Command` retains its configured stdio descriptors so it can be spawned
    // again. Close those parent-side PTY slave copies now, otherwise the
    // master never observes EOF when a short-lived child exits.
    drop(command);
    let pid = child.id() as i32;
    {
        let mut inner = lock_inner(shared);
        let Some(runtime) = inner.runtimes.get_mut(name) else {
            let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
            return;
        };
        if runtime.generation != generation || !runtime.state.is_active() {
            let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
            return;
        }
        runtime.pid = Some(pid);
        shared.changed.notify_all();
    }

    let (status_sender, status_receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = status_sender.send(child.wait());
    });
    let status = read_stream_until_exit(
        shared,
        name,
        generation,
        &sentinel,
        pty_master,
        status_receiver,
    );
    finish_process(shared, name, generation, status);
}

fn read_stream_until_exit(
    shared: &Arc<Shared>,
    name: &str,
    generation: u64,
    sentinel: &str,
    mut stream: File,
    status_receiver: mpsc::Receiver<io::Result<std::process::ExitStatus>>,
) -> io::Result<std::process::ExitStatus> {
    if let Err(error) = set_nonblocking(&stream) {
        emit(
            shared,
            name,
            LogKind::System,
            format!("could not make process output nonblocking: {error}"),
        );
        return receive_exit_status(status_receiver);
    }
    let mut pending = Vec::new();
    let mut chunk = [0_u8; 8192];
    let mut exit_status = None;
    let mut reads_after_exit = 0;
    loop {
        if exit_status.is_none() {
            match status_receiver.try_recv() {
                Ok(status) => exit_status = Some(status),
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(io::Error::other("process waiter disconnected"));
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        match stream.read(&mut chunk) {
            Ok(0) => {
                record_pending_output(shared, name, generation, sentinel, &mut pending);
                return exit_status.unwrap_or_else(|| receive_exit_status(status_receiver));
            }
            Ok(count) => {
                pending.extend_from_slice(&chunk[..count]);
                record_complete_output(shared, name, generation, sentinel, &mut pending);
                if let Some(status) = exit_status.take() {
                    reads_after_exit += 1;
                    if reads_after_exit >= 16 {
                        record_pending_output(shared, name, generation, sentinel, &mut pending);
                        return status;
                    }
                    exit_status = Some(status);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if exit_status.is_none() {
                    match status_receiver.try_recv() {
                        Ok(status) => exit_status = Some(status),
                        Err(mpsc::TryRecvError::Disconnected) => {
                            return Err(io::Error::other("process waiter disconnected"));
                        }
                        Err(mpsc::TryRecvError::Empty) => {}
                    }
                }
                if let Some(status) = exit_status {
                    record_pending_output(shared, name, generation, sentinel, &mut pending);
                    return status;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
            Err(error) => {
                emit(
                    shared,
                    name,
                    LogKind::System,
                    format!("could not read process output: {error}"),
                );
                break;
            }
        }
    }
    record_pending_output(shared, name, generation, sentinel, &mut pending);
    exit_status.unwrap_or_else(|| receive_exit_status(status_receiver))
}

fn set_nonblocking(file: &File) -> io::Result<()> {
    let descriptor = file.as_raw_fd();
    // SAFETY: `descriptor` belongs to the live PTY master and fcntl does not
    // retain the pointer or descriptor beyond either call.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn receive_exit_status(
    receiver: mpsc::Receiver<io::Result<std::process::ExitStatus>>,
) -> io::Result<std::process::ExitStatus> {
    receiver
        .recv()
        .map_err(|_| io::Error::other("process waiter disconnected"))?
}

fn record_complete_output(
    shared: &Arc<Shared>,
    name: &str,
    generation: u64,
    sentinel: &str,
    pending: &mut Vec<u8>,
) {
    while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
        let bytes = pending.drain(..=newline).collect::<Vec<_>>();
        record_output_bytes(shared, name, generation, sentinel, &bytes);
    }
}

fn record_pending_output(
    shared: &Arc<Shared>,
    name: &str,
    generation: u64,
    sentinel: &str,
    pending: &mut Vec<u8>,
) {
    if !pending.is_empty() {
        let bytes = std::mem::take(pending);
        record_output_bytes(shared, name, generation, sentinel, &bytes);
    }
}

fn record_output_bytes(
    shared: &Arc<Shared>,
    name: &str,
    generation: u64,
    sentinel: &str,
    bytes: &[u8],
) {
    let text = String::from_utf8_lossy(bytes);
    let text = sanitize_terminal_text(text.trim_end_matches(['\r', '\n']));
    if text == sentinel {
        mark_running(shared, name, generation);
    } else {
        record_output(shared, name, generation, text);
    }
}

fn mark_running(shared: &Arc<Shared>, name: &str, generation: u64) {
    let changed = {
        let mut inner = lock_inner(shared);
        let Some(runtime) = inner.runtimes.get_mut(name) else {
            return;
        };
        if runtime.generation != generation || runtime.state == CommandState::Stopping {
            false
        } else {
            let now = Instant::now();
            runtime.state = CommandState::Running;
            runtime.running_since = Some(now);
            runtime.last_output_at = Some(now);
            shared.changed.notify_all();
            true
        }
    };
    if changed {
        emit(shared, name, LogKind::System, "running".to_owned());
    }
}

fn record_output(shared: &Arc<Shared>, name: &str, generation: u64, text: String) {
    {
        let mut inner = lock_inner(shared);
        let Some(runtime) = inner.runtimes.get_mut(name) else {
            return;
        };
        if runtime.generation != generation {
            return;
        }
        if runtime.state == CommandState::Running {
            runtime.last_output_at = Some(Instant::now());
            for pattern in &runtime.keyword_patterns {
                let matched = if pattern.case_sensitive {
                    text.contains(&pattern.value)
                } else {
                    text.to_lowercase().contains(&pattern.value.to_lowercase())
                };
                if matched {
                    runtime.keyword_hits.insert(pattern.clone());
                }
            }
        }
        shared.changed.notify_all();
    }
    emit(shared, name, LogKind::Output, text);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadinessWait {
    Ready,
    Forced,
}

fn wait_for_readiness(
    shared: &Arc<Shared>,
    target_name: &str,
    generation: u64,
    wait: &WaitCondition,
) -> Result<ReadinessWait> {
    let description = readiness_description(wait);
    emit(
        shared,
        target_name,
        LogKind::System,
        format!("waiting for {}: {description}", wait.command),
    );
    let deadline = wait
        .timeout
        .map(|seconds| Instant::now() + Duration::from_secs_f64(seconds));
    let mut inner = lock_inner(shared);
    loop {
        let Some(target) = inner.runtimes.get(target_name) else {
            return Err(anyhow!("target command disappeared"));
        };
        if target.generation != generation || !target.state.is_active() {
            return Err(anyhow!("start cancelled"));
        }
        if target.force_start_requested {
            return Ok(ReadinessWait::Forced);
        }
        let dependency = inner
            .runtimes
            .get(&wait.command)
            .ok_or_else(|| anyhow!("missing dependency {:?}", wait.command))?;
        if readiness_met(dependency, &wait.readiness) {
            drop(inner);
            emit(
                shared,
                target_name,
                LogKind::System,
                format!("{} is ready ({description})", wait.command),
            );
            return Ok(ReadinessWait::Ready);
        }
        if matches!(
            dependency.state,
            CommandState::Stopped | CommandState::Completed | CommandState::Failed
        ) {
            return Err(anyhow!(
                "dependency {:?} became {} before it was ready",
                wait.command,
                dependency.state.label()
            ));
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(anyhow!(
                "timed out waiting for {:?} ({description})",
                wait.command
            ));
        }
        let wake_after = deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_millis(100))
            .min(Duration::from_millis(100));
        inner = shared
            .changed
            .wait_timeout(inner, wake_after)
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .0;
    }
}

fn readiness_met(runtime: &Runtime, readiness: &Readiness) -> bool {
    match readiness {
        Readiness::Keyword {
            value,
            case_sensitive,
        } => runtime.keyword_hits.contains(&KeywordPattern {
            value: value.clone(),
            case_sensitive: *case_sensitive,
        }),
        Readiness::Idle { seconds } => {
            runtime.state == CommandState::Running
                && runtime
                    .last_output_at
                    .is_some_and(|last| last.elapsed() >= Duration::from_secs_f64(*seconds))
        }
        Readiness::Delay { seconds } => {
            runtime.state == CommandState::Running
                && runtime
                    .running_since
                    .is_some_and(|started| started.elapsed() >= Duration::from_secs_f64(*seconds))
        }
    }
}

fn stop_command(shared: &Arc<Shared>, name: &str, force: bool) -> Result<()> {
    let (pid, selected_signal, message, escalation) = {
        let mut inner = lock_inner(shared);
        let runtime = inner
            .runtimes
            .get_mut(name)
            .ok_or_else(|| anyhow!("unknown command {name:?}"))?;
        if !runtime.state.is_active() {
            return Ok(());
        }
        if runtime.pid.is_none() {
            runtime.generation += 1;
            runtime.state = CommandState::Stopped;
            runtime.restart_requested = false;
            runtime.restart_parent_after = None;
            runtime.stop_level = 0;
            runtime.stop_deadline = None;
            shared.changed.notify_all();
            drop(inner);
            emit(shared, name, LogKind::System, "start cancelled".to_owned());
            return Ok(());
        }

        let pid = runtime.pid.expect("the pid was checked above");
        if force {
            runtime.stop_level = 3;
        } else {
            runtime.stop_level = runtime.stop_level.saturating_add(1).min(3);
        }
        runtime.state = CommandState::Stopping;
        let selected_signal = stop_signal(runtime.stop_level);
        let message = stop_message(runtime.stop_level);
        let timeout = Duration::from_secs_f64(runtime.config.stop_timeout);
        let escalation = if runtime.stop_level < 3 {
            runtime.stop_deadline = Some(Instant::now() + timeout);
            Some((runtime.generation, pid, runtime.stop_level, timeout))
        } else {
            runtime.stop_deadline = None;
            None
        };
        shared.changed.notify_all();
        (pid, selected_signal, message, escalation)
    };
    let _ = killpg(Pid::from_raw(pid), selected_signal);
    emit(shared, name, LogKind::System, message.to_owned());
    if let Some((generation, pid, stop_level, timeout)) = escalation {
        spawn_escalation(
            Arc::clone(shared),
            name.to_owned(),
            generation,
            pid,
            stop_level,
            timeout,
        );
    }
    Ok(())
}

fn spawn_escalation(
    shared: Arc<Shared>,
    name: String,
    generation: u64,
    pid: i32,
    mut expected_level: u8,
    timeout: Duration,
) {
    thread::spawn(move || {
        loop {
            thread::sleep(timeout);
            let next_level = {
                let mut inner = lock_inner(&shared);
                let Some(runtime) = inner.runtimes.get_mut(&name) else {
                    return;
                };
                if runtime.generation == generation
                    && runtime.pid == Some(pid)
                    && runtime.state == CommandState::Stopping
                    && runtime.stop_level == expected_level
                {
                    let next_level = (expected_level + 1).min(3);
                    runtime.stop_level = next_level;
                    runtime.stop_deadline = if next_level < 3 {
                        Some(Instant::now() + timeout)
                    } else {
                        None
                    };
                    shared.changed.notify_all();
                    next_level
                } else {
                    return;
                }
            };

            let signal = stop_signal(next_level);
            let _ = killpg(Pid::from_raw(pid), signal);
            emit(
                &shared,
                &name,
                LogKind::System,
                match next_level {
                    2 => "grace period elapsed; sent termination signal".to_owned(),
                    _ => "termination grace period elapsed; killed process group".to_owned(),
                },
            );
            if next_level >= 3 {
                return;
            } else {
                expected_level = next_level;
            }
        }
    });
}

fn stop_signal(level: u8) -> Signal {
    match level {
        1 => Signal::SIGINT,
        2 => Signal::SIGTERM,
        _ => Signal::SIGKILL,
    }
}

fn stop_message(level: u8) -> &'static str {
    match level {
        1 => "graceful stop requested (press stop again to terminate)",
        2 => "termination requested (press stop again to kill)",
        _ => "forced kill requested",
    }
}

fn finish_process(
    shared: &Arc<Shared>,
    name: &str,
    generation: u64,
    status: std::io::Result<std::process::ExitStatus>,
) {
    let restart;
    let restart_parent;
    let message;
    {
        let mut inner = lock_inner(shared);
        let Some(runtime) = inner.runtimes.get_mut(name) else {
            return;
        };
        if runtime.generation != generation {
            return;
        }
        runtime.pid = None;
        restart = runtime.restart_requested;
        runtime.restart_requested = false;
        runtime.running_since = None;
        runtime.last_output_at = None;
        let mut completed_successfully = false;
        match status {
            Ok(status) => {
                runtime.exit_code = status.code();
                if runtime.state == CommandState::Stopping {
                    runtime.state = CommandState::Stopped;
                    message = match status.code() {
                        Some(code) => format!("stopped (exit code {code})"),
                        None => "stopped by signal".to_owned(),
                    };
                } else if status.success() {
                    runtime.state = CommandState::Completed;
                    completed_successfully = true;
                    message = "completed successfully".to_owned();
                } else {
                    runtime.state = CommandState::Failed;
                    message = match status.code() {
                        Some(code) => format!("failed with exit code {code}"),
                        None => "failed after receiving a signal".to_owned(),
                    };
                }
            }
            Err(error) => {
                runtime.exit_code = None;
                runtime.state = CommandState::Failed;
                message = format!("could not wait for process: {error}");
            }
        }
        restart_parent = completed_successfully
            .then(|| runtime.restart_parent_after.take())
            .flatten();
        runtime.restart_parent_after = None;
        runtime.stop_level = 0;
        runtime.stop_deadline = None;
        shared.changed.notify_all();
    }
    emit(shared, name, LogKind::System, message);
    if restart {
        emit(shared, name, LogKind::System, "restarting".to_owned());
        let _ = Runner {
            shared: Arc::clone(shared),
        }
        .start(name);
    } else if let Some(parent) = restart_parent {
        emit(
            shared,
            name,
            LogKind::System,
            format!("restarting parent command {parent:?}"),
        );
        let _ = Runner {
            shared: Arc::clone(shared),
        }
        .restart(&parent);
    }
}

fn mark_spawn_failure(shared: &Arc<Shared>, name: &str, generation: u64, error: String) {
    {
        let mut inner = lock_inner(shared);
        let Some(runtime) = inner.runtimes.get_mut(name) else {
            return;
        };
        if runtime.generation != generation {
            return;
        }
        runtime.state = CommandState::Failed;
        runtime.pid = None;
        runtime.exit_code = None;
        runtime.stop_level = 0;
        runtime.stop_deadline = None;
        runtime.restart_parent_after = None;
        shared.changed.notify_all();
    }
    emit(
        shared,
        name,
        LogKind::System,
        format!("could not start shell: {error}"),
    );
}

fn shell_script(config: &CommandConfig, sentinel: &str) -> String {
    // Interactive shells enable job control when attached to a PTY. Disabling
    // it keeps the shell and its command in Blade's process group, so a stop
    // signal still reaches the complete command tree.
    let mut script = String::from("set +m\nexec 2>&1\n");
    for step in config.shell_setup.iter().chain(&config.pre) {
        script.push_str(step);
        script.push_str(
            "\n__blade_status=$?\nif [ \"$__blade_status\" -ne 0 ]; then exit \"$__blade_status\"; fi\n",
        );
    }
    script.push_str("command printf '%s\\n' '");
    script.push_str(sentinel);
    script.push_str("'\n");
    script.push_str(&config.run);
    script.push('\n');
    script
}

fn attach_pty(command: &mut Command) -> io::Result<File> {
    // A deliberately wide virtual terminal avoids wrapping application log
    // lines before Blade applies its own viewport clipping.
    let window_size = Winsize {
        ws_row: 24,
        ws_col: 240,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pair = openpty(Some(&window_size), None::<&Termios>).map_err(io::Error::from)?;
    let stdin = pair.slave.try_clone()?;
    let stdout = pair.slave.try_clone()?;
    command
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(pair.slave));

    // SAFETY: pre_exec runs in the single-threaded child after fork. Both
    // calls are async-signal-safe syscalls. Stdio has already mapped the PTY
    // slave onto fd 0, making it safe to install as the controlling terminal.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(File::from(pair.master))
}

fn readiness_description(wait: &WaitCondition) -> String {
    match &wait.readiness {
        Readiness::Keyword { value, .. } => format!("log contains {value:?}"),
        Readiness::Idle { seconds } => format!("output idle for {seconds} seconds"),
        Readiness::Delay { seconds } => format!("running for {seconds} seconds"),
    }
}

fn emit(shared: &Arc<Shared>, name: &str, kind: LogKind, text: String) {
    let (line, path, rotate_bytes, rotate_keep) = {
        let mut inner = lock_inner(shared);
        let Some(runtime) = inner.runtimes.get_mut(name) else {
            return;
        };
        let line = runtime.logs.push(kind, text);
        let path = log_path(&runtime.config);
        shared.changed.notify_all();
        (
            line,
            path,
            runtime.config.log_rotate_bytes,
            runtime.config.log_rotate_keep,
        )
    };
    if let Some(path) = path {
        let _guard = shared
            .log_io
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = append_log_file(&path, &line, rotate_bytes, rotate_keep);
    }
}

fn log_path(command: &CommandConfig) -> Option<PathBuf> {
    command.log_file.clone().or_else(|| {
        command
            .log_dir
            .as_ref()
            .map(|directory| directory.join(format!("{}.log", safe_filename(&command.name))))
    })
}

fn append_log_file(
    path: &std::path::Path,
    line: &LogLine,
    rotate_bytes: Option<u64>,
    rotate_keep: usize,
) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let entry = format!("{}\n", line.file_display());
    if let Some(max_bytes) = rotate_bytes {
        let current_bytes = fs::metadata(path)
            .map(|metadata| metadata.len())
            .unwrap_or_default();
        if current_bytes > 0 && current_bytes.saturating_add(entry.len() as u64) > max_bytes {
            rotate_log_file(path, rotate_keep)?;
        }
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(entry.as_bytes())
}

fn rotate_log_file(path: &std::path::Path, keep: usize) -> io::Result<()> {
    let oldest = backup_path(path, keep);
    if oldest.exists() {
        fs::remove_file(oldest)?;
    }
    for index in (1..keep).rev() {
        let source = backup_path(path, index);
        if source.exists() {
            fs::rename(source, backup_path(path, index + 1))?;
        }
    }
    if path.exists() {
        fs::rename(path, backup_path(path, 1))?;
    }
    Ok(())
}

fn backup_path(path: &std::path::Path, index: usize) -> PathBuf {
    let mut path = path.as_os_str().to_os_string();
    path.push(format!(".{index}"));
    PathBuf::from(path)
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

fn lock_inner(shared: &Shared) -> MutexGuard<'_, Inner> {
    shared
        .inner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, File},
        process::Command,
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };

    use chrono::Local;
    use nix::pty::openpty;
    use tempfile::tempdir;

    use crate::{
        config::{combine_projects, validate_file, validate_file_for_combined},
        log_buffer::{LogKind, LogLine},
    };

    use super::{
        CommandState, Runner, append_log_file, dependency_order, finish_process, lock_inner,
        read_stream_until_exit,
    };

    fn wait_until_inactive(runner: &Runner, name: &str) {
        for _ in 0..100 {
            if !runner.snapshot(name).unwrap().state.is_active() {
                return;
            }
            thread::sleep(Duration::from_millis(30));
        }
        panic!("{name} did not stop in time");
    }

    fn wait_until_running(runner: &Runner, name: &str) {
        for _ in 0..100 {
            if runner.snapshot(name).unwrap().state == CommandState::Running {
                return;
            }
            thread::sleep(Duration::from_millis(30));
        }
        panic!("{name} did not start in time");
    }

    #[test]
    fn pre_steps_share_the_commands_shell_environment() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
name = "test"
shell = "/bin/sh"
[[groups]]
name = "all"
[[groups.commands]]
name = "env"
pre = ["export BLADE_TEST_VALUE=preserved"]
run = "echo value=$BLADE_TEST_VALUE"
"#,
        )
        .unwrap();
        let project = validate_file(&path).project.unwrap();
        let runner = Runner::new(project);
        runner.start("env").unwrap();
        wait_until_inactive(&runner, "env");
        assert_eq!(
            runner.snapshot("env").unwrap().state,
            CommandState::Completed
        );
        assert!(
            runner
                .logs("env")
                .iter()
                .any(|line| line.text == "value=preserved")
        );
    }

    #[test]
    fn systemd_watcher_marks_a_failed_unit_as_failed_while_journal_is_alive() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
name = "test"
shell = "/bin/sh"
[[groups]]
name = "all"
[[groups.commands]]
name = "service"
pre = ['''
systemctl() {
  if [ "$1" = "show" ]; then
    printf 'failed\n'
  fi
}
journalctl() {
  while :; do sleep 1; done
}
''']
run = '''
blade_watch_systemd_unit() {
  blade_unit="$1"
  journalctl --follow --unit "$blade_unit" --lines=0 --output=cat &
  blade_journal_pid=$!

  while kill -0 "$blade_journal_pid" 2>/dev/null; do
    blade_unit_state=$(systemctl show "$blade_unit" --property=ActiveState --value 2>/dev/null) || blade_unit_state=unknown
    case "$blade_unit_state" in
      active|activating|reloading|deactivating) sleep 0.5 ;;
      inactive) blade_unit_status=0; break ;;
      failed)
        echo "$blade_unit entered the failed state"
        blade_unit_status=1
        break
        ;;
      *) blade_unit_status=1; break ;;
    esac
  done

  if ! kill -0 "$blade_journal_pid" 2>/dev/null; then
    wait "$blade_journal_pid"
    blade_unit_status=1
  else
    kill "$blade_journal_pid" 2>/dev/null || true
    wait "$blade_journal_pid" 2>/dev/null || true
  fi
  return "${blade_unit_status:-1}"
}

blade_watch_systemd_unit example.service
'''
"#,
        )
        .unwrap();
        let project = validate_file(&path).project.unwrap();
        let runner = Runner::new(project);

        runner.start("service").unwrap();
        wait_until_inactive(&runner, "service");

        assert_eq!(
            runner.snapshot("service").unwrap().state,
            CommandState::Failed
        );
        assert!(
            runner
                .logs("service")
                .iter()
                .any(|line| line.text == "example.service entered the failed state")
        );
    }

    #[test]
    fn nested_actions_run_with_inherited_setup_and_independent_logs() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
name = "test"
shell = "/bin/sh"
[[groups]]
name = "all"
[[groups.commands]]
name = "app"
run = "echo parent"
shell_setup = ["export BLADE_ACTION_VALUE=inherited"]
[[groups.commands.actions]]
name = "inspect"
run = "echo action=$BLADE_ACTION_VALUE"
"#,
        )
        .unwrap();
        let project = validate_file(&path).project.unwrap();
        let action_id = project.command("app").unwrap().actions[0].id.clone();
        let runner = Runner::new(project);

        runner.start_all();
        wait_until_inactive(&runner, "app");
        assert_eq!(
            runner.snapshot(&action_id).unwrap().state,
            CommandState::Stopped
        );

        runner.start(&action_id).unwrap();
        wait_until_inactive(&runner, &action_id);
        assert_eq!(
            runner.snapshot(&action_id).unwrap().state,
            CommandState::Completed
        );
        assert!(
            runner
                .logs(&action_id)
                .iter()
                .any(|line| line.text == "action=inherited")
        );
        assert!(
            runner
                .logs("app")
                .iter()
                .all(|line| line.text != "action=inherited")
        );
    }

    #[test]
    fn stopped_parent_action_stops_and_restarts_a_running_parent() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
name = "test"
shell = "/bin/sh"
stop_timeout = 0.2
[[groups]]
name = "all"
[[groups.commands]]
name = "app"
run = "trap 'exit 0' INT TERM; while :; do sleep 0.05; done"
[[groups.commands.actions]]
name = "update"
run = "echo updated"
requires_stopped = true
restart_after = "if-running"
"#,
        )
        .unwrap();
        let project = validate_file(&path).project.unwrap();
        let action_id = project.command("app").unwrap().actions[0].id.clone();
        let runner = Runner::new(project);
        runner.start("app").unwrap();
        wait_until_running(&runner, "app");
        let original_pid = runner.snapshot("app").unwrap().pid;

        runner.start(&action_id).unwrap();
        wait_until_inactive(&runner, &action_id);
        assert_eq!(
            runner.snapshot(&action_id).unwrap().state,
            CommandState::Completed
        );
        for _ in 0..100 {
            let snapshot = runner.snapshot("app").unwrap();
            if snapshot.state == CommandState::Running && snapshot.pid != original_pid {
                runner.shutdown(true);
                return;
            }
            thread::sleep(Duration::from_millis(30));
        }
        runner.shutdown(true);
        panic!("parent command was not restarted after the action");
    }

    #[test]
    fn start_all_ignores_individual_autostart_settings() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
name = "test"
shell = "/bin/sh"
[[groups]]
name = "all"
[[groups.commands]]
name = "first"
run = "echo FIRST"
[[groups.commands]]
name = "second"
run = "echo SECOND"
"#,
        )
        .unwrap();
        let project = validate_file(&path).project.unwrap();
        let runner = Runner::new(project);

        runner.start_all();
        wait_until_inactive(&runner, "first");
        wait_until_inactive(&runner, "second");

        assert!(runner.logs("first").iter().any(|line| line.text == "FIRST"));
        assert!(
            runner
                .logs("second")
                .iter()
                .any(|line| line.text == "SECOND")
        );
    }

    #[test]
    fn dynamically_added_commands_run_log_and_can_be_removed() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
name = "test"
shell = "/bin/sh"
[[groups]]
name = "all"
[[groups.commands]]
name = "base"
run = "true"
"#,
        )
        .unwrap();
        let project = validate_file(&path).project.unwrap();
        let mut dynamic = project.command("base").unwrap().clone();
        dynamic.id = "__ephemeral::1".to_owned();
        dynamic.name = "ephemeral".to_owned();
        dynamic.run = "echo EPHEMERAL_READY".to_owned();
        let runner = Runner::new(project);

        runner.add_command(dynamic).unwrap();
        assert_eq!(runner.command_count(), 2);
        runner.start("__ephemeral::1").unwrap();
        wait_until_inactive(&runner, "__ephemeral::1");

        assert_eq!(
            runner.snapshot("__ephemeral::1").unwrap().state,
            CommandState::Completed
        );
        assert!(
            runner
                .logs("__ephemeral::1")
                .iter()
                .any(|line| line.text == "EPHEMERAL_READY")
        );
        runner.remove_command("__ephemeral::1").unwrap();
        assert_eq!(runner.command_count(), 1);
        assert!(runner.snapshot("__ephemeral::1").is_none());
    }

    #[test]
    fn active_dynamic_commands_must_be_stopped_before_removal() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
name = "test"
shell = "/bin/sh"
[[groups]]
name = "all"
[[groups.commands]]
name = "base"
run = "true"
"#,
        )
        .unwrap();
        let project = validate_file(&path).project.unwrap();
        let mut dynamic = project.command("base").unwrap().clone();
        dynamic.id = "__ephemeral::active".to_owned();
        dynamic.name = "active".to_owned();
        dynamic.run = "sleep 30".to_owned();
        let runner = Runner::new(project);

        runner.add_command(dynamic).unwrap();
        runner.start("__ephemeral::active").unwrap();
        wait_until_running(&runner, "__ephemeral::active");
        assert!(
            runner
                .remove_command("__ephemeral::active")
                .unwrap_err()
                .to_string()
                .contains("stop it before removing")
        );

        runner.shutdown(true);
        wait_until_inactive(&runner, "__ephemeral::active");
        runner.remove_command("__ephemeral::active").unwrap();
    }

    #[test]
    fn combined_projects_run_same_named_commands_independently() {
        let directory = tempdir().unwrap();
        let first_path = directory.path().join("first.blade");
        let second_path = directory.path().join("second.blade");
        for (path, output) in [
            (&first_path, "FIRST_PROJECT"),
            (&second_path, "SECOND_PROJECT"),
        ] {
            fs::write(
                path,
                format!(
                    r#"
shell = "/bin/sh"
[[groups]]
name = "all"
[[groups.commands]]
name = "server"
run = "echo {output}"
"#
                ),
            )
            .unwrap();
        }
        let combined = combine_projects(
            directory.path().join("projects.config"),
            vec![
                (
                    "First".to_owned(),
                    validate_file(&first_path).project.unwrap(),
                ),
                (
                    "Second".to_owned(),
                    validate_file(&second_path).project.unwrap(),
                ),
            ],
        )
        .unwrap();
        let first_id = combined.groups[0].commands[0].id.clone();
        let second_id = combined.groups[1].commands[0].id.clone();
        let runner = Runner::new(combined);

        runner.start_all();
        wait_until_inactive(&runner, &first_id);
        wait_until_inactive(&runner, &second_id);

        assert!(
            runner
                .logs(&first_id)
                .iter()
                .any(|line| line.text == "FIRST_PROJECT")
        );
        assert!(
            runner
                .logs(&second_id)
                .iter()
                .any(|line| line.text == "SECOND_PROJECT")
        );
    }

    #[test]
    fn cross_project_dependency_starts_and_waits_for_the_target_command() {
        let directory = tempdir().unwrap();
        let backend_path = directory.path().join("backend.blade");
        let frontend_path = directory.path().join("frontend.blade");
        fs::write(
            &backend_path,
            r#"
shell = "/bin/sh"
[[groups]]
name = "Backend"
[[groups.commands]]
name = "api"
run = "echo API_READY; sleep 0.2"
"#,
        )
        .unwrap();
        fs::write(
            &frontend_path,
            r#"
shell = "/bin/sh"
[[groups]]
name = "Frontend"
[[groups.commands]]
name = "dashboard"
run = "echo DASHBOARD_STARTED"
[[groups.commands.wait_for]]
command = "Backend::api"
kind = "keyword"
value = "API_READY"
timeout = 2
"#,
        )
        .unwrap();
        let combined = combine_projects(
            directory.path().join("projects.config"),
            vec![
                (
                    "Backend".to_owned(),
                    validate_file_for_combined(&backend_path).project.unwrap(),
                ),
                (
                    "Frontend".to_owned(),
                    validate_file_for_combined(&frontend_path).project.unwrap(),
                ),
            ],
        )
        .unwrap();
        let api_id = combined.groups[0].commands[0].id.clone();
        let dashboard_id = combined.groups[1].commands[0].id.clone();
        assert_eq!(
            dependency_order(&combined),
            vec![api_id.clone(), dashboard_id.clone()]
        );
        let runner = Runner::new(combined);

        runner.start(&dashboard_id).unwrap();
        wait_until_inactive(&runner, &dashboard_id);
        wait_until_inactive(&runner, &api_id);

        assert!(
            runner
                .logs(&api_id)
                .iter()
                .any(|line| line.text == "API_READY")
        );
        assert!(
            runner
                .logs(&dashboard_id)
                .iter()
                .any(|line| line.text == "DASHBOARD_STARTED")
        );
    }

    #[test]
    fn commands_receive_a_pseudo_terminal_for_immediate_output() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
name = "test"
shell = "/bin/sh"
[[groups]]
name = "all"
[[groups.commands]]
name = "tty"
run = "if [ -t 0 ] && [ -t 1 ] && [ -t 2 ]; then echo HAS_PTY; else echo NO_PTY; fi"
"#,
        )
        .unwrap();
        let project = validate_file(&path).project.unwrap();
        let runner = Runner::new(project);
        runner.start("tty").unwrap();
        wait_until_inactive(&runner, "tty");
        assert_eq!(
            runner.snapshot("tty").unwrap().state,
            CommandState::Completed
        );
        assert!(runner.logs("tty").iter().any(|line| line.text == "HAS_PTY"));
    }

    #[test]
    fn keyword_dependency_starts_automatically_and_unblocks_target() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
name = "test"
shell = "/bin/sh"
[[groups]]
name = "all"
[[groups.commands]]
name = "server"
run = "echo READY; sleep 0.2"
[[groups.commands]]
name = "client"
run = "echo CLIENT_STARTED"
[[groups.commands.wait_for]]
command = "server"
kind = "keyword"
value = "READY"
timeout = 2
"#,
        )
        .unwrap();
        let project = validate_file(&path).project.unwrap();
        let runner = Runner::new(project);
        runner.start("client").unwrap();
        wait_until_inactive(&runner, "client");
        assert_eq!(
            runner.snapshot("client").unwrap().state,
            CommandState::Completed
        );
        assert!(
            runner
                .logs("client")
                .iter()
                .any(|line| line.text == "CLIENT_STARTED")
        );
        wait_until_inactive(&runner, "server");
    }

    #[test]
    fn force_start_bypasses_remaining_dependency_readiness() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
name = "test"
shell = "/bin/sh"
[[groups]]
name = "all"
[[groups.commands]]
name = "server"
run = "sleep 0.3"
[[groups.commands]]
name = "client"
run = "echo FORCE_STARTED"
[[groups.commands.wait_for]]
command = "server"
kind = "keyword"
value = "NEVER_READY"
timeout = 2
"#,
        )
        .unwrap();
        let project = validate_file(&path).project.unwrap();
        let runner = Runner::new(project);

        runner.start("client").unwrap();
        assert_eq!(
            runner.snapshot("client").unwrap().state,
            CommandState::Waiting
        );
        runner.force_start("client").unwrap();
        wait_until_inactive(&runner, "client");

        assert_eq!(
            runner.snapshot("client").unwrap().state,
            CommandState::Completed
        );
        assert!(
            runner
                .logs("client")
                .iter()
                .any(|line| line.text == "FORCE_STARTED")
        );
        assert!(runner.logs("client").iter().any(|line| {
            line.text
                .contains("bypassing remaining readiness conditions")
        }));
        assert!(runner.force_start("client").is_err());
        wait_until_inactive(&runner, "server");
    }

    #[test]
    fn idle_and_delay_readiness_conditions_both_unblock() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
name = "test"
shell = "/bin/sh"
[[groups]]
name = "all"
[[groups.commands]]
name = "server"
run = "echo BOOTED; sleep 0.2"
[[groups.commands]]
name = "client"
run = "echo CONDITIONS_PASSED"
[[groups.commands.wait_for]]
command = "server"
kind = "idle"
seconds = 0.05
timeout = 2
[[groups.commands.wait_for]]
command = "server"
kind = "delay"
seconds = 0.05
timeout = 2
"#,
        )
        .unwrap();
        let project = validate_file(&path).project.unwrap();
        let runner = Runner::new(project);
        runner.start("client").unwrap();
        wait_until_inactive(&runner, "client");
        assert_eq!(
            runner.snapshot("client").unwrap().state,
            CommandState::Completed
        );
        assert!(
            runner
                .logs("client")
                .iter()
                .any(|line| line.text == "CONDITIONS_PASSED")
        );
        wait_until_inactive(&runner, "server");
    }

    #[test]
    fn graceful_stop_reaches_the_shell_and_its_process_group() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
name = "test"
shell = "/bin/sh"
[[groups]]
name = "all"
[[groups.commands]]
name = "service"
stop_timeout = 1
run = """
trap 'echo INTERRUPTED; exit 0' INT
while :; do sleep 0.05; done
"""
"#,
        )
        .unwrap();
        let project = validate_file(&path).project.unwrap();
        let runner = Runner::new(project);
        runner.start("service").unwrap();
        wait_until_running(&runner, "service");
        runner.stop("service").unwrap();
        wait_until_inactive(&runner, "service");
        assert_eq!(
            runner.snapshot("service").unwrap().state,
            CommandState::Stopped
        );
        assert!(
            runner
                .logs("service")
                .iter()
                .any(|line| line.text.contains("INTERRUPTED"))
        );
    }

    #[test]
    fn repeated_stop_keeps_automatic_escalation_active() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
name = "test"
shell = "/bin/sh"
[[groups]]
name = "all"
[[groups.commands]]
name = "stubborn service"
stop_timeout = 0.05
run = """
trap '' INT TERM
while :; do sleep 0.05; done
"""
"#,
        )
        .unwrap();
        let project = validate_file(&path).project.unwrap();
        let runner = Runner::new(project);
        runner.start("stubborn service").unwrap();
        wait_until_running(&runner, "stubborn service");

        runner.stop("stubborn service").unwrap();
        runner.stop("stubborn service").unwrap();
        wait_until_inactive(&runner, "stubborn service");

        assert_eq!(
            runner.snapshot("stubborn service").unwrap().state,
            CommandState::Stopped
        );
    }

    #[test]
    fn exited_shell_finishes_even_when_another_process_holds_the_pty_open() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
name = "test"
shell = "/bin/sh"
[[groups]]
name = "all"
[[groups.commands]]
name = "service"
run = "true"
"#,
        )
        .unwrap();
        let project = validate_file(&path).project.unwrap();
        let runner = Runner::new(project);
        {
            let mut inner = lock_inner(&runner.shared);
            let runtime = inner.runtimes.get_mut("service").unwrap();
            runtime.state = CommandState::Stopping;
            runtime.pid = Some(1234);
        }
        let pair = openpty(None, None).unwrap();
        let _held_slave = pair.slave;
        let (sender, receiver) = mpsc::sync_channel(1);
        sender.send(Command::new("true").status()).unwrap();

        let started = Instant::now();
        let status = read_stream_until_exit(
            &runner.shared,
            "service",
            0,
            "unused sentinel",
            File::from(pair.master),
            receiver,
        );
        assert!(started.elapsed() < Duration::from_millis(200));
        finish_process(&runner.shared, "service", 0, status);

        assert_eq!(
            runner.snapshot("service").unwrap().state,
            CommandState::Stopped
        );
    }

    #[test]
    fn shutdown_cancels_a_pending_restart() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
name = "test"
shell = "/bin/sh"
[[groups]]
name = "all"
[[groups.commands]]
name = "restarting service"
stop_timeout = 0.05
run = """
trap '' INT TERM
while :; do sleep 0.05; done
"""
"#,
        )
        .unwrap();
        let project = validate_file(&path).project.unwrap();
        let runner = Runner::new(project);
        runner.start("restarting service").unwrap();
        wait_until_running(&runner, "restarting service");

        runner.restart("restarting service").unwrap();
        runner.shutdown(false);
        wait_until_inactive(&runner, "restarting service");
        thread::sleep(Duration::from_millis(100));

        assert_eq!(
            runner.snapshot("restarting service").unwrap().state,
            CommandState::Stopped
        );
        assert_eq!(runner.active_count(), 0);
    }

    #[test]
    fn configured_log_directory_receives_timestamped_output() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".blade");
        fs::write(
            &path,
            r#"
name = "test"
shell = "/bin/sh"
log_dir = "logs"
[[groups]]
name = "all"
[[groups.commands]]
name = "logged command"
run = "echo PERSISTED"
"#,
        )
        .unwrap();
        let project = validate_file(&path).project.unwrap();
        let runner = Runner::new(project);
        runner.start("logged command").unwrap();
        wait_until_inactive(&runner, "logged command");
        let contents = fs::read_to_string(directory.path().join("logs/logged_command.log"))
            .expect("the configured log file should be created");
        assert!(contents.contains("PERSISTED"));
        assert!(contents.contains('T'));
    }

    #[test]
    fn logfile_rotation_keeps_only_the_configured_backups() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("command.log");
        for text in ["first", "second", "third", "fourth"] {
            append_log_file(
                &path,
                &LogLine {
                    sequence: 0,
                    timestamp: Local::now(),
                    text: text.to_owned(),
                    kind: LogKind::Output,
                },
                Some(1),
                2,
            )
            .unwrap();
        }

        assert!(fs::read_to_string(&path).unwrap().contains("fourth"));
        assert!(
            fs::read_to_string(directory.path().join("command.log.1"))
                .unwrap()
                .contains("third")
        );
        assert!(
            fs::read_to_string(directory.path().join("command.log.2"))
                .unwrap()
                .contains("second")
        );
        assert!(!directory.path().join("command.log.3").exists());
    }
}
