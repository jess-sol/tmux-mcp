import { exec as execCallback } from "child_process";
import { promisify } from "util";
import { v4 as uuidv4 } from 'uuid';

const exec = promisify(execCallback);

// Basic interfaces for tmux objects
export interface TmuxSession {
  id: string;
  name: string;
  attached: boolean;
  windows: number;
}

export interface TmuxWindow {
  id: string;
  name: string;
  active: boolean;
  sessionId: string;
}

export interface TmuxPane {
  id: string;
  windowId: string;
  active: boolean;
  currentCommand: string;
  cwd: string;
  title: string;
}

interface CommandExecution {
  id: string;
  paneId: string;
  command: string;
  status: 'pending' | 'completed' | 'error';
  startTime: Date;
  result?: string;
  exitCode?: number;
  rawMode?: boolean;
}

export type ShellType = 'bash' | 'zsh' | 'fish';

let shellConfig: { type: ShellType } = { type: 'bash' };

export function setShellConfig(config: { type: string }): void {
  // Validate shell type
  const validShells: ShellType[] = ['bash', 'zsh', 'fish'];

  if (validShells.includes(config.type as ShellType)) {
    shellConfig = { type: config.type as ShellType };
  } else {
    shellConfig = { type: 'bash' };
  }
}

/**
 * Execute a tmux command and return the result
 */
export async function executeTmux(tmuxCommand: string): Promise<string> {
  try {
    const { stdout } = await exec(`tmux ${tmuxCommand}`);
    return stdout.trim();
  } catch (error: any) {
    throw new Error(`Failed to execute tmux command: ${error.message}`);
  }
}

/**
 * Check if tmux server is running
 */
export async function isTmuxRunning(): Promise<boolean> {
  try {
    await executeTmux("list-sessions -F '#{session_name}'");
    return true;
  } catch (error) {
    return false;
  }
}

/**
 * List all tmux sessions
 */
export async function listSessions(): Promise<TmuxSession[]> {
  const format = "#{session_id}:#{session_name}:#{?session_attached,1,0}:#{session_windows}";
  const output = await executeTmux(`list-sessions -F '${format}'`);

  if (!output) return [];

  return output.split('\n').map(line => {
    const [id, name, attached, windows] = line.split(':');
    return {
      id,
      name,
      attached: attached === '1',
      windows: parseInt(windows, 10)
    };
  });
}

/**
 * Find a session by name
 */
export async function findSessionByName(name: string): Promise<TmuxSession | null> {
  try {
    const sessions = await listSessions();
    return sessions.find(session => session.name === name) || null;
  } catch (error) {
    return null;
  }
}

/**
 * List windows in a session
 */
export async function listWindows(sessionId: string): Promise<TmuxWindow[]> {
  const format = "#{window_id}:#{window_name}:#{?window_active,1,0}";
  const output = await executeTmux(`list-windows -t '${sessionId}' -F '${format}'`);

  if (!output) return [];

  return output.split('\n').map(line => {
    const [id, name, active] = line.split(':');
    return {
      id,
      name,
      active: active === '1',
      sessionId
    };
  });
}

/**
 * List panes in a window
 */
export async function listPanes(windowId: string): Promise<TmuxPane[]> {
  const format = "#{pane_id}|#{pane_title}|#{pane_current_command}|#{pane_current_path}|#{?pane_active,1,0}";
  const output = await executeTmux(`list-panes -t '${windowId}' -F '${format}'`);

  if (!output) return [];

  return output.split('\n').map(line => {
    const [id, title, currentCommand, cwd, active] = line.split('|');
    return {
      id,
      windowId,
      currentCommand,
      cwd,
      title: title,
      active: active === '1'
    };
  });
}

/**
 * List panes in the active window of the attached session
 */
export async function listActivePanes(): Promise<TmuxPane[]> {
  const sessions = await listSessions();
  const activeSession = sessions.find(s => s.attached);
  if (!activeSession) return [];

  const windows = await listWindows(activeSession.id);
  const activeWindow = windows.find(w => w.active);
  if (!activeWindow) return [];

  return listPanes(activeWindow.id);
}

/**
 * Capture content from a specific pane, by default the latest 200 lines.
 */
export async function capturePaneContent(paneId: string, lines: number = 200, includeColors: boolean = false): Promise<string> {
  const colorFlag = includeColors ? '-e' : '';
  return executeTmux(`capture-pane -p ${colorFlag} -t '${paneId}' -S -${lines} -E -`);
}

/**
 * Create a new tmux session
 */
export async function createSession(name: string): Promise<TmuxSession | null> {
  await executeTmux(`new-session -d -s "${name}"`);
  return findSessionByName(name);
}

/**
 * Create a new window in a session
 */
export async function createWindow(sessionId: string, name: string): Promise<TmuxWindow | null> {
  const output = await executeTmux(`new-window -t '${sessionId}' -n '${name}'`);
  const windows = await listWindows(sessionId);
  return windows.find(window => window.name === name) || null;
}

/**
 * Kill a tmux session by ID
 */
export async function killSession(sessionId: string): Promise<void> {
  await executeTmux(`kill-session -t '${sessionId}'`);
}

/**
 * Kill a tmux window by ID
 */
export async function killWindow(windowId: string): Promise<void> {
  await executeTmux(`kill-window -t '${windowId}'`);
}

/**
 * Kill a tmux pane by ID
 */
export async function killPane(paneId: string): Promise<void> {
  await executeTmux(`kill-pane -t '${paneId}'`);
}

/**
 * Split a tmux pane horizontally or vertically
 */
export async function splitPane(
  targetPaneId: string,
  direction: 'horizontal' | 'vertical' = 'vertical',
  size?: number
): Promise<TmuxPane | null> {
  // Build the split-window command
  let splitCommand = 'split-window';

  // Add direction flag (-h for horizontal, -v for vertical)
  if (direction === 'horizontal') {
    splitCommand += ' -h';
  } else {
    splitCommand += ' -v';
  }

  // Add target pane
  splitCommand += ` -t '${targetPaneId}'`;

  // Add size if specified (as percentage)
  if (size !== undefined && size > 0 && size < 100) {
    splitCommand += ` -p ${size}`;
  }

  // Execute the split command
  await executeTmux(splitCommand);

  // Get the window ID from the target pane to list all panes
  const windowInfo = await executeTmux(`display-message -p -t '${targetPaneId}' '#{window_id}'`);

  // List all panes in the window to find the newly created one
  const panes = await listPanes(windowInfo);

  // The newest pane is typically the last one in the list
  return panes.length > 0 ? panes[panes.length - 1] : null;
}

// Map to track ongoing command executions
const activeCommands = new Map<string, CommandExecution>();

const startMarkerText = 'TMUX_MCP_START';
const endMarkerPrefix = "TMUX_MCP_DONE_";

// Valid shells that can execute commands
const VALID_SHELLS = ['bash', 'zsh', 'fish', 'sh', 'dash', 'ksh', 'tcsh', 'csh', 'ssh', 'mosh'];

// Execute a command in a tmux pane and track its execution
export async function executeCommand(paneId: string, command: string, rawMode?: boolean, noEnter?: boolean): Promise<string> {
  // Check if the pane is running a shell (only for non-raw, non-noEnter commands)
  if (!rawMode && !noEnter) {
    const format = "#{pane_current_command}";
    const currentCommand = await executeTmux(`display-message -p -t '${paneId}' '${format}'`);

    if (!VALID_SHELLS.includes(currentCommand.toLowerCase())) {
      throw new Error(`Cannot execute command: pane ${paneId} is running '${currentCommand}', not a shell. Use rawMode for interactive applications.`);
    }
  }

  // Generate unique ID for this command execution
  const commandId = uuidv4();

  // Store command in tracking map
  activeCommands.set(commandId, {
    id: commandId,
    paneId,
    command,
    status: 'pending',
    startTime: new Date(),
    rawMode: rawMode || noEnter
  });

  // Send the command to the tmux pane
  if (noEnter) {
    // Check if this is a special key (e.g., Up, Down, Left, Right, Escape, Tab, etc.)
    // Special keys in tmux are typically capitalized or have special names
    const specialKeys = ['Up', 'Down', 'Left', 'Right', 'Escape', 'Tab', 'Enter', 'Space',
      'BSpace', 'Delete', 'Home', 'End', 'PageUp', 'PageDown',
      'F1', 'F2', 'F3', 'F4', 'F5', 'F6', 'F7', 'F8', 'F9', 'F10', 'F11', 'F12'];

    if (specialKeys.includes(command)) {
      // Send special key as-is
      await executeTmux(`send-keys -t '${paneId}' ${command}`);
    } else {
      // For regular text, send each character individually to ensure proper processing
      // This handles both single characters (like 'q', 'f') and strings (like 'beam')
      for (const char of command) {
        await executeTmux(`send-keys -t '${paneId}' '${char.replace(/'/g, "'\\''")}'`);
      }
    }
  } else if (rawMode) {
    // Raw mode: send command without markers (for REPLs, interactive apps)
    await executeTmux(`send-keys -t '${paneId}' '${command.replace(/'/g, "'\\''")}' Enter`);
  } else {
    // Normal mode: use __mcp_start to set up markers via PROMPT_COMMAND
    // __mcp_start outputs TMUX_MCP_START and installs a one-shot hook that
    // outputs TMUX_MCP_DONE_<exit_code> after the command completes
    const escapedCommand = command.replace(/'/g, "'\\''");
    await executeTmux(`send-keys -t '${paneId}' '__mcp_start' Enter '${escapedCommand}' Enter`);
  }

  return commandId;
}

export async function checkCommandStatus(commandId: string): Promise<CommandExecution | null> {
  const command = activeCommands.get(commandId);
  if (!command) return null;

  if (command.status !== 'pending') return command;

  const content = await capturePaneContent(command.paneId, 1000);

  if (command.rawMode) {
    command.result = 'Status tracking unavailable for rawMode commands. Use capture-pane to monitor interactive apps instead.';
    return command;
  }

  // Find the last occurrence of the markers
  const startIndex = content.lastIndexOf(startMarkerText);
  const endIndex = content.lastIndexOf(endMarkerPrefix);

  if (startIndex === -1 || endIndex === -1 || endIndex <= startIndex) {
    command.result = "Command output could not be captured properly";
    return command;
  }

  // Extract exit code from the end marker line
  const endLine = content.substring(endIndex).split('\n')[0];
  const endMarkerRegex = new RegExp(`${endMarkerPrefix}(\\d+)`);
  const exitCodeMatch = endLine.match(endMarkerRegex);

  if (exitCodeMatch) {
    const exitCode = parseInt(exitCodeMatch[1], 10);

    command.status = exitCode === 0 ? 'completed' : 'error';
    command.exitCode = exitCode;

    // Extract output between the start and end markers
    const outputStart = startIndex + startMarkerText.length;
    const outputContent = content.substring(outputStart, endIndex).trim();

    command.result = outputContent.substring(outputContent.indexOf('\n') + 1).trim();

    // Update in map
    activeCommands.set(commandId, command);
  }

  return command;
}

// Get command by ID
export function getCommand(commandId: string): CommandExecution | null {
  return activeCommands.get(commandId) || null;
}

// Get all active command IDs
export function getActiveCommandIds(): string[] {
  return Array.from(activeCommands.keys());
}

// Clean up completed commands older than a certain time
export function cleanupOldCommands(maxAgeMinutes: number = 60): void {
  const now = new Date();

  for (const [id, command] of activeCommands.entries()) {
    const ageMinutes = (now.getTime() - command.startTime.getTime()) / (1000 * 60);

    if (command.status !== 'pending' && ageMinutes > maxAgeMinutes) {
      activeCommands.delete(id);
    }
  }
}

function getEndMarkerText(): string {
  return shellConfig.type === 'fish'
    ? `${endMarkerPrefix}$status`
    : `${endMarkerPrefix}$?`;
}

