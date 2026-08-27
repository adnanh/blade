use std::{
    collections::HashMap,
    env, fs,
    io::{BufRead, BufReader, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::{
    config::{CommandConfig, ProjectConfig},
    log_buffer::{LogLine, LogMetadata, LogUpdate},
    runner::{CommandState, Runner, RuntimeSnapshot},
};

const PROTOCOL_VERSION: u32 = 1;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionProject {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SessionSource {
    Project {
        path: PathBuf,
    },
    Combined {
        project_list: PathBuf,
        projects: Vec<SessionProject>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDescriptor {
    pub protocol: u32,
    pub id: String,
    pub name: String,
    pub socket: PathBuf,
    pub source: SessionSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EphemeralState {
    pub config: CommandConfig,
    pub project: Option<String>,
    pub suggested_group: String,
}

#[derive(Clone)]
pub enum RunnerHandle {
    Local(Runner),
    Remote(SupervisorClient),
}

impl From<Runner> for RunnerHandle {
    fn from(runner: Runner) -> Self {
        Self::Local(runner)
    }
}

impl RunnerHandle {
    pub fn project(&self) -> &ProjectConfig {
        match self {
            Self::Local(runner) => runner.project(),
            Self::Remote(client) => &client.inner.project,
        }
    }

    pub fn autostart(&self) {
        match self {
            Self::Local(runner) => runner.autostart(),
            Self::Remote(client) => {
                let _ = client.call(&Request::Autostart);
            }
        }
    }

    pub fn start_all(&self) {
        match self {
            Self::Local(runner) => runner.start_all(),
            Self::Remote(client) => {
                let _ = client.call(&Request::StartAll);
            }
        }
    }

    pub fn start(&self, id: &str) -> Result<()> {
        match self {
            Self::Local(runner) => runner.start(id),
            Self::Remote(client) => client.call_ok(&Request::Start { id: id.to_owned() }),
        }
    }

    pub fn stop(&self, id: &str) -> Result<()> {
        match self {
            Self::Local(runner) => runner.stop(id),
            Self::Remote(client) => client.call_ok(&Request::Stop { id: id.to_owned() }),
        }
    }

    pub fn force_start(&self, id: &str) -> Result<()> {
        match self {
            Self::Local(runner) => runner.force_start(id),
            Self::Remote(client) => client.call_ok(&Request::ForceStart { id: id.to_owned() }),
        }
    }

    pub fn restart(&self, id: &str) -> Result<()> {
        match self {
            Self::Local(runner) => runner.restart(id),
            Self::Remote(client) => client.call_ok(&Request::Restart { id: id.to_owned() }),
        }
    }

    pub fn shutdown(&self, force: bool) {
        match self {
            Self::Local(runner) => runner.shutdown(force),
            Self::Remote(client) => {
                let _ = client.call(&Request::Shutdown { force });
            }
        }
    }

    pub fn active_count(&self) -> usize {
        match self {
            Self::Local(runner) => runner.active_count(),
            Self::Remote(client) => client.call_count(&Request::ActiveCount),
        }
    }

    pub fn active_command_count(&self) -> usize {
        match self {
            Self::Local(runner) => runner.active_command_count(),
            Self::Remote(client) => client.call_count(&Request::ActiveCommandCount),
        }
    }

    pub fn command_count(&self) -> usize {
        match self {
            Self::Local(runner) => runner.command_count(),
            Self::Remote(client) => client.call_count(&Request::CommandCount),
        }
    }

    pub fn configured_commands(&self) -> Vec<CommandConfig> {
        match self {
            Self::Local(_) => Vec::new(),
            Self::Remote(client) => match client.call(&Request::ConfiguredCommands) {
                Ok(Response::Commands(commands)) => commands,
                _ => Vec::new(),
            },
        }
    }

    pub fn add_command(&self, config: CommandConfig) -> Result<()> {
        match self {
            Self::Local(runner) => runner.add_command(config),
            Self::Remote(client) => client.call_ok(&Request::AddCommand { config }),
        }
    }

    pub fn add_ephemeral(&self, state: EphemeralState) -> Result<()> {
        match self {
            Self::Local(runner) => runner.add_command(state.config),
            Self::Remote(client) => client.call_ok(&Request::AddEphemeral { state }),
        }
    }

    pub fn update_command(&self, config: CommandConfig) -> Result<()> {
        match self {
            Self::Local(runner) => runner.update_command(config),
            Self::Remote(client) => client.call_ok(&Request::UpdateCommand { config }),
        }
    }

    pub fn remove_command(&self, id: &str) -> Result<()> {
        match self {
            Self::Local(runner) => runner.remove_command(id),
            Self::Remote(client) => client.call_ok(&Request::RemoveCommand { id: id.to_owned() }),
        }
    }

    pub fn mark_ephemeral_persisted(&self, id: &str) -> Result<()> {
        match self {
            Self::Local(_) => Ok(()),
            Self::Remote(client) => {
                client.call_ok(&Request::MarkEphemeralPersisted { id: id.to_owned() })
            }
        }
    }

    pub fn snapshot(&self, id: &str) -> Option<RuntimeSnapshot> {
        match self {
            Self::Local(runner) => runner.snapshot(id),
            Self::Remote(client) => match client.call(&Request::Snapshot { id: id.to_owned() }) {
                Ok(Response::Snapshot(snapshot)) => snapshot.map(RuntimeSnapshotWire::into_runtime),
                _ => None,
            },
        }
    }

    pub fn logs(&self, id: &str) -> Vec<LogLine> {
        match self {
            Self::Local(runner) => runner.logs(id),
            Self::Remote(client) => match client.call(&Request::Logs { id: id.to_owned() }) {
                Ok(Response::Logs(logs)) => logs,
                _ => Vec::new(),
            },
        }
    }

    pub fn log_update(&self, id: &str, after: Option<u64>) -> Option<LogUpdate> {
        match self {
            Self::Local(runner) => runner.log_update(id, after),
            Self::Remote(client) => match client.call(&Request::LogUpdate {
                id: id.to_owned(),
                after,
            }) {
                Ok(Response::LogUpdate(update)) => update,
                _ => None,
            },
        }
    }

    pub fn clear_logs(&self, id: &str) -> Result<()> {
        match self {
            Self::Local(runner) => runner.clear_logs(id),
            Self::Remote(client) => client.call_ok(&Request::ClearLogs { id: id.to_owned() }),
        }
    }

    pub fn ephemeral_commands(&self) -> Vec<EphemeralState> {
        match self {
            Self::Local(_) => Vec::new(),
            Self::Remote(client) => match client.call(&Request::Ephemerals) {
                Ok(Response::Ephemerals(commands)) => commands,
                _ => Vec::new(),
            },
        }
    }

    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::Local(_) => None,
            Self::Remote(client) => Some(&client.inner.descriptor.id),
        }
    }

    pub fn reload_executable(&self) -> Option<&Path> {
        match self {
            Self::Local(_) => None,
            Self::Remote(client) => Some(&client.inner.executable),
        }
    }

    pub fn terminate_session(&self) -> Result<()> {
        match self {
            Self::Local(_) => Ok(()),
            Self::Remote(client) => {
                client.call_ok(&Request::TerminateSession)?;
                if let Ok(mut child) = client.inner.child.lock()
                    && let Some(mut child) = child.take()
                {
                    let _ = child.wait();
                }
                Ok(())
            }
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Remote(_))
    }
}

#[derive(Clone)]
pub struct SupervisorClient {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    project: Arc<ProjectConfig>,
    descriptor: SessionDescriptor,
    executable: PathBuf,
    connection: Mutex<BufReader<UnixStream>>,
    child: Mutex<Option<Child>>,
}

impl SupervisorClient {
    fn connect(
        project: ProjectConfig,
        descriptor: SessionDescriptor,
        child: Option<Child>,
    ) -> Result<Self> {
        let executable = env::current_exe().context("could not locate the running Blade binary")?;
        let stream = UnixStream::connect(&descriptor.socket).with_context(|| {
            format!(
                "could not connect to Blade session {} at {}",
                descriptor.id,
                descriptor.socket.display()
            )
        })?;
        let client = Self {
            inner: Arc::new(ClientInner {
                project: Arc::new(project),
                descriptor,
                executable,
                connection: Mutex::new(BufReader::new(stream)),
                child: Mutex::new(child),
            }),
        };
        match client.call(&Request::Ping)? {
            Response::Pong {
                protocol: PROTOCOL_VERSION,
            } => Ok(client),
            Response::Pong { protocol } => bail!(
                "Blade session protocol {protocol} is incompatible with client protocol {PROTOCOL_VERSION}"
            ),
            response => bail!("unexpected response from Blade supervisor: {response:?}"),
        }
    }

    fn call(&self, request: &Request) -> Result<Response> {
        let mut connection = self
            .inner
            .connection
            .lock()
            .map_err(|_| anyhow!("Blade supervisor connection lock is poisoned"))?;
        serde_json::to_writer(connection.get_mut(), request)
            .context("could not encode Blade supervisor request")?;
        connection
            .get_mut()
            .write_all(b"\n")
            .context("could not write Blade supervisor request")?;
        connection
            .get_mut()
            .flush()
            .context("could not flush Blade supervisor request")?;
        let mut response = String::new();
        if connection
            .read_line(&mut response)
            .context("could not read Blade supervisor response")?
            == 0
        {
            bail!("Blade supervisor disconnected");
        }
        serde_json::from_str(&response).context("could not decode Blade supervisor response")
    }

    fn call_ok(&self, request: &Request) -> Result<()> {
        match self.call(request)? {
            Response::Ok => Ok(()),
            Response::Error { message } => Err(anyhow!(message)),
            response => bail!("unexpected response from Blade supervisor: {response:?}"),
        }
    }

    fn call_count(&self, request: &Request) -> usize {
        match self.call(request) {
            Ok(Response::Count(count)) => count,
            _ => 0,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "kebab-case")]
enum Request {
    Ping,
    Autostart,
    StartAll,
    Start { id: String },
    Stop { id: String },
    ForceStart { id: String },
    Restart { id: String },
    Shutdown { force: bool },
    ActiveCount,
    ActiveCommandCount,
    CommandCount,
    ConfiguredCommands,
    AddCommand { config: CommandConfig },
    AddEphemeral { state: EphemeralState },
    UpdateCommand { config: CommandConfig },
    RemoveCommand { id: String },
    MarkEphemeralPersisted { id: String },
    Snapshot { id: String },
    Logs { id: String },
    LogUpdate { id: String, after: Option<u64> },
    ClearLogs { id: String },
    Ephemerals,
    TerminateSession,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result", content = "data", rename_all = "kebab-case")]
enum Response {
    Ok,
    Error { message: String },
    Pong { protocol: u32 },
    Count(usize),
    Snapshot(Option<RuntimeSnapshotWire>),
    Logs(Vec<LogLine>),
    LogUpdate(Option<LogUpdate>),
    Ephemerals(Vec<EphemeralState>),
    Commands(Vec<CommandConfig>),
}

#[derive(Debug, Serialize, Deserialize)]
struct RuntimeSnapshotWire {
    state: CommandState,
    pid: Option<i32>,
    exit_code: Option<i32>,
    stop_level: u8,
    stop_remaining_millis: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    log_metadata: Option<LogMetadata>,
}

impl RuntimeSnapshotWire {
    fn from_runtime(snapshot: RuntimeSnapshot) -> Self {
        Self {
            state: snapshot.state,
            pid: snapshot.pid,
            exit_code: snapshot.exit_code,
            stop_level: snapshot.stop_level,
            stop_remaining_millis: snapshot.stop_deadline.map(|deadline| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64
            }),
            log_metadata: snapshot.log_metadata,
        }
    }

    fn into_runtime(self) -> RuntimeSnapshot {
        RuntimeSnapshot {
            state: self.state,
            pid: self.pid,
            exit_code: self.exit_code,
            stop_level: self.stop_level,
            stop_deadline: self
                .stop_remaining_millis
                .map(|millis| Instant::now() + Duration::from_millis(millis)),
            log_metadata: self.log_metadata,
        }
    }
}

struct ServerState {
    runner: Runner,
    ephemerals: Mutex<HashMap<String, EphemeralState>>,
    terminate: AtomicBool,
    terminate_ready: AtomicBool,
}

impl ServerState {
    fn respond(&self, request: Request) -> Response {
        match request {
            Request::Ping => Response::Pong {
                protocol: PROTOCOL_VERSION,
            },
            Request::Autostart => {
                self.runner.autostart();
                Response::Ok
            }
            Request::StartAll => {
                self.runner.start_all();
                Response::Ok
            }
            Request::Start { id } => result_response(self.runner.start(&id)),
            Request::Stop { id } => result_response(self.runner.stop(&id)),
            Request::ForceStart { id } => result_response(self.runner.force_start(&id)),
            Request::Restart { id } => result_response(self.runner.restart(&id)),
            Request::Shutdown { force } => {
                self.runner.shutdown(force);
                Response::Ok
            }
            Request::ActiveCount => Response::Count(self.runner.active_count()),
            Request::ActiveCommandCount => Response::Count(self.runner.active_command_count()),
            Request::CommandCount => Response::Count(self.runner.command_count()),
            Request::ConfiguredCommands => {
                let ephemeral_ids = self
                    .ephemerals
                    .lock()
                    .map(|ephemerals| ephemerals.keys().cloned().collect::<Vec<_>>())
                    .unwrap_or_default();
                Response::Commands(
                    self.runner
                        .command_configs()
                        .into_iter()
                        .filter(|command| !ephemeral_ids.contains(&command.id))
                        .collect(),
                )
            }
            Request::AddCommand { config } => result_response(self.runner.add_command(config)),
            Request::AddEphemeral { state } => {
                let id = state.config.id.clone();
                match self.runner.add_command(state.config.clone()) {
                    Ok(()) => match self.ephemerals.lock() {
                        Ok(mut ephemerals) => {
                            ephemerals.insert(id, state);
                            Response::Ok
                        }
                        Err(_) => Response::Error {
                            message: "Blade ephemeral state lock is poisoned".to_owned(),
                        },
                    },
                    Err(error) => Response::Error {
                        message: format!("{error:#}"),
                    },
                }
            }
            Request::UpdateCommand { config } => {
                let id = config.id.clone();
                match self.runner.update_command(config.clone()) {
                    Ok(()) => {
                        if let Ok(mut ephemerals) = self.ephemerals.lock()
                            && let Some(ephemeral) = ephemerals.get_mut(&id)
                        {
                            ephemeral.config = config;
                        }
                        Response::Ok
                    }
                    Err(error) => Response::Error {
                        message: format!("{error:#}"),
                    },
                }
            }
            Request::RemoveCommand { id } => match self.runner.remove_command(&id) {
                Ok(()) => {
                    if let Ok(mut ephemerals) = self.ephemerals.lock() {
                        ephemerals.remove(&id);
                    }
                    Response::Ok
                }
                Err(error) => Response::Error {
                    message: format!("{error:#}"),
                },
            },
            Request::MarkEphemeralPersisted { id } => match self.ephemerals.lock() {
                Ok(mut ephemerals) => {
                    ephemerals.remove(&id);
                    Response::Ok
                }
                Err(_) => Response::Error {
                    message: "Blade ephemeral state lock is poisoned".to_owned(),
                },
            },
            Request::Snapshot { id } => Response::Snapshot(
                self.runner
                    .snapshot(&id)
                    .map(RuntimeSnapshotWire::from_runtime),
            ),
            Request::Logs { id } => Response::Logs(self.runner.logs(&id)),
            Request::LogUpdate { id, after } => {
                Response::LogUpdate(self.runner.log_update(&id, after))
            }
            Request::ClearLogs { id } => result_response(self.runner.clear_logs(&id)),
            Request::Ephemerals => match self.ephemerals.lock() {
                Ok(ephemerals) => Response::Ephemerals(ephemerals.values().cloned().collect()),
                Err(_) => Response::Error {
                    message: "Blade ephemeral state lock is poisoned".to_owned(),
                },
            },
            Request::TerminateSession => {
                self.runner.shutdown(true);
                self.terminate.store(true, Ordering::Release);
                Response::Ok
            }
        }
    }
}

fn result_response(result: Result<()>) -> Response {
    match result {
        Ok(()) => Response::Ok,
        Err(error) => Response::Error {
            message: format!("{error:#}"),
        },
    }
}

pub fn descriptor_for(source: SessionSource, name: String) -> Result<SessionDescriptor> {
    let runtime = runtime_directory()?;
    let encoded = serde_json::to_vec(&source).context("could not identify Blade session source")?;
    let id = format!("{:016x}", fnv1a(&encoded));
    Ok(SessionDescriptor {
        protocol: PROTOCOL_VERSION,
        socket: runtime.join(format!("{id}.sock")),
        id,
        name,
        source,
    })
}

pub fn descriptor_path(id: &str) -> Result<PathBuf> {
    Ok(runtime_directory()?.join(format!("{id}.session.toml")))
}

pub fn load_descriptor(path: &Path) -> Result<SessionDescriptor> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("could not read Blade session {}", path.display()))?;
    let descriptor: SessionDescriptor = toml::from_str(&text)
        .with_context(|| format!("could not parse Blade session {}", path.display()))?;
    if descriptor.protocol != PROTOCOL_VERSION {
        bail!(
            "Blade session {} uses protocol {}, expected {}",
            descriptor.id,
            descriptor.protocol,
            PROTOCOL_VERSION
        );
    }
    Ok(descriptor)
}

pub fn find_descriptor(query: Option<&str>) -> Result<SessionDescriptor> {
    let runtime = runtime_directory()?;
    let mut sessions = fs::read_dir(&runtime)
        .with_context(|| format!("could not list Blade sessions in {}", runtime.display()))?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.ends_with(".session.toml"))
        })
        .filter_map(|entry| load_descriptor(&entry.path()).ok())
        .filter(|session| UnixStream::connect(&session.socket).is_ok())
        .filter(|session| {
            query.is_none_or(|query| {
                session.id == query
                    || session.id.starts_with(query)
                    || session.name.eq_ignore_ascii_case(query)
            })
        })
        .collect::<Vec<_>>();
    sessions.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
    match sessions.len() {
        0 => bail!("no matching detached Blade session was found"),
        1 => Ok(sessions.remove(0)),
        _ => {
            let choices = sessions
                .iter()
                .map(|session| format!("{} ({})", session.name, session.id))
                .collect::<Vec<_>>()
                .join(", ");
            bail!("multiple Blade sessions match; choose one by id: {choices}")
        }
    }
}

pub fn connect_existing(
    project: ProjectConfig,
    descriptor: SessionDescriptor,
) -> Result<RunnerHandle> {
    Ok(RunnerHandle::Remote(SupervisorClient::connect(
        project, descriptor, None,
    )?))
}

pub fn connect_or_spawn(
    project: ProjectConfig,
    descriptor: SessionDescriptor,
) -> Result<(RunnerHandle, bool)> {
    if let Ok(client) = SupervisorClient::connect(project.clone(), descriptor.clone(), None) {
        return Ok((RunnerHandle::Remote(client), false));
    }

    let path = descriptor_path(&descriptor.id)?;
    let encoded =
        toml::to_string_pretty(&descriptor).context("could not encode Blade session descriptor")?;
    fs::write(&path, encoded)
        .with_context(|| format!("could not write Blade session {}", path.display()))?;

    let executable = env::current_exe().context("could not locate the running Blade binary")?;
    let mut command = Command::new(executable);
    command
        .arg("__supervisor")
        .arg("--session-file")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: pre_exec runs after fork in the single-threaded child and setsid
    // is async-signal-safe. A separate session prevents the supervisor from
    // receiving the TUI terminal's SIGHUP when its client detaches or crashes.
    unsafe {
        command.pre_exec(|| {
            if nix::libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .context("could not start the Blade supervisor")?;
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        match SupervisorClient::connect(project.clone(), descriptor.clone(), None) {
            Ok(mut client) => {
                Arc::get_mut(&mut client.inner)
                    .expect("new supervisor client must have unique state")
                    .child = Mutex::new(Some(child));
                return Ok((RunnerHandle::Remote(client), true));
            }
            Err(error) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("Blade supervisor did not become ready");
            }
            Err(_) => {
                if let Some(status) = child
                    .try_wait()
                    .context("could not inspect the Blade supervisor")?
                {
                    bail!("Blade supervisor exited before becoming ready ({status})");
                }
                thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

pub fn serve(project: ProjectConfig, descriptor: SessionDescriptor) -> Result<()> {
    if descriptor.socket.exists() {
        if UnixStream::connect(&descriptor.socket).is_ok() {
            bail!("Blade session {} is already running", descriptor.id);
        }
        fs::remove_file(&descriptor.socket).with_context(|| {
            format!(
                "could not remove stale Blade socket {}",
                descriptor.socket.display()
            )
        })?;
    }
    let listener = UnixListener::bind(&descriptor.socket).with_context(|| {
        format!(
            "could not bind Blade session socket {}",
            descriptor.socket.display()
        )
    })?;
    listener
        .set_nonblocking(true)
        .context("could not configure Blade session socket")?;
    let _cleanup = SessionCleanup {
        socket: descriptor.socket.clone(),
        descriptor: descriptor_path(&descriptor.id)?,
    };
    let state = Arc::new(ServerState {
        runner: Runner::new(project),
        ephemerals: Mutex::new(HashMap::new()),
        terminate: AtomicBool::new(false),
        terminate_ready: AtomicBool::new(false),
    });
    loop {
        if state.terminate_ready.load(Ordering::Acquire) {
            return Ok(());
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let state = Arc::clone(&state);
                thread::spawn(move || {
                    handle_client(&state, stream);
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error).context("could not accept Blade session client"),
        }
    }
}

fn handle_client(state: &ServerState, stream: UnixStream) {
    let mut reader = BufReader::new(stream);
    loop {
        let mut request = String::new();
        match reader.read_line(&mut request) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let response = match serde_json::from_str(&request) {
            Ok(request) => state.respond(request),
            Err(error) => Response::Error {
                message: format!("invalid supervisor request: {error}"),
            },
        };
        if serde_json::to_writer(reader.get_mut(), &response).is_err()
            || reader.get_mut().write_all(b"\n").is_err()
            || reader.get_mut().flush().is_err()
        {
            return;
        }
        if state.terminate.load(Ordering::Acquire) {
            state.terminate_ready.store(true, Ordering::Release);
            return;
        }
    }
}

pub fn exec_attach(executable: &Path, session_id: &str) -> Result<()> {
    let error = Command::new(executable)
        .arg("attach")
        .arg(session_id)
        .exec();
    Err(error).context("could not reload Blade")
}

fn runtime_directory() -> Result<PathBuf> {
    let directory = env::var_os("XDG_RUNTIME_DIR")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // SAFETY: geteuid has no preconditions and does not retain memory.
            let uid = unsafe { nix::libc::geteuid() };
            env::temp_dir().join(format!("blade-{uid}"))
        })
        .join("blade");
    if directory.exists() {
        if !directory.is_dir() {
            bail!(
                "Blade runtime path {} is not a directory",
                directory.display()
            );
        }
    } else {
        fs::create_dir_all(&directory).with_context(|| {
            format!(
                "could not create Blade runtime directory {}",
                directory.display()
            )
        })?;
    }
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).with_context(|| {
        format!(
            "could not secure Blade runtime directory {}",
            directory.display()
        )
    })?;
    Ok(directory)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

struct SessionCleanup {
    socket: PathBuf,
    descriptor: PathBuf,
}

impl Drop for SessionCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket);
        let _ = fs::remove_file(&self.descriptor);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::Mutex,
        thread,
        time::{Duration, Instant},
    };

    use tempfile::tempdir;

    use crate::{config::validate_file, runner::Runner};

    use super::{EphemeralState, Request, Response, RuntimeSnapshotWire, ServerState};

    #[test]
    fn snapshots_from_older_supervisors_without_log_metadata_remain_compatible() {
        let snapshot: RuntimeSnapshotWire = serde_json::from_str(
            r#"{
                "state": "Running",
                "pid": 42,
                "exit_code": null,
                "stop_level": 0,
                "stop_remaining_millis": null
            }"#,
        )
        .unwrap();

        assert!(snapshot.log_metadata.is_none());
    }

    #[test]
    fn supervisor_state_retains_a_live_ephemeral_process_and_its_logs_between_clients() {
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
        let state = ServerState {
            runner: Runner::new(project.clone()),
            ephemerals: Mutex::new(Default::default()),
            terminate: Default::default(),
            terminate_ready: Default::default(),
        };
        let mut config = project.command("base").unwrap().clone();
        config.id = "__ephemeral::1".to_owned();
        config.name = "live scratch".to_owned();
        config.run = "echo REATTACHED_LOG; sleep 2".to_owned();
        config.actions.clear();
        assert!(matches!(
            state.respond(Request::AddEphemeral {
                state: EphemeralState {
                    config: config.clone(),
                    project: None,
                    suggested_group: "Project".to_owned(),
                },
            }),
            Response::Ok
        ));
        assert!(matches!(
            state.respond(Request::Start {
                id: config.id.clone()
            }),
            Response::Ok
        ));
        wait_until(&|| {
            matches!(
                state.respond(Request::Snapshot {
                    id: config.id.clone()
                }),
                Response::Snapshot(Some(snapshot)) if snapshot.pid.is_some()
            )
        });

        assert!(matches!(
            state.respond(Request::Snapshot {
                id: config.id.clone()
            }),
            Response::Snapshot(Some(snapshot)) if snapshot.state.is_active()
        ));
        assert!(matches!(
            state.respond(Request::Ephemerals),
            Response::Ephemerals(commands) if commands[0].config.name == "live scratch"
        ));
        wait_until(&|| {
            matches!(
                state.respond(Request::Logs {
                    id: config.id.clone()
                }),
                Response::Logs(logs) if logs.iter().any(|line| line.text == "REATTACHED_LOG")
            )
        });
        assert!(matches!(
            state.respond(Request::LogUpdate {
                id: config.id.clone(),
                after: None,
            }),
            Response::LogUpdate(Some(update))
                if update.metadata.count > 0
                    && update.lines.iter().any(|line| line.text == "REATTACHED_LOG")
        ));
        assert!(matches!(
            state.respond(Request::Stop {
                id: config.id.clone()
            }),
            Response::Ok
        ));
        wait_until(&|| {
            matches!(
                state.respond(Request::Snapshot {
                    id: config.id.clone()
                }),
                Response::Snapshot(Some(snapshot)) if !snapshot.state.is_active()
            )
        });
        assert!(matches!(
            state.respond(Request::TerminateSession),
            Response::Ok
        ));
    }

    fn wait_until(condition: &impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !condition() {
            assert!(Instant::now() < deadline, "condition did not become true");
            thread::sleep(Duration::from_millis(10));
        }
    }
}
