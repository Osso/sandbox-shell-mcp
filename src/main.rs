use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use portable_pty::{CommandBuilder, PtyPair, PtySize, native_pty_system};
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
#[cfg(not(test))]
use rmcp::{ServiceExt, transport::stdio};
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
                Ok(n) => append_output_chunk(&buffer, &buf[..n]),
                Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
}

fn append_output_chunk(buffer: &OutputBuffer, chunk: &[u8]) {
    let mut b = match buffer.lock() {
        Ok(b) => b,
        Err(p) => p.into_inner(),
    };
    b.extend_from_slice(chunk);
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

fn pty_size(params: &PtyStartParams) -> PtySize {
    PtySize {
        cols: params.cols.unwrap_or(120),
        rows: params.rows.unwrap_or(40),
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn open_pty_pair(params: &PtyStartParams) -> Result<PtyPair, String> {
    native_pty_system()
        .openpty(pty_size(params))
        .map_err(|e| format!("failed to open PTY: {e}"))
}

fn spawn_pty_session(params: &PtyStartParams) -> Result<(String, PtySession), String> {
    let pair = open_pty_pair(params)?;
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

#[cfg(not(test))]
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let service = PtyMcp::new();
    let server = service.serve(stdio()).await?;
    server.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use portable_pty::{ChildKiller, ExitStatus};
    use std::fmt;
    use std::io::{self, Cursor};
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Debug, Default)]
    struct TestWriter {
        writes: Arc<StdMutex<Vec<u8>>>,
        fail_writes: bool,
    }

    impl Write for TestWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.fail_writes {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"));
            }
            self.writes.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Clone)]
    struct TestChild {
        killed: Arc<AtomicBool>,
    }

    impl fmt::Debug for TestChild {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("TestChild").finish_non_exhaustive()
        }
    }

    impl ChildKiller for TestChild {
        fn kill(&mut self) -> io::Result<()> {
            self.killed.store(true, Ordering::SeqCst);
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(self.clone())
        }
    }

    impl portable_pty::Child for TestChild {
        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            Ok(Some(ExitStatus::with_exit_code(0)))
        }

        fn wait(&mut self) -> io::Result<ExitStatus> {
            Ok(ExitStatus::with_exit_code(0))
        }

        fn process_id(&self) -> Option<u32> {
            Some(42)
        }
    }

    fn output_buffer(bytes: &[u8]) -> OutputBuffer {
        Arc::new(StdMutex::new(bytes.to_vec()))
    }

    fn test_session(output: OutputBuffer) -> (PtySession, Arc<StdMutex<Vec<u8>>>, Arc<AtomicBool>) {
        let writes = Arc::new(StdMutex::new(Vec::new()));
        let killed = Arc::new(AtomicBool::new(false));
        let writer = TestWriter {
            writes: writes.clone(),
            fail_writes: false,
        };
        let child = TestChild {
            killed: killed.clone(),
        };
        (
            PtySession {
                writer: Box::new(writer),
                output,
                child: Box::new(child),
            },
            writes,
            killed,
        )
    }

    #[test]
    fn drain_buffer_strips_ansi_and_clears_bytes() {
        let buffer = output_buffer(b"\r\x1b[31mred\x1b[0m\nplain\x1b]0;title\x07!");

        let drained = drain_buffer(&buffer);
        let second = drain_buffer(&buffer);

        assert_eq!(drained, "red\nplain!");
        assert_eq!(second, "");
    }

    #[test]
    fn strip_ansi_handles_csi_osc_st_and_incomplete_escape() {
        let text = b"one\x1b[2Ktwo\x1b]0;ignored\x1b\\three\x1b";

        assert_eq!(strip_ansi(text), "onetwothree");
    }

    #[test]
    fn reader_thread_collects_until_eof() {
        let buffer = output_buffer(b"");
        let reader = Cursor::new(b"hello\r\n".to_vec());

        spawn_reader_thread(Box::new(reader), buffer.clone());
        std::thread::sleep(Duration::from_millis(50));

        assert_eq!(drain_buffer(&buffer), "hello\n");
    }

    #[tokio::test]
    async fn pty_read_reports_missing_empty_and_buffered_sessions() {
        let service = PtyMcp::new();
        let missing = service
            .pty_read(Parameters(PtyReadParams {
                session_id: "missing".to_string(),
            }))
            .await;
        assert_eq!(missing, "Error: session 'missing' not found");

        let (session, _, _) = test_session(output_buffer(b""));
        service
            .sessions
            .lock()
            .await
            .insert("empty".to_string(), session);
        let empty = service
            .pty_read(Parameters(PtyReadParams {
                session_id: "empty".to_string(),
            }))
            .await;
        assert_eq!(empty, "(no new output)");

        let (session, _, _) = test_session(output_buffer(b"\x1b[32mok\x1b[0m"));
        service
            .sessions
            .lock()
            .await
            .insert("buffered".to_string(), session);
        let output = service
            .pty_read(Parameters(PtyReadParams {
                session_id: "buffered".to_string(),
            }))
            .await;
        assert_eq!(output, "ok");
    }

    #[tokio::test]
    async fn pty_write_reports_missing_writes_input_and_drains_output() {
        let service = PtyMcp::new();
        let missing = service
            .pty_write(Parameters(PtyWriteParams {
                session_id: "missing".to_string(),
                input: "ignored".to_string(),
            }))
            .await;
        assert_eq!(missing, "Error: session 'missing' not found");

        let (session, writes, _) = test_session(output_buffer(b"after write"));
        service
            .sessions
            .lock()
            .await
            .insert("session".to_string(), session);

        let output = service
            .pty_write(Parameters(PtyWriteParams {
                session_id: "session".to_string(),
                input: "echo hi\n".to_string(),
            }))
            .await;

        assert_eq!(output, "after write");
        assert_eq!(&*writes.lock().unwrap(), b"echo hi\n");
    }

    #[tokio::test]
    async fn pty_write_reports_no_immediate_output_and_write_errors() {
        let service = PtyMcp::new();
        let writes = Arc::new(StdMutex::new(Vec::new()));
        let session = PtySession {
            writer: Box::new(TestWriter {
                writes,
                fail_writes: false,
            }),
            output: output_buffer(b""),
            child: Box::new(TestChild {
                killed: Arc::new(AtomicBool::new(false)),
            }),
        };
        service
            .sessions
            .lock()
            .await
            .insert("quiet".to_string(), session);

        let quiet = service
            .pty_write(Parameters(PtyWriteParams {
                session_id: "quiet".to_string(),
                input: "pwd\n".to_string(),
            }))
            .await;
        assert_eq!(
            quiet,
            "Written (no output yet — use pty_read to check later)"
        );

        let session = PtySession {
            writer: Box::new(TestWriter {
                writes: Arc::new(StdMutex::new(Vec::new())),
                fail_writes: true,
            }),
            output: output_buffer(b""),
            child: Box::new(TestChild {
                killed: Arc::new(AtomicBool::new(false)),
            }),
        };
        service
            .sessions
            .lock()
            .await
            .insert("broken".to_string(), session);

        let broken = service
            .pty_write(Parameters(PtyWriteParams {
                session_id: "broken".to_string(),
                input: "pwd\n".to_string(),
            }))
            .await;
        assert_eq!(broken, "Error: write failed: closed");
    }

    #[tokio::test]
    async fn pty_close_removes_session_and_kills_child() {
        let service = PtyMcp::new();
        let missing = service
            .pty_close(Parameters(PtyCloseParams {
                session_id: "missing".to_string(),
            }))
            .await;
        assert_eq!(missing, "Error: session 'missing' not found");

        let (session, _, killed) = test_session(output_buffer(b""));
        service
            .sessions
            .lock()
            .await
            .insert("live".to_string(), session);

        let closed = service
            .pty_close(Parameters(PtyCloseParams {
                session_id: "live".to_string(),
            }))
            .await;

        assert_eq!(closed, "Session 'live' closed");
        assert!(killed.load(Ordering::SeqCst));
        assert!(!service.sessions.lock().await.contains_key("live"));
    }

    #[tokio::test]
    async fn pty_start_reports_spawn_errors_and_initial_output() {
        let service = PtyMcp::new();
        let error = service
            .pty_start(Parameters(PtyStartParams {
                command: "/definitely/not/a/command".to_string(),
                args: None,
                cwd: None,
                cols: None,
                rows: None,
            }))
            .await;
        assert!(error.starts_with("Error: failed to spawn:"));

        let started = service
            .pty_start(Parameters(PtyStartParams {
                command: "printf".to_string(),
                args: Some(vec!["ready".to_string()]),
                cwd: None,
                cols: Some(80),
                rows: Some(24),
            }))
            .await;

        assert!(started.starts_with("Session started: pty-"));
        assert!(started.ends_with("\n\nready"));

        let session_id = started
            .lines()
            .next()
            .unwrap()
            .replace("Session started: ", "");
        let closed = service
            .pty_close(Parameters(PtyCloseParams { session_id }))
            .await;
        assert!(closed.ends_with("closed"));
    }
}
