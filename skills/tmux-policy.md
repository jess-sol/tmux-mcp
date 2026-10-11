---
name: tmux-policy
description: Add or modify tmux-mcp policy rules (allow/ask/deny commands)
user-invocable: true
---

# tmux-mcp Policy Rule Management

Add or modify policy rules. Rules are CEL expressions in TOML config files. Changes reload immediately — no restart needed.

## Philosophy

- **Read-only in-project** → `allow`
- **Read-only out-of-project** → `ask`
- **Write/modify in-project** → `allow`
- **Write/modify out-of-project** → `ask`
- **Destructive/circumvention** → `deny`
- When unsure, be more conservative.

## Config files

- **User-wide**: `~/.claude/tmux-mcp.toml`
- **Project**: `.claude/tmux-mcp.toml`

Top-level config:

```toml
# Directories considered "in-project".
# Relative paths resolved against pane cwd. Default: ["."]
project_dirs = [".", "../shared-lib"]
```

## Rules

```toml
[[rules]]
description = "short name"
when = 'CEL expression'
action = "allow"  # or "ask" or "deny"
# message = "shown when triggered"
# override = true  # an override rule decides alone when it matches
```

Every matching rule counts, and the strictest wins: deny, then ask, then allow. Default is `ask`. An Unknown condition fires ask and deny rules, never allow or override rules.

## CEL context

| Variable | Type | Description |
|----------|------|-------------|
| `command.name` | string | Command name |
| `pane.foreground` | string/null | Foreground process |

## CEL functions

| Function | Description |
|----------|-------------|
| `glob(pattern, str)` | Glob match |
| `contains(str, sub)` | String contains |
| `startsWith(str, pre)` | String prefix |
| `or(val, fallback)` | Null coalescing |
| `a + b` | Int addition or list concat |
| `cond ? a : b` | Ternary |
| `list[n]` | Index access |
| `.exists(v, pred)` | Any element matches |
| `.all(v, pred)` | All elements match |
| `.map(v, expr)` | Transform elements |
| `.filter(v, pred)` | Keep matching elements |

## Instructions

1. **Determine what the user wants**: recently blocked command or a specific pattern.

2. **If no arguments**: check conversation for the most recent policy rejection.

3. **Generate CEL**: be specific over broad.

4. **Ask scope** (user-wide or project) unless obvious.

5. **Read config**, append rule, write file.

6. **Confirm** — takes effect immediately.

## Arguments

`$ARGUMENTS` — Optional: `allow cargo install`, `deny rm -rf`, or empty for interactive.
