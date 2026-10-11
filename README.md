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

The **MCP server** is stateless — it spawns on demand and connects to a **background daemon** over a unix socket. The daemon attaches to tmux in control mode, maintaining a live model of every pane. It auto-spawns on first use and exits after 5 minutes when no clients are connected.

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
| `search` | Regex filter, applied first: `head`, `tail` and `next` then count matching lines |

For active commands, `next` blocks up to a timeout waiting for new output — no polling. This keeps token usage minimal even for commands with massive output.

### CEL policy engine

This isn't a sandbox against adversarial agents — it's guardrails for good-faith ones that occasionally reach for `rm -rf` or `curl | sh` without thinking twice. Every command line is parsed into its commands (via [brush](https://github.com/reubeno/brush)); each command's grammar parses its words into named parts and marks the paths it reads and writes; CEL rules then decide. The evaluator uses **three-valued logic** (true / false / unknown) because some properties — like what a variable expands to — can't be known statically. When a rule cannot be ruled out, it asks or denies rather than allows.

Wrappers (`sudo`, `ssh`, `timeout`, `xargs`, `sh -c`, `find -exec`) are judged by the command they run, and a line takes the strictest decision of its commands. A command whose name can't be known (`$CMD args`, `bash -c "$X"`) is denied as dynamic dispatch.

Built-in **lints** are deny rules for habits that waste output or hide failures (`ls | tail -5`, `make 2>&1`, `echo $?`, a trailing `&`). Their message tells Claude the fix, such as `command_run`'s `tail` parameter, so Claude corrects the command without asking you.

Grammars and rules come from three files (built-in, user, project) and reload on the next command after a change. Every matching rule counts and the strictest wins; a rule loosens another only by naming its id in `exempts`:

```toml
[commands.deploy]
parser = "gnu"
arguments = ["targets...", "--force|-f", "--output|-o:"]
marks.write = ["output"]

[[rules]]
id = "deploy-staging"
command = "deploy"
when = 'args.targets == ["staging"] && !args.force'
action = "allow"
```

Use `/tmux-policy` to write grammars, rules and exemptions.

### Context-aware approval drift detection

When the policy engine returns "ask" and a human approves, that approval is bound to a snapshot of the current context: the command text, hostname, cwd, user, and foreground process. If any of these change before execution (e.g., an `ssh` command changed the hostname, or `cd` moved the working directory), the approval is invalidated and must be re-requested. Approvals expire after 30 minutes.

### Dual-mode security

- **With hook** (human-in-the-loop): Safe commands auto-approve silently, risky ones trigger Claude Code's native approval prompt, and denied ones are blocked with a reason Claude reads, without asking you. The policy hook runs as a Claude Code `PreToolUse` hook.
- **Without hook** (autonomous): The daemon enforces policy directly — safe commands execute, everything else is rejected with the rule's reason. No human needed.

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
  - Safe commands on files in the project (ls, cat, grep, git status, cargo test, etc.) auto-approve silently
  - Risky commands, and anything touching paths outside the project, trigger Claude Code's native approval prompt
  - Denied commands are blocked with a reason telling Claude what to do instead
  - Approvals are context-aware — if the pane's hostname, cwd, user, or foreground process changes between approval and execution, the approval is invalidated
- **`press_key`** is intentionally omitted. It's a potential bypass for command_run policy (type a command, press Enter). Claude Code prompts for each use.

### 4. Install the policy skill (optional)

Copy the skill to your Claude Code skills directory:

```sh
mkdir -p ~/.claude/skills/tmux-policy
cp skills/tmux-policy.md ~/.claude/skills/tmux-policy/SKILL.md
```

This adds the `/tmux-policy` command, which writes a rule, a command grammar, or an exemption for you. It is also the reference for the policy format. Examples:

```
/tmux-policy                         # fix whatever the last blocked command ran into
/tmux-policy allow cargo install
/tmux-policy ask rm                  # require approval for rm
```

Rules are saved to `~/.claude/tmux-mcp.toml` (user-wide) or `.claude/tmux-mcp.toml` (project). Changes take effect on the next command.

### 5. Steer Bash toward tmux (optional)

Claude Code's own Bash tool runs where you can't see it. A `PreToolUse` hook on `Bash` can turn away the commands you'd rather watch in a pane. Exit status 2 blocks the call and hands stderr to Claude. Save this as `~/.claude/hooks/prefer-tmux.sh` and make it executable:

```sh
#!/usr/bin/env bash
cmd=$(jq -r '.tool_input.command')
if grep -qE '^(cargo|make|npm|podman|kubectl)( |$)' <<<"$cmd"; then
  echo "Run this in tmux with mcp__tmux-mcp__command_run so the user can watch it." >&2
  exit 2
fi
```

Then add it beside the policy hook in `~/.claude/settings.json`:

```json
{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "~/.claude/hooks/prefer-tmux.sh" }] }
```

The match is on the line's first word only, so `cd x && cargo build` passes through.

### Without the hook (autonomous mode)

If no hook is configured but `command_run` is in the allow list, the daemon enforces policy directly:
- Safe commands execute immediately
- Everything else is rejected with the rule's reason, so Claude can adjust the command

This is useful for autonomous sessions where no human is in the loop.

## Policy Configuration

Policy files hold command grammars and CEL rules. Rules from all three sources count together; a later source's grammar for a command replaces the earlier one. A file that does not load denies every command until it is fixed:

| Source | Path | Purpose |
|--------|------|---------|
| Built-in | (compiled into binary) | Built-in grammars, rules and lints |
| User | `~/.claude/tmux-mcp.toml` | Personal rules across all projects |
| Project | `.claude/tmux-mcp.toml` | Project-specific rules |

A reason starts with the id of the rule that decided. `(could not be ruled out)` means the rule fired on something unknown, usually an option the command's grammar doesn't declare. To loosen a built-in rule in one project, exempt its id:

```toml
[[rules]]
id = "push-with-lease"
command = "git push"
when = 'args.force_with_lease && !args.force'
action = "allow"
exempts = ["git-push-force"]
```

Use `/tmux-policy` to write rules, or edit the files directly.
