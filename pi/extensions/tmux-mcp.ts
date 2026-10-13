/**
 * tmux-mcp for Pi
 *
 * Wraps the `tmux-mcp` MCP server so Pi gets policy-controlled tmux access:
 *
 * - Inside tmux (`TMUX` + `TMUX_PANE` set): the extension is active. It gates
 *   `command_run` through the daemon's CEL policy and approval store, always
 *   confirms `press_key`, disables Pi's built-in `bash` tool in favor of tmux
 *   `command_run`, and publishes a status line that pi-open-tui renders in the
 *   footer.
 * - Native `bash` calls are policy-checked independently of MCP, including
 *   outside tmux. No tools are replaced or panes created outside tmux.
 *
 * MCP calls are intercepted through one of two paths:
 *
 * - `pi-mcp-adapter` (the usual host): every resolved MCP call emits
 *   `pi-mcp-adapter:tool-approval-request` on the Pi event bus. We claim the
 *   request for tmux-mcp and run the policy check there.
 * - Pi's built-in MCP: a call to `mcp__tmux_mcp__command_run` reaches the
 *   regular `tool_call` pipeline, which we gate by name.
 *
 * The two paths are mutually exclusive, chosen from the tool registry so a
 * direct tool is never gated twice.
 */

import { spawn } from "node:child_process";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";

const SERVER_NAME = "tmux-mcp";
const STATUS_KEY = "tmux-mcp";
const BINARY = process.env.TMUX_MCP_BINARY?.trim() || "tmux-mcp";
const BUILTIN_TOOL_PREFIX = "mcp__tmux_mcp__";

const READ_ONLY_TOOLS = new Set([
  "list_panes",
  "command_read",
  "command_history",
  "debug_pane",
  "inject_osc133",
]);

const GATED_TOOLS = new Set(["command_run", "press_key"]);

type DecisionKind = "allow" | "passthrough" | "deny" | "ask";

interface PolicyResponse {
  decision: DecisionKind;
  rule?: string;
  message?: string;
  context?: {
    hostname?: string | null;
    cwd?: string | null;
    user?: string | null;
    foreground?: string | null;
  };
}

interface DecisionOutcome {
  decision: DecisionKind | "error";
  reason?: string;
}

interface BrokerRequest {
  serverName?: string;
  originalToolName?: string;
  args?: Record<string, unknown>;
  claim?(handler: () => Promise<string> | string): boolean;
}

function inTmux(): boolean {
  return Boolean(process.env.TMUX) && Boolean(process.env.TMUX_PANE);
}

/** Map a registered tool name to its tmux-mcp tool suffix, if it is one. */
function toolSuffix(name: string): string | undefined {
  if (name.startsWith(BUILTIN_TOOL_PREFIX)) {
    return name.slice(BUILTIN_TOOL_PREFIX.length);
  }
  const lower = name.toLowerCase();
  if (!lower.includes("tmux")) return undefined;
  for (const suffix of [...READ_ONLY_TOOLS, ...GATED_TOOLS]) {
    if (lower.endsWith(`_${suffix}`) || lower.endsWith(`-${suffix}`) || lower === suffix) {
      return suffix;
    }
  }
  return undefined;
}

function runBinary(
  args: string[],
  input: string,
): Promise<{ stdout: string; stderr: string; code: number | null }> {
  return new Promise((resolve, reject) => {
    const child = spawn(BINARY, args, {
      // Force the neutral Pi decision object instead of the Claude hook envelope.
      env: { ...process.env, TMUX_MCP_HARNESS: "pi" },
    });
    let stdout = "";
    let stderr = "";
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk: string) => {
      stdout += chunk;
    });
    child.stderr.on("data", (chunk: string) => {
      stderr += chunk;
    });
    child.on("error", reject);
    child.on("close", (code) => resolve({ stdout, stderr, code }));
    child.stdin.end(input);
  });
}

async function requestPolicy(input: Record<string, unknown>): Promise<PolicyResponse> {
  const payload = JSON.stringify(input);
  const { stdout, stderr, code } = await runBinary(["policy-check"], payload);
  if (code !== 0) {
    throw new Error((stderr || `policy-check exited with code ${code}`).trim());
  }
  const text = stdout.trim();
  if (!text) throw new Error("policy-check produced no output");
  try {
    return JSON.parse(text) as PolicyResponse;
  } catch {
    throw new Error(`policy-check returned invalid JSON: ${text.slice(0, 200)}`);
  }
}

export default function tmuxMcpExtension(pi: ExtensionAPI): void {
  let enabled = inTmux();
  let latestCtx: ExtensionContext | undefined;
  let serverSummary: string | undefined;
  let scratchPane: string | undefined;
  let warnedMissingTools = false;

  const renderStatus = (ctx: ExtensionContext | undefined): void => {
    if (!ctx?.ui || !enabled) return;
    let dot = "●";
    try {
      dot = ctx.ui.theme.fg("success", "●");
    } catch {
      // Theme is unavailable outside the interactive TUI; keep the plain dot.
    }
    const detail = [serverSummary ?? "ready", scratchPane ? `pane ${scratchPane}` : undefined]
      .filter(Boolean)
      .join(" · ");
    try {
      ctx.ui.setStatus(STATUS_KEY, `${dot} tmux-mcp ${detail}`);
    } catch {
      // Status is cosmetic; never fail a turn over it.
    }
  };

  const clearStatus = (ctx: ExtensionContext | undefined): void => {
    try {
      ctx?.ui?.setStatus(STATUS_KEY, undefined);
    } catch {
      // ignore
    }
  };

  /** True when pi-mcp-adapter owns MCP calls (so the broker event will fire). */
  const adapterOwnsMcp = (): boolean => {
    return pi.getAllTools().some((tool) => {
      if (tool.name === "mcp" || tool.name === "mcpScript") return true;
      const source = (tool.sourceInfo?.source ?? "").toLowerCase();
      return source.includes("mcp-adapter") || source.includes("mcp_adapter");
    });
  };

  /**
   * Remove the built-in `bash` tool in favor of tmux `command_run`, but only
   * once `command_run` is actually registered and active. Otherwise the model
   * would be left without a shell.
   */
  const syncTools = (ctx: ExtensionContext): void => {
    if (!enabled) return;
    const commandRun = pi.getAllTools().map((t) => t.name).find((name) => toolSuffix(name) === "command_run");
    if (!commandRun) {
      if (!warnedMissingTools) {
        warnedMissingTools = true;
        try {
          ctx.ui.notify(
            "tmux-mcp: command_run is not available yet; keeping bash. Add the tmux-mcp server to ~/.pi/agent/mcp.json with \"exposure\": \"direct\", then reload.",
            "warning",
          );
        } catch {
          // ignore
        }
      }
      return;
    }
    warnedMissingTools = false;
    const active = pi.getActiveTools();
    if (active.includes(commandRun) && !active.includes("bash")) return;
    const next = active.filter((name) => name !== "bash" && name !== commandRun);
    next.push(commandRun);
    pi.setActiveTools(next);
  };

  /**
   * Ask the binary for a safe target pane. When the agent's own pane is the
   * only one in its window, the binary splits a scratch pane and arms it with
   * shell integration; otherwise it returns an existing pane. The origin pane
   * (where this agent runs) is never returned.
   */
  const ensureTargetPane = async (ctx: ExtensionContext): Promise<void> => {
    if (!enabled || !ctx.hasUI) return;
    if (/^(1|true|yes)$/i.test(process.env.TMUX_MCP_NO_AUTO_PANE ?? "")) return;
    try {
      const { stdout } = await runBinary(["ensure-target", "--json"], "");
      const info = JSON.parse(stdout.trim()) as { pane_id?: string; created?: boolean };
      if (!info?.pane_id) return;
      scratchPane = info.pane_id;
      if (info.created) {
        try {
          ctx.ui.notify(
            `tmux-mcp: created scratch pane ${info.pane_id} for shell commands.`,
            "info",
          );
        } catch {
          // ignore
        }
      }
      renderStatus(ctx);
    } catch {
      // Non-fatal: the model can still call ensure_pane or list_panes itself.
    }
  };

  const decideCommandRun = async (
    args: Record<string, unknown>,
    ctx: ExtensionContext,
  ): Promise<DecisionOutcome> => {
    const paneId = typeof args.pane_id === "string" ? args.pane_id : process.env.TMUX_PANE ?? "";
    const command = typeof args.command === "string" ? args.command : "";
    if (!paneId || !command) {
      return { decision: "error", reason: "tmux-mcp: missing pane_id or command" };
    }

    let response: PolicyResponse;
    try {
      response = await requestPolicy({ tool_input: { pane_id: paneId, command } });
    } catch (error) {
      return {
        decision: "error",
        reason: `tmux-mcp: policy check failed (${(error as Error).message}). Blocked for safety.`,
      };
    }

    switch (response.decision) {
      case "allow":
        return { decision: "allow" };
      case "deny":
        return { decision: "deny", reason: response.rule || response.message || "tmux-mcp: denied by policy" };
      case "ask": {
        if (!ctx.hasUI) {
          return {
            decision: "ask",
            reason: `tmux-mcp requires approval (rule: ${response.rule || "?"}), but no UI is available. Update the policy to allow it in autonomous mode.`,
          };
        }
        const c = response.context ?? {};
        const approved = await ctx.ui.confirm(
          "tmux-mcp: allow command?",
          `${command}\n\npane ${paneId}  user ${c.user ?? "?"}  host ${c.hostname ?? "local"}  cwd ${c.cwd ?? "?"}  shell ${c.foreground ?? "?"}\nrule: ${response.rule || "?"}`,
        );
        return approved
          ? { decision: "allow" }
          : { decision: "ask", reason: "tmux-mcp: denied by user" };
      }
      default:
        return { decision: "error", reason: `tmux-mcp: unexpected decision ${String(response.decision)}` };
    }
  };

  // This path deliberately runs before any tmux/adapter checks: native bash
  // is local, and needs neither an MCP server nor a pane approval token.
  const decideBash = async (
    args: Record<string, unknown>,
    ctx: ExtensionContext,
  ): Promise<DecisionOutcome> => {
    const command = typeof args.command === "string" ? args.command : "";
    if (!command) return { decision: "error", reason: "tmux-mcp: missing bash command" };
    let response: PolicyResponse;
    try {
      response = await requestPolicy({ tool_name: "bash", cwd: ctx.cwd, tool_input: { command } });
    } catch (error) {
      return { decision: "error", reason: `tmux-mcp: policy check failed (${(error as Error).message}). Blocked for safety.` };
    }
    const reason = response.rule || response.message || "tmux-mcp: policy requires approval";
    switch (response.decision) {
      case "allow":
      case "passthrough":
        return { decision: response.decision };
      case "deny":
        return { decision: "deny", reason };
      case "ask":
        if (!ctx.hasUI) {
          return { decision: "ask", reason: `${reason}\nNo approval UI is available; command blocked.` };
        }
        return await ctx.ui.confirm("tmux-mcp: allow local bash command?", `${command}\n\ncwd: ${ctx.cwd}\n${reason}`)
          ? { decision: "allow" }
          : { decision: "ask", reason: "tmux-mcp: denied by user" };
      default:
        return { decision: "error", reason: `tmux-mcp: unexpected decision ${String(response.decision)}` };
    }
  };

  const decidePressKey = async (
    args: Record<string, unknown>,
    ctx: ExtensionContext,
  ): Promise<DecisionOutcome> => {
    const paneId = typeof args.pane_id === "string" ? args.pane_id : process.env.TMUX_PANE ?? "";
    const keys = typeof args.keys === "string" ? args.keys : "";
    const desc = `pane ${paneId || "?"}  keys ${keys || "?"}`;
    if (!ctx.hasUI) {
      return { decision: "ask", reason: `tmux-mcp press_key requires approval, but no UI is available: ${desc}` };
    }
    const approved = await ctx.ui.confirm(
      "tmux-mcp: send key?",
      `${desc}\n\npress_key can type into the pane and bypass command_run policy.`,
    );
    return approved
      ? { decision: "allow" }
      : { decision: "ask", reason: "tmux-mcp: press_key denied by user" };
  };

  // --- Lifecycle ---

  pi.on("session_start", async (_event, ctx) => {
    latestCtx = ctx;
    enabled = inTmux();
    if (!enabled) {
      clearStatus(ctx);
      return;
    }
    renderStatus(ctx);
    syncTools(ctx);
    // Create a scratch pane before the first turn if the agent's pane is the
    // only one, so commands have somewhere to run. Bounded so a slow or
    // missing daemon can never stall startup.
    await Promise.race([
      ensureTargetPane(ctx),
      new Promise((resolve) => setTimeout(resolve, 8000)),
    ]);
  });

  pi.on("before_agent_start", async (_event, ctx) => {
    latestCtx = ctx;
    if (!enabled) return;
    renderStatus(ctx);
    syncTools(ctx);
  });

  pi.on("session_shutdown", async (_event, ctx) => {
    clearStatus(ctx);
  });

  // --- Path A: pi-mcp-adapter approval broker ---

  let brokerClaimed = false;
  pi.events.on("pi-mcp-adapter:tool-approval-request", (raw: unknown) => {
    const request = raw as BrokerRequest;
    if (!enabled || request?.serverName !== SERVER_NAME) return;
    const tool = request.originalToolName ?? "";
    if (READ_ONLY_TOOLS.has(tool)) return; // abstain: read-only tools run freely
    if (!GATED_TOOLS.has(tool)) return;
    if (typeof request.claim !== "function") return;
    brokerClaimed = true;
    request.claim(async () => {
      const ctx = latestCtx;
      if (!ctx) return "deny";
      const args = request.args ?? {};
      const outcome =
        tool === "press_key" ? await decidePressKey(args, ctx) : await decideCommandRun(args, ctx);
      serverSummary = outcome.decision === "allow" ? "ok" : "blocked";
      renderStatus(ctx);
      if (outcome.decision === "allow") return "allow_once";
      // The broker carries no reason string. Let the daemon emit the precise
      // policy-denial error for the model, and only deny here for user/no-UI blocks.
      if (outcome.decision === "deny") return "allow_once";
      return "deny";
    });
  });

  // --- Path B: Pi built-in MCP tool_call fallback ---

  pi.on("tool_call", async (event, ctx) => {
    latestCtx = ctx;
    if (event.toolName === "bash") {
      const outcome = await decideBash((event.input ?? {}) as Record<string, unknown>, ctx);
      if (outcome.decision === "allow" || outcome.decision === "passthrough") return undefined;
      return { block: true, reason: outcome.reason ?? "tmux-mcp: bash blocked" };
    }
    if (!enabled) return undefined;
    // Skip when the adapter broker handles the same call; direct tools would
    // otherwise be gated twice (once here, once in the broker).
    if (brokerClaimed || adapterOwnsMcp()) return undefined;
    const tool = toolSuffix(event.toolName);
    if (!tool || !GATED_TOOLS.has(tool)) return undefined;
    const args = (event.input ?? {}) as Record<string, unknown>;
    const outcome =
      tool === "press_key" ? await decidePressKey(args, ctx) : await decideCommandRun(args, ctx);
    serverSummary = outcome.decision === "allow" ? "ok" : "blocked";
    renderStatus(ctx);
    if (outcome.decision === "allow") return undefined;
    return { block: true, reason: outcome.reason ?? `tmux-mcp ${tool} blocked` };
  });

  // --- Adapter server status -> footer ---

  pi.events.on("pi-mcp-adapter/status/v1", (raw: unknown) => {
    const snapshot = raw as {
      servers?: Array<{ name?: string; status?: string; toolCount?: number }>;
    };
    const match = snapshot?.servers?.find((server) => server.name === SERVER_NAME);
    if (!match) return;
    serverSummary = `${match.status ?? "?"} · ${match.toolCount ?? 0} tools`;
    if (latestCtx) {
      renderStatus(latestCtx);
      syncTools(latestCtx);
    }
  });

  // --- /tmux-mcp status command ---

  pi.registerCommand("tmux-mcp", {
    description: "Show tmux-mcp extension status",
    handler: async (_args, ctx) => {
      latestCtx = ctx;
      const lines = [
        `tmux integration: ${enabled ? "enabled (inside tmux)" : "disabled (not inside tmux)"}`,
        "native bash policy: active (inside and outside tmux)",
        `binary: ${BINARY}`,
        `policy-check output: ${adapterOwnsMcp() ? "pi-mcp-adapter broker" : "built-in tool_call"}`,
        `server: ${serverSummary ?? "unknown"}`,
        `target pane: ${scratchPane ?? "(not resolved yet)"}`,
      ];
      ctx.ui.notify(lines.join("\n"), "info");
    },
  });
}
