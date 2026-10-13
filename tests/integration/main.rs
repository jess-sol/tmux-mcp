//! Integration tests: the real tmux-mcp binary, driven over MCP against a
//! private tmux server. See `harness`.

mod harness;

mod command_read;
mod command_run;
mod daemon;
mod guards;
mod harness_contract;
mod keys;
mod panes;
mod pi;
mod pi_bash;
mod policy;
