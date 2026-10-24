//! Interactive chat (writer C: chat.rs).
//!
//! Mirrors Python `chat.py` line for line where possible: state in
//! `chat.json` (`{session_id, title}`, default main chat), banner,
//! slash commands, poll-history sending (`POLL` 1s, first-reply
//! `WAIT` 300s, follow-up `QUIET` 10s), and the loop's error handling
//! (`AuthError` -> "login expired" hint, other errors -> reconnect
//! attempt).
//!
//! Known limitation, no silent equivalent: Python catches
//! `KeyboardInterrupt` (Ctrl+C) and prints `\nBye.`; Rust has no
//! signal-handling crate in `Cargo.toml` (and writer C may not edit
//! it), so Ctrl+C terminates the process with SIGINT instead of
//! printing the goodbye line. EOF and `/exit`/`/quit` still print
//! `\nBye.` and close the gateway.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::commands::{fmt_event, history_events, local_datetime};
use crate::config::{load_config, Config};
use crate::errors::CliError;
use crate::gateway::Gateway;

/// Seconds to wait for the first reply (`chat.py:WAIT`).
pub const WAIT: u64 = 300;
/// Seconds of silence after a reply before taking the next message
/// (`chat.py:QUIET`).
pub const QUIET: u64 = 10;
/// Seconds between history reads while waiting (`chat.py:POLL`).
pub const POLL: u64 = 1;

const HELP: &str = "Commands:\n  /chats          list your conversations\n  /open N         switch to conversation number N from /chats\n  /main           switch to your main chat\n  /new [title]    start a new conversation\n  /history [N]    read the last N messages here (default 10)\n  /login          save your muse.ai login from Chrome again (same as `muse-cli auth export`)\n  /exit           quit (Ctrl+C works too)";

/// `chat.json` state: the conversation the user last used.
#[derive(Debug, Clone)]
pub struct ChatState {
    pub session_id: Option<String>,
    pub title: String,
}

fn state_path(cfg: &Config) -> PathBuf {
    cfg.config_dir.join("chat.json")
}

/// Load state; any read/parse problem yields the default, like
/// Python's `except (OSError, ValueError)` in `load_state`.
pub fn load_state(cfg: &Config) -> ChatState {
    let default = ChatState {
        session_id: None,
        title: "main chat".to_string(),
    };
    let Ok(text) = std::fs::read_to_string(state_path(cfg)) else {
        return default;
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return default;
    };
    let Some(map) = value.as_object() else {
        return default;
    };
    ChatState {
        session_id: map
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        title: map
            .get("title")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "main chat".to_string()),
    }
}

/// Save state (creates the config dir, like Python's `save_state`).
pub fn save_state(cfg: &Config, state: &ChatState) -> Result<(), CliError> {
    std::fs::create_dir_all(&cfg.config_dir)?;
    let text = serde_json::to_string_pretty(&serde_json::json!({
        "session_id": state.session_id,
        "title": state.title,
    }))?;
    std::fs::write(state_path(cfg), text)?;
    Ok(())
}

/// Re-run `auth export`; true exactly when it exits 0, mirroring
/// Python `chat.login` (which catches only `SystemExit`).
fn login() -> Result<bool, CliError> {
    match crate::commands::cmd_auth_export()? {
        0 => Ok(true),
        _ => Ok(false),
    }
}

/// One interactive chat session.
pub struct Chat {
    cfg: Config,
    st: ChatState,
    gw: Gateway,
    listing: Vec<Value>,
}

impl Chat {
    fn new() -> Result<Self, CliError> {
        let cfg = load_config()?;
        let st = load_state(&cfg);
        let gw = Gateway::connect(&cfg)?;
        Ok(Self {
            cfg,
            st,
            gw,
            listing: Vec::new(),
        })
    }

    fn scope(&self) -> Option<&str> {
        self.st.session_id.as_deref()
    }

    fn events(&mut self, limit: i64) -> Result<Vec<Value>, CliError> {
        let scope = self.st.session_id.clone();
        let mut events = history_events(&mut self.gw, scope.as_deref(), limit)?;
        events.sort_by_key(|e| e.get("seq").and_then(|v| v.as_i64()).unwrap_or(0));
        Ok(events)
    }

    fn messages(&mut self, limit: i64) -> Result<Vec<(String, String)>, CliError> {
        let mut out = Vec::new();
        for ev in self.events(limit)? {
            let formatted = fmt_event(&ev);
            let text = formatted
                .get("text")
                .map(crate::commands::json_truthy_string)
                .unwrap_or_default();
            let name = ev.get("event_name").and_then(|v| v.as_str()).unwrap_or("");
            if !text.is_empty() && (name == "message.user" || name == "message.assistant") {
                out.push((
                    if name == "message.user" {
                        "You".to_string()
                    } else {
                        "Muse".to_string()
                    },
                    text,
                ));
            }
        }
        Ok(out)
    }

    fn show_history(&mut self, n: i64) -> Result<(), CliError> {
        let all = self.messages(n.saturating_mul(3))?;
        let msgs: &[(String, String)] = if n == 0 {
            // Python: messages(0)[-0:] == messages(0) (all of it).
            &all
        } else if (all.len() as i64) <= n {
            &all
        } else {
            &all[all.len() - n as usize..]
        };
        if msgs.is_empty() {
            println!("No messages here yet.");
        }
        for (who, text) in msgs {
            println!("\n{who}: {text}");
        }
        Ok(())
    }

    fn last_seq(&mut self) -> Result<i64, CliError> {
        let evs = self.events(1)?;
        Ok(evs
            .last()
            .and_then(|e| e.get("seq"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0))
    }

    fn new_replies(&mut self, after: i64) -> Result<Vec<Value>, CliError> {
        let mut found = Vec::new();
        for ev in self.events(15)? {
            let payload = match ev.get("payload") {
                Some(Value::Object(map)) => Some(map),
                _ => None,
            };
            let has_reply_to = ev
                .get("reply_to_message_id")
                .is_some_and(crate::commands::json_truthy)
                || payload
                    .and_then(|p| p.get("reply_to_message_id"))
                    .is_some_and(crate::commands::json_truthy);
            if has_reply_to {
                continue;
            }
            let status_ok = payload
                .and_then(|p| p.get("status"))
                .is_some_and(crate::commands::json_truthy);
            let text = fmt_event(&ev)
                .get("text")
                .map(crate::commands::json_truthy_string)
                .unwrap_or_default();
            if ev.get("event_name").and_then(|v| v.as_str()) == Some("message.assistant")
                && ev.get("seq").and_then(|v| v.as_i64()).unwrap_or(0) > after
                && status_ok
                && !text.is_empty()
            {
                found.push(ev);
            }
        }
        Ok(found)
    }

    fn send(&mut self, text: &str) -> Result<(), CliError> {
        let mut seen = self.last_seq()?;
        let mut body = serde_json::json!({
            "items": [{"type": "text", "text": text}],
            "node_id": self.cfg.node_id,
            "capabilities": {},
        });
        if let Some(session_id) = self.scope() {
            body.as_object_mut().map(|map| {
                map.insert(
                    "session_id".to_string(),
                    Value::String(session_id.to_string()),
                )
            });
        }
        self.gw.open_stream("chat.stream", &[], Some(&body), None)?;
        println!("Muse is thinking...");
        let mut deadline = Instant::now() + Duration::from_secs(WAIT);
        let mut got_any = false;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_secs(POLL));
            let evs = match self.new_replies(seen) {
                Ok(evs) => evs,
                Err(CliError::Gateway { .. } | CliError::Timeout(_)) => continue,
                Err(e) => return Err(e),
            };
            for ev in &evs {
                let text = fmt_event(ev)
                    .get("text")
                    .map(crate::commands::json_truthy_string)
                    .unwrap_or_default();
                println!("\nMuse: {text}");
                if let Some(seq) = ev.get("seq").and_then(|v| v.as_i64()) {
                    seen = seq;
                }
            }
            if !evs.is_empty() {
                got_any = true;
                deadline = Instant::now() + Duration::from_secs(QUIET);
            }
        }
        if !got_any {
            println!("(No reply within {WAIT} seconds. Type /history later to check.)");
        }
        Ok(())
    }

    fn list_chats(&mut self) -> Result<(), CliError> {
        let d = self
            .gw
            .call_json("sessions.list", &[], None, None, Duration::from_secs(30))?;
        let sessions = d
            .get("sessions")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        self.listing = sessions
            .iter()
            .filter(|s| !s.get("archived").is_some_and(crate::commands::json_truthy))
            .cloned()
            .collect();
        for (i, s) in self.listing.iter().enumerate() {
            let when = local_datetime(
                s.get("updated_at_ms")
                    .and_then(|v| v.as_f64())
                    .map(|ms| (ms / 1000.0) as i64)
                    .unwrap_or(0),
            );
            let main = if s.get("is_thread").is_some_and(crate::commands::json_truthy) {
                ""
            } else {
                " (main chat)"
            };
            let title = s
                .get("title")
                .and_then(|v| v.as_str())
                .filter(|t| !t.is_empty())
                .unwrap_or("untitled");
            println!("{}. {title}{main}, updated {when}", i + 1);
        }
        println!("Type /open and a number to switch.");
        Ok(())
    }

    fn switch(&mut self, session_id: Option<String>, title: &str) -> Result<(), CliError> {
        self.st = ChatState {
            session_id,
            title: title.to_string(),
        };
        save_state(&self.cfg, &self.st)?;
        println!("Now in: {title}");
        self.show_history(3)?;
        Ok(())
    }

    fn open_chat(&mut self, arg: &str) -> Result<(), CliError> {
        if self.listing.is_empty() {
            self.list_chats()?;
            return Ok(());
        }
        let n: i64 = match arg.parse() {
            Ok(n) => n,
            Err(_) => {
                println!("Type /open and a number from /chats.");
                return Ok(());
            }
        };
        if n >= 1 && (n as usize) <= self.listing.len() {
            let s = self.listing[(n - 1) as usize].clone();
            let session_id = s
                .get("session_id")
                .and_then(|v| v.as_str())
                .map(|v| v.to_string());
            let title = s
                .get("title")
                .and_then(|v| v.as_str())
                .filter(|t| !t.is_empty())
                .unwrap_or("untitled")
                .to_string();
            let session_id = match session_id {
                Some(id) => Some(id),
                None => {
                    return Err(CliError::value("session has no session_id".to_string()));
                }
            };
            self.switch(session_id, &title)?;
        } else {
            println!("No conversation with that number. Type /chats to see them.");
        }
        Ok(())
    }

    fn new_chat(&mut self, title: Option<&str>) -> Result<(), CliError> {
        let mut params = serde_json::json!({"origin": "fresh", "lifecycle": "persistent"});
        if let Some(title) = title {
            params
                .as_object_mut()
                .map(|map| map.insert("title".to_string(), Value::String(title.to_string())));
        }
        let d = self.gw.call_json(
            "session.start",
            &[],
            Some(&serde_json::json!({"method": "/api/session/start", "params": params})),
            None,
            Duration::from_secs(30),
        )?;
        let session_id = d
            .get("session_id")
            .and_then(|v| v.as_str())
            .or_else(|| d.get("id").and_then(|v| v.as_str()))
            .map(|s| s.to_string());
        let title = title.unwrap_or("new conversation");
        self.switch(session_id, title)?;
        Ok(())
    }

    fn reconnect(&mut self) -> Result<(), CliError> {
        self.gw.close();
        self.gw = Gateway::connect(&self.cfg)?;
        Ok(())
    }

    fn handle(&mut self, line: &str) -> Result<(), CliError> {
        let (cmd, rest) = match line.split_once(' ') {
            Some((cmd, rest)) => (cmd, rest),
            None => (line, ""),
        };
        let arg = rest.trim();
        match cmd {
            "/help" | "/?" => println!("{HELP}"),
            "/chats" => self.list_chats()?,
            "/open" => self.open_chat(arg)?,
            "/main" => self.switch(None, "main chat")?,
            "/new" => {
                let title = if arg.is_empty() { None } else { Some(arg) };
                self.new_chat(title)?;
            }
            "/history" => {
                let n = if !arg.is_empty() && arg.chars().all(|c| c.is_ascii_digit()) {
                    arg.parse::<i64>().unwrap_or(10)
                } else {
                    10
                };
                self.show_history(n)?;
            }
            "/login" => {
                if login()? {
                    self.reconnect()?;
                    println!("Signed in.");
                }
            }
            _ => println!("Unknown command. Type /help."),
        }
        Ok(())
    }
}

/// Start a chat session, retrying once through `auth export` on an
/// auth error at startup, like Python `chat.start`.
fn start() -> Result<Chat, CliError> {
    match Chat::new() {
        Ok(chat) => Ok(chat),
        Err(CliError::Auth(msg)) => {
            println!("Can't sign in to muse.ai: {msg}");
            if login()? {
                Chat::new()
            } else {
                Err(CliError::Auth(msg))
            }
        }
        Err(e) => Err(e),
    }
}

/// Run the interactive chat (the default with no subcommand).
/// Returns the process exit code (always 0; auth errors at startup
/// propagate as `CliError::Auth` -> exit 2 via `main.rs`).
pub fn run_chat() -> Result<i32, CliError> {
    let mut chat = start()?;
    println!(
        "Muse chat, in: {}. Type /help for commands, /exit to quit.",
        chat.st.title
    );
    chat.show_history(3)?;
    loop {
        print!("\nYou: ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let read = std::io::stdin().read_line(&mut line);
        match read {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => return Err(CliError::from(e)),
        }
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if matches!(
            line.to_lowercase().as_str(),
            "/exit" | "/quit" | "exit" | "quit"
        ) {
            break;
        }
        let result = if line.starts_with('/') {
            chat.handle(&line)
        } else {
            chat.send(&line)
        };
        match result {
            Ok(()) => {}
            Err(CliError::Auth(_)) => {
                println!("Your login expired. Type /login to sign in again.");
            }
            Err(e) => {
                println!("Connection problem ({e}). Reconnected; please try again.");
                match chat.reconnect() {
                    Ok(()) => {}
                    Err(e2) => println!("Still can't connect: {e2}"),
                }
            }
        }
    }
    chat.gw.close();
    println!("\nBye.");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::path::PathBuf;

    fn test_config(dir: &PathBuf) -> Config {
        Config {
            config_dir: dir.clone(),
            cookies_file: dir.join("cookies.txt"),
            vm_id: None,
            node_id: "0123456789abcdef".to_string(),
            auto_approve: None,
            subscription_action_id: None,
        }
    }

    #[test]
    fn state_defaults_when_file_missing_or_broken() {
        let dir =
            std::env::temp_dir().join(format!("muse-cli-rust-chat-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = test_config(&dir);
        let st = load_state(&cfg);
        assert_eq!(st.session_id, None);
        assert_eq!(st.title, "main chat");
        std::fs::write(dir.join("chat.json"), "{broken").unwrap();
        let st = load_state(&cfg);
        assert_eq!(st.title, "main chat");
        std::fs::write(
            dir.join("chat.json"),
            "{\"session_id\": \"s1\", \"title\": \"Side\"}",
        )
        .unwrap();
        let st = load_state(&cfg);
        assert_eq!(st.session_id.as_deref(), Some("s1"));
        assert_eq!(st.title, "Side");
        let round = ChatState {
            session_id: Some("s2".to_string()),
            title: "Other".to_string(),
        };
        save_state(&cfg, &round).unwrap();
        let back = load_state(&cfg);
        assert_eq!(back.session_id.as_deref(), Some("s2"));
        assert_eq!(back.title, "Other");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn help_lists_the_documented_commands() {
        assert!(HELP.contains("/chats"));
        assert!(HELP.contains("/open N"));
        assert!(HELP.contains("/main"));
        assert!(HELP.contains("/new [title]"));
        assert!(HELP.contains("/history [N]"));
        assert!(HELP.contains("/login"));
        assert!(HELP.contains("/exit"));
    }
}
