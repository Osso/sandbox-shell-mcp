use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::stdio,
};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::Mutex;

type SessionMap = Arc<Mutex<HashMap<String, PtySession>>>;
type OutputBuffer = Arc<StdMutex<Vec<u8>>>;

const ESC: char = '\x1b';
const BEL: char = '\x07';
const RANDOM_ID_MASK: u128 = 0xFFFF_FFFF;

struct PtySession {
    writer: Box<dyn Write + Send>,
    output: OutputBuffer,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

#[derive(Clone)]
struct PtyMcp {
    sessions: SessionMap,
    tool_router: ToolRouter<Self>,
}

impl PtyMcp {
    fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            tool_router: Self::tool_router(),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PtyStartParams {
    #[schemars(description = "Command to run (e.g. 'claude-sandbox', 'bash')")]
    command: String,
    #[schemars(description = "Arguments to pass to the command")]
    args: Option<Vec<String>>,
    #[schemars(description = "Working directory")]
    cwd: Option<String>,
    #[schemars(description = "Terminal columns (default 120)")]
    cols: Option<u16>,
    #[schemars(description = "Terminal rows (default 40)")]
    rows: Option<u16>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PtyWriteParams {
    #[schemars(description = "Session ID from pty_start")]
    session_id: String,
    #[schemars(description = "Text to write to the PTY (newline NOT auto-appended)")]
    input: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PtyReadParams {
    #[schemars(description = "Session ID from pty_start")]
    session_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PtyCloseParams {
    #[schemars(description = "Session ID from pty_start")]
    session_id: String,
}

fn drain_buffer(buffer: &OutputBuffer) -> String {
    let bytes = {
        let mut b = match buffer.lock() {
            Ok(b) => b,
            Err(p) => p.into_inner(),
        };
        std::mem::take(&mut *b)
    };
    strip_ansi(&bytes)
}

fn spawn_reader_thread(mut reader: Box<dyn Read + Send>, buffer: OutputBuffer) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 16_384];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = &buf[..n];
                    let mut b = match buffer.lock() {
                        Ok(b) => b,
                        Err(p) => p.into_inner(),
                    };
                    b.extend_from_slice(chunk);
                }
                Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
}

fn strip_ansi(input: &[u8]) -> String {
    let text = String::from_utf8_lossy(input);
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ESC {
            consume_escape_sequence(&mut chars);
        } else if c != '\r' {
            result.push(c);
        }
    }
    result
}

fn consume_escape_sequence(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    let Some(prefix) = chars.next() else {
        return;
    };

    if prefix == '[' {
        consume_csi_sequence(chars);
        return;
    }
    if prefix == ']' {
        consume_osc_sequence(chars);
    }
}

fn consume_csi_sequence(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for next in chars.by_ref() {
        if next.is_ascii_alphabetic() || next == '~' {
            return;
        }
    }
}

/// OSC sequences end with BEL or ST (ESC + backslash).
fn consume_osc_sequence(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    let mut prev_was_esc = false;
    for c in chars.by_ref() {
        let is_terminator = c == BEL || (prev_was_esc && c == '\\');
        if is_terminator {
            return;
        }
        prev_was_esc = c == ESC;
    }
}

fn build_command(params: &PtyStartParams) -> CommandBuilder {
    let mut cmd = CommandBuilder::new(&params.command);
    if let Some(args) = &params.args {
        for arg in args {
            cmd.arg(arg);
        }
    }
    if let Some(cwd) = &params.cwd {
        cmd.cwd(cwd);
    }
    for var in ["HOME", "PATH", "USER", "TERM", "LANG", "SHELL"] {
        if let Ok(val) = std::env::var(var) {
            cmd.env(var, val);
        }
    }
    cmd.env("TERM", "xterm-256color");
    cmd
}

fn spawn_pty_session(params: &PtyStartParams) -> Result<(String, PtySession), String> {
    let pty_system = native_pty_system();
    let size = PtySize {
        cols: params.cols.unwrap_or(120),
        rows: params.rows.unwrap_or(40),
        pixel_width: 0,
        pixel_height: 0,
    };
    let pair = pty_system
        .openpty(size)
        .map_err(|e| format!("failed to open PTY: {e}"))?;
    let cmd = build_command(params);
    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("failed to spawn: {e}"))?;
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("reader: {e}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("writer: {e}"))?;
    let session_id = format!("pty-{}", rand_id());
    let output: OutputBuffer = Arc::new(StdMutex::new(Vec::new()));
    spawn_reader_thread(reader, output.clone());
    Ok((
        session_id,
        PtySession {
            writer,
            output,
            child,
        },
    ))
}

fn rand_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{:x}", t & RANDOM_ID_MASK)
}

#[tool_router]
impl PtyMcp {
    #[tool(description = "Start an interactive PTY session with a command. Returns a session ID.")]
    async fn pty_start(&self, Parameters(params): Parameters<PtyStartParams>) -> String {
        let (session_id, session) = match spawn_pty_session(&params) {
            Ok(v) => v,
            Err(e) => return format!("Error: {e}"),
        };
        let output = session.output.clone();
        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), session);
        tokio::time::sleep(Duration::from_millis(500)).await;

        let initial = drain_buffer(&output);
        if initial.is_empty() {
            format!("Session started: {session_id}")
        } else {
            format!("Session started: {session_id}\n\n{initial}")
        }
    }

    #[tool(
        description = "Write text to a PTY session. Newline is NOT auto-appended — include \\n if you want to press Enter."
    )]
    async fn pty_write(&self, Parameters(params): Parameters<PtyWriteParams>) -> String {
        let buffer = {
            let mut sessions = self.sessions.lock().await;
            let session = match sessions.get_mut(&params.session_id) {
                Some(s) => s,
                None => return format!("Error: session '{}' not found", params.session_id),
            };
            if let Err(e) = session.writer.write_all(params.input.as_bytes()) {
                return format!("Error: write failed: {e}");
            }
            let _ = session.writer.flush();
            session.output.clone()
        };
        tokio::time::sleep(Duration::from_millis(500)).await;

        let output = drain_buffer(&buffer);
        if output.is_empty() {
            "Written (no output yet — use pty_read to check later)".to_string()
        } else {
            output
        }
    }

    #[tool(description = "Read available output from a PTY session.")]
    async fn pty_read(&self, Parameters(params): Parameters<PtyReadParams>) -> String {
        let buffer = {
            let sessions = self.sessions.lock().await;
            match sessions.get(&params.session_id) {
                Some(s) => s.output.clone(),
                None => return format!("Error: session '{}' not found", params.session_id),
            }
        };
        let output = drain_buffer(&buffer);
        if output.is_empty() {
            "(no new output)".to_string()
        } else {
            output
        }
    }

    #[tool(description = "Close a PTY session and kill the process.")]
    async fn pty_close(&self, Parameters(params): Parameters<PtyCloseParams>) -> String {
        let mut sessions = self.sessions.lock().await;
        let mut session = match sessions.remove(&params.session_id) {
            Some(s) => s,
            None => return format!("Error: session '{}' not found", params.session_id),
        };
        let _ = session.child.kill();
        format!("Session '{}' closed", params.session_id)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for PtyMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Interactive PTY session manager. Use pty_start to launch a command, pty_write to send input, pty_read to get output, pty_close to end."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let service = PtyMcp::new();
    let server = service.serve(stdio()).await?;
    server.waiting().await?;
    Ok(())
}
