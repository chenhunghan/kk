//! Ratatui TUI: split-pane chat (left 2/3) + log panel (right 1/3).

use std::collections::VecDeque;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use tokio::sync::mpsc;

use kk_core::nq;
use kk_core::paths::DataPaths;
use kk_core::types::{ChannelType, InboundMessage, OutboundMessage};

const CHANNEL_NAME: &str = "terminal";
const GROUP: &str = "terminal";
const MAX_LOG_LINES: usize = 500;

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

enum ChatEntry {
    User(String),
    Agent(String),
    Streaming(String),
}

struct App {
    entries: Vec<ChatEntry>,
    log_lines: VecDeque<String>,
    log_state: ListState,
    input: String,
}

impl App {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            log_lines: VecDeque::with_capacity(MAX_LOG_LINES),
            log_state: ListState::default(),
            input: String::new(),
        }
    }

    fn push_user(&mut self, text: String) {
        self.entries.push(ChatEntry::User(text));
    }

    fn push_agent(&mut self, text: String) {
        self.entries
            .retain(|e| !matches!(e, ChatEntry::Streaming(_)));
        self.entries.push(ChatEntry::Agent(text));
    }

    fn set_streaming(&mut self, text: String) {
        if let Some(last) = self.entries.last_mut()
            && let ChatEntry::Streaming(s) = last
        {
            *s = text;
        } else {
            self.entries.push(ChatEntry::Streaming(text));
        }
    }

    fn push_log(&mut self, line: String) {
        if self.log_lines.len() >= MAX_LOG_LINES {
            self.log_lines.pop_front();
        }
        self.log_lines.push_back(line);
        self.log_state.select(Some(self.log_lines.len() - 1));
    }
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Register the terminal channel in groups.d so the gateway routes to it.
pub fn register_terminal_group(paths: &DataPaths) -> Result<()> {
    use kk_core::types::{ChannelMapping, GroupEntry, GroupsConfig, TriggerMode};
    use std::collections::HashMap;

    let groups_d_file = paths.groups_d_dir().join(format!("{CHANNEL_NAME}.json"));

    if groups_d_file.exists() {
        return Ok(());
    }

    let config = GroupsConfig {
        groups: HashMap::from([(
            GROUP.to_string(),
            GroupEntry {
                trigger_mode: TriggerMode::Always,
                trigger_pattern: None,
                channels: HashMap::from([(
                    CHANNEL_NAME.to_string(),
                    ChannelMapping {
                        chat_id: "local".to_string(),
                    },
                )]),
            },
        )]),
    };

    let json = serde_json::to_string_pretty(&config)?;
    std::fs::write(&groups_d_file, json)?;
    Ok(())
}

/// Run the TUI. Blocks until the user quits (Ctrl+C or q).
pub async fn run(paths: DataPaths, log_rx: mpsc::UnboundedReceiver<String>) -> Result<()> {
    // Install a panic hook that restores the terminal before printing the panic.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
        original_hook(info);
    }));

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal, &paths, log_rx).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

// ---------------------------------------------------------------------------
// Main event loop
// ---------------------------------------------------------------------------

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    paths: &DataPaths,
    mut log_rx: mpsc::UnboundedReceiver<String>,
) -> Result<()> {
    // On startup, clear the stream and outbox dirs for the terminal channel.
    // Both are transient display state — leftover files from a killed session
    // would otherwise replay old responses into the fresh TUI.
    clear_channel_dirs(paths);

    let (resp_tx, mut resp_rx) = mpsc::unbounded_channel::<String>();
    let (stream_tx, mut stream_rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(poll_outbound_loop(paths.clone(), resp_tx, stream_tx));

    let mut app = App::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(50));

    loop {
        // Drain all channels before rendering
        while let Ok(line) = log_rx.try_recv() {
            app.push_log(line);
        }
        while let Ok(text) = stream_rx.try_recv() {
            app.set_streaming(text);
        }
        while let Ok(text) = resp_rx.try_recv() {
            app.push_agent(text);
        }

        // Process all pending keyboard events (non-blocking)
        while event::poll(Duration::ZERO)? {
            match event::read()? {
                Event::Key(key) => match (key.code, key.modifiers) {
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(()),
                    (KeyCode::Enter, _) => {
                        let text = app.input.trim().to_string();
                        if !text.is_empty() {
                            app.input.clear();
                            enqueue_message(paths, &text)?;
                            app.push_user(text);
                        }
                    }
                    (KeyCode::Backspace, _) => {
                        app.input.pop();
                    }
                    (KeyCode::Char(c), _) => {
                        app.input.push(c);
                    }
                    _ => {}
                },
                Event::Resize(_, _) => {} // triggers a redraw on next tick
                _ => {}
            }
        }

        terminal.draw(|f| render(f, &mut app))?;
        ticker.tick().await;
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render(f: &mut Frame, app: &mut App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(67), Constraint::Percentage(33)])
        .split(f.area());

    render_chat(f, app, cols[0]);
    render_logs(f, app, cols[1]);
}

fn render_chat(f: &mut Frame, app: &App, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(3)])
        .split(area);

    // Build lines — show last N entries that fit in the viewport
    let viewport_h = rows[0].height.saturating_sub(2) as usize; // subtract borders
    let lines_per_entry = 2usize; // message + blank line
    let max_entries = (viewport_h / lines_per_entry).max(1);
    let start = app.entries.len().saturating_sub(max_entries);

    let mut lines: Vec<Line> = Vec::new();
    for entry in &app.entries[start..] {
        match entry {
            ChatEntry::User(text) => {
                lines.push(Line::from(vec![
                    Span::styled(
                        "you  ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(text.as_str()),
                ]));
                lines.push(Line::raw(""));
            }
            ChatEntry::Agent(text) => {
                lines.push(Line::from(vec![
                    Span::styled(
                        "agent",
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                    Span::raw(text.as_str()),
                ]));
                lines.push(Line::raw(""));
            }
            ChatEntry::Streaming(text) => {
                lines.push(Line::from(vec![
                    Span::styled(
                        "agent",
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                    Span::styled(text.as_str(), Style::default().fg(Color::DarkGray)),
                    Span::styled(" ▌", Style::default().fg(Color::Yellow)),
                ]));
                lines.push(Line::raw(""));
            }
        }
    }

    let chat = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" chat "))
        .wrap(Wrap { trim: false });
    f.render_widget(chat, rows[0]);

    // Input box
    let input_text = format!("> {}", app.input);
    let input_box =
        Paragraph::new(input_text.as_str()).block(Block::default().borders(Borders::ALL));
    f.render_widget(input_box, rows[1]);

    // Show cursor at end of input
    let cursor_x = rows[1].x + 1 + "> ".len() as u16 + app.input.chars().count() as u16;
    let cursor_y = rows[1].y + 1;
    if cursor_x < rows[1].x + rows[1].width - 1 {
        f.set_cursor_position((cursor_x, cursor_y));
    }
}

fn render_logs(f: &mut Frame, app: &mut App, area: Rect) {
    let items: Vec<ListItem> = app
        .log_lines
        .iter()
        .map(|line| {
            let style = if line.contains("ERROR") || line.contains("\"level\":\"ERROR\"") {
                Style::default().fg(Color::Red)
            } else if line.contains("WARN") || line.contains("\"level\":\"WARN\"") {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            ListItem::new(Span::styled(line.as_str(), style))
        })
        .collect();

    let log_list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" logs "))
        .highlight_style(Style::default()); // no cursor highlight needed
    f.render_stateful_widget(log_list, area, &mut app.log_state);
}

// ---------------------------------------------------------------------------
// Message queueing + outbound polling
// ---------------------------------------------------------------------------

/// Archive any completed (non-running) terminal sessions before the results
/// loop starts. Called synchronously in main before spawning loops.
///
/// Without this, restarting kk-cli after a killed session would cause the
/// results loop to find a done session with no stream file (cleared by
/// clear_channel_dirs) and fall back to the outbox — replaying a stale
/// response in the fresh TUI.
pub fn archive_orphaned_sessions(paths: &DataPaths) {
    let results_root = paths.results_dir("");
    let done_dir = paths.results_done_dir();
    let _ = std::fs::create_dir_all(&done_dir);

    let Ok(entries) = std::fs::read_dir(&results_root) else {
        return;
    };

    for entry in entries.flatten() {
        let dir_name = entry.file_name().to_string_lossy().to_string();
        if dir_name.starts_with('.') {
            continue;
        }

        // Leave running sessions alone.
        let status = std::fs::read_to_string(paths.result_status(&dir_name)).unwrap_or_default();
        if status.trim() == "running" {
            continue;
        }

        // Only archive sessions that belong to this terminal channel.
        let manifest_path = paths.request_manifest(&dir_name);
        let Ok(raw) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        let channel = val.get("channel").and_then(|v| v.as_str()).unwrap_or("");
        if channel != CHANNEL_NAME {
            continue;
        }

        let src = paths.results_dir(&dir_name);
        let dst = done_dir.join(&dir_name);
        let _ = std::fs::rename(&src, &dst);
    }
}

/// Clear the stream and outbox dirs for the terminal channel on startup.
/// Both hold transient display state. A running kk-agent will re-populate
/// the stream dir; outbox nq messages from a previous session are stale.
fn clear_channel_dirs(paths: &DataPaths) {
    for dir in [
        paths.stream_dir(CHANNEL_NAME),
        paths.outbox_dir(CHANNEL_NAME),
    ] {
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

fn enqueue_message(paths: &DataPaths, text: &str) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let msg = InboundMessage {
        channel: CHANNEL_NAME.to_string(),
        channel_type: ChannelType::Terminal,
        group: GROUP.to_string(),
        thread_id: None,
        sender: "user".to_string(),
        text: text.to_string(),
        timestamp: now,
        meta: serde_json::json!({}),
    };

    let payload = serde_json::to_vec(&msg)?;
    nq::enqueue(&paths.inbound_dir(), now, &payload)?;
    Ok(())
}

enum StreamUpdate {
    Partial(String),
    Final(String),
}

async fn poll_outbound_loop(
    paths: DataPaths,
    resp_tx: mpsc::UnboundedSender<String>,
    stream_tx: mpsc::UnboundedSender<String>,
) {
    let outbox_dir = paths.outbox_dir(CHANNEL_NAME);
    let stream_dir = paths.stream_dir(CHANNEL_NAME);
    let mut last_stream_text: Option<String> = None;

    loop {
        match read_stream_update(&stream_dir, &mut last_stream_text) {
            Some(StreamUpdate::Partial(text)) => {
                let _ = stream_tx.send(text);
            }
            Some(StreamUpdate::Final(text)) => {
                last_stream_text = None;
                let _ = resp_tx.send(text);
            }
            None => {}
        }

        if let Ok(files) = nq::list_pending(&outbox_dir) {
            for file_path in files {
                if let Ok(raw) = nq::read_message(&file_path)
                    && let Ok(msg) = serde_json::from_slice::<OutboundMessage>(&raw)
                {
                    last_stream_text = None;
                    let _ = resp_tx.send(msg.text);
                }
                let _ = nq::delete(&file_path);
            }
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn read_stream_update(stream_dir: &Path, last_text: &mut Option<String>) -> Option<StreamUpdate> {
    let entries = std::fs::read_dir(stream_dir).ok()?;

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }

        let data = std::fs::read(entry.path()).ok()?;
        let msg: OutboundMessage = serde_json::from_slice(&data).ok()?;

        let is_final = msg
            .meta
            .get("final")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if is_final {
            *last_text = None;
            let _ = std::fs::remove_file(entry.path());
            let state_file = stream_dir.join(format!(".stream-{name}"));
            let _ = std::fs::remove_file(&state_file);
            return Some(StreamUpdate::Final(msg.text));
        }

        if last_text.as_deref() != Some(&msg.text) {
            *last_text = Some(msg.text.clone());
            return Some(StreamUpdate::Partial(msg.text));
        }
    }

    None
}
