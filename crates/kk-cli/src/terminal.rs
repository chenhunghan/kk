//! Terminal connector: reads stdin, writes to inbound queue, prints responses from outbox/stream.

use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::Result;
use tokio::time::{Duration, sleep};

use kk_core::nq;
use kk_core::paths::DataPaths;
use kk_core::types::{ChannelType, InboundMessage, OutboundMessage};

const CHANNEL_NAME: &str = "terminal";
const GROUP: &str = "terminal";

/// Register the terminal channel in groups.d so the gateway routes messages to it.
pub fn register_terminal_group(paths: &DataPaths) -> Result<()> {
    use kk_core::types::{ChannelMapping, GroupEntry, GroupsConfig, TriggerMode};
    use std::collections::HashMap;

    let groups_d_file = paths.groups_d_dir().join(format!("{CHANNEL_NAME}.json"));

    // Don't overwrite if already exists (user may have customized it)
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

/// Run the terminal connector. This blocks the current thread on stdin.
/// Reads user input, enqueues to inbound, and polls outbox/stream for responses.
pub async fn run(paths: DataPaths) -> Result<()> {
    // Ensure outbox/stream dirs exist
    std::fs::create_dir_all(paths.outbox_dir(CHANNEL_NAME))?;
    std::fs::create_dir_all(paths.stream_dir(CHANNEL_NAME))?;

    // Spawn the outbound poller (prints responses to stdout)
    let poller_paths = paths.clone();
    let outbound_handle = tokio::spawn(async move {
        poll_outbound_loop(&poller_paths).await;
    });

    // Read stdin in a blocking task
    let stdin_paths = paths.clone();
    let stdin_handle = tokio::task::spawn_blocking(move || {
        read_stdin_loop(&stdin_paths);
    });

    tokio::select! {
        _ = outbound_handle => {}
        _ = stdin_handle => {}
    }

    Ok(())
}

/// Blocking loop: reads lines from stdin and enqueues them as inbound messages.
fn read_stdin_loop(paths: &DataPaths) {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    // Print initial prompt
    print_prompt(&mut stdout);

    for line in stdin.lock().lines() {
        let text = match line {
            Ok(t) => t,
            Err(_) => break, // EOF or error
        };

        let text = text.trim().to_string();
        if text.is_empty() {
            print_prompt(&mut stdout);
            continue;
        }

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
            text,
            timestamp: now,
            meta: serde_json::json!({}),
        };

        let payload = serde_json::to_vec(&msg).unwrap();
        if let Err(e) = nq::enqueue(&paths.inbound_dir(), now, &payload) {
            eprintln!("[kk] failed to enqueue message: {e}");
        }
    }
}

/// Async loop: polls outbox and stream dirs, prints responses to stdout.
async fn poll_outbound_loop(paths: &DataPaths) {
    let outbox_dir = paths.outbox_dir(CHANNEL_NAME);
    let stream_dir = paths.stream_dir(CHANNEL_NAME);
    let mut last_stream_text: Option<String> = None;

    loop {
        // Check stream files first (for real-time partial updates)
        if let Some(text) = read_stream_update(&stream_dir, &mut last_stream_text) {
            // Clear line and reprint with the latest streaming text
            print!("\r\x1b[2K{text}");
            std::io::stdout().flush().ok();
        }

        // Check outbox for final messages
        if let Ok(files) = nq::list_pending(&outbox_dir) {
            for file_path in files {
                if let Ok(raw) = nq::read_message(&file_path)
                    && let Ok(msg) = serde_json::from_slice::<OutboundMessage>(&raw)
                {
                    // Clear any streaming line, print final response
                    if last_stream_text.is_some() {
                        print!("\r\x1b[2K");
                        last_stream_text = None;
                    }
                    println!("{}\n", msg.text);
                    print_prompt(&mut std::io::stdout());
                }
                let _ = nq::delete(&file_path);
            }
        }

        sleep(Duration::from_millis(200)).await;
    }
}

/// Read the latest stream file update for the terminal channel.
/// Returns `Some(text)` if there's new text to display, `None` otherwise.
fn read_stream_update(stream_dir: &Path, last_text: &mut Option<String>) -> Option<String> {
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
            // Final message — let the outbox handle it, clean up stream state
            *last_text = None;
            let _ = std::fs::remove_file(entry.path());
            // Also clean up .stream-* state file
            let state_file = stream_dir.join(format!(".stream-{name}"));
            let _ = std::fs::remove_file(&state_file);
            return None;
        }

        // Only display if text changed
        if last_text.as_deref() != Some(&msg.text) {
            *last_text = Some(msg.text.clone());
            return Some(msg.text);
        }
    }

    None
}

fn print_prompt(stdout: &mut std::io::Stdout) {
    print!("> ");
    stdout.flush().ok();
}
