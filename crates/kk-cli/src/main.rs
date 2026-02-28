use std::io::{self, BufRead, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

enum Msg {
    Output(usize, String),
    Done(usize),
    #[allow(dead_code)]
    TailPid(usize, u32),
}

struct Job {
    nq_ids: Vec<String>,
    session_id: String,
    #[allow(dead_code)]
    prompt: String,
    active_tails: usize,
}

struct Config {
    claude_args: Vec<String>,
}

struct App {
    jobs: Vec<Job>,
    input: String,
    cursor: usize,
    selected: Option<usize>,
    nqdir: PathBuf,
    config: Config,
    quit: bool,
    job_list_area: Rect,
    scroll_offset: usize,
    pending_lines: Vec<String>,
    tail_pids: Arc<Mutex<Vec<u32>>>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn gen_uuid() -> String {
    let mut b = [0u8; 16];
    let mut f = std::fs::File::open("/dev/urandom").expect("/dev/urandom");
    f.read_exact(&mut b).expect("urandom read");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15],
    )
}

fn queue_dir(session: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let base = if cfg!(target_os = "macos") {
        PathBuf::from(&home).join("Library/Caches/kk/queue")
    } else {
        let state =
            std::env::var("XDG_STATE_HOME").unwrap_or_else(|_| format!("{}/.local/state", home));
        PathBuf::from(state).join("kk/queue")
    };
    base.join(&session[..8])
}

fn check_dependency(name: &str) {
    let ok = Command::new("which")
        .arg(name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("error: '{}' not found in PATH", name);
        eprintln!(
            "kk requires nq (https://github.com/leahneukirchen/nq) and claude (Claude Code CLI)"
        );
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// CLI argument parsing
// ---------------------------------------------------------------------------

const VALUE_FLAGS: &[&str] = &[
    "--permission-mode",
    "--model",
    "--fallback-model",
    "--max-budget-usd",
    "--allowedTools",
    "--disallowedTools",
    "--system-prompt",
    "--append-system-prompt",
    "--agent",
    "--agents",
    "--mcp-config",
    "--tools",
];

const BOOL_FLAGS: &[&str] = &[
    "--dangerously-skip-permissions",
    "--no-session-persistence",
    "--verbose",
    "--strict-mcp-config",
];

fn parse_args() -> Config {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut claude_args = Vec::new();

    let mut i = 0;
    while i < args.len() {
        if args[i] == "--help" || args[i] == "-h" {
            print_help();
            std::process::exit(0);
        }
        if VALUE_FLAGS.contains(&args[i].as_str()) {
            if i + 1 >= args.len() {
                eprintln!("error: {} requires a value", args[i]);
                std::process::exit(1);
            }
            claude_args.push(args[i].clone());
            claude_args.push(args[i + 1].clone());
            i += 2;
        } else if BOOL_FLAGS.contains(&args[i].as_str()) {
            claude_args.push(args[i].clone());
            i += 1;
        } else {
            eprintln!("error: unknown flag: {}", args[i]);
            std::process::exit(1);
        }
    }

    Config { claude_args }
}

fn print_help() {
    println!("kk — TUI job queue for Claude Code CLI\n");
    println!("Usage: kk [claude flags...]\n");
    println!("All flags are forwarded to `claude -p`. Examples:");
    println!("  kk");
    println!("  kk --model haiku");
    println!("  kk --dangerously-skip-permissions --model sonnet\n");
    println!("Forwarded value flags:");
    println!("  --permission-mode, --model, --fallback-model, --max-budget-usd,");
    println!("  --allowedTools, --disallowedTools, --system-prompt,");
    println!("  --append-system-prompt, --agent, --agents, --mcp-config, --tools\n");
    println!("Forwarded boolean flags:");
    println!("  --dangerously-skip-permissions, --no-session-persistence,");
    println!("  --verbose, --strict-mcp-config");
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

impl App {
    fn new(config: Config) -> Self {
        let session = gen_uuid();
        let nqdir = queue_dir(&session);
        std::fs::create_dir_all(&nqdir).expect("create NQDIR");
        Self {
            jobs: Vec::new(),
            input: String::new(),
            cursor: 0,
            selected: None,
            nqdir,
            config,
            quit: false,
            job_list_area: Rect::default(),
            scroll_offset: 0,
            pending_lines: Vec::new(),
            tail_pids: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn enqueue(&mut self, tx: &mpsc::Sender<Msg>) {
        let raw = self.input.trim().to_string();
        if raw.is_empty() {
            return;
        }
        self.input.clear();
        self.cursor = 0;

        // /new or /new "prompt" forces a new job
        let (force_new, prompt) = if raw == "/new" {
            // bare /new with no prompt — just deselect
            self.selected = None;
            return;
        } else if let Some(rest) = raw.strip_prefix("/new ") {
            (true, rest.trim().to_string())
        } else {
            (false, raw)
        };

        let is_followup = self.selected.is_some() && !force_new;
        let session_id = if is_followup {
            self.jobs[self.selected.unwrap()].session_id.clone()
        } else {
            gen_uuid()
        };

        let mut cmd = Command::new("nq");
        cmd.env("NQDIR", &self.nqdir);
        cmd.arg("claude").arg("-p");

        if is_followup {
            cmd.arg("--resume").arg(&session_id);
        } else {
            cmd.arg("--session-id").arg(&session_id);
        }

        for arg in &self.config.claude_args {
            cmd.arg(arg);
        }

        cmd.arg("--").arg(&prompt);

        let output = match cmd.output() {
            Ok(o) => o,
            Err(_) => return,
        };

        let nq_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if nq_id.is_empty() {
            return;
        }

        let idx = if is_followup {
            let sel = self.selected.unwrap();
            let job = &mut self.jobs[sel];
            job.nq_ids.push(nq_id.clone());
            job.active_tails += 1;
            self.pending_lines.push(format!("> {prompt}"));
            sel
        } else {
            let idx = self.jobs.len();
            self.jobs.push(Job {
                nq_ids: vec![nq_id.clone()],
                session_id,
                prompt: prompt.clone(),
                active_tails: 1,
            });
            self.selected = Some(idx);
            self.ensure_visible(idx);
            self.pending_lines.push(format!("> {prompt}"));
            idx
        };

        let nqdir = self.nqdir.clone();
        let pids = self.tail_pids.clone();
        let tx = tx.clone();
        thread::spawn(move || tail_job(idx, nq_id, nqdir, tx, pids));
    }

    fn handle_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Output(idx, line) => {
                if idx < self.jobs.len() {
                    self.pending_lines.push(line);
                }
            }
            Msg::Done(idx) => {
                if let Some(job) = self.jobs.get_mut(idx) {
                    job.active_tails = job.active_tails.saturating_sub(1);
                }
            }
            Msg::TailPid(_, _) => {}
        }
    }

    fn select_prev(&mut self) {
        match self.selected {
            None if !self.jobs.is_empty() => {
                let last = self.jobs.len() - 1;
                self.selected = Some(last);
                self.ensure_visible(last);
            }
            Some(0) => {}
            Some(i) => {
                self.selected = Some(i - 1);
                self.ensure_visible(i - 1);
            }
            _ => {}
        }
    }

    fn select_next(&mut self) {
        match self.selected {
            None if !self.jobs.is_empty() => {
                self.selected = Some(0);
                self.ensure_visible(0);
            }
            Some(i) if i + 1 < self.jobs.len() => {
                self.selected = Some(i + 1);
                self.ensure_visible(i + 1);
            }
            _ => {}
        }
    }

    fn ensure_visible(&mut self, idx: usize) {
        let h = self.job_list_area.height.saturating_sub(2) as usize;
        if h == 0 {
            return;
        }
        if idx >= self.scroll_offset + h {
            self.scroll_offset = idx - h + 1;
        }
        if idx < self.scroll_offset {
            self.scroll_offset = idx;
        }
    }

    fn kill_all(&self) {
        if let Ok(pids) = self.tail_pids.lock() {
            for pid in pids.iter() {
                let _ = Command::new("kill")
                    .args(["-TERM", &pid.to_string()])
                    .status();
            }
        }
        for job in &self.jobs {
            if job.active_tails > 0 {
                for nq_id in &job.nq_ids {
                    if let Some(pid_str) = nq_id.rsplit('.').next() {
                        if let Ok(pid) = pid_str.parse::<u32>() {
                            let _ = Command::new("kill")
                                .args(["-TERM", &pid.to_string()])
                                .status();
                        }
                    }
                }
            }
        }
    }

    fn running_count(&self) -> usize {
        self.jobs.iter().filter(|j| j.active_tails > 0).count()
    }
}

// ---------------------------------------------------------------------------
// Background streaming
// ---------------------------------------------------------------------------

fn tail_job(
    idx: usize,
    nq_id: String,
    nqdir: PathBuf,
    tx: mpsc::Sender<Msg>,
    pids: Arc<Mutex<Vec<u32>>>,
) {
    let mut child = match Command::new("nqtail")
        .arg(&nq_id)
        .env("NQDIR", &nqdir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => {
            let _ = tx.send(Msg::Done(idx));
            return;
        }
    };

    let pid = child.id();
    if let Ok(mut v) = pids.lock() {
        v.push(pid);
    }
    let _ = tx.send(Msg::TailPid(idx, pid));

    if let Some(stdout) = child.stdout.take() {
        for line in io::BufReader::new(stdout).lines() {
            match line {
                Ok(l) => {
                    if l.starts_with("==> ") || l.starts_with("exec ") {
                        continue;
                    }
                    if tx.send(Msg::Output(idx, l)).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    }

    let _ = child.wait();
    if let Ok(mut v) = pids.lock() {
        v.retain(|&p| p != pid);
    }
    let _ = tx.send(Msg::Done(idx));
}

// ---------------------------------------------------------------------------
// TUI rendering
// ---------------------------------------------------------------------------

fn ui(f: &mut Frame, app: &mut App) {
    // Job list takes remaining space, input bar fixed at 3 lines
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(3)])
        .split(f.area());

    draw_job_list(f, app, chunks[0]);
    draw_input(f, app, chunks[1]);
}

fn draw_job_list(f: &mut Frame, app: &mut App, area: Rect) {
    app.job_list_area = area;

    let running = app.running_count();
    let title = if running > 0 {
        format!(" jobs ({running} running) ")
    } else {
        " jobs ".to_string()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let visible = inner.height as usize;
    let start = app.scroll_offset;
    let end = (start + visible).min(app.jobs.len());

    for (i, job) in app.jobs[start..end].iter().enumerate() {
        let job_idx = start + i;
        let done = job.active_tails == 0;
        let icon = if done { "\u{2713}" } else { "\u{25cf}" };
        let icon_color = if done { Color::DarkGray } else { Color::Green };

        let max_w = (inner.width as usize).saturating_sub(4);
        let prompt_display: String = if job.prompt.chars().count() > max_w && max_w > 1 {
            let s: String = job.prompt.chars().take(max_w - 1).collect();
            format!("{s}\u{2026}")
        } else {
            job.prompt.clone()
        };

        let selected = app.selected == Some(job_idx);
        let text_style = if selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        let line = Line::from(vec![
            Span::raw(" "),
            Span::styled(icon, Style::default().fg(icon_color)),
            Span::raw(" "),
            Span::styled(prompt_display, text_style),
        ]);

        let y = inner.y + i as u16;
        if y < inner.y + inner.height {
            f.render_widget(Paragraph::new(line), Rect::new(inner.x, y, inner.width, 1));
        }
    }
}

fn draw_input(f: &mut Frame, app: &App, area: Rect) {
    let (title, border_color) = match app.selected {
        Some(sel) if sel < app.jobs.len() => {
            let sid = &app.jobs[sel].session_id;
            (format!(" follow-up [{}] > ", &sid[..8]), Color::Yellow)
        }
        _ => (" new > ".to_string(), Color::Magenta),
    };

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(title);
    if app.selected.is_some() {
        block = block.title_bottom(
            Line::from(" /new to start a new job ")
                .style(Style::default().fg(Color::DarkGray))
                .right_aligned(),
        );
    }
    let para = Paragraph::new(app.input.as_str()).block(block);
    f.render_widget(para, area);

    let x = area.x + 1 + app.cursor as u16;
    let y = area.y + 1;
    if x < area.x + area.width - 1 {
        f.set_cursor_position((x, y));
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    for dep in &["nq", "nqtail", "claude"] {
        check_dependency(dep);
    }

    let config = parse_args();
    let mut app = App::new(config);

    enable_raw_mode().expect("raw mode");
    let stdout = io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(8),
        },
    )
    .expect("terminal");

    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        original_hook(info);
    }));

    let (tx, rx) = mpsc::channel::<Msg>();

    loop {
        while let Ok(msg) = rx.try_recv() {
            app.handle_msg(msg);
        }

        // Flush completed lines to terminal scrollback
        if !app.pending_lines.is_empty() {
            let all_lines: Vec<Line> = app.pending_lines.drain(..).map(Line::raw).collect();
            if !all_lines.is_empty() {
                let count = all_lines.len() as u16;
                let _ = terminal.insert_before(count, |buf| {
                    for (i, line) in all_lines.into_iter().enumerate() {
                        if (i as u16) < buf.area.height {
                            buf.set_line(buf.area.x, buf.area.y + i as u16, &line, buf.area.width);
                        }
                    }
                });
            }
        }

        terminal.draw(|f| ui(f, &mut app)).expect("draw");

        if event::poll(Duration::from_millis(30)).unwrap_or(false) {
            if let Ok(Event::Key(key)) = event::read() {
                match (key.code, key.modifiers) {
                    (KeyCode::Char('c'), KeyModifiers::CONTROL)
                    | (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                        app.quit = true;
                    }
                    (KeyCode::Enter, _) => app.enqueue(&tx),
                    (KeyCode::Esc, _) => app.selected = None,
                    (KeyCode::Up, _) => app.select_prev(),
                    (KeyCode::Down, _) => app.select_next(),
                    (KeyCode::Left, _) => {
                        if app.cursor > 0 {
                            app.cursor -= 1;
                        }
                    }
                    (KeyCode::Right, _) => {
                        if app.cursor < app.input.len() {
                            app.cursor += 1;
                        }
                    }
                    (KeyCode::Backspace, _) => {
                        if app.cursor > 0 {
                            app.input.remove(app.cursor - 1);
                            app.cursor -= 1;
                        }
                    }
                    (KeyCode::Char(c), _) => {
                        app.input.insert(app.cursor, c);
                        app.cursor += 1;
                    }
                    _ => {}
                }
            }
        }

        if app.quit {
            break;
        }
    }

    app.kill_all();
    let _ = disable_raw_mode();
    let _ = terminal.show_cursor();
    let _ = std::fs::remove_dir_all(&app.nqdir);
}
