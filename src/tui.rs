//! A compact, write-free terminal UI for managed setup and status flows.

use std::{
    fmt,
    io::{self, Stdout},
};

use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph, Wrap},
    Frame, Terminal,
};

const MIN_WIDTH: u16 = 52;
const MIN_HEIGHT: u16 = 16;

const PRIMARY: Color = Color::White;
const MUTED: Color = Color::DarkGray;
const FOCUS: Color = Color::Cyan;
const HEALTHY: Color = Color::Green;
const ATTENTION: Color = Color::Yellow;
const ERROR: Color = Color::Red;

/// An agent shown by the human UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Agent {
    Codex,
    Claude,
    Cursor,
}

impl Agent {
    /// Returns the user-facing agent name.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
            Self::Cursor => "Cursor",
        }
    }
}

impl fmt::Display for Agent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// The MCP resources to copy from the canonical source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpChoice {
    None,
    Selected(Vec<String>),
    All,
}

/// Cursor history behavior selected during setup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorHistoryChoice {
    Disabled,
    ExportOnly,
    ExportAndQmd,
}

/// The complete result of the setup UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupSelection {
    pub source: Agent,
    pub targets: Vec<Agent>,
    pub mcp: McpChoice,
    pub cursor_history: CursorHistoryChoice,
}

/// MCP server names available from one potential canonical source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMcpServers {
    pub agent: Agent,
    pub servers: Vec<String>,
}

/// Inputs for the progressive setup UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupScreen {
    pub available_sources: Vec<Agent>,
    pub target_agents: Vec<Agent>,
    pub preserve_initial_source: bool,
    pub mcp_servers: Vec<AgentMcpServers>,
    pub qmd_available: bool,
    pub include_references: bool,
    pub initial: SetupSelection,
    pub error: Option<String>,
}

/// The semantic color of a status value or action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Normal,
    Muted,
    Healthy,
    Attention,
    Error,
}

/// Overall health shown on the status screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthState {
    Healthy,
    Attention,
    Error,
    Unknown,
}

/// One label and value in the status summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLine {
    pub label: String,
    pub value: String,
    pub tone: Tone,
}

/// A write action offered by agent-sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionItem {
    pub id: String,
    pub label: String,
    pub detail: String,
    pub tone: Tone,
}

/// Inputs for the read-only status and action menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusScreen {
    pub source: Option<Agent>,
    pub targets: Vec<Agent>,
    pub health: HealthState,
    pub summary: Vec<StatusLine>,
    pub actions: Vec<ActionItem>,
    pub message: Option<String>,
}

/// The screen to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiRequest {
    Setup(SetupScreen),
    Status(StatusScreen),
}

/// A user decision returned to agent-sync. The UI never performs the action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiOutcome {
    Cancelled,
    Setup(SetupSelection),
    Action(String),
}

/// Opens the terminal UI and waits for a setup selection, action, or cancellation.
///
/// Terminal state is restored before this function returns, including when drawing or input
/// fails. Agent-sync remains responsible for previewing and performing any selected write.
pub fn run(request: TuiRequest) -> io::Result<TuiOutcome> {
    let mut terminal = TerminalSession::enter()?;
    let mut app = App::new(request);

    loop {
        terminal.terminal.draw(|frame| render(frame, &app))?;

        match event::read()? {
            Event::Key(key) if accepts_key(key) => {
                if let Some(outcome) = app.handle_key(key) {
                    terminal.restore()?;
                    return Ok(outcome);
                }
            }
            Event::Resize(_, _) => {}
            _ => {}
        }
    }
}

fn accepts_key(key: KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

type HumanTerminal = Terminal<CrosstermBackend<Stdout>>;

struct TerminalSession {
    terminal: HumanTerminal,
    restored: bool,
}

impl TerminalSession {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;

        let mut output = io::stdout();
        if let Err(error) = execute!(output, EnterAlternateScreen, Hide) {
            let _ = disable_raw_mode();
            return Err(error);
        }

        let backend = CrosstermBackend::new(output);
        let terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = disable_raw_mode();
                let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
                return Err(error);
            }
        };

        Ok(Self {
            terminal,
            restored: false,
        })
    }

    fn restore(&mut self) -> io::Result<()> {
        if self.restored {
            return Ok(());
        }

        let raw_result = disable_raw_mode();
        let screen_result = execute!(self.terminal.backend_mut(), Show, LeaveAlternateScreen);
        let cursor_result = self.terminal.show_cursor();
        self.restored = raw_result.is_ok() && screen_result.is_ok() && cursor_result.is_ok();

        raw_result.and(screen_result).and(cursor_result)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

enum App {
    Setup(SetupApp),
    Status(StatusApp),
}

impl App {
    fn new(request: TuiRequest) -> Self {
        match request {
            TuiRequest::Setup(screen) => Self::Setup(SetupApp::new(screen)),
            TuiRequest::Status(screen) => Self::Status(StatusApp::new(screen)),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<TuiOutcome> {
        if is_cancel_key(key) {
            return Some(TuiOutcome::Cancelled);
        }

        match self {
            Self::Setup(app) => app.handle_key(key),
            Self::Status(app) => app.handle_key(key),
        }
    }
}

fn is_cancel_key(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('q')
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupStage {
    Source,
    Targets,
    McpMode,
    McpServers,
    CursorHistory,
    Review,
}

impl SetupStage {
    const fn label(self) -> &'static str {
        match self {
            Self::Source => "Source",
            Self::Targets => "Targets",
            Self::McpMode => "MCP scope",
            Self::McpServers => "Servers",
            Self::CursorHistory => "History",
            Self::Review => "Review",
        }
    }
}

struct SetupApp {
    available_sources: Vec<Agent>,
    target_agents: Vec<Agent>,
    preserved_unavailable_source: Option<Agent>,
    mcp_servers: Vec<AgentMcpServers>,
    qmd_available: bool,
    include_references: bool,
    source: Option<Agent>,
    targets: Vec<Agent>,
    mcp: McpChoice,
    cursor_history: CursorHistoryChoice,
    stage: SetupStage,
    cursor: usize,
    message: Option<(Tone, String)>,
    supplied_error: Option<String>,
}

impl SetupApp {
    fn new(screen: SetupScreen) -> Self {
        let SetupScreen {
            available_sources,
            target_agents,
            preserve_initial_source,
            mcp_servers,
            qmd_available,
            include_references,
            initial,
            error,
        } = screen;
        let available_sources = unique_agents(available_sources);
        let target_agents = unique_agents(target_agents);
        let preserved_unavailable_source = preserve_initial_source
            .then_some(initial.source)
            .filter(|source| !available_sources.contains(source));
        let source = if available_sources.contains(&initial.source)
            || preserved_unavailable_source.is_some()
        {
            Some(initial.source)
        } else {
            available_sources.first().copied()
        };

        let mut targets = unique_agents(initial.targets);
        targets.retain(|target| target_agents.contains(target) && Some(*target) != source);

        let mcp = normalize_mcp(initial.mcp);
        let cursor_history = initial.cursor_history;

        let mut app = Self {
            available_sources,
            target_agents,
            preserved_unavailable_source,
            mcp_servers: normalize_mcp_sources(mcp_servers),
            qmd_available,
            include_references,
            source,
            targets,
            mcp,
            cursor_history,
            stage: SetupStage::Source,
            cursor: 0,
            message: None,
            supplied_error: error,
        };
        app.sync_cursor_to_value();
        app
    }

    fn stages(&self) -> Vec<SetupStage> {
        let mut stages = vec![SetupStage::Source, SetupStage::Targets, SetupStage::McpMode];
        if matches!(self.mcp, McpChoice::Selected(_)) {
            stages.push(SetupStage::McpServers);
        }
        if self.source == Some(Agent::Cursor) || self.targets.contains(&Agent::Cursor) {
            stages.push(SetupStage::CursorHistory);
        }
        stages.push(SetupStage::Review);
        stages
    }

    fn source_options(&self) -> Vec<Agent> {
        let mut sources = self.available_sources.clone();
        if let Some(source) = self
            .preserved_unavailable_source
            .filter(|source| Some(*source) == self.source)
        {
            sources.insert(0, source);
        }
        sources
    }

    fn source_is_available(&self, source: Agent) -> bool {
        self.available_sources.contains(&source)
    }

    fn target_options(&self) -> Vec<Agent> {
        self.target_agents
            .iter()
            .copied()
            .filter(|agent| Some(*agent) != self.source)
            .collect()
    }

    fn available_mcp_servers(&self) -> &[String] {
        mcp_servers_for(&self.mcp_servers, self.source)
    }

    fn mcp_server_options(&self) -> Vec<String> {
        let mut servers = self.available_mcp_servers().to_vec();
        if let McpChoice::Selected(selected) = &self.mcp {
            for server in selected {
                if !servers.contains(server) {
                    servers.push(server.clone());
                }
            }
        }
        servers
    }

    fn mcp_server_is_available(&self, server: &str) -> bool {
        self.available_mcp_servers()
            .iter()
            .any(|available| available == server)
    }

    fn option_count(&self) -> usize {
        match self.stage {
            SetupStage::Source => self.source_options().len(),
            SetupStage::Targets => self.target_options().len(),
            SetupStage::McpMode => 3,
            SetupStage::McpServers => self.mcp_server_options().len(),
            SetupStage::CursorHistory => 3,
            SetupStage::Review => 1,
        }
    }

    fn sync_cursor_to_value(&mut self) {
        self.cursor = match self.stage {
            SetupStage::Source => self
                .source
                .and_then(|source| {
                    self.source_options()
                        .iter()
                        .position(|option| *option == source)
                })
                .unwrap_or(0),
            SetupStage::Targets | SetupStage::McpServers | SetupStage::Review => 0,
            SetupStage::McpMode => match self.mcp {
                McpChoice::None => 0,
                McpChoice::Selected(_) => 1,
                McpChoice::All => 2,
            },
            SetupStage::CursorHistory => match self.cursor_history {
                CursorHistoryChoice::Disabled => 0,
                CursorHistoryChoice::ExportOnly => 1,
                CursorHistoryChoice::ExportAndQmd => 2,
            },
        };
        self.clamp_cursor();
    }

    fn clamp_cursor(&mut self) {
        self.cursor = self.cursor.min(self.option_count().saturating_sub(1));
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<TuiOutcome> {
        self.message = None;

        match key.code {
            KeyCode::Esc | KeyCode::BackTab | KeyCode::Left | KeyCode::Backspace => {
                if !self.move_stage(-1) {
                    return Some(TuiOutcome::Cancelled);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.option_count().saturating_sub(1),
            KeyCode::Char(' ') => self.toggle_current(),
            KeyCode::Enter | KeyCode::Tab | KeyCode::Right => return self.activate(),
            _ => {}
        }

        None
    }

    fn move_cursor(&mut self, delta: isize) {
        let count = self.option_count();
        if count == 0 {
            self.cursor = 0;
            return;
        }

        self.cursor = ((self.cursor as isize + delta).rem_euclid(count as isize)) as usize;
    }

    fn toggle_current(&mut self) {
        match self.stage {
            SetupStage::Source => self.choose_source(),
            SetupStage::Targets => self.toggle_target(),
            SetupStage::McpMode => self.choose_mcp_mode(),
            SetupStage::McpServers => self.toggle_mcp_server(),
            SetupStage::CursorHistory => self.choose_cursor_history(),
            SetupStage::Review => {}
        }
    }

    fn activate(&mut self) -> Option<TuiOutcome> {
        match self.stage {
            SetupStage::Source => self.choose_source(),
            SetupStage::McpMode => self.choose_mcp_mode(),
            SetupStage::CursorHistory => {
                self.choose_cursor_history();
                if self.message.is_some() {
                    return None;
                }
            }
            SetupStage::Review => {
                return self.selection().map(TuiOutcome::Setup).or_else(|| {
                    self.message = Some((Tone::Error, self.validation_error()));
                    None
                });
            }
            SetupStage::Targets | SetupStage::McpServers => {}
        }

        if self.validate_stage() {
            self.move_stage(1);
        }
        None
    }

    fn choose_source(&mut self) {
        let Some(source) = self.source_options().get(self.cursor).copied() else {
            self.message = Some((Tone::Error, "No canonical source is available.".into()));
            return;
        };
        let previous = self.source;
        self.source = Some(source);
        self.targets.retain(|target| *target != source);
        if previous != Some(source) {
            self.preserved_unavailable_source = None;
            let available = self.available_mcp_servers().to_vec();
            if let McpChoice::Selected(servers) = &mut self.mcp {
                servers.retain(|server| available.contains(server));
            }
        }
    }

    fn toggle_target(&mut self) {
        let Some(target) = self.target_options().get(self.cursor).copied() else {
            self.message = Some((Tone::Error, "No target agents are available.".into()));
            return;
        };
        toggle_value(&mut self.targets, target);
    }

    fn choose_mcp_mode(&mut self) {
        self.mcp = match self.cursor {
            0 => McpChoice::None,
            1 => match std::mem::replace(&mut self.mcp, McpChoice::None) {
                McpChoice::Selected(servers) => McpChoice::Selected(servers),
                _ => McpChoice::Selected(Vec::new()),
            },
            _ => McpChoice::All,
        };
    }

    fn toggle_mcp_server(&mut self) {
        let Some(server) = self.mcp_server_options().get(self.cursor).cloned() else {
            self.message = Some((
                Tone::Error,
                "No MCP servers were found at the source.".into(),
            ));
            return;
        };
        if let McpChoice::Selected(servers) = &mut self.mcp {
            toggle_value(servers, server);
        }
        self.clamp_cursor();
    }

    fn choose_cursor_history(&mut self) {
        let choice = match self.cursor {
            0 => CursorHistoryChoice::Disabled,
            1 => CursorHistoryChoice::ExportOnly,
            _ => CursorHistoryChoice::ExportAndQmd,
        };
        if choice == CursorHistoryChoice::ExportAndQmd
            && !self.qmd_available
            && self.cursor_history != CursorHistoryChoice::ExportAndQmd
        {
            self.message = Some((
                Tone::Attention,
                "QMD is not available. Choose export only or install QMD first.".into(),
            ));
            return;
        }
        self.cursor_history = choice;
    }

    fn validate_stage(&mut self) -> bool {
        let error = match self.stage {
            SetupStage::Source if self.source.is_none() => Some("No agent source is available."),
            SetupStage::Source
                if self
                    .source
                    .is_some_and(|source| !self.source_is_available(source)) =>
            {
                Some("The configured source is unavailable. Install it or choose another source.")
            }
            SetupStage::Targets if self.targets.is_empty() => {
                Some("Choose at least one target agent.")
            }
            SetupStage::McpServers if matches!(&self.mcp, McpChoice::Selected(servers) if servers.is_empty()) => {
                Some("Choose at least one MCP server, or go back and select none.")
            }
            _ => None,
        };

        if let Some(error) = error {
            self.message = Some((Tone::Attention, error.into()));
            false
        } else {
            true
        }
    }

    fn validation_error(&self) -> String {
        if self.source.is_none() {
            "No canonical source is available.".into()
        } else if self
            .source
            .is_some_and(|source| !self.source_is_available(source))
        {
            "The configured source is unavailable. Install it or choose another source.".into()
        } else if self.targets.is_empty() {
            "Choose at least one target agent.".into()
        } else if matches!(&self.mcp, McpChoice::Selected(servers) if servers.is_empty()) {
            "Choose at least one MCP server.".into()
        } else {
            "The setup selection is incomplete.".into()
        }
    }

    fn selection(&self) -> Option<SetupSelection> {
        let source = self.source?;
        if !self.source_is_available(source)
            || self.targets.is_empty()
            || matches!(&self.mcp, McpChoice::Selected(servers) if servers.is_empty())
        {
            return None;
        }

        Some(SetupSelection {
            source,
            targets: self.targets.clone(),
            mcp: self.mcp.clone(),
            cursor_history: if source == Agent::Cursor || self.targets.contains(&Agent::Cursor) {
                self.cursor_history
            } else {
                CursorHistoryChoice::Disabled
            },
        })
    }

    fn move_stage(&mut self, delta: isize) -> bool {
        let stages = self.stages();
        let Some(index) = stages.iter().position(|stage| *stage == self.stage) else {
            self.stage = SetupStage::Source;
            self.sync_cursor_to_value();
            return false;
        };
        let next = index as isize + delta;
        if !(0..stages.len() as isize).contains(&next) {
            return false;
        }
        self.stage = stages[next as usize];
        self.sync_cursor_to_value();
        true
    }

    fn route(&self) -> String {
        route_text(self.source, &self.targets)
    }
}

fn normalize_mcp(choice: McpChoice) -> McpChoice {
    match choice {
        McpChoice::Selected(servers) => McpChoice::Selected(unique_nonempty(servers)),
        other => other,
    }
}

fn normalize_mcp_sources(sources: Vec<AgentMcpServers>) -> Vec<AgentMcpServers> {
    let mut normalized: Vec<AgentMcpServers> = Vec::new();
    for source in sources {
        let servers = unique_nonempty(source.servers);
        if let Some(existing) = normalized
            .iter_mut()
            .find(|existing| existing.agent == source.agent)
        {
            for server in servers {
                if !existing.servers.contains(&server) {
                    existing.servers.push(server);
                }
            }
        } else {
            normalized.push(AgentMcpServers {
                agent: source.agent,
                servers,
            });
        }
    }
    normalized
}

fn mcp_servers_for(sources: &[AgentMcpServers], source: Option<Agent>) -> &[String] {
    sources
        .iter()
        .find(|servers| Some(servers.agent) == source)
        .map_or(&[], |servers| servers.servers.as_slice())
}

fn unique_agents(agents: Vec<Agent>) -> Vec<Agent> {
    let mut unique = Vec::new();
    for agent in agents {
        if !unique.contains(&agent) {
            unique.push(agent);
        }
    }
    unique
}

fn unique_nonempty(values: Vec<String>) -> Vec<String> {
    let mut unique = Vec::new();
    for value in values {
        let value = value.trim().to_owned();
        if !value.is_empty() && !unique.contains(&value) {
            unique.push(value);
        }
    }
    unique
}

fn toggle_value<T: PartialEq>(values: &mut Vec<T>, value: T) {
    if let Some(index) = values.iter().position(|existing| *existing == value) {
        values.remove(index);
    } else {
        values.push(value);
    }
}

struct StatusApp {
    screen: StatusScreen,
    cursor: usize,
}

impl StatusApp {
    fn new(screen: StatusScreen) -> Self {
        Self { screen, cursor: 0 }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<TuiOutcome> {
        match key.code {
            KeyCode::Esc => Some(TuiOutcome::Cancelled),
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_cursor(-1);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_cursor(1);
                None
            }
            KeyCode::Home => {
                self.cursor = 0;
                None
            }
            KeyCode::End => {
                self.cursor = self.screen.actions.len().saturating_sub(1);
                None
            }
            KeyCode::Enter => self
                .screen
                .actions
                .get(self.cursor)
                .map(|action| TuiOutcome::Action(action.id.clone())),
            _ => None,
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        let count = self.screen.actions.len();
        if count == 0 {
            self.cursor = 0;
            return;
        }
        self.cursor = ((self.cursor as isize + delta).rem_euclid(count as isize)) as usize;
    }

    fn route(&self) -> String {
        route_text(self.screen.source, &self.screen.targets)
    }
}

fn route_text(source: Option<Agent>, targets: &[Agent]) -> String {
    if source.is_none() && targets.is_empty() {
        return "agent-sync".to_string();
    }
    let source: String =
        source.map_or_else(|| "choose source".into(), |agent| agent.label().into());
    let targets = if targets.is_empty() {
        "choose targets".into()
    } else {
        targets
            .iter()
            .map(|agent| agent.label())
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!("{source} → {targets}")
}

fn render(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        render_small(frame, area);
        return;
    }

    let outer = Block::default()
        .title(" agent-sync ")
        .title_style(Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(MUTED));
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    match app {
        App::Setup(app) => render_setup(frame, inner, app),
        App::Status(app) => render_status(frame, inner, app),
    }
}

fn render_small(frame: &mut Frame<'_>, area: Rect) {
    let block = Block::default()
        .title(" agent-sync ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(MUTED));
    let paragraph = Paragraph::new(vec![
        Line::from(Span::styled("Terminal too small", style(Tone::Attention))),
        Line::from(Span::styled(
            format!("Resize to at least {MIN_WIDTH}×{MIN_HEIGHT}."),
            style(Tone::Muted),
        )),
        Line::from(Span::styled("q  cancel", style(Tone::Muted))),
    ])
    .alignment(Alignment::Center)
    .block(block)
    .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

fn render_setup(frame: &mut Frame<'_>, area: Rect, app: &SetupApp) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Min(7),
            Constraint::Length(2),
        ])
        .split(area);

    render_route(frame, rows[0], &app.route());
    render_steps(frame, rows[1], app);
    render_setup_stage(frame, rows[2], app);
    render_setup_footer(frame, rows[3], app);
}

fn render_route(frame: &mut Frame<'_>, area: Rect, route: &str) {
    let line = Line::from(vec![
        Span::styled("route  ", style(Tone::Muted)),
        Span::styled(route, Style::default().fg(PRIMARY)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_steps(frame: &mut Frame<'_>, area: Rect, app: &SetupApp) {
    let stages = app.stages();
    let current = stages
        .iter()
        .position(|stage| *stage == app.stage)
        .unwrap_or(0);
    let spans = stages
        .iter()
        .enumerate()
        .flat_map(|(index, stage)| {
            let tone = if index == current {
                Tone::Normal
            } else {
                Tone::Muted
            };
            let mut step_style = style(tone);
            if index == current {
                step_style = step_style.fg(FOCUS).add_modifier(Modifier::BOLD);
            }
            let step = Span::styled(format!("{:02} {}", index + 1, stage.label()), step_style);
            if index + 1 == stages.len() {
                vec![step]
            } else {
                vec![step, Span::styled("  ·  ", style(Tone::Muted))]
            }
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_setup_stage(frame: &mut Frame<'_>, area: Rect, app: &SetupApp) {
    let lines = match app.stage {
        SetupStage::Source => setup_source_lines(app),
        SetupStage::Targets => setup_target_lines(app),
        SetupStage::McpMode => setup_mcp_mode_lines(app),
        SetupStage::McpServers => setup_mcp_server_lines(app),
        SetupStage::CursorHistory => setup_history_lines(app),
        SetupStage::Review => setup_review_lines(app),
    };
    let lines = if app.stage == SetupStage::Review {
        lines
    } else {
        visible_lines(lines, 3, app.cursor, area.height as usize)
    };
    frame.render_widget(Paragraph::new(lines), area);
}

fn visible_lines(
    lines: Vec<Line<'static>>,
    header_count: usize,
    cursor: usize,
    height: usize,
) -> Vec<Line<'static>> {
    if lines.len() <= height || height <= header_count {
        return lines.into_iter().take(height).collect();
    }

    let header_count = header_count.min(lines.len());
    let option_count = lines.len() - header_count;
    let visible_options = height - header_count;
    let max_start = option_count.saturating_sub(visible_options);
    let start = cursor
        .saturating_sub(visible_options.saturating_sub(1))
        .min(max_start);

    let mut lines = lines.into_iter();
    let header = lines.by_ref().take(header_count).collect::<Vec<_>>();
    header
        .into_iter()
        .chain(lines.skip(start).take(visible_options))
        .collect()
}

fn setup_source_lines(app: &SetupApp) -> Vec<Line<'static>> {
    let options = app.source_options();
    let mut lines = prompt_lines(
        "Choose the canonical source",
        "This agent owns the configuration copied to every target.",
    );
    if options.is_empty() {
        lines.push(empty_line("No agent installation was found."));
    } else {
        for (index, agent) in options.iter().enumerate() {
            let available = app.source_is_available(*agent);
            let mut line = choice_line(
                index == app.cursor,
                app.source == Some(*agent),
                agent.label(),
                if available {
                    ""
                } else {
                    "configured source is not available"
                },
            );
            if !available {
                line = line.patch_style(style(Tone::Attention));
            }
            lines.push(line);
        }
    }
    lines
}

fn setup_target_lines(app: &SetupApp) -> Vec<Line<'static>> {
    let options = app.target_options();
    let mut lines = prompt_lines(
        "Choose target agents",
        "You will review the target changes before anything is written.",
    );
    if options.is_empty() {
        lines.push(empty_line("No target agents are available."));
    } else {
        for (index, agent) in options.iter().enumerate() {
            lines.push(choice_line(
                index == app.cursor,
                app.targets.contains(agent),
                agent.label(),
                "",
            ));
        }
    }
    lines
}

fn setup_mcp_mode_lines(app: &SetupApp) -> Vec<Line<'static>> {
    let selected = match app.mcp {
        McpChoice::None => 0,
        McpChoice::Selected(_) => 1,
        McpChoice::All => 2,
    };
    let options = [
        ("None", "Leave target MCP configuration unchanged"),
        ("Selected", "Copy only reviewed server names"),
        ("All", "Copy every source MCP server"),
    ];
    let mut lines = prompt_lines(
        "Choose MCP scope",
        "Selected is the safest choice when targets have local servers.",
    );
    for (index, (label, detail)) in options.iter().enumerate() {
        lines.push(choice_line(
            index == app.cursor,
            index == selected,
            label,
            detail,
        ));
    }
    lines
}

fn setup_mcp_server_lines(app: &SetupApp) -> Vec<Line<'static>> {
    let selected = match &app.mcp {
        McpChoice::Selected(servers) => servers,
        _ => return vec![empty_line("Go back and choose Selected MCP servers.")],
    };
    let mut lines = prompt_lines(
        "Choose MCP servers",
        "Only these server names will be included in the managed sync.",
    );
    let options = app.mcp_server_options();
    if options.is_empty() {
        lines.push(empty_line("No MCP servers were found at the source."));
    } else {
        for (index, server) in options.iter().enumerate() {
            let available = app.mcp_server_is_available(server);
            let mut line = choice_line(
                index == app.cursor,
                selected.contains(server),
                server,
                if available {
                    ""
                } else {
                    "configured, not currently found"
                },
            );
            if !available {
                line = line.patch_style(style(Tone::Attention));
            }
            lines.push(line);
        }
    }
    lines
}

fn setup_history_lines(app: &SetupApp) -> Vec<Line<'static>> {
    let selected = match app.cursor_history {
        CursorHistoryChoice::Disabled => 0,
        CursorHistoryChoice::ExportOnly => 1,
        CursorHistoryChoice::ExportAndQmd => 2,
    };
    let qmd_detail = if app.qmd_available {
        "Privately export chats and refresh a dedicated QMD collection"
    } else if app.cursor_history == CursorHistoryChoice::ExportAndQmd {
        "Configured; QMD is not currently available"
    } else {
        "QMD is not available"
    };
    let options = [
        ("Disabled", "Do not install the Cursor history hook"),
        ("Export only", "Maintain Markdown exports without indexing"),
        ("Export + QMD", qmd_detail),
    ];
    let mut lines = prompt_lines(
        "Choose Cursor history behavior",
        "Exports contain chat text. They stay private under ~/.agent-sync.",
    );
    for (index, (label, detail)) in options.iter().enumerate() {
        let checked = index == selected;
        let mut line = choice_line(index == app.cursor, checked, label, detail);
        if index == 2 && !app.qmd_available {
            let tone = if checked {
                Tone::Attention
            } else {
                Tone::Muted
            };
            line = line.patch_style(style(tone));
        }
        lines.push(line);
    }
    lines
}

fn setup_review_lines(app: &SetupApp) -> Vec<Line<'static>> {
    let mut lines = prompt_lines(
        "Review setup",
        "Press Enter to review and confirm this setup.",
    );
    lines.push(review_line("Route", app.route(), Tone::Normal));
    lines.push(review_line("MCP", mcp_summary(&app.mcp), Tone::Normal));
    lines.push(review_line(
        "References",
        if app.include_references {
            "included"
        } else {
            "excluded"
        }
        .to_string(),
        if app.include_references {
            Tone::Attention
        } else {
            Tone::Healthy
        },
    ));
    if app.source == Some(Agent::Cursor) || app.targets.contains(&Agent::Cursor) {
        lines.push(review_line(
            "History",
            history_summary(app.cursor_history),
            Tone::Normal,
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "› Continue to preview",
        Style::default().fg(FOCUS).add_modifier(Modifier::BOLD),
    )));
    lines
}

fn prompt_lines(title: &str, detail: &str) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            title.to_owned(),
            Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(detail.to_owned(), style(Tone::Muted))),
        Line::from(""),
    ]
}

fn choice_line(selected: bool, checked: bool, label: &str, detail: &str) -> Line<'static> {
    let marker = if selected { "›" } else { " " };
    let check = if checked { "●" } else { "○" };
    let label_style = if selected {
        Style::default().fg(FOCUS).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(PRIMARY)
    };
    let mut spans = vec![
        Span::styled(format!("{marker} {check} "), label_style),
        Span::styled(label.to_owned(), label_style),
    ];
    if !detail.is_empty() {
        spans.push(Span::styled(format!("  {detail}"), style(Tone::Muted)));
    }
    Line::from(spans)
}

fn empty_line(message: &str) -> Line<'static> {
    Line::from(Span::styled(message.to_owned(), style(Tone::Attention)))
}

fn review_line(label: &str, value: String, tone: Tone) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<12}"), style(Tone::Muted)),
        Span::styled(value, style(tone)),
    ])
}

fn mcp_summary(choice: &McpChoice) -> String {
    match choice {
        McpChoice::None => "none".into(),
        McpChoice::Selected(servers) => format!("selected: {}", servers.join(", ")),
        McpChoice::All => "all source servers".into(),
    }
}

fn history_summary(choice: CursorHistoryChoice) -> String {
    match choice {
        CursorHistoryChoice::Disabled => "disabled",
        CursorHistoryChoice::ExportOnly => "export only",
        CursorHistoryChoice::ExportAndQmd => "export + QMD",
    }
    .into()
}

fn render_setup_footer(frame: &mut Frame<'_>, area: Rect, app: &SetupApp) {
    let message = app
        .message
        .as_ref()
        .map(|(tone, message)| (*tone, message.as_str()))
        .or_else(|| {
            app.supplied_error
                .as_deref()
                .map(|message| (Tone::Error, message))
        });
    let line = if let Some((tone, message)) = message {
        Line::from(Span::styled(message.to_owned(), style(tone)))
    } else {
        let help = match app.stage {
            SetupStage::Source => "↑↓ move  enter choose  esc or q cancel",
            SetupStage::Targets | SetupStage::McpServers => {
                "↑↓ move  space toggle  enter next  esc back  q cancel"
            }
            SetupStage::Review => "enter preview  esc back  q cancel",
            SetupStage::McpMode | SetupStage::CursorHistory => {
                "↑↓ move  enter choose  esc back  q cancel"
            }
        };
        Line::from(Span::styled(help, style(Tone::Muted)))
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn render_status(frame: &mut Frame<'_>, area: Rect, app: &StatusApp) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Length((app.screen.summary.len() as u16).saturating_add(2)),
            Constraint::Min(4),
            Constraint::Length(2),
        ])
        .split(area);

    render_route(frame, rows[0], &app.route());
    render_health(frame, rows[1], app.screen.health);
    render_summary(frame, rows[2], &app.screen.summary);
    render_actions(frame, rows[3], app);
    render_status_footer(frame, rows[4], app);
}

fn render_health(frame: &mut Frame<'_>, area: Rect, health: HealthState) {
    let status = match health {
        HealthState::Healthy => vec![Span::styled(
            "HEALTHY",
            style(Tone::Healthy).add_modifier(Modifier::BOLD),
        )],
        HealthState::Attention => vec![
            Span::styled("HEALTHY", style(Tone::Healthy).add_modifier(Modifier::BOLD)),
            Span::styled(" · ", style(Tone::Muted)),
            Span::styled("NOTE", style(Tone::Attention).add_modifier(Modifier::BOLD)),
        ],
        HealthState::Error => vec![Span::styled(
            "ERROR",
            style(Tone::Error).add_modifier(Modifier::BOLD),
        )],
        HealthState::Unknown => vec![Span::styled(
            "UNKNOWN",
            style(Tone::Muted).add_modifier(Modifier::BOLD),
        )],
    };
    let mut line = vec![Span::styled("health ", style(Tone::Muted))];
    line.extend(status);
    frame.render_widget(Paragraph::new(Line::from(line)), area);
}

fn render_summary(frame: &mut Frame<'_>, area: Rect, summary: &[StatusLine]) {
    let mut lines = vec![Line::from(Span::styled(
        "Status",
        Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD),
    ))];
    if summary.is_empty() {
        lines.push(Line::from(Span::styled(
            "No status details available.",
            style(Tone::Muted),
        )));
    } else {
        let label_width = summary
            .iter()
            .map(|line| line.label.chars().count())
            .max()
            .unwrap_or(0)
            .min(18);
        for item in summary {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{:<width$}  ", item.label, width = label_width),
                    style(Tone::Muted),
                ),
                Span::styled(item.value.clone(), style(item.tone)),
            ]));
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_actions(frame: &mut Frame<'_>, area: Rect, app: &StatusApp) {
    let heading = Line::from(Span::styled(
        "Actions",
        Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD),
    ));
    let mut action_lines = Vec::new();
    if app.screen.actions.is_empty() {
        action_lines.push(Line::from(Span::styled(
            "No actions available.",
            style(Tone::Muted),
        )));
    } else {
        for (index, action) in app.screen.actions.iter().enumerate() {
            let selected = index == app.cursor;
            let marker_style = if selected {
                Style::default().fg(FOCUS).add_modifier(Modifier::BOLD)
            } else {
                style(Tone::Muted)
            };
            let mut label_style = style(action.tone);
            if selected {
                label_style = label_style.add_modifier(Modifier::BOLD);
            }
            action_lines.push(Line::from(vec![
                Span::styled(if selected { "› " } else { "  " }, marker_style),
                Span::styled(action.label.clone(), label_style),
                Span::styled(format!("  {}", action.detail), style(Tone::Muted)),
            ]));
        }
    }
    let available = (area.height as usize).saturating_sub(1);
    let max_start = action_lines.len().saturating_sub(available);
    let start = app
        .cursor
        .saturating_sub(available.saturating_sub(1))
        .min(max_start);
    let mut lines = vec![heading];
    lines.extend(action_lines.into_iter().skip(start).take(available));
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_status_footer(frame: &mut Frame<'_>, area: Rect, app: &StatusApp) {
    let line = if let Some(message) = &app.screen.message {
        let tone = match app.screen.health {
            HealthState::Healthy => Tone::Healthy,
            HealthState::Attention => Tone::Attention,
            HealthState::Error => Tone::Error,
            HealthState::Unknown => Tone::Muted,
        };
        Line::from(Span::styled(message.clone(), style(tone)))
    } else if app.screen.actions.is_empty() {
        Line::from(Span::styled("q or esc  close", style(Tone::Muted)))
    } else {
        Line::from(Span::styled(
            "↑↓ move  enter select  q or esc close",
            style(Tone::Muted),
        ))
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn style(tone: Tone) -> Style {
    let color = match tone {
        Tone::Normal => PRIMARY,
        Tone::Muted => MUTED,
        Tone::Healthy => HEALTHY,
        Tone::Attention => ATTENTION,
        Tone::Error => ERROR,
    };
    Style::default().fg(color)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn setup_screen() -> SetupScreen {
        SetupScreen {
            available_sources: vec![Agent::Codex, Agent::Claude, Agent::Cursor],
            target_agents: vec![Agent::Codex, Agent::Claude, Agent::Cursor],
            preserve_initial_source: false,
            mcp_servers: vec![
                AgentMcpServers {
                    agent: Agent::Codex,
                    servers: vec!["github".into(), "qmd".into()],
                },
                AgentMcpServers {
                    agent: Agent::Claude,
                    servers: vec!["github".into()],
                },
                AgentMcpServers {
                    agent: Agent::Cursor,
                    servers: vec!["cursor-tools".into()],
                },
            ],
            qmd_available: true,
            include_references: false,
            initial: SetupSelection {
                source: Agent::Codex,
                targets: vec![Agent::Claude],
                mcp: McpChoice::None,
                cursor_history: CursorHistoryChoice::Disabled,
            },
            error: None,
        }
    }

    fn render_text(app: App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| render(frame, &app))
            .expect("render succeeds");
        let buffer = terminal.backend().buffer();
        let mut rendered = String::new();
        for y in 0..height {
            for x in 0..width {
                rendered.push_str(buffer[(x, y)].symbol());
            }
            rendered.push('\n');
        }
        rendered
    }

    #[test]
    fn setup_renders_signature_route_and_progressive_source_step() {
        let text = render_text(App::new(TuiRequest::Setup(setup_screen())), 90, 24);

        assert!(text.contains("Codex → Claude Code"));
        assert!(text.contains("Choose the canonical source"));
        assert!(!text.contains("Choose Cursor history behavior"));
    }

    #[test]
    fn target_step_requires_one_target() {
        let mut screen = setup_screen();
        screen.initial.targets.clear();
        let mut app = SetupApp::new(screen);
        app.stage = SetupStage::Targets;

        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        assert_eq!(app.stage, SetupStage::Targets);
        assert_eq!(
            app.message,
            Some((Tone::Attention, "Choose at least one target agent.".into()))
        );
    }

    #[test]
    fn changing_source_removes_it_from_targets() {
        let mut screen = setup_screen();
        screen.initial.targets = vec![Agent::Claude, Agent::Cursor];
        let mut app = SetupApp::new(screen);
        app.cursor = 1;

        app.toggle_current();

        assert_eq!(app.source, Some(Agent::Claude));
        assert_eq!(app.targets, vec![Agent::Cursor]);
    }

    #[test]
    fn changing_source_replaces_the_mcp_inventory_and_drops_unavailable_names() {
        let mut screen = setup_screen();
        screen.initial.mcp = McpChoice::Selected(vec!["qmd".into()]);
        let mut app = SetupApp::new(screen);
        app.cursor = 1;

        app.toggle_current();

        assert_eq!(app.source, Some(Agent::Claude));
        assert_eq!(app.available_mcp_servers(), &["github".to_string()]);
        assert_eq!(app.mcp, McpChoice::Selected(Vec::new()));
    }

    #[test]
    fn configured_mcp_names_survive_temporary_discovery_gaps() {
        let mut screen = setup_screen();
        screen.initial.mcp = McpChoice::Selected(vec!["qmd".into(), "temporarily-missing".into()]);
        let mut app = SetupApp::new(screen);
        app.stage = SetupStage::McpServers;

        assert_eq!(
            app.mcp,
            McpChoice::Selected(vec!["qmd".into(), "temporarily-missing".into()])
        );
        assert_eq!(
            app.selection().unwrap().mcp,
            McpChoice::Selected(vec!["qmd".into(), "temporarily-missing".into()])
        );
        assert_eq!(
            app.mcp_server_options(),
            vec!["github", "qmd", "temporarily-missing"]
        );
        let text = render_text(App::Setup(app), 90, 24);
        assert!(text.contains("temporarily-missing"));
        assert!(text.contains("configured, not currently found"));
    }

    #[test]
    fn cursor_can_be_the_source_and_enables_the_history_step() {
        let mut screen = setup_screen();
        screen.initial.source = Agent::Cursor;
        screen.initial.targets = vec![Agent::Codex];
        let app = SetupApp::new(screen);

        assert_eq!(app.source, Some(Agent::Cursor));
        assert_eq!(app.available_mcp_servers(), &["cursor-tools".to_string()]);
        assert!(app.stages().contains(&SetupStage::CursorHistory));
    }

    #[test]
    fn mcp_step_never_renders_servers_owned_only_by_another_source() {
        let mut app = SetupApp::new(setup_screen());
        app.stage = SetupStage::McpServers;
        app.mcp = McpChoice::Selected(Vec::new());

        let text = render_text(App::Setup(app), 80, 24);

        assert!(text.contains("github"));
        assert!(!text.contains("cursor-tools"));
    }

    #[test]
    fn selected_mcp_mode_adds_server_step_and_toggles_names() {
        let mut app = SetupApp::new(setup_screen());
        app.stage = SetupStage::McpMode;
        app.cursor = 1;

        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        assert_eq!(app.stage, SetupStage::McpServers);
        assert_eq!(app.mcp, McpChoice::Selected(Vec::new()));

        app.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(app.mcp, McpChoice::Selected(vec!["github".into()]));
    }

    #[test]
    fn qmd_choice_is_unavailable_when_qmd_is_missing() {
        let mut screen = setup_screen();
        screen.initial.targets.push(Agent::Cursor);
        screen.qmd_available = false;
        let mut app = SetupApp::new(screen);
        app.stage = SetupStage::CursorHistory;
        app.cursor = 2;

        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        assert_eq!(app.stage, SetupStage::CursorHistory);
        assert_eq!(app.cursor_history, CursorHistoryChoice::Disabled);
        assert!(matches!(app.message, Some((Tone::Attention, _))));
    }

    #[test]
    fn configured_qmd_choice_survives_temporary_qmd_unavailability() {
        let mut screen = setup_screen();
        screen.initial.targets.push(Agent::Cursor);
        screen.initial.cursor_history = CursorHistoryChoice::ExportAndQmd;
        screen.qmd_available = false;
        let mut app = SetupApp::new(screen);
        app.stage = SetupStage::CursorHistory;
        app.sync_cursor_to_value();

        assert_eq!(app.cursor_history, CursorHistoryChoice::ExportAndQmd);
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        assert_eq!(app.stage, SetupStage::Review);
        assert_eq!(app.cursor_history, CursorHistoryChoice::ExportAndQmd);
        assert_eq!(
            app.selection().unwrap().cursor_history,
            CursorHistoryChoice::ExportAndQmd
        );
    }

    #[test]
    fn configured_unavailable_source_is_visible_but_cannot_be_saved() {
        let mut screen = setup_screen();
        screen.available_sources = vec![Agent::Cursor];
        screen.preserve_initial_source = true;
        let app = SetupApp::new(screen);

        assert_eq!(app.source, Some(Agent::Codex));
        assert_eq!(app.source_options(), vec![Agent::Codex, Agent::Cursor]);
        let text = render_text(App::Setup(app), 90, 24);
        assert!(text.contains("configured source is not available"));

        let mut screen = setup_screen();
        screen.available_sources = vec![Agent::Cursor];
        screen.preserve_initial_source = true;
        let mut app = SetupApp::new(screen);
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        assert_eq!(app.stage, SetupStage::Source);
        assert!(matches!(app.message, Some((Tone::Attention, _))));
    }

    #[test]
    fn first_setup_selects_only_an_available_source() {
        let mut screen = setup_screen();
        screen.available_sources = vec![Agent::Cursor];
        let app = SetupApp::new(screen);

        assert_eq!(app.source, Some(Agent::Cursor));
        assert_eq!(app.source_options(), vec![Agent::Cursor]);
    }

    #[test]
    fn review_shows_the_preserved_references_policy() {
        let mut screen = setup_screen();
        screen.include_references = true;
        let mut app = SetupApp::new(screen);
        app.stage = SetupStage::Review;

        let text = render_text(App::Setup(app), 90, 24);

        assert!(text.contains("References"));
        assert!(text.contains("included"));
    }

    #[test]
    fn review_returns_the_complete_selection_without_writing() {
        let mut app = SetupApp::new(setup_screen());
        app.stage = SetupStage::Review;

        let outcome = app.handle_key(key(KeyCode::Enter));

        assert_eq!(
            outcome,
            Some(TuiOutcome::Setup(SetupSelection {
                source: Agent::Codex,
                targets: vec![Agent::Claude],
                mcp: McpChoice::None,
                cursor_history: CursorHistoryChoice::Disabled,
            }))
        );
    }

    #[test]
    fn status_navigation_returns_the_selected_action_id() {
        let mut app = StatusApp::new(StatusScreen {
            source: Some(Agent::Codex),
            targets: vec![Agent::Claude],
            health: HealthState::Attention,
            summary: Vec::new(),
            actions: vec![
                ActionItem {
                    id: "sync".into(),
                    label: "Preview sync".into(),
                    detail: String::new(),
                    tone: Tone::Normal,
                },
                ActionItem {
                    id: "doctor".into(),
                    label: "Run doctor".into(),
                    detail: String::new(),
                    tone: Tone::Attention,
                },
            ],
            message: None,
        });

        app.handle_key(key(KeyCode::Down));

        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Some(TuiOutcome::Action("doctor".into()))
        );
    }

    #[test]
    fn status_renders_empty_and_error_states() {
        let app = App::new(TuiRequest::Status(StatusScreen {
            source: None,
            targets: Vec::new(),
            health: HealthState::Error,
            summary: Vec::new(),
            actions: Vec::new(),
            message: Some("Managed config is unreadable.".into()),
        }));

        let text = render_text(app, 80, 24);

        assert!(text.contains("agent-sync"));
        assert!(text.contains("ERROR"));
        assert!(text.contains("No status details available."));
        assert!(text.contains("No actions available."));
        assert!(text.contains("Managed config is unreadable."));
    }

    #[test]
    fn status_renders_advisory_warnings_as_healthy_notes() {
        let app = App::new(TuiRequest::Status(StatusScreen {
            source: Some(Agent::Codex),
            targets: vec![Agent::Cursor],
            health: HealthState::Attention,
            summary: Vec::new(),
            actions: Vec::new(),
            message: Some("20 target-owned resource(s) are intentionally preserved".into()),
        }));

        let text = render_text(app, 80, 24);

        assert!(text.contains("HEALTHY · NOTE"), "{text}");
        assert!(!text.contains("ATTENTION"), "{text}");
        assert!(text.contains("20 target-owned resource(s) are intentionally preserved"));
    }

    #[test]
    fn small_terminal_renders_resize_state() {
        let text = render_text(App::new(TuiRequest::Setup(setup_screen())), 40, 10);

        assert!(text.contains("Terminal too small"));
        assert!(text.contains("Resize to at least 52×16."));
    }

    #[test]
    fn long_mcp_list_keeps_the_focused_server_visible() {
        let mut screen = setup_screen();
        screen.mcp_servers[0].servers = (0..12).map(|index| format!("server-{index:02}")).collect();
        screen.initial.mcp = McpChoice::Selected(vec!["server-00".into()]);
        let mut setup = SetupApp::new(screen);
        setup.stage = SetupStage::McpServers;
        setup.cursor = 11;

        let text = render_text(App::Setup(setup), 60, 16);

        assert!(text.contains("server-11"), "{text}");
        assert!(!text.contains("server-01"));
    }

    #[test]
    fn long_action_list_keeps_the_focused_action_visible() {
        let actions = (0..12)
            .map(|index| ActionItem {
                id: format!("action-{index:02}"),
                label: format!("Action {index:02}"),
                detail: String::new(),
                tone: Tone::Normal,
            })
            .collect();
        let mut status = StatusApp::new(StatusScreen {
            source: Some(Agent::Codex),
            targets: vec![Agent::Cursor],
            health: HealthState::Healthy,
            summary: Vec::new(),
            actions,
            message: None,
        });
        status.cursor = 11;

        let text = render_text(App::Status(status), 60, 16);

        assert!(text.contains("Action 11"));
        assert!(!text.contains("Action 01"));
    }

    #[test]
    fn escape_moves_back_then_cancels_at_the_first_step() {
        let mut app = SetupApp::new(setup_screen());
        app.stage = SetupStage::Targets;

        assert_eq!(app.handle_key(key(KeyCode::Esc)), None);
        assert_eq!(app.stage, SetupStage::Source);
        assert_eq!(
            app.handle_key(key(KeyCode::Esc)),
            Some(TuiOutcome::Cancelled)
        );
    }

    #[test]
    fn global_quit_key_cancels_any_screen() {
        let mut app = App::new(TuiRequest::Setup(setup_screen()));

        assert_eq!(
            app.handle_key(key(KeyCode::Char('q'))),
            Some(TuiOutcome::Cancelled)
        );
    }
}
