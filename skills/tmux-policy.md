---
name: tmux-policy
description: Add or modify tmux-mcp policy rules and custom wrappers (allow/ask/deny commands)
user-invocable: true
---

# tmux-mcp Policy Rule & Wrapper Management

Add or modify policy rules or custom wrappers. Both are CEL expressions in TOML config files. Changes reload immediately — no restart needed.

## Philosophy

- **Read-only in-project** → `allow`
- **Read-only out-of-project** → `ask`
- **Write/modify in-project** → `allow`
- **Write/modify out-of-project** → `ask`
- **Destructive/circumvention** → `deny`
- When unsure, be more conservative.

## Config files

- **User-wide**: `~/.claude/tmux-mcp.toml`
- **Project**: `.claude/tmux-mcp.toml` (overrides user at same order)

Both `[[rules]]` and `[[wrappers]]` go in the same file. Top-level config:

```toml
# Directories considered "in-project" for in_project() checks.
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
# order = 0  # negative = before builtins, positive = after
# capture_cwd = 'path(command.args[0])'  # track cwd changes (for cd/pushd)
```

First match wins. Default is `ask`. `allow` rules implicitly require same user/host unless the CEL references `command.effective_user` or `command.effective_host`.

Compound commands (`cd /tmp && rm -rf .`) are evaluated sequentially — cwd changes captured via `capture_cwd` propagate to subsequent commands in the chain.

## Wrappers

Extract inner commands from wrapper commands (sudo, ssh, docker exec, custom tools).

```toml
[[wrappers]]
name = "label"
when = 'CEL match'
getopt = "u:C:v"                   # POSIX optstring (: = takes value)
# getopt_gnu = "n:c:v"            # GNU mode (flags anywhere before --)
# getopt = { short = "s:k:", long = ["signal:", "kill-after:"] }  # with long opts
inner = 'command.getopt.operands'  # CEL → inner command
# capture_user = 'or(command.getopt.value("u"), "root")'
# capture_host = 'rsplit(command.getopt.positional(0), "@", 2)[1]'
# skip_wrapper = true             # false = evaluate both wrapper and inner
# args_complete = true            # false = inner receives unknown additional args
```

**Optstring**: `"isvu:C:"` — `i`,`s`,`v` standalone; `u`,`C` take values. POSIX stops at first operand. GNU processes flags anywhere.

**`inner` return types**: `List[String]` = single command, `List[List[String]]` = multiple commands, `String` = reparse as shell command, `Null` = extraction failed.

**Transparency**: `skip_wrapper = true` (default) skips wrapper in rule evaluation. Set `false` for wrappers with uncaptured side effects (env vars, etc.).

**Non-exhaustive args**: `args_complete = false` or unknown getopt flags → absent values return `Unknown` instead of `Null` → args-dependent allow rules conservatively fall to Ask.

### Getopt result accessors

| Accessor | Description |
|----------|-------------|
| `command.getopt.operands` | Non-flag arguments |
| `command.getopt.operands_from(n)` | Operands from index n |
| `command.getopt.value("u")` | Flag value (accepts `"-u"` or `"u"`) |
| `command.getopt.positional(n)` | Nth operand |

## CEL context

### command.*

| Variable | Type | Description |
|----------|------|-------------|
| `command.name` | string | Command name |
| `command.args` | list[string] | Literal arguments |
| `command.args_complete` | bool | `false` if unknown args possible |
| `command.is_pipe_target` | bool | Receiving piped stdin |
| `command.has_inner` | bool | Wrapper with extracted inner commands |
| `command.effective_user` | string/unknown | User context (pane.user if unchanged) |
| `command.effective_host` | string/unknown | Host context (pane.hostname if unchanged) |
| `command.write_targets` | list[string] | Write redirect targets (`>`, `>>`) |
| `command.read_targets` | list[string] | Read redirect sources (`<`) |
| `command.parent` | object/null | Parent wrapper (walk `.parent.parent` for chains) |
| `command.getopt` | object/null | Parsed getopt result |

### pane.*

| Variable | Type | Description |
|----------|------|-------------|
| `pane.hostname` | string/null | null for local |
| `pane.cwd` | string/null | Working directory |
| `pane.user` | string/null | User |
| `pane.foreground` | string/null | Foreground process |
| `pane.project_dirs` | list[string] | Resolved project directories (from `project_dirs` config) |

## CEL functions

| Function | Description |
|----------|-------------|
| `path(arg)` | Resolve path relative to pane.cwd. Handles `~`, `..`. Null for flags. |
| `in_project(path)` | Check if resolved path falls within any `project_dirs` directory |
| `glob(pattern, str)` | Glob match |
| `contains(str, sub)` | String contains |
| `startsWith(str, pre)` | String prefix |
| `has_short_flag(args, f)` | Short flag check, handles combined (`-rf`) |
| `or(val, fallback)` | Null coalescing |
| `rsplit(str, sep [, n])` | Split string; with n, null-pad left |
| `slice(list, n)` | List from index n |
| `take_until(list, tokens)` | Elements before first token match |
| `split_at(list, markers)` | Split into groups at markers |
| `getopt(args, optstring)` | POSIX getopt (escape hatch for non-command.args) |
| `a + b` | Int addition or list concat |
| `cond ? a : b` | Ternary |
| `list[n]` | Index access |
| `.exists(v, pred)` | Any element matches |
| `.all(v, pred)` | All elements match |
| `.map(v, expr)` | Transform elements |
| `.filter(v, pred)` | Keep matching elements |

## Built-in wrappers

| Wrapper | Mode | Behavior |
|---------|------|----------|
| `command`, `builtin`, `nohup` | — | All args = inner |
| `nice`, `strace`, `watch` | POSIX | Operands = inner |
| `timeout` | POSIX+long | Skip duration, rest = inner |
| `sudo`, `doas` | POSIX | Capture `-u` → effective_user |
| `su`, `sh -c`, `bash -c` | POSIX | `-c` value reparsed |
| `ssh` | POSIX | Capture user@host, skip host |
| `docker`/`podman` exec | GNU | Skip exec + container |
| `kubectl` exec | GNU | Skip exec + pod |
| `find` -exec | list ops | Extract all exec blocks |
| `xargs` | POSIX | args_complete=false |
| `env` | POSIX | skip_wrapper=false |

## Wrapper patterns

**Privilege escalation:**
```toml
[[wrappers]]
name = "my-elevate"
when = 'command.name == "my-elevate"'
getopt = "u:"
inner = 'command.getopt.operands'
capture_user = 'or(command.getopt.value("u"), "root")'
```

**GNU subcommand:**
```toml
[[wrappers]]
name = "mytool run"
when = 'command.name == "mytool" && command.args.exists(a, a == "run")'
getopt_gnu = "e:v"
inner = 'command.getopt.operands_from(1)'
```

**Non-transparent (side effects):**
```toml
[[wrappers]]
name = "my-env"
when = 'command.name == "my-env"'
getopt = "u:"
inner = 'command.getopt.operands'
skip_wrapper = false
```

## Rule patterns

**Path containment pair** (in-project allow + catch-all ask):
```toml
[[rules]]
description = "readers (in-project)"
when = 'command.name in ["cat","head","tail"] && command.args.all(a, startsWith(a, "-") || in_project(path(a)))'
action = "allow"

[[rules]]
description = "readers (out-of-project)"
when = 'command.name in ["cat","head","tail"]'
action = "ask"
```

**Flag detection:**
```cel
command.name == "rm" && has_short_flag(command.args, "r")
command.name == "cargo" && command.args.exists(a, a == "install")
```

**Write redirect protection:**
```toml
[[rules]]
description = "write redirect out-of-project"
when = 'command.write_targets.exists(t, !in_project(path(t)))'
action = "ask"
order = -1
```

**Host/user rules:**
```cel
glob("prod-*", command.effective_host)
command.effective_user == "root"
```

**Privilege override** (reference effective_user to lift implicit same-user constraint):
```cel
command.name == "mkdir" && command.effective_user == "root" && startsWith(path(command.args[0]), pane.cwd)
```

## Instructions

1. **Determine what the user wants**: recently blocked command, specific pattern, or custom wrapper.

2. **If no arguments**: check conversation for the most recent policy rejection.

3. **Rule or wrapper?**
   - **Rule**: allow/ask/deny a command pattern
   - **Wrapper**: command wraps another command, engine should extract and evaluate inner
   - **Both**: non-transparent wrapper may also need a rule

4. **Generate CEL**:
   - Be specific over broad
   - Read-only → in-project allow + out-of-project ask (pair pattern)
   - Use `has_short_flag` for `-rf` style flags
   - Use `path()` for file arguments
   - For wrappers: POSIX `getopt` for C programs, `getopt_gnu` for Go/cobra. Declare all known flags.

5. **Ask scope** (user-wide or project) unless obvious.

6. **Read config**, append rule/wrapper, write file.

7. **Confirm** — takes effect immediately.

## Arguments

`$ARGUMENTS` — Optional: `allow cargo install`, `deny rm -rf`, `add wrapper for my-tool`, or empty for interactive.
