//! Integration tests: the real tmux-mcp binary, driven over MCP against a
//! private tmux server. See `harness`.

mod harness;

mod command_read;
mod command_run;
mod guards;
mod harness_contract;
mod keys;
mod panes;
mod policy;
