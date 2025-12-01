# Tmux MCP Server

Model Context Protocol server that enables Claude Desktop to interact with and view tmux session content. This integration allows AI assistants to read from, control, and observe your terminal sessions.

## Features

- List and search tmux sessions
- View and navigate tmux windows and panes
- Capture and expose terminal content from any pane
- Execute commands in tmux panes and retrieve results (use it at your own risk ⚠️)
- Create new tmux sessions and windows
- Split panes horizontally or vertically with customizable sizes
- Kill tmux sessions, windows, and panes

Check out this short video to get excited!

</br>

[![youtube video](http://i.ytimg.com/vi/3W0pqRF1RS0/hqdefault.jpg)](https://www.youtube.com/watch?v=3W0pqRF1RS0)

## Prerequisites

- Node.js
- tmux installed and running

## Usage

### Configure Claude Desktop

Add this MCP server to your Claude Desktop configuration:

```json
"mcpServers": {
  "tmux": {
    "command": "npx",
    "args": ["-y", "tmux-mcp"]
  }
}
```

### MCP server options

You can optionally specify the command line shell you are using, if unspecified it defaults to `bash`

```json
"mcpServers": {
  "tmux": {
    "command": "npx",
    "args": ["-y", "tmux-mcp", "--shell-type=fish"]
  }
}
```

The MCP server needs to know the shell only when executing commands, to properly read its exit status.

## Available Resources

- `tmux://sessions` - List all tmux sessions
- `tmux://pane/{paneId}` - View content of a specific tmux pane
- `tmux://command/{commandId}/result` - Results from executed commands

## Available Tools

- `list-sessions` - List all active tmux sessions
- `find-session` - Find a tmux session by name
- `list-windows` - List windows in a tmux session
- `list-panes` - List panes in a tmux window
- `list-active-panes` - List panes in the active tmux window
- `capture-pane` - Capture content from a tmux pane
- `create-session` - Create a new tmux session
- `create-window` - Create a new window in a tmux session
- `split-pane` - Split a tmux pane horizontally or vertically with optional size
- `kill-session` - Kill a tmux session by ID
- `kill-window` - Kill a tmux window by ID
- `kill-pane` - Kill a tmux pane by ID
- `execute-command` - Execute a command in a tmux pane
- `get-command-result` - Get the result of an executed command

## Bash Integration (Optional)

For cleaner command execution, add the following to your `.bashrc`. This keeps executed commands on their own line and out of bash history, while still allowing tmux-mcp to parse command output and exit codes.

```bash
# Exclude __mcp_start from history
HISTIGNORE="__mcp_start:$HISTIGNORE"

# --- tmux-mcp integration ---
# __mcp_start is called by tmux-mcp before executing commands.
# Uses PROMPT_COMMAND to output markers after commands complete.
#
# Flow: __mcp_start -> disables prompt generators, installs minimal PROMPT_COMMAND
#       PROMPT_COMMAND fires after __mcp_start -> outputs START
#       actual command runs
#       PROMPT_COMMAND fires after command -> outputs DONE_<rc>, restores prompt
__mcp_armed=false
__mcp_saved_prompt_cmd=""
__mcp_prompt_hook() {
  local rc=$?
  if ! $__mcp_armed; then
    # First call after __mcp_start - output start marker
    __mcp_armed=true
    echo "TMUX_MCP_START"
  else
    # After real command - output end marker, restore and run PROMPT_COMMAND
    __mcp_armed=false
    PROMPT_COMMAND="$__mcp_saved_prompt_cmd"
    echo "TMUX_MCP_DONE_$rc"
    eval "$PROMPT_COMMAND"
  fi
  return $rc
}

__mcp_start() {
  __mcp_armed=false
  __mcp_saved_prompt_cmd="$PROMPT_COMMAND"
  PROMPT_COMMAND="__mcp_prompt_hook"
  PS1='$ '
}
```

