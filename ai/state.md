# State

tmux-mcp lets an AI agent work in a person's real tmux panes: it runs
commands, reads their output and screens as the person would see them,
and asks the person before anything risky. Each subsystem serves one of
four aims: report exactly what was received, know when a command starts
and finishes, steer the agent with guardrails rather than sandbox it, and
prove behavior by driving the shipped binary.

## Daemon

One daemon serves one tmux session on one tmux server, identified by the
server socket, the server pid and the session id from `$TMUX`. Its socket,
lock and log live in `$XDG_RUNTIME_DIR/tmux-mcp`, so no two tmux servers
ever share a daemon. It holds one tmux control-mode connection and serves
MCP servers and the policy hook over a Unix socket, one JSON-RPC message
per line.

Each request runs in a task of its own and its reply carries its id, so a
long `command_run` holds up no other call. A request runs to its end even
when its client leaves: cancelling it between registering a tmux command
and writing it would put the control-mode replies out of step. This rules
out per-request cancellation.

## MCP server

One per agent session. It shares one daemon connection; the client routes
each reply to its request by id, so tool calls overlap. A tool error shows
the daemon's message only.

## Pane tracking

Each pane's `%output` bytes feed a processor: a terminal emulator plus
OSC 133/7 interception. Bytes stay bytes until the processor, which keeps
an incomplete UTF-8 tail for the next chunk. What the daemon reports —
the screen, typing detection, `press_key`'s answer — is the emulator's
view, in step with the output received; `tmux capture-pane` is never the
source, because it can run ahead of what was processed.

OSC 133 markers give each command its life: C starts it, D finishes it
with an exit code, B marks the prompt back. "Finished" is D, or B when no
D comes; the busy guard frees a pane only at B, since a command can be
sent only to a prompt. `command_run` holds the pane's run lock from its
busy check until the C marker, so two calls cannot both pass the check.
A pane's foreground process is the terminal's foreground process group
(`tpgid`), not a walk down child processes, so a background job never
masquerades as the foreground.

Typing detection is strict: any text on the input line refuses a
command, because a person editing mid-line must still read as typing.

## Policy

The Claude Code hook (`tmux-mcp policy-check`) and `command_run` both ask
the policy. A deny reaches Claude Code as exit status 2 with the reason on
stderr; a lint refusal as a JSON deny.

A command line parses (brush) into a flat list of commands. Wrappers
(`sudo`, `ssh`, `find -exec`, `sh -c`, …) unwrap inner commands; the first
matching wrapper gives a command its one reading, so wrappers keep
`order`. Commands of a line combine strictest-wins, and so do rules
within a command: deny, then ask, then allow, no match asking. An
*Override rule* group decides alone when any of it matches. Order never
decides a rule's result, and no rule names another, because projects
hold hundreds of rules across several files.

Rule conditions are CEL, three-valued inside; at the top Unknown fails
closed — it fires deny and ask rules, never allow rules or overrides.
Deny rules must be written tight: a command whose name is Unknown is
denied, since it can be `eval`.

The policy is a guardrail for Claude, not a sandbox: it covers what
Claude plausibly does in ordinary work, and leaves adversarial spellings
alone. Today rules read raw words (with quotes kept), wrappers hold the
only real argument grammars, and nine lints run before the rules as
Rust code that no rule can override.

## press_key

An escape hatch for TUIs that a person approves call by call: listed key
names, or text of three characters at most, sent quoted with control
characters refused. It is not a security boundary.

## Tests

Integration tests are one binary, `tests/integration/`. A `Harness` runs
a private tmux server in a temp dir with its own `HOME`, `XDG_RUNTIME_DIR`
and a `.bashrc` holding the product's OSC 133 lines, drives the real
binary over MCP from a pane of its own, and asserts on the text a model
reads; tests wait on tmux's own screen as independent ground truth.
Policy tests are tables of commands and decisions, and can write a
project policy. A known product defect is a red pin,
`#[ignore = "<roadmap id>: red — <defect>"]`; a harness fault is fixed,
never pinned.
