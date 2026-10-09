# Decisions

Settled choices, newest first. Each title is the sentence the ledger and
roadmap Journals cite.

## Every matching policy rule counts, and override rules decide alone

2026-10-08 · active

Within one command, every rule whose `when` matches counts, and the
strictest action wins: deny, then ask, then allow; no match asks. A rule
with `override = true` is an *Override rule*: when any matches, the
strictest matching override decides and the other rules do not count.
Rule order decides nothing; `order` stays only on wrappers.

**Why:** the user ("remove ordering from rule matching today, and add an
override bucket which, all matching rules go through the strictest
framing, and if at least one matches we skip normal and return that
result"). Commands of a compound line already combined strictest-wins;
first match made a rule's meaning depend on its position, and the
built-in `order = -1` device-redirect allow let `eval true > /dev/null`
through. A disposable host needs one rule that allows everything there.

**Alternatives considered:**
- First match (the old engine): order-dependent across hundreds of rules.
- One override tier that is first-match: blind to orthogonal rules.
- `exempts = [ids]`: names other rules (see the next decision).
- IAM Deny > Allow > Ask: any allow silences an orthogonal ask (`echo hi > /tmp/x`).
- "Ask is the meeting point" ladder, and an `approve` action: replaced by the override group.

## A policy rule never names another rule

2026-10-08 · active

No rule refers to another by id, description or position.

**Why:** the user: "a rule should never have to know about other rules,
we will have 100s of rules for a project across multiple files, we can't
be linking them to each other, that isn't reasonable".

**Alternatives considered:**
- Scoped `exempts = [ids]` (a Gemini council's convergence): links rules.
- Disable or replace a built-in by id: loosens unrelated guards.

## An Unknown policy condition fails closed

2026-10-08 · active

A `when` stays three-valued (Kleene) inside; at the top it resolves by
action. Unknown matches a deny or ask rule with that rule's action, never
an allow rule, never an override. A rule matched on Unknown says
"(could not be ruled out)". A command whose name is Unknown is denied,
because it can be `eval`; deny rules must be written tight. A list is
exhaustive only when everything it comes from is.

**Why:** the user: "If there's a command we're running which is unknown,
it could be `eval`, and that rule should probably fire on it and deny
it". Resolving inside an expression is wrong for one polarity: the same
sub-expression appears plain in an ask rule and negated in an allow rule.

**Alternatives considered:**
- Skip an Unknown rule (the old engine): `git push $F origin status` was Allow.
- Unknown deny caps at Ask: a hole on a deny rule should still deny.
- A `known()` CEL guard on name rules: a concept every deny rule must remember.

## The policy is a guardrail for Claude, not a sandbox

2026-10-08 · active

A policy hole earns a rule when Claude plausibly makes it in ordinary
work; spellings only an adversary writes do not.

**Why:** the user: "we're putting guardrails on claude, not trying to
make a sandbox … Get the 80%".

**Alternatives considered:**
- Closed flag lists per tool: `grep --color=never` would ask.
- Unquoted glob, brace and `~user` words as Unknown: `cat src/*.rs` would ask.
- Pins for `$'eval'`, `/dev/fd/..`, `rg --pre`, `sort --compress-program`: adversarial.

**Revisit when:** the policy must stop an adversary rather than steer Claude.

## press_key is an approved escape hatch, not a security boundary

2026-10-08 · active

The key names the schema lists pass; other text is capped at three
characters (`:wq`, `q`, `y`); control characters are refused; keys go to
tmux quoted, so text cannot add a tmux command.

**Why:** the user: press_key serves TUIs (`:wq` to leave vim, `q` to
leave less, `C-c`, `Enter`); the cap nudges toward `command_run`; every
call already needs the user's approval in Claude Code.

**Alternatives considered:**
- A full allowlist of key names and literal tokens: designed, not worth it for an approved, rarely used tool.
- The old 5-character cap: refused `BSpace` and `Escape`, and did not stop injection.

## A command is finished at D; the busy guard waits for B

2026-10-08 · active

A command finishes at its OSC 133 D marker, or at B when no D arrives
(`running_command()`). The busy guard frees a pane only when the prompt
returns, at B (`active_command()`).

**Why:** the user: the busy guard "shouldn't see it as free till B
triggers; the point is when a command can be sent, and one can't be sent
till prompt is up". B stays the fallback when D never comes (Ctrl-C,
nested shells).

**Alternatives considered:**
- One notion of done at B: finished output read as still running.
- A 100 ms wait for B after D: an added wait in place of a definition.

## Typing detection stays strict; a garbled line earns a Ctrl-C hint

2026-10-08 · active

Any text on the input line refuses `command_run`. When no text sits
between the prompt and the cursor, the refusal adds that the line may be
garbled and `press_key C-c` clears it; the refusal carries the screen.

**Why:** the user: a user who pressed Ctrl-A must still read as typing;
the model can decide from the screen.

**Alternatives considered:**
- Count only text before the cursor as typing: misses Ctrl-A editing.

## The screen a tool reports comes from the daemon's emulator

2026-10-08 · active

`capture_screen`, typing detection and `press_key` read the emulator's
one view of a pane (rows and cursor), in step with the output the daemon
has received. `press_key` returns when that view changes, or after 1 s.

**Why:** the user: what the daemon reports, typing detection above all,
must match what it has received; `tmux capture-pane` can run ahead of it.

**Alternatives considered:**
- `tmux capture-pane`: out of step with processed output.
- A fixed delay after a key: returned the screen from before the key.

## Search filters before the window counts lines

2026-10-08 · active

`search` filters first; `next`, `head` and `tail` then count matching
lines. Search "a" with next=3 shows three matches.

**Why:** the user ruled the comment wrong and the code right.

**Alternatives considered:**
- Search within the first N lines: not what a model asking for N matches wants.

## Model-facing tool errors carry only the daemon's message

2026-10-07 · active

A tool error shows the daemon's message alone; the JSON-RPC code stays
in the transport.

**Why:** the user: the code is transport detail a model cannot use.

**Alternatives considered:**
- The "RPC error <code>:" prefix (the old `Display`).

## Red pins mark product defects only, and tests come before fixes

2026-10-07 · active

A known product defect gets a test that fails, marked
`#[ignore = "<roadmap id>: red — <defect>"]`. A test or harness fault is
fixed, never pinned. Tests are written from the product's or user's
side, as many defects as can be found, before fixes start.

**Why:** the user ordered it: rewrite the tests first, pin red only
where the product is at fault, and fix product defects once the suite is
in a good state.

**Alternatives considered:**
- Fix each defect as found: loses the census of what is broken.

## A roadmap demolishes what it replaces before it builds

2026-10-07 · active

Delete everything a roadmap replaces as early as possible, accepting a
broken or absent suite for a stage or two. Dies holds what the roadmap
replaces, not every dead item a review finds.

**Why:** the user: "Dies before Born"; and, rejecting a dead-code sweep,
"we aren't just removing dead code".

**Alternatives considered:**
- Build the replacement beside the old code, then delete.
- Sweep all dead code into the demolition stage.

## Integration tests drive the shipped binary through a Harness

2026-10-07 · active

Integration tests run the real `tmux-mcp` binary over MCP against a
private tmux server in one temp dir (its own `HOME`, `XDG_RUNTIME_DIR`,
and a `.bashrc` holding `OSC133_INJECT_LINES`), and assert on the text a
model reads. tmux's own `capture-pane` is the ground truth a test waits
on.

**Why:** the user directed adopting fern's harness design: isolated
tmux, the real binary as the subject, `wait_until` failing with the
screen, product-vocabulary fixtures, a harness contract file. The old
suite called the daemon in-process, used the developer's bash and
history, and asserted on JSON.

**Alternatives considered:**
- Keep the in-process `TestDaemon` suite.
- A bare shell plus `inject_osc133` in setup: every test would depend on injection.
- A test-only env override for the daemon socket: punts isolation to callers.

## Daemon files live in their own directory, named by tmux server pid and session

2026-10-07 · active

Socket, lock and log live in `$XDG_RUNTIME_DIR/tmux-mcp` (fallback
`/tmp/tmux-mcp-<uid>`, mode 0700), named
`<server socket name>-<server pid>-<session id>.{sock,lock,log}`.

**Why:** the user: tmux's socket directory is no place for more than a
socket; a hash of the socket path is "dumb" when `$TMUX` already carries
the server pid, unique among live servers.

**Alternatives considered:**
- Beside the tmux server's socket (first chosen, then reversed).
- A hashed key of the socket path.
- `/tmp/tmux-mcp-{session}`: shared by every tmux server, world-writable.
