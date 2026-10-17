import assert from "node:assert/strict";
import { chmodSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, test } from "node:test";

// Exercise the actual extension entry point without loading a Pi UI or MCP
// server. The fake binary only replaces the subprocess boundary; Rust tests
// separately exercise the real policy-check contract and evaluator.
const dir = mkdtempSync(join(tmpdir(), "pi-tmux-extension-"));
const binary = join(dir, "policy-check");
const trace = join(dir, "payload.json");
writeFileSync(binary, `#!${process.execPath}
const fs = require('node:fs');
let input = '';
process.stdin.on('data', s => input += s);
process.stdin.on('end', () => {
  fs.writeFileSync(process.env.PI_TMUX_TEST_TRACE, input);
  if (process.env.PI_TMUX_TEST_FAIL === '1') process.exit(2);
  console.log(process.env.PI_TMUX_TEST_RESPONSE);
});
`);
chmodSync(binary, 0o755);
process.env.TMUX_MCP_BINARY = binary;
process.env.PI_TMUX_TEST_TRACE = trace;
const { default: extension } = await import("../extensions/tmux-mcp.ts");
after(() => rmSync(dir, { recursive: true, force: true }));

function setup({ tmux = false, adapter = false, hasUI = true, approved = true, paneTools = true } = {}) {
  if (tmux) {
    process.env.TMUX = "/fake/socket,1,0";
    process.env.TMUX_PANE = "%0";
  } else {
    delete process.env.TMUX;
    delete process.env.TMUX_PANE;
  }
  delete process.env.PI_TMUX_TEST_FAIL;
  const handlers = new Map();
  const broker = new Map();
  const confirmations = [];
  let active = ["bash", "read", "mcp__tmux_mcp__command_run"];
  const api = {
    on: (name, handler) => handlers.set(name, handler),
    events: { on: (name, handler) => broker.set(name, handler) },
    registerCommand: () => {},
    getAllTools: () => [
      ...(paneTools ? [{ name: "mcp__tmux_mcp__command_run" }] : []),
      ...(adapter ? [{ name: "mcp" }] : []),
    ],
    getActiveTools: () => active,
    setActiveTools: tools => active = tools,
  };
  const ctx = {
    cwd: "/project/nested",
    hasUI,
    ui: {
      confirm: async (...args) => { confirmations.push(args); return approved; },
      setStatus: () => {},
      notify: () => {},
      theme: { fg: (_style, text) => text },
    },
  };
  extension(api);
  return { ctx, handlers, broker, confirmations, active: () => active,
    call: (name, input = { command: "ls" }) => handlers.get("tool_call")({ toolName: name, input }, ctx) };
}

function response(decision, rule = "rule-id: actionable fix") {
  process.env.PI_TMUX_TEST_RESPONSE = JSON.stringify({ decision, rule });
}

test("native bash runs outside tmux and bypasses MCP adapter ownership", async () => {
  const h = setup({ adapter: true });
  for (const decision of ["allow", "passthrough"]) {
    response(decision);
    assert.equal(await h.call("bash"), undefined);
    assert.deepEqual(JSON.parse(readFileSync(trace)), {
      tool_name: "bash", cwd: h.ctx.cwd, tool_input: { command: "ls" },
    });
  }
  assert.equal(h.confirmations.length, 0);
  await h.handlers.get("session_start")({}, h.ctx);
  assert.ok(h.active().includes("bash"));
});

test("native Bash stays available and gated before pane tools connect", async () => {
  const h = setup({ tmux: true, paneTools: false });
  await h.handlers.get("before_agent_start")({}, h.ctx);
  assert.ok(h.active().includes("bash"));
  response("deny", "prefer-tmux: connect pane tools first");
  assert.equal((await h.call("bash", { command: "cargo build" })).block, true);
});

test("native denials preserve multiline corrective messages without prompting", async () => {
  const h = setup();
  response("deny", "prefer-tmux: use a pane\nsecond-rule: another fix");
  const result = await h.call("bash", { command: "cargo build" });
  assert.equal(result.block, true);
  assert.match(result.reason, /prefer-tmux: use a pane\nsecond-rule: another fix/);
  assert.equal(h.confirmations.length, 0);
});

test("native ask confirms the exact command and cwd, or blocks on refusal", async () => {
  response("ask");
  const yes = setup();
  assert.equal(await yes.call("bash", { command: "rm -r out" }), undefined);
  assert.match(yes.confirmations[0][1], /rm -r out/);
  assert.match(yes.confirmations[0][1], /cwd: \/project\/nested/);
  const no = setup({ approved: false });
  assert.equal((await no.call("bash")).block, true);
});

test("native ask fails closed in headless mode and on subprocess failure", async () => {
  const h = setup({ hasUI: false });
  response("ask");
  assert.match((await h.call("bash")).reason, /No approval UI/);
  assert.equal(h.confirmations.length, 0);
  process.env.PI_TMUX_TEST_FAIL = "1";
  assert.match((await h.call("bash")).reason, /policy check failed/);
  delete process.env.PI_TMUX_TEST_FAIL;
  process.env.PI_TMUX_TEST_RESPONSE = "not JSON";
  assert.match((await h.call("bash")).reason, /invalid JSON/);
  response("unexpected");
  assert.match((await h.call("bash")).reason, /unexpected decision/);
});

test("inside tmux keeps Bash replacement and the existing pane approval payload", async () => {
  const h = setup({ tmux: true });
  await h.handlers.get("before_agent_start")({}, h.ctx);
  assert.ok(!h.active().includes("bash"));
  response("allow");
  assert.equal(await h.call("mcp__tmux_mcp__command_run", { pane_id: "%1", command: "ls" }), undefined);
  assert.deepEqual(JSON.parse(readFileSync(trace)), { tool_input: { pane_id: "%1", command: "ls" } });
  response("deny", "constant-echo: run the real command instead");
  assert.match((await h.call("mcp__tmux_mcp__command_run", { pane_id: "%1", command: "echo marker" })).reason, /run the real command/);
});

test("native Bash stays guarded after the adapter claims an MCP call", async () => {
  const h = setup({ tmux: true, adapter: true });
  await h.handlers.get("before_agent_start")({}, h.ctx);
  response("allow");
  let claimed;
  h.broker.get("pi-mcp-adapter:tool-approval-request")({
    serverName: "tmux-mcp", originalToolName: "command_run", args: { pane_id: "%1", command: "ls" },
    claim: handler => { claimed = handler; return true; },
  });
  assert.equal(await claimed(), "allow_once");
  response("deny", "eval: blocked");
  assert.equal((await h.call("bash", { command: "eval x" })).block, true);
});
