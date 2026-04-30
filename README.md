# tmux-mcp

MCP server for interacting with tmux panes. Lets Claude run commands, read output, and manage terminal sessions through tmux.

## Setup

### 1. Build

```sh
cargo build --release
```

### 2. Register the MCP server

```sh
claude mcp add --transport stdio --scope user tmux-mcp -- tmux-mcp
```

Or add manually to `~/.claude.json`:

```json
{
  "mcpServers": {
    "tmux-mcp": {
      "type": "stdio",
      "command": "tmux-mcp"
    }
  }
}
```

If `tmux-mcp` isn't on your PATH, use the full path to the binary.

The MCP server auto-spawns a background daemon that connects to your tmux session. The daemon persists across MCP server restarts and exits after 5 minutes of inactivity.

### 3. Configure permissions and policy hook

Add to `~/.claude/settings.json` (merge with existing):

```json
{
  "permissions": {
    "allow": [
      "mcp__tmux-mcp__list_panes",
      "mcp__tmux-mcp__command_read",
      "mcp__tmux-mcp__command_history",
      "mcp__tmux-mcp__debug_pane",
      "mcp__tmux-mcp__inject_osc133",
      "mcp__tmux-mcp__command_run"
    ]
  },
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "mcp__tmux-mcp__command_run",
        "hooks": [
          {
            "type": "command",
            "command": "tmux-mcp policy-check"
          }
        ]
      }
    ]
  }
}
```

**What each piece does:**

- **Read-only tools** (`list_panes`, `command_read`, `command_history`, `debug_pane`, `inject_osc133`) are auto-allowed — they can't modify anything.
- **`command_run`** is auto-allowed so calls reach the daemon, but the **policy hook** gates execution:
  - Safe commands (ls, cat, grep, git status, cargo test, etc.) auto-approve silently
  - Everything else triggers Claude Code's native approval prompt
  - Approvals are context-aware — if the pane's hostname, cwd, user, or foreground process changes between approval and execution, the approval is invalidated
- **`press_key`** is intentionally omitted. It's a potential bypass for command_run policy (type a command, press Enter). Claude Code prompts for each use.

### Without the hook (autonomous mode)

If no hook is configured but `command_run` is in the allow list, the daemon enforces policy directly:
- Safe commands execute immediately
- Everything else is rejected with an actionable hint telling Claude which commands are available

This is useful for autonomous sessions where no human is in the loop.

## Tools

| Tool | Description |
|------|-------------|
| `list_panes` | List all panes with status, cwd, hostname, and running process |
| `command_run` | Run a command and wait for completion. Supports head/tail/next/search output windowing |
| `command_read` | Read or stream output from a running/completed command |
| `command_history` | List recent commands and their exit codes |
| `press_key` | Send control keys (C-c, C-d, Enter, etc.) |
| `debug_pane` | Low-level screen capture for debugging |
| `inject_osc133` | Inject shell integration into bash panes (needed for SSH sessions) |
