# pi-tmux-mcp

Pi integration for the [`tmux-mcp`](../) MCP server: policy-controlled tmux access, `bash` replacement, and a footer status line.

## What it does

- **Inside tmux** (`TMUX` + `TMUX_PANE` set):
  - Gates `command_run` through the daemon's CEL policy engine and approval store, with the same context-drift invalidation as the Claude Code hook.
  - Always confirms `press_key`, since it can type into a pane and bypass `command_run` policy.
  - Removes Pi's built-in `bash` tool once tmux `command_run` is available, so all shell work goes through tmux and the policy engine.
  - At startup, ensures a safe target pane: if Pi's own pane is the only one in the window, it splits a **scratch pane** and arms it with shell integration, so `command_run` has somewhere to run instead of typing into Pi itself.
  - Publishes a status line (`● tmux-mcp …`) that [pi-open-tui](https://github.com/OldSuns/pi-open-tui) renders in the footer.
- **Native `bash`, inside or outside tmux**: evaluates each command through the shared Bash-channel policy using Pi's current working directory. Allows and passthrough continue normally, denials return corrective reasons, and risky commands require confirmation (or are blocked without a UI). This needs no daemon or MCP connection.
- **Outside tmux**: no tool replacement, scratch panes, or tmux footer status; native Bash policy checks still run.

## Target panes

The pane Pi runs in is the **origin** pane. `command_run` never targets it: a shell command sent there would be typed into Pi's own terminal. `list_panes` marks it with `*` in the pane table.

- `ensure_pane` (MCP tool) and `tmux-mcp ensure-target` (CLI) return a safe pane. They pick an existing non-origin pane, or — when Pi's pane is the only one — create a scratch pane and inject shell integration into it.
- The extension calls `ensure-target` once at startup (interactive only) and shows the chosen pane in the status line. Set `TMUX_MCP_NO_AUTO_PANE=1` to disable automatic pane creation.
- `command_run` on the origin pane is refused by the daemon. Set `TMUX_MCP_ALLOW_ORIGIN=1` to override (for headless callers that deliberately set `TMUX_PANE` to the pane they drive).

## Shell integration

`command_run` and `command_history` rely on shell integration (OSC 133) in the target pane. `inject_osc133` supports **bash** and **fish**; for any other shell it returns the detected shell name and a clear message instead of sending a script that would garble the prompt. When the extension creates a scratch pane it injects automatically.

On bash 4.4+ the `C` marker is emitted from `PS0` and covers all command forms. Bash 3.2 (the macOS default) has no `PS0` and falls back to a `DEBUG` trap, which does not run inside `( ... )` subshells — use bash 4.4+ (Homebrew/nix) or fish for full coverage.

## Requirements

- Pi 1.0 or later.
- A running tmux session for pane tools; native Bash policy works outside tmux too.
- The `tmux-mcp` binary on `PATH` (or set `TMUX_MCP_BINARY` to its path).
- `pi-mcp-adapter` (recommended) or Pi's built-in MCP support.

## Install

```sh
# from the repo root
pi install ./pi

# or try it for one session
pi -e ./pi
```

Then merge the server into `~/.pi/agent/mcp.json` (see `mcp.example.json`):

```json
{
  "mcpServers": {
    "tmux-mcp": {
      "command": "tmux-mcp",
      "args": [],
      "description": "Policy-controlled tmux panes with structured command tracking.",
      "exposure": "direct"
    }
  }
}
```

Use Pi's own fields here. `pi-mcp-adapter` translates `exposure: "direct"` into its direct tools; adapter-only keys such as `directTools` or `lifecycle` placed in Pi's `mcp.json` are ignored and produce a startup warning. Without the adapter, Pi's built-in MCP reads `exposure` directly. Run `/reload` after editing `mcp.json`, then `mcp({ connect: "tmux-mcp" })` once if the tool metadata cache is empty.

> `exposure: "direct"` is required: the extension only removes `bash` once `command_run` is a registered tool. Until then it keeps `bash` and warns once.

## How it intercepts calls

| Host | Path |
|------|------|
| `pi-mcp-adapter` | Claims `pi-mcp-adapter:tool-approval-request` on the Pi event bus for `serverName === "tmux-mcp"`. |
| Pi built-in MCP | `tool_call` handler matching `mcp__tmux_mcp__command_run` / `press_key`. |
| Pi native `bash` | Independent `tool_call` handler, before tmux/adapter ownership checks. |

The two are mutually exclusive and selected from the tool registry, so a direct tool is never gated twice.

## Policy decisions

The extension invokes `tmux-mcp policy-check` with the Pi contract:

- `allow` → continue.
- `passthrough` → continue through Pi's remaining permission handlers (native Bash only).
- `deny` → block with the complete rule reasons, including corrective messages. Lints are deny rules, not a separate decision. On the MCP adapter path, a policy denial is allowed to reach the daemon so its precise error reaches the model; the daemon still refuses execution.
- `ask` → confirm the exact command in the Pi UI; fails closed when no UI is available. A native Bash approval permits only the current call and creates no pane approval token.

Native Bash checks send `{"tool_name":"bash","cwd":"…","tool_input":{"command":"…"}}`. Existing bare `tool_input` pane checks remain compatible. Native checks use the same evaluator as Claude's Bash hook and require neither tmux nor a daemon.

Policy files: `~/.pi/agent/tmux-mcp.toml` and `<project root>/.pi/tmux-mcp.toml`, falling back to the `.claude` locations. Project roots are discovered upward from the execution cwd, stopping below `$HOME`. Manage grammars, rules, and exemptions with `/skill:tmux-policy`.

### Native Bash versus pane policy

Rules default to the `tmux` channel; use `via = "bash"` or `via = ["tmux", "bash"]` to cover native Bash. The Bash channel passes unruled commands through, asks only about sharp forms (such as forced pushes, recursive removal, SIGKILL, and recursive permission changes), and redirects builds, SSH, transfers, databases, and infrastructure operations to visible panes. Local-only exceptions such as `helm template` remain available.

Inside tmux, the extension still removes native `bash` once `command_run` is registered. If native Bash remains available (outside tmux, before MCP connects, or when explicitly reactivated), its policy checks still apply.

To permit native builds in one project, explicitly exempt the routing deny:

```toml
[[rules]]
id = "cargo-in-bash"
command = "cargo"
via = "bash"
action = "allow"
exempts = ["prefer-tmux"]
```

Verify changes without executing commands or storing approvals:

```sh
tmux-mcp policy-eval --cwd /path/to/project --via bash --json 'cargo build'
```

A malformed policy fails closed with `policy:load_error`; fix the file before retrying.

## Footer status

Enable `extensionStatuses` in `pi-open-tui` (on by default). The status shows `ready`, the adapter's server state (`connected · N tools`), the target pane (`pane %1`), and the last decision (`ok` / `blocked`). Disable it by setting `footerSegments.extensionStatuses` to `false` in `~/.pi/agent/open-tui.json`.

## Commands and settings

- `/tmux-mcp` — show tmux integration status (enabled, binary, interception path, server state, target pane). Native Bash policy is active even when tmux integration is disabled.
- `TMUX_MCP_BINARY` — override the `tmux-mcp` executable path.
- `TMUX_MCP_NO_AUTO_PANE` — set to `1` to stop the extension creating a scratch pane at startup.
- `TMUX_MCP_ALLOW_ORIGIN` — set to `1` to let `command_run` target the origin pane (daemon-side escape hatch).

## Development checks

From the repo root, build the binary and run the Pi contract and skill-example tests:

```sh
cargo build
cargo test --test integration pi
```

Then check the extension's event handling and types (Node 22.18+ for the test runner; Pi itself loads TypeScript with jiti):

```sh
cd pi
npm ci
npm test
npm run typecheck
```

## Limitations

- For pane tools, Pi must run inside the tmux session it controls; the MCP server requires `TMUX`/`TMUX_PANE`. Native Bash checks have no such requirement.
- Policy gates model-issued native `bash` tool calls, not user-entered `!`/`!!` shell commands. Passthrough does not override other permission extensions.
- Automatic shell integration supports bash and fish. Other shells are reported by name rather than instrumented; run a bash/fish pane and target it.
- `press_key` prompts on every call in interactive sessions and is denied outright in headless sessions.
- If `pi-mcp-adapter` is configured with `toolPrefix: "mcp"`, direct tool names can collide with built-in names; use the default prefix.
