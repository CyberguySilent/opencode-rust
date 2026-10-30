use crate::agent::{Agent, AgentEvent, Decision};
use crate::config::{Config, Permission, Permissions};
use crate::session::{Message, Session, SessionMeta, Store};
use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::collections::HashMap;
use std::io;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

mod editor;
mod ui;

use editor::Editor;

const TICK: Duration = Duration::from_millis(80);
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const MAX_INPUT_LINES: usize = 6;

pub struct PendingPermission {
    pub tool: String,
    pub detail: String,
    pub reply: Option<oneshot::Sender<Decision>>,
}

pub struct Picker {
    pub items: Vec<SessionMeta>,
    pub selected: usize,
}

pub struct App {
    pub config: Config,
    pub store: Store,
    pub session: Session,
    pub metas: Vec<SessionMeta>,
    pub editor: Editor,
    pub scroll_from_bottom: u16,
    pub chat_inner_height: u16,
    pub busy: bool,
    pub stream_text: String,
    pub active_tool: Option<(String, String)>,
    pub tool_status: HashMap<String, &'static str>,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub pending: Option<PendingPermission>,
    pub picker: Option<Picker>,
    pub show_help: bool,
    pub spinner: usize,
    pub started: Instant,
    pub should_quit: bool,
    pub events_tx: tokio::sync::mpsc::UnboundedSender<AgentEvent>,
    pub events_rx: tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
    default_permissions: Permissions,
}

impl App {
    pub fn new(config: Config) -> Result<Self> {
        let store = Store::new(config.session_dir.clone())?;
        let mut metas = store.list()?;
        let session = store
            .latest()?
            .filter(|s| !s.messages.is_empty())
            .unwrap_or_else(Session::new);
        if metas.is_empty() {
            metas.push(SessionMeta {
                id: session.id.clone(),
                title: session.title.clone(),
                created_at: session.created_at,
                updated_at: session.updated_at,
                count: session.messages.len(),
            });
        }
        let default_permissions = config.permissions.clone();
        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
        Ok(Self {
            config,
            store,
            session,
            metas,
            editor: Editor::new(),
            scroll_from_bottom: 0,
            chat_inner_height: 10,
            busy: false,
            stream_text: String::new(),
            active_tool: None,
            tool_status: HashMap::new(),
            error: None,
            notice: None,
            pending: None,
            picker: None,
            show_help: false,
            spinner: 0,
            started: Instant::now(),
            should_quit: false,
            events_tx,
            events_rx,
            default_permissions,
        })
    }

    fn refresh_metas(&mut self) {
        if let Ok(metas) = self.store.list() {
            if !metas.is_empty() {
                self.metas = metas;
                return;
            }
        }
        self.metas = vec![SessionMeta {
            id: self.session.id.clone(),
            title: self.session.title.clone(),
            created_at: self.session.created_at,
            updated_at: self.session.updated_at,
            count: self.session.messages.len(),
        }];
    }

    fn save(&mut self) {
        let _ = self.store.save(&self.session);
        self.refresh_metas();
    }

    pub fn tick(&mut self) {
        self.spinner = (self.spinner + 1) % SPINNER.len();
    }

    pub fn spinner_char(&self) -> &'static str {
        SPINNER[self.spinner]
    }

    fn on_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Text(delta) => self.stream_text.push_str(&delta),
            AgentEvent::Message(message) => {
                self.stream_text.clear();
                self.session.push(message);
            }
            AgentEvent::ToolStart { name, detail, .. } => {
                self.active_tool = Some((name, detail));
            }
            AgentEvent::ToolEnd {
                id,
                status,
                ..
            } => {
                self.tool_status.insert(id, status);
                self.active_tool = None;
            }
            AgentEvent::Permission { tool, detail, reply } => {
                self.pending = Some(PendingPermission {
                    tool,
                    detail,
                    reply: Some(reply),
                });
            }
            AgentEvent::PermissionAlways { tool } => {
                self.config.permissions.set_tool(&tool, Permission::Allow);
                self.default_permissions.set_tool(&tool, Permission::Allow);
            }
            AgentEvent::Error(error) => self.error = Some(error),
            AgentEvent::TurnDone => {
                self.busy = false;
                self.stream_text.clear();
                self.active_tool = None;
                self.save();
            }
        }
    }

    fn submit(&mut self) {
        if self.busy {
            self.notice = Some("busy — wait for the turn to finish".into());
            return;
        }
        let text = self.editor.take_text();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        if trimmed.starts_with('/') {
            self.run_command(trimmed);
            return;
        }
        self.error = None;
        self.notice = None;
        self.session.push(Message::user(trimmed));
        self.busy = true;
        self.stream_text.clear();
        self.active_tool = None;
        self.scroll_from_bottom = 0;
        self.started = Instant::now();

        let messages = self.session.messages.clone();
        let tx = self.events_tx.clone();
        let config = self.config.clone();
        tokio::spawn(async move {
            let mut agent = match Agent::new(&config) {
                Ok(agent) => agent,
                Err(error) => {
                    let _ = tx.send(AgentEvent::Error(error));
                    let _ = tx.send(AgentEvent::TurnDone);
                    return;
                }
            };
            let _ = agent.run_turn(messages, &tx).await;
            let _ = tx.send(AgentEvent::TurnDone);
        });
    }

    fn new_session(&mut self) {
        if !self.session.messages.is_empty() {
            self.save();
        }
        self.session = Session::new();
        self.metas.push(SessionMeta {
            id: self.session.id.clone(),
            title: self.session.title.clone(),
            created_at: self.session.created_at,
            updated_at: self.session.updated_at,
            count: 0,
        });
        self.stream_text.clear();
        self.error = None;
        self.notice = Some("new session".into());
        self.tool_status.clear();
        self.scroll_from_bottom = 0;
    }

    fn open_picker(&mut self) {
        let items = self.store.list().unwrap_or_default();
        let selected = items
            .iter()
            .position(|m| m.id == self.session.id)
            .unwrap_or(0);
        self.picker = Some(Picker { items, selected });
    }

    fn pick_selected(&mut self) {
        let Some(picker) = self.picker.take() else { return };
        let Some(meta) = picker.items.get(picker.selected) else {
            self.new_session();
            return;
        };
        if meta.id == self.session.id {
            return;
        }
        if !self.session.messages.is_empty() {
            self.save();
        }
        match self.store.load(&meta.id) {
            Ok(Some(loaded)) => {
                self.session = loaded;
                self.notice = Some(format!("switched to {}", self.session.title));
            }
            _ => {
                self.session = Session::new();
                self.notice = Some("session not found".into());
            }
        }
        self.stream_text.clear();
        self.error = None;
        self.tool_status.clear();
        self.scroll_from_bottom = 0;
    }

    fn run_command(&mut self, text: &str) {
        let mut parts = text.splitn(2, char::is_whitespace);
        let command = parts.next().unwrap_or("");
        let arg = parts.next().unwrap_or("").trim();

        match command {
            "/help" | "/h" | "/?" => self.show_help = true,
            "/new" => self.new_session(),
            "/clear" => {
                self.session.clear();
                self.tool_status.clear();
                self.error = None;
                self.stream_text.clear();
                self.scroll_from_bottom = 0;
                self.notice = Some("conversation cleared".into());
            }
            "/model" => {
                if arg.is_empty() {
                    self.notice = Some(format!("model: {}", self.config.model));
                } else {
                    self.config.model = arg.to_string();
                    self.notice = Some(format!("model → {arg}"));
                }
            }
            "/provider" => match crate::config::ProviderKind::parse(arg) {
                Ok(kind) => {
                    self.config.apply_provider(kind);
                    self.notice = Some(format!(
                        "provider → {} ({})",
                        kind.label(),
                        self.config.base_url
                    ));
                }
                Err(_) if arg.is_empty() => {
                    self.notice = Some(format!(
                        "provider: {} · {}",
                        self.config.provider.label(),
                        self.config.base_url
                    ));
                }
                Err(error) => self.error = Some(error.to_string()),
            },
            "/config" => self.notice = Some(self.config.describe()),
            "/sessions" => self.open_picker(),
            "/yolo" => {
                self.config.permissions = Permissions::allow_all();
                self.notice = Some("permissions: allow everything".into());
            }
            "/ask" => {
                self.config.permissions = self.default_permissions.clone();
                self.notice = Some("permissions: ask before bash/write/edit".into());
            }
            "/permission" => {
                if let Some((tool, value)) = arg.split_once(char::is_whitespace) {
                    match Permission::parse(value) {
                        Ok(level) if self.tool_exists(tool) => {
                            self.config.permissions.set_tool(tool, level);
                            self.default_permissions.set_tool(tool, level);
                            self.notice = Some(format!("{tool} → {}", level.label()));
                        }
                        Ok(_) => self.error = Some(format!("unknown tool `{tool}`")),
                        Err(error) => self.error = Some(error.to_string()),
                    }
                } else {
                    let p = &self.config.permissions;
                    self.notice = Some(format!(
                        "bash={} read={} write={} edit={}",
                        p.bash.label(),
                        p.read.label(),
                        p.write.label(),
                        p.edit.label()
                    ));
                }
            }
            "/quit" | "/q" | "/exit" => self.should_quit = true,
            other => self.error = Some(format!("unknown command `{other}`, try /help")),
        }
    }

    fn tool_exists(&self, tool: &str) -> bool {
        matches!(
            tool,
            "bash" | "read" | "write" | "edit" | "glob" | "grep"
        )
    }

    fn handle_permission(&mut self, key: &KeyEvent) -> bool {
        if self.pending.is_none() {
            return false;
        }
        let decision = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(Decision::Allow),
            KeyCode::Char('a') | KeyCode::Char('A') => Some(Decision::AllowAlways),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(Decision::Deny),
            _ => None,
        };
        if let Some(decision) = decision {
            if let Some(mut pending) = self.pending.take() {
                if let Some(reply) = pending.reply.take() {
                    let _ = reply.send(decision);
                }
            }
        }
        true
    }

    fn handle_picker(&mut self, key: &KeyEvent) -> bool {
        let Some(picker) = self.picker.as_mut() else {
            return false;
        };
        let last = picker.items.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc => {
                self.picker = None;
            }
            KeyCode::Enter => {
                self.pick_selected();
            }
            KeyCode::Up | KeyCode::Char('k') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                picker.selected = picker.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                picker.selected = picker.selected.saturating_add(1).min(last);
            }
            KeyCode::PageUp => picker.selected = picker.selected.saturating_sub(5),
            KeyCode::PageDown => {
                picker.selected = picker.selected.saturating_add(5).min(last);
            }
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.picker = None;
                self.new_session();
            }
            _ => {}
        }
        true
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if self.handle_permission(&key) {
            return;
        }
        if self.handle_picker(&key) {
            return;
        }
        if self.show_help {
            match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::F(1) => {
                    self.show_help = false
                }
                _ => {}
            }
            return;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        match key.code {
            KeyCode::Char('c') if ctrl => {
                if self.editor.is_empty() {
                    self.should_quit = true;
                } else {
                    self.editor.clear();
                }
            }
            KeyCode::Char('d') if ctrl && self.editor.is_empty() => self.should_quit = true,
            KeyCode::Char('q') if ctrl && self.editor.is_empty() => self.should_quit = true,
            KeyCode::Char('n') if ctrl => self.new_session(),
            KeyCode::Char('k') if ctrl => self.open_picker(),
            KeyCode::Char('j') if ctrl => self.editor.newline(),
            KeyCode::Char('u') if ctrl => self.editor.clear(),
            KeyCode::Char('l') if ctrl => {
                self.error = None;
                self.notice = None;
            }
            KeyCode::F(1) => self.show_help = true,
            KeyCode::Enter if ctrl || alt || key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.editor.newline()
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Tab => {
                for _ in 0..2 {
                    self.editor.insert_char(' ');
                }
            }
            KeyCode::Backspace => self.editor.backspace(),
            KeyCode::Delete => self.editor.delete(),
            KeyCode::Left => self.editor.left(),
            KeyCode::Right => self.editor.right(),
            KeyCode::Up => self.editor.up(),
            KeyCode::Down => self.editor.down(),
            KeyCode::PageUp => {
                self.scroll_from_bottom = self
                    .scroll_from_bottom
                    .saturating_add(self.chat_inner_height.max(1));
            }
            KeyCode::PageDown => {
                self.scroll_from_bottom = self
                    .scroll_from_bottom
                    .saturating_sub(self.chat_inner_height.max(1));
            }
            KeyCode::End if ctrl => self.scroll_from_bottom = 0,
            KeyCode::Home if ctrl => self.scroll_from_bottom = u16::MAX,
            KeyCode::Home => self.editor.home(),
            KeyCode::End => self.editor.end(),
            KeyCode::Char(c) if !ctrl => self.editor.insert_char(c),
            _ => {}
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        if self.pending.is_some() || self.picker.is_some() || self.show_help {
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(3);
            }
            MouseEventKind::ScrollDown => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(3);
            }
            _ => {}
        }
    }

    fn on_paste(&mut self, text: String) {
        if self.pending.is_some() || self.picker.is_some() || self.show_help {
            return;
        }
        if self.editor.lines.len() + text.matches('\n').count() > MAX_INPUT_LINES * 4 {
            return;
        }
        self.editor.insert_str(&text);
    }

    pub fn shutdown(&mut self) {
        if !self.session.messages.is_empty() {
            let _ = self.store.save(&self.session);
        }
    }
}

type Tui = Terminal<CrosstermBackend<io::Stdout>>;

fn restore_terminal() {
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = execute!(
        stdout,
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    );
}

fn setup_terminal() -> Result<Tui> {
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous_hook(info);
    }));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    Ok(terminal)
}

pub fn run(config: Config) -> Result<()> {
    let mut terminal = setup_terminal()?;
    let mut app = App::new(config)?;

    loop {
        terminal.draw(|frame| ui::draw(frame, &mut app))?;

        while let Ok(event) = app.events_rx.try_recv() {
            app.on_agent_event(event);
        }

        if event::poll(TICK)? {
            match event::read()? {
                Event::Key(key) => app.on_key(key),
                Event::Paste(text) => app.on_paste(text),
                Event::Mouse(mouse) => app.on_mouse(mouse),
                Event::Resize(_, _) => {}
                _ => {}
            }
        } else {
            app.tick();
        }

        if app.should_quit {
            break;
        }
    }

    app.shutdown();
    restore_terminal();
    Ok(())
}
