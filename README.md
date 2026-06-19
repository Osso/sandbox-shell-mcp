# sandbox-shell-mcp

An MCP server that exposes interactive PTY sessions as tools. Lets an MCP client (e.g. Claude Code) launch a long-running command — `bash`, `claude-sandbox`, a REPL, an SSH session — and drive it turn by turn instead of one-shot via `bash`.

## Tools

| Tool | Purpose |
|---|---|
| `pty_start` | Spawn a command in a PTY. Returns a `session_id` and any initial output. |
| `pty_write` | Send input to a session. Newline is **not** auto-appended — include `\n` to press Enter. |
| `pty_read`  | Drain pending output from a session. |
| `pty_close` | Kill the process and drop the session. |

Output is ANSI-stripped (CSI + OSC sequences removed, `\r` dropped) so the client sees clean text.

## Build & Install

```bash
./deploy.sh
```

This runs `cargo install --path .`, which puts the binary at `~/.cargo/bin/sandbox-shell-mcp`.

## Register with Claude Code

Once the binary is installed, register it as an MCP server:

```bash
claude mcp add sandbox-shell ~/.cargo/bin/sandbox-shell-mcp
```

Pick a scope with `--scope`:
- `local` (default) — current project only
- `user` — available in every project for your user
- `project` — committed to `.mcp.json` in the repo

Verify:

```bash
claude mcp list
claude mcp get sandbox-shell
```

Restart Claude Code to load the server.

### Manual config

If you'd rather edit config directly, add to `~/.claude.json` under `mcpServers`:

```json
{
  "mcpServers": {
    "sandbox-shell": {
      "command": "/home/youruser/.cargo/bin/sandbox-shell-mcp"
    }
  }
}
```

## Removing

```bash
claude mcp remove sandbox-shell
```
