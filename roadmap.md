# tmux-mcp Roadmap

A daemon that attaches to a tmux session, captures terminal output from every pane, and maintains structured, queryable state. An MCP server (later) queries this daemon to give LLMs terminal access.

Built bottom-up from tmux. Each layer is battle-hardened with heavy unit testing before the next layer is built on top of it.

## Architecture

```
┌─────────────────────────────────────────────────┐
│                  MCP Server                      │  queries daemon state (future)
├─────────────────────────────────────────────────┤
│                  Daemon / RPC                    │  orchestration, Unix socket API
├─────────────────────────────────────────────────┤
│                  Pane State                      │  stream + parser → queryable state
├─────────────────────────────────────────────────┤
│             Parsers (pluggable)                  │  Osc133, Prompt, TUI, Raw
├─────────────────────────────────────────────────┤
│             Raw Stream (per-pane)                │  position-indexed ring buffer
├─────────────────────────────────────────────────┤
│          Tmux Byte Interface                     │  tmux -C, %output, octal unescape
└─────────────────────────────────────────────────┘
                      │
                   tmux(1)
```

## Data Flow

```
tmux control mode (%output %0 \033]133;A\007...)
         │
         ▼
┌──────────────────┐
│  Octal Unescape  │  \033 → 0x1b, \007 → 0x07, \134 → \
└────────┬─────────┘
         │  raw terminal bytes
         ▼
┌──────────────────────────────────────────────┐
│              Raw Stream                       │
│                                               │
│  Fully raw bytes. Position-indexed. Ring      │
│  buffer with safe eviction at command         │
│  boundaries. Strip on read for LLM/human     │
│  consumption. THE ground truth.               │
│                                               │
│  tail ◄──────────────────────────────► head   │
│  (oldest)                           (newest)  │
│                     ▲                         │
│                     │ safe eviction boundary   │
│                     │ (parser sets this)       │
└────────┬─────────────────────────────────────┘
         │  bytes from cursor position
         ▼
┌──────────────────────────────────────────────┐
│           Active Parser (per-pane)            │
│                                               │
│  ┌──────────┐  ┌──────────┐  ┌─────┐  ┌───┐ │
│  │  Osc133  │  │  Prompt  │  │ TUI │  │Raw│ │
│  │          │  │  (regex) │  │     │  │   │ │
│  └──────────┘  └──────────┘  └─────┘  └───┘ │
│                                               │
│  Swappable at runtime. Rewindable.            │
│  Produces PaneEvents.                         │
└────────┬─────────────────────────────────────┘
         │  PaneEvents
         ▼
┌──────────────────────────────────────────────┐
│              Pane State                       │
│                                               │
│  commands: history of completed commands      │
│  active_command: currently executing          │
│  activity: Idle | Busy | Unknown              │
│  cwd / hostname: from OSC 7                   │
│                                               │
│  Queryable uniformly regardless of parser.    │
└──────────────────────────────────────────────┘
```

## Parser Replay

The raw stream is the authoritative record. When parsing needs to change, the daemon rewinds and replays.

```
       raw stream
  ┌──────────────────────────────────────┐
  │ ████████████████████████░░░░░░░░░░░░ │
  │ ▲ finalized              ▲ live     ▲│
  │ │ (evictable)            │ region   ││
  │                     safe boundary   head
  └──────────────────────────────────────┘

  Mode switch (e.g., Raw → Prompt with regex):
  1. Swap parser
  2. Rewind cursor to safe boundary
  3. Replay live region through new parser
  4. Rebuild structured state from replay
  5. Finalized history preserved
```

This enables the LLM to iterate:

```
  LLM sees garbled output
    → reads raw stream (stripped view)
    → identifies prompt pattern
    → calls set_pane_mode(pane, prompt, "Router#\s*$")
    → daemon replays, structured state rebuilt
    → if still wrong, LLM adjusts pattern, replays again
```

## Parser Modes

```
┌──────────────────────────────────────────────────────────┐
│ Osc133                                                    │
│                                                           │
│ For bash/zsh with shell integration markers.              │
│ Tracks: A (prompt) → B (input/LATCH) → C (exec)          │
│         → E (command text) → D (done + exit code)         │
│ B-Latch pattern for reliable async completion.            │
│ Injection: send bash snippet on mode set or user trigger. │
│ Exit codes: yes. Command text: yes (from E marker).       │
├──────────────────────────────────────────────────────────┤
│ Prompt                                                    │
│                                                           │
│ For NOS (Cisco, Junos, VyOS), REPLs, non-OSC shells.     │
│ Configurable regex matches prompt in output stream.       │
│ Text between prompts = command output.                    │
│ LLM tunes the regex. Replay on pattern change.            │
│ Exit codes: no. Command text: heuristic (post-prompt).    │
├──────────────────────────────────────────────────────────┤
│ TUI                                                       │
│                                                           │
│ For vim, htop, etc. Screen-oriented apps.                 │
│ Doesn't accumulate %output (too noisy).                   │
│ Tracks last activity time only.                           │
│ Screen state via capture-pane on demand.                  │
├──────────────────────────────────────────────────────────┤
│ Raw                                                       │
│                                                           │
│ Default / fallback. No command parsing.                   │
│ Raw stream is the only interface.                         │
│ LLM reads stripped stream to figure out what mode to use. │
└──────────────────────────────────────────────────────────┘
```

## Mode Lifecycle

```
  Pane created / monitoring starts
         │
         ▼
       [Raw]  ◄── default for all panes
         │
         ├── auto-detect: shell process? ──► probe for OSC 133
         │                                      │
         │                          markers found?
         │                          yes ──► [Osc133]
         │                          no  ──► inject, then [Osc133]
         │
         ├── MCP calls set_pane_mode("prompt", {pattern}) ──► [Prompt]
         │
         ├── MCP calls set_pane_mode("tui") ──► [TUI]
         │
         └── user presses prefix+I ──► inject OSC 133 ──► [Osc133]

  Any mode can transition to any other mode at any time.
  Command history is preserved across transitions.
  Only the parser state resets (with replay from safe boundary).
```

---

## Build Plan

### Layer 0: Project Skeleton

Cargo.toml, module skeleton, clap CLI, tracing. Green build.

---

### Layer 1: Tmux Byte Interface

**Goal**: Get bytes out of tmux correctly.

| Component | What it does | Test focus |
|-----------|-------------|------------|
| `parse/escape.rs` | Tmux octal unescape + shell_escape | Every octal value, partial escapes, UTF-8, buffer boundaries |
| `tmux/notification.rs` | Parse control mode lines (%output, %begin/%end, etc.) | Every notification type, malformed input, framing edge cases |
| `tmux/reader.rs` | Biased select reader task, response/notification split | Race prevention (register before read) |
| `tmux/connection.rs` | RawTmuxConnection (spawn tmux -C, execute, send_keys) | Integration: attach, send, receive |

**Exit criteria**: Attach to tmux, send commands, receive correctly unescaped %output bytes. Every unescape edge case covered.

---

### Layer 2: Raw Stream

**Goal**: Position-indexed ring buffer that never loses data it shouldn't.

| Component | What it does | Test focus |
|-----------|-------------|------------|
| `stream.rs` | Ring buffer with append, read_from, safe eviction, offset math | Wrap-around, offset math, safe boundaries, capacity limits |
| `stream/strip.rs` | Strip ANSI/control chars on read | SGR removal, CRLF normalize, text preservation |

**Exit criteria**: Fuzz-level coverage of offset math. Safe eviction never drops data the parser hasn't finalized.

---

### Layer 3: Parsers

**Goal**: Rock-solid parsing. This is where the most tests live.

| Component | What it does | Test count target |
|-----------|-------------|-------------------|
| `parse/osc133.rs` | OSC 133 streaming parser, B-Latch | 50+ |
| `parse/prompt.rs` | Configurable prompt regex parser | 20+ |
| `parse/osc7.rs` | OSC 7 cwd/hostname extraction | 10+ |
| `parse/readline.rs` | Readline emulation for typing detection | 10+ |
| `parse/tui.rs` | Activity tracking | 3+ |
| `parse/raw.rs` | No-op cursor advance | 2+ |

**OSC 133 test categories**:
- Basics: normal command, error exit, empty command, no-output command
- Marker sequences: full cycle, out-of-order, duplicates, missing markers
- Chunk splitting: ESC at boundary, marker split across chunks, byte-at-a-time
- Content: unicode, binary, huge output, progress bars, ANSI colors
- Complex: subshells, background jobs, heredocs, multiline, pipelines
- Recovery: garbled markers, truncated sequences, state recovery
- B-Latch: not finalized until B, D-without-B, B-without-D, output accumulation

**Prompt parser test categories**:
- NOS: Cisco IOS, Junos, VyOS prompt patterns
- REPLs: Python, Node, psql
- Shells without OSC 133
- Prompt changes mid-stream
- Replay: rewind with different pattern, verify rebuilt state

**Exit criteria**: Every parser handles every edge case. Parsers are pure functions — fast, no I/O, no async. Full test suite runs in < 1 second.

---

### Layer 4: Pane State

**Goal**: Integrate stream + parser into a uniform queryable interface.

| Component | What it does | Test focus |
|-----------|-------------|------------|
| `state/command.rs` | ActiveCommand (B-Latch), CommandRecord | Lifecycle, is_complete, final_exit_code |
| `state/mod.rs` | PaneState: stream + parser + structured state | Mode switch with replay, event application |
| `state/window.rs` | WindowSubscription (ref-counted) | Ref-count lifecycle |
| `state/session.rs` | SessionState (all panes, all windows) | Pane add/remove |

**Exit criteria**: Mode switching replays correctly. Queryable interface works the same regardless of parser mode.

---

### Layer 5: Daemon

**Goal**: Running service that accepts RPC and manages tmux state.

| Component | What it does |
|-----------|-------------|
| `daemon/control.rs` | Control task loop (3-way biased select) |
| `daemon/inject.rs` | OSC 133 bash injection, probe-then-inject |
| `daemon/rpc.rs` | Unix socket JSON-RPC server, method handlers |
| `daemon/lint.rs` | Command linting |
| `daemon/path.rs` | cwd relative path helpers |
| `daemon/mod.rs` | Daemon lifecycle, lock file, idle timeout, shutdown |

**RPC methods**: list_panes, command_run, command_read, command_history, command_cancel, raw_read, raw_send, raw_start, command_init, set_pane_mode, read_stream

**Exit criteria**: `cargo run -- daemon` works end-to-end. Accepts clients, routes %output through stream → parser → state, serves RPC.

---

### Layer 6: User Injection

- `tmux-mcp inject [pane_id]` CLI subcommand
- Tmux keybinding: `bind I run-shell 'tmux-mcp inject #{pane_id}'`
- MCP can also inject via `set_pane_mode(pane, "osc133")`

**Exit criteria**: User SSHs into a box, presses `prefix + I`, gets structured command tracking.

---

### Layer 7: Integration Tests

Real tmux sessions. Full daemon lifecycle. OSC 133 end-to-end. Mode switching with replay. Prompt mode with NOS output. Daemon restart / stale socket / lock contention.

---

### Layer 8: MCP Server (future)

MCP tool definitions (rmcp). set_pane_mode + read_stream tools for LLM-driven parsing. DaemonClient. Runs as thread within daemon.

---

## Reference: Previous Implementation

Source at `~/src/tmux-mcp/src/`. Port patterns, not code.

| File | Lines | Contains |
|------|-------|----------|
| tmux.rs | 3,090 | OSC 133 parser, control task, reader task, state types |
| daemon.rs | 1,882 | RPC handlers, output formatting |
| protocol.rs | 575 | Wire protocol, JSON-RPC types |
| lint.rs | 232 | Command linting |
| client.rs | 357 | MCP→daemon client |
| main.rs | 740 | MCP tool definitions |
