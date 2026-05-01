# tmux-mcp

An MCP server that gives AI agents policy-controlled access to tmux sessions — with structured command tracking, output pagination, and a CEL-based security engine.

A persistent daemon sits between the AI and your terminals, providing:

- **Structured command lifecycle tracking** instead of scraping terminal text
- **Policy-controlled execution** with hot-reloadable CEL rules
- **Context-aware security** that invalidates approvals when the environment changes
- **Efficient output handling** via windowing and regex search

Targets **tmux + bash** specifically. Other shells or terminal multiplexers probably won't work.

## Architecture

```
┌──────────────┐    stdio     ┌──────────────┐  unix socket  ┌───────────────────┐
│  Claude Code │◄────────────►│  MCP Server  │◄─────────────►│      Daemon       │
│  (or any     │   JSON-RPC   │  (stateless) │     RPC       │   (persistent)    │
│   MCP host)  │              └──────────────┘               │                   │
└──────────────┘                                             │ ┌───────────────┐ │
                                                             │ │ Policy Engine │ │
                                                             │ └───────────────┘ │
                   ┌───────────────────────┐  control mode   │ ┌───────────────┐ │
                   │         tmux          │◄───────────────►│ │ Pane Registry │ │
                   │  ┌───────┬───────┐    │  %output events │ └───────────────┘ │
                   │  │ pane  │ pane  │    │                 │ ┌───────────────┐ │
                   │  │       │       │    │                 │ │ VTE Emulator  │ │
                   │  └───────┴───────┘    │                 │ └───────────────┘ │
                   └───────────────────────┘                 └───────────────────┘
```

The **MCP server** is stateless — it spawns on demand and connects to a **background daemon** over a unix socket. The daemon attaches to tmux in control mode, maintaining a live model of every pane. It auto-spawns on first use and exits after 5 minutes of inactivity.

## Key Features

### Headless terminal emulation

Each pane gets its own VTE emulator (Alacritty's engine) that processes the raw byte stream from tmux control mode. This gives accurate screen state, proper wide-character handling, and the ability to read command text directly from the terminal grid — no regex scraping.

### OSC 133 + OSC 7 shell integration

The daemon tracks the full command lifecycle through [OSC 133 semantic prompts](https://gitlab.freedesktop.org/Per_Bothner/specifications/blob/master/proposals/semantic-prompts.md):

```
A (prompt shown) → B (input submitted) → C (execution started) → D (command finished)
```

Each transition is tracked as a state machine with graceful recovery — if markers arrive out of order or a subshell swallows them, the daemon recovers without corrupting state. For SSH sessions where the remote shell lacks integration, `inject_osc133` installs markers into a running bash session on the fly.

[OSC 7](https://codeberg.org/dnkl/foot/wiki/OSC7) sequences are used to track the current working directory, hostname, and user for each pane. The policy engine uses this context to scope rules — e.g., allowing `cargo` only under certain paths, or blocking commands on production hosts.

### Output windowing

Command output is captured as a structured log, not a terminal dump. Reading supports four modes that compose with regex search:

| Mode | Behavior |
|------|----------|
| `head` | First N lines |
| `tail` | Last N lines |
| `next` | Stream N lines from cursor (stateful pagination) |
| `search` | Regex filter, combinable with any of the above |

For active commands, `next` blocks up to a timeout waiting for new output — no polling. This keeps token usage minimal even for commands with massive output.

### CEL policy engine

This isn't a sandbox against adversarial agents — it's guardrails for good-faith ones that occasionally reach for `rm -rf` or `curl | sh` without thinking twice. Every command is parsed into a structured tree (via [brush](https://github.com/reubeno/brush)), then evaluated against CEL rules. The evaluator uses **three-valued logic** (true / false / unknown) because some properties — like what a variable expands to — can't be known statically. When any command in a pipeline is unknown, the engine falls through to human approval rather than guessing.

Wrapper commands (sudo, ssh, docker exec, etc.) are defined declaratively in TOML alongside rules. The engine parses their flags using POSIX or GNU getopt, extracts inner commands, and evaluates those instead. Transparent wrappers are skipped — their security effects are captured on inner commands via `effective_user` and `effective_host`. `allow` rules implicitly require same-user/same-host unless the rule explicitly references these fields.

Rules and wrappers are loaded from three tiers (built-in < user < project) and hot-reload on file change:

```toml
[[rules]]
description = "block production hosts"
when = 'pane.hostname != null && glob("prod-*", pane.hostname)'
action = "deny"
message = "production hosts are read-only"

# Custom wrapper: extract inner command from my-sudo
[[wrappers]]
name = "my-sudo"
when = 'command.name == "my-sudo"'
getopt = "u:"                                          # POSIX optstring: -u takes a value
inner = 'command.getopt.operands'                      # inner = everything after flags
capture_user = 'or(command.getopt.value("u"), "root")' # -u value or default root
```

When the engine encounters `my-sudo -u admin rm -rf /`, it parses the flags, extracts `rm -rf /` as the inner command with `effective_user = "admin"`, and evaluates `rm` against rules. Commands with non-exhaustive args (xargs, find -exec) use three-valued logic — args-dependent allow rules conservatively fall through to Ask when args are uncertain.

Available context: `command.name`, `command.args`, `command.getopt.*`, `command.effective_user`, `command.effective_host`, `command.parent`, `pane.cwd`, `pane.hostname`, `pane.user`, plus helpers like `path()`, `glob()`, `startsWith()`, `has_short_flag()`, `slice()`, `take_until()`, `split_at()`. Use `/tmux-policy` for interactive rule and wrapper generation.

### Context-aware approval drift detection

When the policy engine returns "ask" and a human approves, that approval is bound to a snapshot of the current context: the command text, hostname, cwd, user, and foreground process. If any of these change before execution (e.g., an `ssh` command changed the hostname, or `cd` moved the working directory), the approval is invalidated and must be re-requested. Approvals also expire after 30 seconds.

### Dual-mode security

- **With hook** (human-in-the-loop): Safe commands auto-approve silently. Everything else triggers Claude Code's native approval prompt. The policy hook runs as a Claude Code `PreToolUse` hook.
- **Without hook** (autonomous): The daemon enforces policy directly — safe commands execute, everything else is rejected with an actionable hint. No human needed.

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

### 4. Install the policy skill (optional)

Copy the skill to your Claude Code skills directory:

```sh
mkdir -p ~/.claude/skills/tmux-policy
cp skills/tmux-policy.md ~/.claude/skills/tmux-policy/SKILL.md
```

This adds the `/tmux-policy` command. When a command gets blocked, type `/tmux-policy allow` to generate and save a CEL rule for it. Examples:

```
/tmux-policy allow cargo install
/tmux-policy allow                   # allow the last blocked command
/tmux-policy ask rm                  # require approval for rm
```

Rules are saved to `~/.claude/tmux-mcp.toml` (user-wide) or `.claude/tmux-mcp.toml` (project). Changes take effect immediately via file watcher.

### Without the hook (autonomous mode)

If no hook is configured but `command_run` is in the allow list, the daemon enforces policy directly:
- Safe commands execute immediately
- Everything else is rejected with an actionable hint telling Claude which commands are available

This is useful for autonomous sessions where no human is in the loop.

## Policy Configuration

Policy rules are CEL expressions in TOML files. Three sources are merged (built-in < user < project):

| Source | Path | Purpose |
|--------|------|---------|
| Built-in | (compiled into binary) | Default safe/dangerous command lists |
| User | `~/.claude/tmux-mcp.toml` | Personal rules across all projects |
| Project | `.claude/tmux-mcp.toml` | Project-specific rules |

Use `/tmux-policy` to generate rules interactively, or edit the files directly.
