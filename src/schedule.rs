use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    fs::{self, File, OpenOptions},
    io::{self, ErrorKind, Write},
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::fsx::{remove_file_if_unchanged, replace_file_if_unchanged};

pub const LAUNCH_AGENT_LABEL: &str = "com.agent-sync.sync";
pub const DEFAULT_INTERVAL_SECONDS: u64 = 24 * 60 * 60;

const MIN_INTERVAL_SECONDS: u64 = 5 * 60;
const MANAGED_MARKER: &str = "agent-sync-schedule-v1";
const OWNERSHIP_STATE_MAGIC: &[u8] = b"agent-sync-schedule-state-v1\n";
const PLIST_NAME: &str = "com.agent-sync.sync.plist";
const STATE_NAME: &str = "schedule-state-v1";
const STDOUT_LOG_NAME: &str = "sync.stdout.log";
const STDERR_LOG_NAME: &str = "sync.stderr.log";
static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);

pub type ScheduleResult<T> = Result<T, ScheduleError>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScheduleError {
    message: String,
}

impl ScheduleError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for ScheduleError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ScheduleError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScheduleSpec {
    home: PathBuf,
    executable: PathBuf,
    global_arguments: Vec<String>,
    environment_path: Vec<PathBuf>,
    interval_seconds: u64,
}

impl ScheduleSpec {
    pub fn new(home: impl Into<PathBuf>, executable: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            executable: executable.into(),
            global_arguments: Vec::new(),
            environment_path: vec![
                PathBuf::from("/opt/homebrew/bin"),
                PathBuf::from("/usr/local/bin"),
                PathBuf::from("/usr/bin"),
                PathBuf::from("/bin"),
                PathBuf::from("/usr/sbin"),
                PathBuf::from("/sbin"),
            ],
            interval_seconds: DEFAULT_INTERVAL_SECONDS,
        }
    }

    pub fn with_global_arguments(mut self, global_arguments: Vec<String>) -> Self {
        self.global_arguments = global_arguments;
        self
    }

    pub fn with_environment_path(mut self, environment_path: Vec<PathBuf>) -> Self {
        self.environment_path = environment_path;
        self
    }

    pub fn with_interval_seconds(mut self, interval_seconds: u64) -> Self {
        self.interval_seconds = interval_seconds;
        self
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub const fn interval_seconds(&self) -> u64 {
        self.interval_seconds
    }

    pub fn plist_path(&self) -> PathBuf {
        self.home
            .join("Library")
            .join("LaunchAgents")
            .join(PLIST_NAME)
    }

    pub fn log_dir(&self) -> PathBuf {
        self.home.join(".agent-sync").join("logs")
    }

    pub fn stdout_log_path(&self) -> PathBuf {
        self.log_dir().join(STDOUT_LOG_NAME)
    }

    pub fn stderr_log_path(&self) -> PathBuf {
        self.log_dir().join(STDERR_LOG_NAME)
    }

    pub fn ownership_state_path(&self) -> PathBuf {
        self.home.join(".agent-sync").join("state").join(STATE_NAME)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScheduleOperation {
    Status,
    Install,
    Uninstall,
}

impl Display for ScheduleOperation {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Status => "status",
            Self::Install => "install",
            Self::Uninstall => "uninstall",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScheduleAction {
    Add,
    Update,
    Activate,
    Remove,
    Unchanged,
    Conflict,
}

impl Display for ScheduleAction {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Add => "add",
            Self::Update => "update",
            Self::Activate => "activate",
            Self::Remove => "remove",
            Self::Unchanged => "unchanged",
            Self::Conflict => "conflict",
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScheduleReport {
    pub operation: ScheduleOperation,
    pub action: ScheduleAction,
    pub dry_run: bool,
    pub healthy: bool,
    pub loaded: bool,
    pub plist_path: PathBuf,
    pub log_dir: PathBuf,
    pub backup: Option<PathBuf>,
    pub detail: String,
    pub next_action: String,
}

impl ScheduleReport {
    pub fn to_text(&self) -> String {
        let mode = if self.dry_run { " preview" } else { "" };
        let mut output = format!(
            "Schedule {}{mode}: {}\nLaunchAgent: {}\nStatus: {}\nLogs: {}\n",
            self.operation,
            self.action,
            self.plist_path.display(),
            self.detail,
            self.log_dir.display()
        );
        if let Some(backup) = &self.backup {
            output.push_str(&format!("Backup: {}\n", backup.display()));
        }
        output.push_str(&format!("Next action: {}\n", self.next_action));
        output
    }
}

/// Abstracts launchd activation so schedule rendering and file ownership can be
/// tested without invoking the user's real launchd session.
pub trait LaunchAgentController {
    fn is_loaded(&mut self, label: &str) -> ScheduleResult<bool>;

    /// Returns `None` when the label is not loaded. A loaded job returns
    /// `Some(true)` only when its plist path, program, and arguments match.
    fn loaded_job_matches(&mut self, _spec: &ScheduleSpec) -> ScheduleResult<Option<bool>> {
        self.is_loaded(LAUNCH_AGENT_LABEL)
            .map(|loaded| loaded.then_some(true))
    }

    fn bootstrap(&mut self, plist_path: &Path) -> ScheduleResult<()>;
    fn bootout(&mut self, label: &str) -> ScheduleResult<()>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SystemLaunchAgentController {
    domain: String,
    launchctl: PathBuf,
}

impl SystemLaunchAgentController {
    pub fn for_current_user() -> ScheduleResult<Self> {
        let output = Command::new("/usr/bin/id")
            .arg("-u")
            .output()
            .map_err(|error| schedule_io_error("run /usr/bin/id -u", error))?;
        if !output.status.success() {
            return Err(command_error("/usr/bin/id -u", &output));
        }
        let uid = String::from_utf8(output.stdout)
            .map_err(|error| ScheduleError::new(format!("read user id: {error}")))?;
        let uid = uid.trim();
        if uid.is_empty() || !uid.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(ScheduleError::new(format!(
                "/usr/bin/id -u returned an invalid user id: {uid:?}"
            )));
        }
        Ok(Self {
            domain: format!("gui/{uid}"),
            launchctl: PathBuf::from("/bin/launchctl"),
        })
    }

    fn run(&self, arguments: &[&str]) -> ScheduleResult<Output> {
        Command::new(&self.launchctl)
            .args(arguments)
            .output()
            .map_err(|error| {
                schedule_io_error(
                    &format!("run {} {}", self.launchctl.display(), arguments.join(" ")),
                    error,
                )
            })
    }

    fn print_loaded_job(&self, label: &str) -> ScheduleResult<Option<Output>> {
        let service = format!("{}/{label}", self.domain);
        let output = self.run(&["print", &service])?;
        if output.status.success() {
            return Ok(Some(output));
        }
        let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
        if output.status.code() == Some(113)
            || stderr.contains("could not find service")
            || stderr.contains("service not found")
            || stderr.contains("could not find specified service")
        {
            return Ok(None);
        }
        Err(command_error(
            &format!("{} print {service}", self.launchctl.display()),
            &output,
        ))
    }
}

impl LaunchAgentController for SystemLaunchAgentController {
    fn is_loaded(&mut self, label: &str) -> ScheduleResult<bool> {
        self.print_loaded_job(label).map(|output| output.is_some())
    }

    fn loaded_job_matches(&mut self, spec: &ScheduleSpec) -> ScheduleResult<Option<bool>> {
        let Some(output) = self.print_loaded_job(LAUNCH_AGENT_LABEL)? else {
            return Ok(None);
        };
        loaded_job_matches_spec(&output.stdout, spec).map(Some)
    }

    fn bootstrap(&mut self, plist_path: &Path) -> ScheduleResult<()> {
        let plist = path_text(plist_path, "LaunchAgent path")?;
        let output = self.run(&["bootstrap", &self.domain, plist])?;
        if output.status.success() {
            Ok(())
        } else {
            Err(command_error(
                &format!(
                    "{} bootstrap {} {}",
                    self.launchctl.display(),
                    self.domain,
                    plist
                ),
                &output,
            ))
        }
    }

    fn bootout(&mut self, label: &str) -> ScheduleResult<()> {
        let service = format!("{}/{label}", self.domain);
        let output = self.run(&["bootout", &service])?;
        if output.status.success() {
            Ok(())
        } else {
            Err(command_error(
                &format!("{} bootout {service}", self.launchctl.display()),
                &output,
            ))
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct LoadedJobIdentity {
    plist_path: String,
    program: String,
    arguments: Vec<String>,
}

fn loaded_job_matches_spec(output: &[u8], spec: &ScheduleSpec) -> ScheduleResult<bool> {
    let actual = parse_loaded_job_identity(output)?;
    let expected = LoadedJobIdentity {
        plist_path: path_text(&spec.plist_path(), "loaded LaunchAgent path")?.to_string(),
        program: path_text(&spec.executable, "loaded agent-sync executable")?.to_string(),
        arguments: expected_program_arguments(spec)?,
    };
    Ok(actual == expected)
}

fn expected_program_arguments(spec: &ScheduleSpec) -> ScheduleResult<Vec<String>> {
    let mut arguments = Vec::with_capacity(spec.global_arguments.len() + 4);
    arguments.push(path_text(&spec.executable, "agent-sync executable")?.to_string());
    arguments.extend(spec.global_arguments.iter().cloned());
    arguments.extend(["sync", "--yes", "--automation"].map(str::to_string));
    Ok(arguments)
}

fn parse_loaded_job_identity(output: &[u8]) -> ScheduleResult<LoadedJobIdentity> {
    let output = std::str::from_utf8(output)
        .map_err(|error| ScheduleError::new(format!("read loaded LaunchAgent: {error}")))?;
    let mut plist_path = None;
    let mut program = None;
    let mut arguments = None;
    let mut lines = output.lines();
    while let Some(line) = lines.next() {
        let line = line.trim();
        if plist_path.is_none() {
            plist_path = line.strip_prefix("path = ").map(str::to_string);
        }
        if program.is_none() {
            program = line.strip_prefix("program = ").map(str::to_string);
        }
        if arguments.is_none() && line == "arguments = {" {
            arguments = Some(parse_launchctl_block(&mut lines, "arguments")?);
        }
    }
    Ok(LoadedJobIdentity {
        plist_path: required_launchctl_value(plist_path, "path")?,
        program: required_launchctl_value(program, "program")?,
        arguments: required_launchctl_value(arguments, "arguments")?,
    })
}

fn parse_launchctl_block<'a>(
    lines: &mut impl Iterator<Item = &'a str>,
    label: &str,
) -> ScheduleResult<Vec<String>> {
    let mut values = Vec::new();
    for line in lines {
        let line = line.trim();
        if line == "}" {
            return Ok(values);
        }
        if !line.is_empty() {
            values.push(line.to_string());
        }
    }
    Err(ScheduleError::new(format!(
        "loaded LaunchAgent {label} block is incomplete"
    )))
}

fn required_launchctl_value<T>(value: Option<T>, label: &str) -> ScheduleResult<T> {
    value.ok_or_else(|| {
        ScheduleError::new(format!(
            "loaded LaunchAgent does not report its {label}; schedule identity cannot be verified"
        ))
    })
}

pub fn render_launch_agent(spec: &ScheduleSpec) -> ScheduleResult<Vec<u8>> {
    validate_spec(spec)?;
    let executable = xml_escape(path_text(&spec.executable, "agent-sync executable")?);
    let home = xml_escape(path_text(&spec.home, "home directory")?);
    let stdout = xml_escape(path_text(&spec.stdout_log_path(), "standard output log")?);
    let stderr = xml_escape(path_text(&spec.stderr_log_path(), "standard error log")?);
    let environment_path = std::env::join_paths(&spec.environment_path)
        .map_err(|error| ScheduleError::new(format!("build LaunchAgent PATH: {error}")))?;
    let environment_path = environment_path.to_str().ok_or_else(|| {
        ScheduleError::new(format!(
            "LaunchAgent PATH is not valid UTF-8: {environment_path:?}"
        ))
    })?;
    let environment_path = xml_escape(environment_path);
    let global_arguments = spec
        .global_arguments
        .iter()
        .map(|argument| format!("    <string>{}</string>\n", xml_escape(argument)))
        .collect::<String>();
    Ok(format!(
        concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
            "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" ",
            "\"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
            "<plist version=\"1.0\">\n",
            "<dict>\n",
            "  <!-- {marker} -->\n",
            "  <key>Label</key>\n",
            "  <string>{label}</string>\n",
            "  <key>ProgramArguments</key>\n",
            "  <array>\n",
            "    <string>{executable}</string>\n",
            "{global_arguments}",
            "    <string>sync</string>\n",
            "    <string>--yes</string>\n",
            "    <string>--automation</string>\n",
            "  </array>\n",
            "  <key>EnvironmentVariables</key>\n",
            "  <dict>\n",
            "    <key>HOME</key>\n",
            "    <string>{home}</string>\n",
            "    <key>PATH</key>\n",
            "    <string>{environment_path}</string>\n",
            "  </dict>\n",
            "  <key>ProcessType</key>\n",
            "  <string>Background</string>\n",
            "  <key>StartInterval</key>\n",
            "  <integer>{interval}</integer>\n",
            "  <key>StandardOutPath</key>\n",
            "  <string>{stdout}</string>\n",
            "  <key>StandardErrorPath</key>\n",
            "  <string>{stderr}</string>\n",
            "</dict>\n",
            "</plist>\n"
        ),
        marker = MANAGED_MARKER,
        label = LAUNCH_AGENT_LABEL,
        executable = executable,
        global_arguments = global_arguments,
        home = home,
        environment_path = environment_path,
        interval = spec.interval_seconds,
        stdout = stdout,
        stderr = stderr,
    )
    .into_bytes())
}

pub fn schedule_status<C: LaunchAgentController>(
    spec: &ScheduleSpec,
    controller: &mut C,
) -> ScheduleResult<ScheduleReport> {
    let desired = render_launch_agent(spec)?;
    let inspection = inspect_schedule(spec, &desired)?;
    let loaded_job = controller.loaded_job_matches(spec)?;
    Ok(status_report(spec, inspection.ownership, loaded_job))
}

pub fn install_schedule<C: LaunchAgentController>(
    spec: &ScheduleSpec,
    controller: &mut C,
    dry_run: bool,
) -> ScheduleResult<ScheduleReport> {
    let desired = render_launch_agent(spec)?;
    let inspection = inspect_schedule(spec, &desired)?;
    let loaded_job = controller.loaded_job_matches(spec)?;
    let action = install_action(&inspection.ownership, loaded_job);
    if action == ScheduleAction::Conflict {
        return Err(conflict_error(spec, &inspection.ownership));
    }
    if dry_run {
        return Ok(install_report(
            spec,
            action,
            true,
            loaded_job.is_some(),
            None,
        ));
    }
    if action == ScheduleAction::Unchanged {
        return Ok(install_report(spec, action, false, true, None));
    }

    let fresh = inspect_schedule(spec, &desired)?;
    let fresh_loaded_job = controller.loaded_job_matches(spec)?;
    let fresh_action = install_action(&fresh.ownership, fresh_loaded_job);
    if fresh_action != action || fresh_loaded_job != loaded_job {
        return Err(ScheduleError::new(
            "the LaunchAgent changed after inspection; next action: preview schedule installation again",
        ));
    }
    if action == ScheduleAction::Activate {
        controller.bootstrap(&spec.plist_path())?;
        if let Err((error, should_unload)) = verify_loaded_job_after_bootstrap(spec, controller) {
            let cleanup = if should_unload {
                controller.bootout(LAUNCH_AGENT_LABEL)
            } else {
                Ok(())
            };
            return Err(activation_verification_failure(error, cleanup));
        }
        return Ok(install_report(spec, action, false, true, None));
    }
    let backup = apply_install(
        spec,
        controller,
        &desired,
        fresh,
        fresh_loaded_job.is_some(),
    )?;
    Ok(install_report(spec, action, false, true, backup))
}

pub fn uninstall_schedule<C: LaunchAgentController>(
    spec: &ScheduleSpec,
    controller: &mut C,
    dry_run: bool,
) -> ScheduleResult<ScheduleReport> {
    let desired = render_launch_agent(spec)?;
    let inspection = inspect_schedule(spec, &desired)?;
    let loaded_job = controller.loaded_job_matches(spec)?;
    let action = uninstall_action(&inspection.ownership, loaded_job.is_some());
    if action == ScheduleAction::Conflict {
        return Err(conflict_error(spec, &inspection.ownership));
    }
    if dry_run {
        return Ok(uninstall_report(
            spec,
            action,
            true,
            loaded_job.is_some(),
            None,
        ));
    }
    if action == ScheduleAction::Unchanged {
        return Ok(uninstall_report(spec, action, false, false, None));
    }

    let fresh = inspect_schedule(spec, &desired)?;
    let fresh_loaded_job = controller.loaded_job_matches(spec)?;
    let fresh_action = uninstall_action(&fresh.ownership, fresh_loaded_job.is_some());
    if fresh_action != action || fresh_loaded_job != loaded_job {
        return Err(ScheduleError::new(
            "the LaunchAgent changed after inspection; next action: preview schedule removal again",
        ));
    }
    let backup = apply_uninstall(spec, controller, fresh, fresh_loaded_job.is_some())?;
    Ok(uninstall_report(spec, action, false, false, backup))
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Ownership {
    Absent,
    ManagedCurrent,
    ManagedOutdated,
    ManagedMissing,
    Unmanaged,
    Modified,
    Unsafe(String),
}

#[derive(Clone, Debug)]
struct Inspection {
    ownership: Ownership,
    plist: Option<Vec<u8>>,
    state: Option<Vec<u8>>,
}

enum ReadPath {
    Missing,
    File(Vec<u8>),
    Unsafe(String),
}

fn inspect_schedule(spec: &ScheduleSpec, desired: &[u8]) -> ScheduleResult<Inspection> {
    let plist_path = spec.plist_path();
    let state_path = spec.ownership_state_path();
    let plist = read_safe_path(&plist_path, "LaunchAgent")?;
    let state = read_safe_path(&state_path, "schedule ownership state")?;

    if let ReadPath::Unsafe(reason) = &state {
        return Ok(Inspection {
            ownership: Ownership::Unsafe(reason.clone()),
            plist: read_file_value(plist),
            state: None,
        });
    }
    if let ReadPath::Unsafe(reason) = &plist {
        return Ok(Inspection {
            ownership: Ownership::Unsafe(reason.clone()),
            plist: None,
            state: read_file_value(state),
        });
    }

    let plist = read_file_value(plist);
    let state = read_file_value(state);
    let ownership = match (&plist, &state) {
        (None, None) => Ownership::Absent,
        (Some(_), None) => Ownership::Unmanaged,
        (plist, Some(state)) => match decode_ownership_state(state) {
            Ok(installed) => match plist {
                None => Ownership::ManagedMissing,
                Some(current) if current != installed => Ownership::Modified,
                Some(current) if current == desired => Ownership::ManagedCurrent,
                Some(_) => Ownership::ManagedOutdated,
            },
            Err(error) => Ownership::Unsafe(error.to_string()),
        },
    };
    Ok(Inspection {
        ownership,
        plist,
        state,
    })
}

fn read_file_value(path: ReadPath) -> Option<Vec<u8>> {
    match path {
        ReadPath::File(content) => Some(content),
        ReadPath::Missing | ReadPath::Unsafe(_) => None,
    }
}

fn install_action(ownership: &Ownership, loaded_job: Option<bool>) -> ScheduleAction {
    match (ownership, loaded_job) {
        (Ownership::Absent, None) | (Ownership::ManagedMissing, None) => ScheduleAction::Add,
        (Ownership::ManagedMissing, Some(_)) => ScheduleAction::Update,
        (Ownership::ManagedCurrent, Some(true)) => ScheduleAction::Unchanged,
        (Ownership::ManagedCurrent, None) => ScheduleAction::Activate,
        (Ownership::ManagedCurrent, Some(false)) => ScheduleAction::Update,
        (Ownership::ManagedOutdated, _) => ScheduleAction::Update,
        _ => ScheduleAction::Conflict,
    }
}

fn uninstall_action(ownership: &Ownership, loaded: bool) -> ScheduleAction {
    match (ownership, loaded) {
        (Ownership::Absent, false) => ScheduleAction::Unchanged,
        (Ownership::ManagedCurrent | Ownership::ManagedOutdated, _) => ScheduleAction::Remove,
        (Ownership::ManagedMissing, _) => ScheduleAction::Remove,
        _ => ScheduleAction::Conflict,
    }
}

fn status_report(
    spec: &ScheduleSpec,
    ownership: Ownership,
    loaded_job: Option<bool>,
) -> ScheduleReport {
    let action = install_action(&ownership, loaded_job);
    let (healthy, detail, next_action) = match action {
        ScheduleAction::Unchanged => (
            true,
            "the managed LaunchAgent is installed and loaded".to_string(),
            "none".to_string(),
        ),
        ScheduleAction::Add => (
            false,
            ownership_detail(&ownership, loaded_job),
            "run `agent-sync schedule install` to preview installation".to_string(),
        ),
        ScheduleAction::Update | ScheduleAction::Activate => (
            false,
            ownership_detail(&ownership, loaded_job),
            "run `agent-sync schedule install` to preview repair".to_string(),
        ),
        ScheduleAction::Conflict => (
            false,
            ownership_detail(&ownership, loaded_job),
            format!(
                "inspect {}; agent-sync will not replace an unowned or modified LaunchAgent",
                spec.plist_path().display()
            ),
        ),
        ScheduleAction::Remove => unreachable!("install status never plans removal"),
    };
    ScheduleReport {
        operation: ScheduleOperation::Status,
        action,
        dry_run: false,
        healthy,
        loaded: loaded_job.is_some(),
        plist_path: spec.plist_path(),
        log_dir: spec.log_dir(),
        backup: None,
        detail,
        next_action,
    }
}

fn install_report(
    spec: &ScheduleSpec,
    action: ScheduleAction,
    dry_run: bool,
    loaded: bool,
    backup: Option<PathBuf>,
) -> ScheduleReport {
    let applied = !dry_run;
    let detail = match (action, applied) {
        (ScheduleAction::Add, false) => "the managed LaunchAgent will be added",
        (ScheduleAction::Add, true) => "the managed LaunchAgent was added and loaded",
        (ScheduleAction::Update, false) => "the managed LaunchAgent will be updated",
        (ScheduleAction::Update, true) => "the managed LaunchAgent was updated and loaded",
        (ScheduleAction::Activate, false) => "the managed LaunchAgent will be loaded",
        (ScheduleAction::Activate, true) => "the managed LaunchAgent was loaded",
        (ScheduleAction::Unchanged, _) => "the managed LaunchAgent is installed and loaded",
        _ => "the schedule needs attention",
    }
    .to_string();
    ScheduleReport {
        operation: ScheduleOperation::Install,
        action,
        dry_run,
        healthy: applied || action == ScheduleAction::Unchanged,
        loaded: if applied { true } else { loaded },
        plist_path: spec.plist_path(),
        log_dir: spec.log_dir(),
        backup,
        detail,
        next_action: if dry_run && action != ScheduleAction::Unchanged {
            "run `agent-sync schedule install --yes` to apply this plan".to_string()
        } else {
            "none".to_string()
        },
    }
}

fn uninstall_report(
    spec: &ScheduleSpec,
    action: ScheduleAction,
    dry_run: bool,
    loaded: bool,
    backup: Option<PathBuf>,
) -> ScheduleReport {
    let detail = match (action, dry_run) {
        (ScheduleAction::Remove, true) => "the managed LaunchAgent will be unloaded and removed",
        (ScheduleAction::Remove, false) => {
            "the managed LaunchAgent was unloaded and removed; logs were preserved"
        }
        (ScheduleAction::Unchanged, _) => "no managed LaunchAgent is installed",
        _ => "the schedule needs attention",
    }
    .to_string();
    ScheduleReport {
        operation: ScheduleOperation::Uninstall,
        action,
        dry_run,
        healthy: !dry_run || action == ScheduleAction::Unchanged,
        loaded: if dry_run { loaded } else { false },
        plist_path: spec.plist_path(),
        log_dir: spec.log_dir(),
        backup,
        detail,
        next_action: if dry_run && action == ScheduleAction::Remove {
            "run `agent-sync schedule uninstall --yes` to apply this plan".to_string()
        } else {
            "none".to_string()
        },
    }
}

fn ownership_detail(ownership: &Ownership, loaded_job: Option<bool>) -> String {
    if loaded_job == Some(false) {
        return "the loaded LaunchAgent does not match the managed plist path, program, and arguments"
            .to_string();
    }
    let loaded = loaded_job.is_some();
    match ownership {
        Ownership::Absent if loaded => {
            "the label is loaded but no managed LaunchAgent file exists".to_string()
        }
        Ownership::Absent => "the managed LaunchAgent is not installed".to_string(),
        Ownership::ManagedCurrent if loaded => {
            "the managed LaunchAgent is installed and loaded".to_string()
        }
        Ownership::ManagedCurrent => {
            "the managed LaunchAgent is installed but not loaded".to_string()
        }
        Ownership::ManagedOutdated => {
            "the managed LaunchAgent does not match the current agent-sync invocation".to_string()
        }
        Ownership::ManagedMissing if loaded => {
            "the managed LaunchAgent file is missing while its label is loaded".to_string()
        }
        Ownership::ManagedMissing => {
            "the managed LaunchAgent file is missing but ownership state remains".to_string()
        }
        Ownership::Unmanaged => "an unowned LaunchAgent already uses this path".to_string(),
        Ownership::Modified => {
            "the managed LaunchAgent was modified after installation".to_string()
        }
        Ownership::Unsafe(reason) => reason.clone(),
    }
}

fn conflict_error(spec: &ScheduleSpec, ownership: &Ownership) -> ScheduleError {
    ScheduleError::new(format!(
        "{}; next action: inspect {}; agent-sync will not replace or remove it",
        ownership_detail(ownership, Some(true)),
        spec.plist_path().display()
    ))
}

fn apply_install<C: LaunchAgentController>(
    spec: &ScheduleSpec,
    controller: &mut C,
    desired: &[u8],
    inspection: Inspection,
    was_loaded: bool,
) -> ScheduleResult<Option<PathBuf>> {
    ensure_install_directories(spec)?;
    let desired_state = encode_ownership_state(desired);
    let backup = backup_inspection(spec, &inspection)?;

    if was_loaded {
        controller.bootout(LAUNCH_AGENT_LABEL)?;
    }
    let write_result = write_install_files(spec, desired, &desired_state, &inspection);
    if let Err(error) = write_result {
        return Err(rollback_after_failure(
            spec,
            controller,
            &inspection,
            desired,
            &desired_state,
            was_loaded,
            error,
        ));
    }
    if let Err(error) = controller.bootstrap(&spec.plist_path()) {
        let _ = controller.bootout(LAUNCH_AGENT_LABEL);
        return Err(rollback_after_failure(
            spec,
            controller,
            &inspection,
            desired,
            &desired_state,
            was_loaded,
            error,
        ));
    }
    if let Err((error, should_unload)) = verify_loaded_job_after_bootstrap(spec, controller) {
        return Err(rollback_after_verification_failure(
            spec,
            controller,
            VerificationRollback {
                inspection: &inspection,
                desired,
                desired_state: &desired_state,
                was_loaded,
                should_unload,
            },
            error,
        ));
    }
    Ok(backup)
}

fn apply_uninstall<C: LaunchAgentController>(
    spec: &ScheduleSpec,
    controller: &mut C,
    inspection: Inspection,
    was_loaded: bool,
) -> ScheduleResult<Option<PathBuf>> {
    ensure_install_directories(spec)?;
    let backup = backup_inspection(spec, &inspection)?;
    if was_loaded {
        controller.bootout(LAUNCH_AGENT_LABEL)?;
    }
    let remove_result = remove_install_files(spec, &inspection);
    if let Err(error) = remove_result {
        let rollback = restore_snapshots(spec, &inspection, None, None);
        let reload = if was_loaded && inspection.plist.is_some() {
            controller.bootstrap(&spec.plist_path())
        } else {
            Ok(())
        };
        return Err(combine_failures("remove schedule", error, rollback, reload));
    }
    Ok(backup)
}

fn write_install_files(
    spec: &ScheduleSpec,
    desired: &[u8],
    desired_state: &[u8],
    inspection: &Inspection,
) -> ScheduleResult<()> {
    write_from_snapshot(&spec.plist_path(), desired, inspection.plist.as_deref())?;
    if let Err(error) = write_from_snapshot(
        &spec.ownership_state_path(),
        desired_state,
        inspection.state.as_deref(),
    ) {
        let rollback = restore_one(
            &spec.plist_path(),
            inspection.plist.as_deref(),
            Some(desired),
        );
        return match rollback {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(ScheduleError::new(format!(
                "write schedule ownership state failed: {error}; plist rollback also failed: {rollback_error}"
            ))),
        };
    }
    Ok(())
}

fn remove_install_files(spec: &ScheduleSpec, inspection: &Inspection) -> ScheduleResult<()> {
    if let Some(expected) = inspection.plist.as_deref() {
        remove_exact(&spec.plist_path(), expected)?;
    }
    if let Some(expected) = inspection.state.as_deref() {
        if let Err(error) = remove_exact(&spec.ownership_state_path(), expected) {
            let rollback = restore_one(&spec.plist_path(), inspection.plist.as_deref(), None);
            return match rollback {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(ScheduleError::new(format!(
                    "remove schedule ownership state failed: {error}; plist rollback also failed: {rollback_error}"
                ))),
            };
        }
    }
    Ok(())
}

fn verify_loaded_job_after_bootstrap<C: LaunchAgentController>(
    spec: &ScheduleSpec,
    controller: &mut C,
) -> Result<(), (ScheduleError, bool)> {
    match controller.loaded_job_matches(spec) {
        Ok(Some(true)) => Ok(()),
        Ok(Some(false)) => Err((
            ScheduleError::new("loaded LaunchAgent identity verification failed after bootstrap"),
            true,
        )),
        Ok(None) => Err((
            ScheduleError::new("LaunchAgent was not loaded after bootstrap"),
            false,
        )),
        Err(error) => Err((
            ScheduleError::new(format!(
                "inspect loaded LaunchAgent after bootstrap: {error}"
            )),
            true,
        )),
    }
}

fn activation_verification_failure(
    verification: ScheduleError,
    cleanup: ScheduleResult<()>,
) -> ScheduleError {
    match cleanup {
        Ok(()) => ScheduleError::new(format!(
            "activate schedule failed: {verification}; the loaded job was removed"
        )),
        Err(cleanup_error) => ScheduleError::new(format!(
            "activate schedule failed: {verification}; loaded job cleanup also failed: {cleanup_error}"
        )),
    }
}

struct VerificationRollback<'a> {
    inspection: &'a Inspection,
    desired: &'a [u8],
    desired_state: &'a [u8],
    was_loaded: bool,
    should_unload: bool,
}

fn rollback_after_verification_failure<C: LaunchAgentController>(
    spec: &ScheduleSpec,
    controller: &mut C,
    context: VerificationRollback<'_>,
    verification: ScheduleError,
) -> ScheduleError {
    let unload = if context.should_unload {
        controller.bootout(LAUNCH_AGENT_LABEL)
    } else {
        Ok(())
    };
    let rollback = restore_snapshots(
        spec,
        context.inspection,
        Some(context.desired),
        Some(context.desired_state),
    );
    let can_reload = unload.is_ok() && rollback.is_ok();
    let reload = if can_reload && context.was_loaded && context.inspection.plist.is_some() {
        controller.bootstrap(&spec.plist_path())
    } else {
        Ok(())
    };
    combine_verification_failures(verification, unload, rollback, reload)
}

fn combine_verification_failures(
    verification: ScheduleError,
    unload: ScheduleResult<()>,
    rollback: ScheduleResult<()>,
    reload: ScheduleResult<()>,
) -> ScheduleError {
    let mut extra = Vec::new();
    if let Err(error) = unload {
        extra.push(format!("loaded job cleanup failed: {error}"));
    }
    if let Err(error) = rollback {
        extra.push(format!("file rollback failed: {error}"));
    }
    if let Err(error) = reload {
        extra.push(format!("previous LaunchAgent reload failed: {error}"));
    }
    if extra.is_empty() {
        ScheduleError::new(format!(
            "install schedule failed: {verification}; previous state was restored"
        ))
    } else {
        ScheduleError::new(format!(
            "install schedule failed: {verification}; {}",
            extra.join("; ")
        ))
    }
}

fn rollback_after_failure<C: LaunchAgentController>(
    spec: &ScheduleSpec,
    controller: &mut C,
    inspection: &Inspection,
    desired: &[u8],
    desired_state: &[u8],
    was_loaded: bool,
    original_error: ScheduleError,
) -> ScheduleError {
    let rollback = restore_snapshots(spec, inspection, Some(desired), Some(desired_state));
    let reload = if was_loaded && inspection.plist.is_some() {
        controller.bootstrap(&spec.plist_path())
    } else {
        Ok(())
    };
    combine_failures("install schedule", original_error, rollback, reload)
}

fn restore_snapshots(
    spec: &ScheduleSpec,
    inspection: &Inspection,
    expected_plist: Option<&[u8]>,
    expected_state: Option<&[u8]>,
) -> ScheduleResult<()> {
    let mut errors = Vec::new();
    if let Err(error) = restore_one(
        &spec.plist_path(),
        inspection.plist.as_deref(),
        expected_plist,
    ) {
        errors.push(error.to_string());
    }
    if let Err(error) = restore_one(
        &spec.ownership_state_path(),
        inspection.state.as_deref(),
        expected_state,
    ) {
        errors.push(error.to_string());
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ScheduleError::new(errors.join("; ")))
    }
}

fn restore_one(
    path: &Path,
    original: Option<&[u8]>,
    expected_installed: Option<&[u8]>,
) -> ScheduleResult<()> {
    let current = read_safe_path(path, "rollback target")?;
    match (original, current) {
        (Some(original), ReadPath::File(current)) if current == original => Ok(()),
        (Some(original), ReadPath::File(current))
            if expected_installed.is_some_and(|expected| current == expected) =>
        {
            replace_exact(
                path,
                expected_installed.expect("matched installed content"),
                original,
            )
        }
        (Some(original), ReadPath::Missing) => write_new_atomic(path, original),
        (None, ReadPath::File(current))
            if expected_installed.is_some_and(|expected| current == expected) =>
        {
            remove_exact(path, expected_installed.expect("matched installed content"))
        }
        (None, ReadPath::Missing) => Ok(()),
        (_, ReadPath::Unsafe(reason)) => Err(ScheduleError::new(reason)),
        _ => Err(ScheduleError::new(format!(
            "refusing to roll back {} because it changed concurrently",
            path.display()
        ))),
    }
}

fn combine_failures(
    operation: &str,
    original: ScheduleError,
    rollback: ScheduleResult<()>,
    reload: ScheduleResult<()>,
) -> ScheduleError {
    let mut extra = Vec::new();
    if let Err(error) = rollback {
        extra.push(format!("file rollback failed: {error}"));
    }
    if let Err(error) = reload {
        extra.push(format!("previous LaunchAgent reload failed: {error}"));
    }
    if extra.is_empty() {
        ScheduleError::new(format!(
            "{operation} failed: {original}; previous state was restored"
        ))
    } else {
        ScheduleError::new(format!(
            "{operation} failed: {original}; {}",
            extra.join("; ")
        ))
    }
}

fn backup_inspection(
    spec: &ScheduleSpec,
    inspection: &Inspection,
) -> ScheduleResult<Option<PathBuf>> {
    if inspection.plist.is_none() && inspection.state.is_none() {
        return Ok(None);
    }
    let root = unique_backup_dir(spec)?;
    if let Some(plist) = &inspection.plist {
        write_new_atomic(&root.join(PLIST_NAME), plist)?;
    }
    if let Some(state) = &inspection.state {
        write_new_atomic(&root.join(STATE_NAME), state)?;
    }
    Ok(Some(root))
}

fn unique_backup_dir(spec: &ScheduleSpec) -> ScheduleResult<PathBuf> {
    let base = spec.home.join(".agent-sync").join("backups");
    ensure_safe_directory(&base, "schedule backup directory")?;
    loop {
        let sequence = UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| ScheduleError::new(format!("read system time: {error}")))?
            .as_millis();
        let candidate = base.join(format!(
            "schedule-{millis}-{}-{sequence}",
            std::process::id()
        ));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(schedule_io_error(
                    &format!("create schedule backup {}", candidate.display()),
                    error,
                ))
            }
        }
    }
}

fn ensure_install_directories(spec: &ScheduleSpec) -> ScheduleResult<()> {
    ensure_safe_directory(
        &spec.home.join("Library").join("LaunchAgents"),
        "LaunchAgents directory",
    )?;
    ensure_safe_directory(&spec.log_dir(), "agent-sync log directory")?;
    let state_parent = spec
        .ownership_state_path()
        .parent()
        .expect("ownership state has a parent")
        .to_path_buf();
    ensure_safe_directory(&state_parent, "agent-sync state directory")
}

fn ensure_safe_directory(path: &Path, label: &str) -> ScheduleResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(ScheduleError::new(format!(
            "refusing to use symlinked {label} {}",
            path.display()
        ))),
        Ok(metadata) if !metadata.is_dir() => Err(ScheduleError::new(format!(
            "{label} is not a directory: {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|error| {
                schedule_io_error(&format!("create {label} {}", path.display()), error)
            })
        }
        Err(error) => Err(schedule_io_error(
            &format!("inspect {label} {}", path.display()),
            error,
        )),
    }
}

fn read_safe_path(path: &Path, label: &str) -> ScheduleResult<ReadPath> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(ReadPath::Missing),
        Err(error) => {
            return Err(schedule_io_error(
                &format!("inspect {label} {}", path.display()),
                error,
            ))
        }
    };
    if metadata.file_type().is_symlink() {
        return Ok(ReadPath::Unsafe(format!(
            "the {label} path is a symlink: {}",
            path.display()
        )));
    }
    if !metadata.is_file() {
        return Ok(ReadPath::Unsafe(format!(
            "the {label} path is not a regular file: {}",
            path.display()
        )));
    }
    fs::read(path)
        .map(ReadPath::File)
        .map_err(|error| schedule_io_error(&format!("read {label} {}", path.display()), error))
}

fn write_from_snapshot(path: &Path, content: &[u8], original: Option<&[u8]>) -> ScheduleResult<()> {
    match original {
        Some(original) => replace_exact(path, original, content),
        None => write_new_atomic(path, content),
    }
}

fn remove_exact(path: &Path, expected: &[u8]) -> ScheduleResult<()> {
    remove_file_if_unchanged(path, expected).map_err(|error| {
        ScheduleError::new(format!(
            "remove managed schedule file {}: {error:#}",
            path.display()
        ))
    })?;
    sync_parent(path)
}

fn replace_exact(path: &Path, expected: &[u8], content: &[u8]) -> ScheduleResult<()> {
    replace_file_if_unchanged(path, expected, content, None).map_err(|error| {
        ScheduleError::new(format!(
            "replace managed schedule file {}: {error:#}",
            path.display()
        ))
    })?;
    sync_parent(path)
}

fn write_new_atomic(path: &Path, content: &[u8]) -> ScheduleResult<()> {
    if let Some(parent) = path.parent() {
        ensure_safe_directory(parent, "schedule file parent")?;
    }
    let (temporary, mut file) = create_temporary_sibling(path)?;
    let result = (|| {
        write_and_sync(&mut file, &temporary, content)?;
        fs::hard_link(&temporary, path).map_err(|error| {
            schedule_io_error(
                &format!("install new schedule file {}", path.display()),
                error,
            )
        })?;
        fs::remove_file(&temporary).map_err(|error| {
            schedule_io_error(&format!("remove {}", temporary.display()), error)
        })?;
        sync_parent(path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn create_temporary_sibling(path: &Path) -> ScheduleResult<(PathBuf, File)> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("schedule");
    loop {
        let sequence = UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{name}.agent-sync-{}-{sequence}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(schedule_io_error(
                    &format!("create temporary file {}", temporary.display()),
                    error,
                ))
            }
        }
    }
}

fn write_and_sync(file: &mut File, path: &Path, content: &[u8]) -> ScheduleResult<()> {
    file.write_all(content)
        .map_err(|error| schedule_io_error(&format!("write {}", path.display()), error))?;
    file.sync_all()
        .map_err(|error| schedule_io_error(&format!("sync {}", path.display()), error))
}

fn sync_parent(path: &Path) -> ScheduleResult<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| schedule_io_error(&format!("sync directory {}", parent.display()), error))
}

fn encode_ownership_state(plist: &[u8]) -> Vec<u8> {
    [OWNERSHIP_STATE_MAGIC, plist].concat()
}

fn decode_ownership_state(state: &[u8]) -> ScheduleResult<&[u8]> {
    let Some(plist) = state.strip_prefix(OWNERSHIP_STATE_MAGIC) else {
        return Err(ScheduleError::new(
            "schedule ownership state has an unsupported format",
        ));
    };
    if !is_managed_plist(plist) {
        return Err(ScheduleError::new(
            "schedule ownership state does not contain an agent-sync LaunchAgent",
        ));
    }
    Ok(plist)
}

fn is_managed_plist(plist: &[u8]) -> bool {
    let Ok(plist) = std::str::from_utf8(plist) else {
        return false;
    };
    plist.contains(&format!("<string>{LAUNCH_AGENT_LABEL}</string>"))
        && plist.contains(&format!("<!-- {MANAGED_MARKER} -->"))
        && plist.contains("<string>sync</string>")
        && plist.contains("<string>--yes</string>")
        && plist.contains("<string>--automation</string>")
}

fn validate_spec(spec: &ScheduleSpec) -> ScheduleResult<()> {
    if !spec.home.is_absolute() {
        return Err(ScheduleError::new(
            "home directory must be an absolute path",
        ));
    }
    if !spec.executable.is_absolute() {
        return Err(ScheduleError::new(
            "agent-sync executable must be an absolute path",
        ));
    }
    if spec.interval_seconds < MIN_INTERVAL_SECONDS {
        return Err(ScheduleError::new(format!(
            "schedule interval must be at least {MIN_INTERVAL_SECONDS} seconds"
        )));
    }
    if spec
        .global_arguments
        .iter()
        .any(|argument| argument.contains('\0'))
    {
        return Err(ScheduleError::new(
            "schedule arguments must not contain NUL bytes",
        ));
    }
    let metadata = fs::metadata(&spec.executable).map_err(|error| {
        schedule_io_error(
            &format!(
                "inspect agent-sync executable {}",
                spec.executable.display()
            ),
            error,
        )
    })?;
    if !metadata.is_file() {
        return Err(ScheduleError::new(format!(
            "agent-sync executable is not a regular file: {}",
            spec.executable.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(ScheduleError::new(format!(
                "agent-sync executable is not executable: {}",
                spec.executable.display()
            )));
        }
    }
    Ok(())
}

fn path_text<'a>(path: &'a Path, label: &str) -> ScheduleResult<&'a str> {
    path.to_str().ok_or_else(|| {
        ScheduleError::new(format!("{label} is not valid UTF-8: {}", path.display()))
    })
}

fn xml_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

fn command_error(command: &str, output: &Output) -> ScheduleError {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if !stderr.trim().is_empty() {
        stderr.trim()
    } else {
        stdout.trim()
    };
    ScheduleError::new(format!("{command} failed with {}: {detail}", output.status))
}

fn schedule_io_error(context: &str, error: io::Error) -> ScheduleError {
    ScheduleError::new(format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeController {
        loaded: bool,
        identity_mismatch: bool,
        bootstrap_identity_mismatches_remaining: usize,
        bootstrap_verification_errors_remaining: usize,
        verification_error_pending: bool,
        calls: Vec<String>,
        bootstrap_failures_remaining: usize,
        fail_bootout: bool,
    }

    impl LaunchAgentController for FakeController {
        fn is_loaded(&mut self, label: &str) -> ScheduleResult<bool> {
            self.calls.push(format!("status:{label}"));
            Ok(self.loaded)
        }

        fn loaded_job_matches(&mut self, _spec: &ScheduleSpec) -> ScheduleResult<Option<bool>> {
            self.calls.push(format!("status:{LAUNCH_AGENT_LABEL}"));
            if self.loaded && self.verification_error_pending {
                self.verification_error_pending = false;
                return Err(ScheduleError::new("simulated identity inspection failure"));
            }
            Ok(self.loaded.then_some(!self.identity_mismatch))
        }

        fn bootstrap(&mut self, plist_path: &Path) -> ScheduleResult<()> {
            self.calls
                .push(format!("bootstrap:{}", plist_path.display()));
            if self.bootstrap_failures_remaining > 0 {
                self.bootstrap_failures_remaining -= 1;
                return Err(ScheduleError::new("simulated bootstrap failure"));
            }
            self.loaded = true;
            self.identity_mismatch = self.bootstrap_identity_mismatches_remaining > 0;
            self.bootstrap_identity_mismatches_remaining = self
                .bootstrap_identity_mismatches_remaining
                .saturating_sub(1);
            self.verification_error_pending = self.bootstrap_verification_errors_remaining > 0;
            self.bootstrap_verification_errors_remaining = self
                .bootstrap_verification_errors_remaining
                .saturating_sub(1);
            Ok(())
        }

        fn bootout(&mut self, label: &str) -> ScheduleResult<()> {
            self.calls.push(format!("bootout:{label}"));
            if self.fail_bootout {
                return Err(ScheduleError::new("simulated bootout failure"));
            }
            self.loaded = false;
            self.identity_mismatch = false;
            self.verification_error_pending = false;
            Ok(())
        }
    }

    struct TestHome {
        path: PathBuf,
    }

    impl TestHome {
        fn new() -> Self {
            loop {
                let sequence = UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "agent-sync-schedule-test-{}-{sequence}",
                    std::process::id()
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self { path },
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("create test home: {error}"),
                }
            }
        }

        fn spec(&self) -> ScheduleSpec {
            let executable = self.path.join("bin/agent-sync & stable");
            fs::create_dir_all(executable.parent().unwrap()).unwrap();
            fs::write(&executable, "test executable\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
            }
            ScheduleSpec::new(&self.path, executable)
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn render_uses_exact_automation_payload_and_escaped_paths() {
        let home = TestHome::new();
        let spec = home.spec();
        let rendered = String::from_utf8(render_launch_agent(&spec).unwrap()).unwrap();

        assert!(rendered.contains("agent-sync &amp; stable"));
        assert!(rendered.contains("<string>sync</string>"));
        assert!(rendered.contains("<string>--yes</string>"));
        assert!(rendered.contains("<string>--automation</string>"));
        assert!(rendered.contains("<integer>86400</integer>"));
        assert!(rendered.contains(".agent-sync/logs/sync.stdout.log"));
        assert!(rendered.contains(".agent-sync/logs/sync.stderr.log"));
        assert!(!rendered.contains("Program</key>"));
    }

    #[test]
    fn render_uses_the_configured_environment_path() {
        let home = TestHome::new();
        let spec = home.spec().with_environment_path(vec![
            home.path.join(".local/share/fnm/aliases/default/bin"),
            PathBuf::from("/usr/bin"),
            PathBuf::from("/bin"),
        ]);

        let rendered = String::from_utf8(render_launch_agent(&spec).unwrap()).unwrap();

        assert!(rendered.contains(&format!(
            "<string>{}:/usr/bin:/bin</string>",
            home.path
                .join(".local/share/fnm/aliases/default/bin")
                .display()
        )));
    }

    #[test]
    fn loaded_job_identity_requires_the_plist_program_and_all_arguments() {
        let home = TestHome::new();
        let spec = home
            .spec()
            .with_global_arguments(vec!["--config".to_string(), "/tmp/config.toml".to_string()]);
        let matching = format!(
            concat!(
                "gui/501/{label} = {{\n",
                "    path = {plist}\n",
                "    program = {program}\n",
                "    arguments = {{\n",
                "        {program}\n",
                "        --config\n",
                "        /tmp/config.toml\n",
                "        sync\n",
                "        --yes\n",
                "        --automation\n",
                "    }}\n",
                "}}\n"
            ),
            label = LAUNCH_AGENT_LABEL,
            plist = spec.plist_path().display(),
            program = spec.executable().display(),
        );

        assert!(loaded_job_matches_spec(matching.as_bytes(), &spec).unwrap());
        assert!(!loaded_job_matches_spec(
            matching
                .replace("        --automation\n", "        --wrong\n")
                .as_bytes(),
            &spec
        )
        .unwrap());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn rendered_launch_agent_passes_plutil_validation() {
        let home = TestHome::new();
        let spec = home.spec();
        let rendered = render_launch_agent(&spec).unwrap();
        let plist = home.path.join("rendered.plist");
        fs::write(&plist, rendered).unwrap();

        let output = Command::new("/usr/bin/plutil")
            .args(["-lint", plist.to_str().unwrap()])
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "plutil failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn preview_is_read_only_and_reports_one_next_action() {
        let home = TestHome::new();
        let spec = home.spec();
        let mut controller = FakeController::default();

        let report = install_schedule(&spec, &mut controller, true).unwrap();

        assert_eq!(report.action, ScheduleAction::Add);
        assert!(!spec.plist_path().exists());
        assert!(!spec.ownership_state_path().exists());
        assert_eq!(
            controller.calls,
            vec![format!("status:{LAUNCH_AGENT_LABEL}")]
        );
        assert_eq!(report.to_text().matches("Next action:").count(), 1);
    }

    #[test]
    fn install_and_status_preserve_unrelated_launch_agents() {
        let home = TestHome::new();
        let spec = home.spec();
        let unrelated = spec
            .plist_path()
            .parent()
            .unwrap()
            .join("com.example.keep.plist");
        fs::create_dir_all(unrelated.parent().unwrap()).unwrap();
        fs::write(&unrelated, "keep\n").unwrap();
        let mut controller = FakeController::default();

        let installed = install_schedule(&spec, &mut controller, false).unwrap();
        let status = schedule_status(&spec, &mut controller).unwrap();

        assert_eq!(installed.action, ScheduleAction::Add);
        assert!(installed.healthy);
        assert_eq!(status.action, ScheduleAction::Unchanged);
        assert!(status.healthy);
        assert_eq!(fs::read_to_string(unrelated).unwrap(), "keep\n");
        assert!(spec.log_dir().is_dir());
    }

    #[test]
    fn status_detects_and_install_repairs_a_mismatched_loaded_job() {
        let home = TestHome::new();
        let spec = home.spec();
        let mut controller = FakeController::default();
        install_schedule(&spec, &mut controller, false).unwrap();
        controller.identity_mismatch = true;

        let status = schedule_status(&spec, &mut controller).unwrap();

        assert_eq!(status.action, ScheduleAction::Update);
        assert!(!status.healthy);
        assert!(status.loaded);
        assert!(status.detail.contains("does not match"));

        let repaired = install_schedule(&spec, &mut controller, false).unwrap();
        let healthy = schedule_status(&spec, &mut controller).unwrap();

        assert_eq!(repaired.action, ScheduleAction::Update);
        assert!(healthy.healthy);
        assert_eq!(healthy.action, ScheduleAction::Unchanged);
    }

    #[test]
    fn install_rolls_back_when_the_bootstrapped_job_identity_does_not_match() {
        let home = TestHome::new();
        let spec = home.spec();
        let mut controller = FakeController {
            bootstrap_identity_mismatches_remaining: 1,
            ..FakeController::default()
        };

        let error = install_schedule(&spec, &mut controller, false).unwrap_err();

        assert!(error.to_string().contains("identity verification failed"));
        assert!(error.to_string().contains("previous state was restored"));
        assert!(!controller.loaded);
        assert!(!spec.plist_path().exists());
        assert!(!spec.ownership_state_path().exists());
    }

    #[test]
    fn update_restores_and_reloads_the_previous_job_after_identity_mismatch() {
        let home = TestHome::new();
        let spec = home.spec();
        let mut controller = FakeController::default();
        install_schedule(&spec, &mut controller, false).unwrap();
        let old_plist = fs::read(spec.plist_path()).unwrap();
        let old_state = fs::read(spec.ownership_state_path()).unwrap();
        let updated = spec.clone().with_interval_seconds(12 * 60 * 60);
        controller.bootstrap_identity_mismatches_remaining = 1;

        let error = install_schedule(&updated, &mut controller, false).unwrap_err();

        assert!(error.to_string().contains("identity verification failed"));
        assert!(error.to_string().contains("previous state was restored"));
        assert!(controller.loaded);
        assert!(!controller.identity_mismatch);
        assert_eq!(fs::read(spec.plist_path()).unwrap(), old_plist);
        assert_eq!(fs::read(spec.ownership_state_path()).unwrap(), old_state);
    }

    #[test]
    fn activate_unloads_the_job_when_terminal_identity_inspection_fails() {
        let home = TestHome::new();
        let spec = home.spec();
        let mut controller = FakeController::default();
        install_schedule(&spec, &mut controller, false).unwrap();
        let old_plist = fs::read(spec.plist_path()).unwrap();
        let old_state = fs::read(spec.ownership_state_path()).unwrap();
        controller.loaded = false;
        controller.bootstrap_verification_errors_remaining = 1;

        let error = install_schedule(&spec, &mut controller, false).unwrap_err();

        assert!(error
            .to_string()
            .contains("simulated identity inspection failure"));
        assert!(error.to_string().contains("loaded job was removed"));
        assert!(!controller.loaded);
        assert_eq!(fs::read(spec.plist_path()).unwrap(), old_plist);
        assert_eq!(fs::read(spec.ownership_state_path()).unwrap(), old_state);
    }

    #[test]
    fn install_refuses_an_unowned_file() {
        let home = TestHome::new();
        let spec = home.spec();
        fs::create_dir_all(spec.plist_path().parent().unwrap()).unwrap();
        fs::write(spec.plist_path(), "user launch agent\n").unwrap();
        let mut controller = FakeController::default();

        let error = install_schedule(&spec, &mut controller, false).unwrap_err();

        assert!(error.to_string().contains("unowned LaunchAgent"));
        assert_eq!(
            fs::read_to_string(spec.plist_path()).unwrap(),
            "user launch agent\n"
        );
    }

    #[test]
    fn update_backs_up_the_owned_file_and_reloads_it() {
        let home = TestHome::new();
        let spec = home.spec();
        let mut controller = FakeController::default();
        install_schedule(&spec, &mut controller, false).unwrap();
        let old_plist = fs::read(spec.plist_path()).unwrap();
        let updated = spec.clone().with_interval_seconds(12 * 60 * 60);
        controller.calls.clear();

        let report = install_schedule(&updated, &mut controller, false).unwrap();

        assert_eq!(report.action, ScheduleAction::Update);
        let backup = report.backup.unwrap();
        assert_eq!(fs::read(backup.join(PLIST_NAME)).unwrap(), old_plist);
        assert!(controller
            .calls
            .iter()
            .any(|call| call == &format!("bootout:{LAUNCH_AGENT_LABEL}")));
        assert!(controller
            .calls
            .iter()
            .any(|call| call.starts_with("bootstrap:")));
    }

    #[test]
    fn modified_managed_file_is_preserved_on_update_and_uninstall() {
        let home = TestHome::new();
        let spec = home.spec();
        let mut controller = FakeController::default();
        install_schedule(&spec, &mut controller, false).unwrap();
        fs::write(spec.plist_path(), "user modification\n").unwrap();

        let install_error = install_schedule(&spec, &mut controller, false).unwrap_err();
        let uninstall_error = uninstall_schedule(&spec, &mut controller, false).unwrap_err();

        assert!(install_error.to_string().contains("modified"));
        assert!(uninstall_error.to_string().contains("modified"));
        assert_eq!(
            fs::read_to_string(spec.plist_path()).unwrap(),
            "user modification\n"
        );
    }

    #[test]
    fn loaded_unowned_label_is_preserved() {
        let home = TestHome::new();
        let spec = home.spec();
        let mut controller = FakeController {
            loaded: true,
            ..FakeController::default()
        };

        let status = schedule_status(&spec, &mut controller).unwrap();
        let error = install_schedule(&spec, &mut controller, false).unwrap_err();

        assert_eq!(status.action, ScheduleAction::Conflict);
        assert!(error.to_string().contains("label is loaded"));
        assert!(!spec.plist_path().exists());
        assert!(!controller
            .calls
            .iter()
            .any(|call| call.starts_with("bootout:")));
    }

    #[test]
    fn bootout_failure_does_not_write_an_update() {
        let home = TestHome::new();
        let spec = home.spec();
        let mut controller = FakeController::default();
        install_schedule(&spec, &mut controller, false).unwrap();
        let old_plist = fs::read(spec.plist_path()).unwrap();
        let old_state = fs::read(spec.ownership_state_path()).unwrap();
        let updated = spec.clone().with_interval_seconds(12 * 60 * 60);
        controller.fail_bootout = true;

        let error = install_schedule(&updated, &mut controller, false).unwrap_err();

        assert!(error.to_string().contains("simulated bootout failure"));
        assert_eq!(fs::read(spec.plist_path()).unwrap(), old_plist);
        assert_eq!(fs::read(spec.ownership_state_path()).unwrap(), old_state);
    }

    #[test]
    fn bootstrap_failure_restores_the_previous_schedule() {
        let home = TestHome::new();
        let spec = home.spec();
        let mut controller = FakeController::default();
        install_schedule(&spec, &mut controller, false).unwrap();
        let old_plist = fs::read(spec.plist_path()).unwrap();
        let old_state = fs::read(spec.ownership_state_path()).unwrap();
        let updated = spec.clone().with_interval_seconds(12 * 60 * 60);
        controller.bootstrap_failures_remaining = 1;

        let error = install_schedule(&updated, &mut controller, false).unwrap_err();

        assert!(error.to_string().contains("previous state was restored"));
        assert_eq!(fs::read(spec.plist_path()).unwrap(), old_plist);
        assert_eq!(fs::read(spec.ownership_state_path()).unwrap(), old_state);
    }

    #[test]
    fn uninstall_removes_only_owned_files_and_keeps_logs() {
        let home = TestHome::new();
        let spec = home.spec();
        let unrelated = spec
            .plist_path()
            .parent()
            .unwrap()
            .join("com.example.keep.plist");
        fs::create_dir_all(unrelated.parent().unwrap()).unwrap();
        fs::write(&unrelated, "keep\n").unwrap();
        let mut controller = FakeController::default();
        install_schedule(&spec, &mut controller, false).unwrap();
        fs::write(spec.stdout_log_path(), "old log\n").unwrap();

        let preview = uninstall_schedule(&spec, &mut controller, true).unwrap();
        assert_eq!(preview.action, ScheduleAction::Remove);
        assert!(spec.plist_path().exists());
        let report = uninstall_schedule(&spec, &mut controller, false).unwrap();

        assert_eq!(report.action, ScheduleAction::Remove);
        assert!(report.backup.is_some());
        assert!(!spec.plist_path().exists());
        assert!(!spec.ownership_state_path().exists());
        assert_eq!(fs::read_to_string(unrelated).unwrap(), "keep\n");
        assert_eq!(
            fs::read_to_string(spec.stdout_log_path()).unwrap(),
            "old log\n"
        );
    }

    #[test]
    fn uninstall_bootout_failure_preserves_owned_files() {
        let home = TestHome::new();
        let spec = home.spec();
        let mut controller = FakeController::default();
        install_schedule(&spec, &mut controller, false).unwrap();
        let old_plist = fs::read(spec.plist_path()).unwrap();
        let old_state = fs::read(spec.ownership_state_path()).unwrap();
        controller.fail_bootout = true;

        let error = uninstall_schedule(&spec, &mut controller, false).unwrap_err();

        assert!(error.to_string().contains("simulated bootout failure"));
        assert_eq!(fs::read(spec.plist_path()).unwrap(), old_plist);
        assert_eq!(fs::read(spec.ownership_state_path()).unwrap(), old_state);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_launch_agent_is_reported_as_a_conflict() {
        use std::os::unix::fs::symlink;

        let home = TestHome::new();
        let spec = home.spec();
        let victim = home.path.join("victim");
        fs::write(&victim, "keep\n").unwrap();
        fs::create_dir_all(spec.plist_path().parent().unwrap()).unwrap();
        symlink(&victim, spec.plist_path()).unwrap();
        let mut controller = FakeController::default();

        let status = schedule_status(&spec, &mut controller).unwrap();
        let error = install_schedule(&spec, &mut controller, false).unwrap_err();

        assert_eq!(status.action, ScheduleAction::Conflict);
        assert!(error.to_string().contains("symlink"));
        assert_eq!(fs::read_to_string(victim).unwrap(), "keep\n");
    }
}
