//! A `Harness`: a private tmux server in one temp dir, the real tmux-mcp
//! binary serving it over MCP, and the `Shell` a test works in. A `Harness`
//! calls tools as Claude Code does, from a pane of its own.
//!
//! The test process runs inside the developer's tmux, so its `$TMUX` is live:
//! every process the harness starts has `TMUX` and `TMUX_PANE` removed, and
//! only tmux-mcp gets them back, naming the harness's own server.

use std::fmt;
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParam;
use rmcp::service::{RoleClient, RunningService};
use serde_json::{Map, Value, json};
use tempfile::TempDir;
use tmux_mcp::daemon::rpc::OSC133_INJECT_LINES;

// --- Types ---

pub const SETTLE: Duration = Duration::from_secs(10);

pub struct Harness {
    session: Session,
    /// What every tmux-mcp process gets: this server, this pane, this HOME.
    env: Vec<(&'static str, String)>,
    socket: PathBuf,
    /// Held for its `Drop`: everything the harness makes lives in it.
    dir: TempDir,
}

/// One MCP server: a tmux-mcp process, talked to as one Claude Code session
/// talks to it.
pub struct Session {
    client: RunningService<RoleClient, ()>,
    /// The MCP server process, killed when the session drops.
    _server: tokio::process::Child,
}

pub struct Shell {
    socket: PathBuf,
    pane: String,
}

/// `wait_until` gave up. Its `Debug`, which `unwrap` prints, is the last
/// screen as tmux showed it.
pub struct Timeout(String);

pub struct PaneTable {
    text: String,
}

/// The read parameters `command_run` and `command_read` share. Any
/// combination is expressible, since a model can send any.
#[derive(Default)]
pub struct Read(Map<String, Value>);

/// What the `policy-check` hook tells Claude Code about a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// No decision: Claude Code's own permissions decide (a Bash call only).
    Pass,
    /// Claude Code asks the human first.
    Ask,
    Deny,
}

/// What the policy decides for one command on each channel, and the id of
/// the rule each decision's reason starts with ("" for none).
#[derive(Debug)]
pub struct Judged {
    pub tmux: Decision,
    pub tmux_rule: String,
    pub bash: Decision,
    pub bash_rule: String,
}

/// `decides!(h, { "command" => terms, … })` checks each command's
/// [`Judged`] against a pattern, then fails once with
/// every command that missed. A command maps to 1 to 4 terms, `_` anywhere:
/// `D` both channels decide `D`; `(D, "rule")` both decide `D` by `rule`;
/// `(Dtmux, Dbash)`; `(Dtmux, Dbash, "rule")` one rule for both;
/// `(Dtmux, "rule", Dbash, "rule")`.
///
/// `decides!(h, shell, { "command" => D, … })` checks the decision the hook
/// gives each command in `shell`, for what only a pane shows: its directory,
/// its host, a program running in it.
macro_rules! decides {
    ($h:expr, { $($line:expr => $want:tt),* $(,)? }) => {{
        let judged = $h.judge(&[$(AsRef::<str>::as_ref(&$line)),*]);
        let mut judged = judged.iter();
        let mut misses: Vec<String> = Vec::new();
        $(
            let j = judged.next().expect("one judgement per command");
            let got = (j.tmux, j.tmux_rule.as_str(), j.bash, j.bash_rule.as_str());
            if !matches!(got, $crate::harness::decides!(@pattern $want)) {
                misses.push(format!("{:?} => {:?}", $line, got));
            }
        )*
        assert!(misses.is_empty(), "these commands missed:\n{}", misses.join("\n"));
    }};
    ($h:expr, $shell:expr, { $($line:expr => $want:pat),* $(,)? }) => {{
        let mut misses: Vec<String> = Vec::new();
        $(
            let got = $h.decision(&$shell, AsRef::<str>::as_ref(&$line));
            if !matches!(got, $want) {
                misses.push(format!("{:?} => {:?}", $line, got));
            }
        )*
        assert!(misses.is_empty(), "these commands missed:\n{}", misses.join("\n"));
    }};
    (@pattern ($t:pat, $tr:pat, $b:pat, $br:pat)) => { ($t, $tr, $b, $br) };
    (@pattern ($t:pat, $b:pat, $r:pat)) => { ($t, $r, $b, $r) };
    // A literal second term is a rule; anything else, the Bash decision.
    (@pattern ($d:pat, $r:literal)) => { ($d, $r, $d, $r) };
    (@pattern ($t:pat, $b:pat)) => { ($t, _, $b, _) };
    (@pattern $d:pat) => { ($d, _, $d, _) };
}
pub(crate) use decides;

const POLL: Duration = Duration::from_millis(10);

// --- Public ---

impl Harness {
    pub async fn new() -> Harness {
        Harness::launch(None).await
    }

    /// A harness whose MCP server, and the daemon it starts, run as another
    /// build of `version`; hooks run as this build.
    pub async fn claiming(version: &str) -> Harness {
        Harness::launch(Some(version)).await
    }

    async fn launch(version: Option<&str>) -> Harness {
        // Under /tmp, so the sockets inside stay within `sun_path`'s 108 bytes.
        let dir = tempfile::Builder::new()
            .prefix("tmux-mcp-harness-")
            .tempdir_in("/tmp")
            .expect("a temp dir");
        let socket = dir.path().join("tmux");
        let home = dir.path().join("home");
        let runtime = dir.path().join("run");
        std::fs::create_dir(&home).expect("a HOME");
        std::fs::write(home.join(".bashrc"), bashrc()).expect("a .bashrc");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&runtime)
            .expect("a runtime dir");

        // The command that starts the server sets the environment every pane
        // inherits. 60 rows keep a Shell beside the harness's pane taller than
        // the 20 rows a tool reply shows of a screen.
        let cwd = dir.path().to_str().expect("a UTF-8 temp dir");
        let mut start = tmux(&socket);
        start.env("HOME", &home).env("XDG_RUNTIME_DIR", &runtime);
        run(start.args([
            "new-session",
            "-d",
            "-x",
            "120",
            "-y",
            "60",
            "-c",
            cwd,
            "sleep infinity",
        ]));
        let ids =
            run(tmux(&socket).args(["display-message", "-p", "#{pid} #{session_id} #{pane_id}"]));
        let ids: Vec<&str> = ids.split_whitespace().collect();
        let [pid, session, pane] = ids[..] else {
            panic!("tmux gave ids {ids:?}")
        };

        let env = vec![
            (
                "TMUX",
                format!(
                    "{},{pid},{}",
                    socket.display(),
                    session.trim_start_matches('$')
                ),
            ),
            ("TMUX_PANE", pane.to_string()),
            ("HOME", home.display().to_string()),
            ("XDG_RUNTIME_DIR", runtime.display().to_string()),
        ];
        let session = Session::start(&env, version).await;
        Harness {
            session,
            env,
            socket,
            dir,
        }
    }

    /// A second MCP server on the same daemon, as a second Claude Code session
    /// in the same window would be.
    pub async fn another_session(&self) -> Session {
        Session::start(&self.env, None).await
    }

    /// A pane beside the harness's own, started in `dir()`. Returns once its
    /// prompt shows and `list_panes` lists it. Each call halves the harness's
    /// pane, so a test makes one.
    pub async fn shell(&self) -> Shell {
        self.open_shell("exec bash --noprofile -i").await
    }

    /// A `shell` whose bash skips the `.bashrc`, so it sends no OSC 133
    /// markers until `inject_osc133`.
    pub async fn shell_without_integration(&self) -> Shell {
        self.open_shell("PS1='$ ' exec bash --noprofile --norc -i")
            .await
    }

    async fn open_shell(&self, program: &str) -> Shell {
        let cwd = self.dir().to_str().expect("a UTF-8 temp dir");
        let split = [
            "split-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-c",
            cwd,
            program,
        ];
        let pane = run(tmux(&self.socket).args(split)).trim().to_string();
        let shell = Shell {
            socket: self.socket.clone(),
            pane,
        };

        shell
            .wait_until(SETTLE, |s| s.lines().any(|row| row == "$"))
            .expect("a prompt");
        let deadline = Instant::now() + SETTLE;
        while self.list_panes().await.find(&shell).is_none() {
            assert!(
                Instant::now() < deadline,
                "list_panes never listed {}",
                shell.pane
            );
            tokio::time::sleep(POLL).await;
        }
        shell
    }

    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// Runs `command` in `shell` as Claude Code would: the `policy-check`
    /// hook first, then `command_run`. The reply is the text a model reads,
    /// an error's message included. An "ask" goes ahead, as the human's yes
    /// would.
    pub async fn run(&self, shell: &Shell, command: &str) -> String {
        self.run_with(shell, command, Read::default()).await
    }

    /// `command_run` with read parameters, after the hook, as `run`.
    pub async fn run_with(&self, shell: &Shell, command: &str, read: Read) -> String {
        if let (Decision::Deny, reason) = self.hook(shell, command) {
            return reason;
        }
        let arguments = read
            .set("pane_id", shell.pane.as_str())
            .set("command", command);
        self.call("command_run", arguments.0).await
    }

    /// Starts `command` and returns while it still runs: a `command_run` that
    /// stops waiting after a second.
    pub async fn start(&self, shell: &Shell, command: &str) {
        self.run_with(shell, command, Read::default().timeout(1))
            .await;
    }

    /// `command_read` of the pane's latest command.
    pub async fn read(&self, shell: &Shell, read: Read) -> String {
        self.call("command_read", read.set("pane_id", shell.pane.as_str()).0)
            .await
    }

    pub async fn history(&self, shell: &Shell) -> String {
        self.call("command_history", shell.target()).await
    }

    pub async fn debug_pane(&self, shell: &Shell) -> String {
        self.call("debug_pane", shell.target()).await
    }

    /// The screen `debug_pane` shows and the screen tmux shows, once they
    /// agree or `SETTLE` passes: the daemon follows tmux's changes a moment
    /// late.
    pub async fn screens(&self, shell: &Shell) -> (String, String) {
        let deadline = Instant::now() + SETTLE;
        loop {
            let daemon = self.debug_pane(shell).await;
            let tmux = shell.screen().trim_end().to_string();
            if daemon == tmux || Instant::now() >= deadline {
                return (daemon, tmux);
            }
            tokio::time::sleep(POLL).await;
        }
    }

    pub async fn inject_osc133(&self, shell: &Shell) -> String {
        self.call("inject_osc133", shell.target()).await
    }

    /// The press_key tool: the reply is the screen it returns.
    pub async fn press_key(&self, shell: &Shell, key: &str) -> String {
        let mut arguments = shell.target();
        arguments.insert("keys".into(), json!(key));
        self.call("press_key", arguments).await
    }

    pub async fn list_panes(&self) -> PaneTable {
        PaneTable {
            text: self.call("list_panes", Map::new()).await,
        }
    }

    /// Writes the project's `.claude/tmux-mcp.toml`, as a user would. The
    /// daemon reloads it on the next decision.
    pub fn project_policy(&self, toml: &str) {
        let dir = self.dir().join(".claude");
        std::fs::create_dir_all(&dir).expect("a .claude dir");
        std::fs::write(dir.join("tmux-mcp.toml"), toml).expect("a project policy");
    }

    /// Writes the user's `~/.claude/tmux-mcp.toml`, as `project_policy` does
    /// the project's.
    pub fn user_policy(&self, toml: &str) {
        let dir = self.dir().join("home").join(".claude");
        std::fs::create_dir_all(&dir).expect("a ~/.claude dir");
        std::fs::write(dir.join("tmux-mcp.toml"), toml).expect("a user policy");
    }

    /// Asks the `policy-check` hook about `command` in `shell`, as Claude Code
    /// does before every `command_run`. An "ask" leaves an approval in the
    /// daemon, which the next `command_run` of that command consumes.
    pub fn decision(&self, shell: &Shell, command: &str) -> Decision {
        self.hook(shell, command).0
    }

    /// The hook's decision and its reason, which a deny shows to Claude.
    pub fn hook(&self, shell: &Shell, command: &str) -> (Decision, String) {
        let input = json!({
            "tool_name": "mcp__tmux-mcp__command_run",
            "tool_input": {"pane_id": shell.pane, "command": command},
        });
        self.ask_hook(&self.env, input)
            .expect("a decision for command_run")
    }

    /// Asks the hook about a Claude Code `Bash` call in the project, from
    /// outside tmux: its decision and reason, `Pass` when the hook leaves it
    /// to Claude Code's permissions.
    pub fn bash(&self, command: &str) -> (Decision, String) {
        let env = self.outside_tmux();
        let input = json!({
            "tool_name": "Bash",
            "cwd": self.dir().to_str().expect("a UTF-8 project dir"),
            "tool_input": {"command": command},
        });
        self.ask_hook(&env, input)
            .unwrap_or((Decision::Pass, String::new()))
    }

    /// What the policy decides for each of `lines`, on both channels, in a
    /// shell at the project root (`policy-eval --json`).
    pub fn judge(&self, lines: &[&str]) -> Vec<Judged> {
        let rows: Vec<Value> = self
            .eval(&[&["--json"][..], lines].concat())
            .lines()
            .map(|row| serde_json::from_str(row).expect("a JSON row"))
            .collect();
        rows.chunks(2)
            .map(|pair| {
                let [tmux, bash] = pair else {
                    panic!("one row per channel: {pair:?}")
                };
                let (tmux, tmux_rule) = judgement(tmux);
                let (bash, bash_rule) = judgement(bash);
                Judged {
                    tmux,
                    tmux_rule,
                    bash,
                    bash_rule,
                }
            })
            .collect()
    }

    /// What `tmux-mcp policy-eval` prints for `args`, run in the project
    /// from outside tmux.
    pub fn eval(&self, args: &[&str]) -> String {
        let output = tmux_mcp(&self.outside_tmux())
            .arg("policy-eval")
            .args(args)
            .current_dir(self.dir())
            .output()
            .expect("the tmux-mcp policy-eval command");
        assert!(
            output.status.success(),
            "policy-eval failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("UTF-8 output")
    }

    /// The hook's answer to a payload for any other tool: None.
    pub fn other_tool(&self) -> Option<(Decision, String)> {
        self.ask_hook(
            &self.env,
            json!({"tool_name": "Read", "tool_input": {"file_path": "x"}}),
        )
    }
}

impl Harness {
    /// The harness's environment without its tmux server: a process run
    /// with it is outside tmux.
    fn outside_tmux(&self) -> Vec<(&'static str, String)> {
        self.env
            .iter()
            .filter(|(name, _)| !name.starts_with("TMUX"))
            .cloned()
            .collect()
    }

    /// Runs `policy-check` on `input` under `env`. No output means no
    /// decision.
    fn ask_hook(&self, env: &[(&'static str, String)], input: Value) -> Option<(Decision, String)> {
        let mut hook = tmux_mcp(env)
            .arg("policy-check")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the tmux-mcp policy-check hook");
        hook.stdin
            .take()
            .expect("piped stdin")
            .write_all(input.to_string().as_bytes())
            .expect("hook input");
        let output = hook.wait_with_output().expect("the hook's answer");
        // Claude Code reads exit status 2 as a denial, with stderr its reason.
        if output.status.code() == Some(2) {
            let reason = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Some((Decision::Deny, reason));
        }
        assert!(
            output.status.success(),
            "hook failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if output.stdout.iter().all(u8::is_ascii_whitespace) {
            return None;
        }
        let answer: Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|_| panic!("hook said {:?}", String::from_utf8_lossy(&output.stdout)));
        let answer = &answer["hookSpecificOutput"];
        let reason = answer["permissionDecisionReason"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let decision = match answer["permissionDecision"].as_str() {
            Some("allow") => Decision::Allow,
            Some("ask") => Decision::Ask,
            Some("deny") => Decision::Deny,
            other => panic!("hook decided {other:?}"),
        };
        Some((decision, reason))
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // The daemon exits when its tmux connection closes.
        let _ = tmux(&self.socket).arg("kill-server").output();
    }
}

impl Shell {
    /// Types `text` as literal keys. Returns once tmux shows it, so whatever
    /// the test does next meets a pane that already holds the text.
    pub fn type_text(&self, text: &str) {
        run(tmux(&self.socket).args(["send-keys", "-l", "-t", &self.pane, "--", text]));
        self.wait_until(SETTLE, |s| s.contains(text))
            .expect("the typed text on screen");
    }

    /// `type_text`, then Enter.
    pub fn say(&self, text: &str) {
        self.type_text(text);
        run(tmux(&self.socket).args(["send-keys", "-t", &self.pane, "Enter"]));
    }

    /// Resizes the pane to `rows`; the harness's own pane takes up the rest.
    pub fn resize(&self, rows: u16) {
        let rows = rows.to_string();
        run(tmux(&self.socket).args(["resize-pane", "-t", &self.pane, "-y", &rows]));
    }

    /// Puts the pane in copy mode, as a person scrolling back would.
    pub fn copy_mode(&self) {
        run(tmux(&self.socket).args(["copy-mode", "-t", &self.pane]));
    }

    /// Leaves copy mode.
    pub fn leave_copy_mode(&self) {
        run(tmux(&self.socket).args(["send-keys", "-t", &self.pane, "-X", "cancel"]));
    }

    /// Polls the screen, as tmux renders it, until `pred` holds.
    pub fn wait_until(
        &self,
        timeout: Duration,
        mut pred: impl FnMut(&str) -> bool,
    ) -> Result<(), Timeout> {
        let deadline = Instant::now() + timeout;
        loop {
            let screen = self.screen();
            if pred(&screen) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Timeout(screen));
            }
            thread::sleep(POLL);
        }
    }
}

impl fmt::Debug for Timeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the screen never matched; it last showed:\n{}", self.0)
    }
}

impl PaneTable {
    pub fn row(&self, shell: &Shell) -> String {
        let row = self
            .find(shell)
            .unwrap_or_else(|| panic!("no row for {} in\n{}", shell.pane, self.text));
        row.to_string()
    }
}

impl Read {
    pub fn next(self, lines: u64) -> Read {
        self.set("next", lines)
    }

    pub fn head(self, lines: u64) -> Read {
        self.set("head", lines)
    }

    pub fn tail(self, lines: u64) -> Read {
        self.set("tail", lines)
    }

    pub fn search(self, pattern: &str) -> Read {
        self.set("search", pattern)
    }

    pub fn before(self, lines: u64) -> Read {
        self.set("before", lines)
    }

    pub fn after(self, lines: u64) -> Read {
        self.set("after", lines)
    }

    pub fn timeout(self, secs: u64) -> Read {
        self.set("timeout_secs", secs)
    }
}

impl Session {
    /// `command_read` of the pane's latest command, from this session.
    pub async fn read(&self, shell: &Shell, read: Read) -> String {
        self.call("command_read", read.set("pane_id", shell.pane.as_str()).0)
            .await
    }
}

// --- Internal ---

impl Harness {
    async fn call(&self, tool: &str, arguments: Map<String, Value>) -> String {
        self.session.call(tool, arguments).await
    }
}

impl Session {
    async fn start(env: &[(&'static str, String)], version: Option<&str>) -> Session {
        let mut server = tokio::process::Command::from(tmux_mcp(env));
        if let Some(version) = version {
            server.env("TMUX_MCP_VERSION", version);
        }
        server
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut server = server.spawn().expect("the tmux-mcp binary");
        let pipes = (
            server.stdout.take().expect("piped stdout"),
            server.stdin.take().expect("piped stdin"),
        );
        let client = ().serve(pipes).await.expect("an MCP session with tmux-mcp");
        Session {
            client,
            _server: server,
        }
    }

    async fn call(&self, tool: &str, arguments: Map<String, Value>) -> String {
        let request = CallToolRequestParam {
            name: tool.to_string().into(),
            arguments: Some(arguments),
            task: None,
        };
        match self.client.call_tool(request).await {
            Ok(result) => {
                let texts: Vec<&str> = result
                    .content
                    .iter()
                    .filter_map(|c| c.as_text())
                    .map(|t| t.text.as_str())
                    .collect();
                texts.join("\n")
            }
            Err(rmcp::ServiceError::McpError(error)) => error.message.to_string(),
            Err(error) => panic!("{tool} never answered: {error}"),
        }
    }
}

impl Shell {
    /// The tool arguments that name this pane.
    fn target(&self) -> Map<String, Value> {
        Map::from_iter([("pane_id".to_string(), json!(self.pane))])
    }

    fn screen(&self) -> String {
        let screen = run(tmux(&self.socket).args(["capture-pane", "-p", "-t", &self.pane]));
        screen
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl PaneTable {
    /// The row whose ID column names `shell`'s pane, `*`-marked or not.
    fn find(&self, shell: &Shell) -> Option<&str> {
        self.text.lines().find(|line| {
            line.split_whitespace()
                .next()
                .map(|id| id.trim_end_matches('*'))
                == Some(&shell.pane)
        })
    }
}

impl Read {
    fn set(mut self, name: &str, value: impl Into<Value>) -> Read {
        self.0.insert(name.to_string(), value.into());
        self
    }
}

/// A tmux client of the harness's server. Every tmux call is built here, so
/// the isolation cannot be forgotten at one call site.
fn tmux(socket: &Path) -> Command {
    let mut cmd = Command::new("tmux");
    cmd.arg("-S")
        .arg(socket)
        .arg("-f")
        .arg("/dev/null")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE");
    cmd
}

/// One `policy-eval --json` row's decision, and the id its first reason
/// starts with ("" for none).
fn judgement(row: &Value) -> (Decision, String) {
    let decision = match row["decision"].as_str() {
        Some("allow") => Decision::Allow,
        Some("passthrough") => Decision::Pass,
        Some("ask") => Decision::Ask,
        Some("deny") => Decision::Deny,
        other => panic!("policy-eval decided {other:?}"),
    };
    let reason = row["reasons"][0].as_str().unwrap_or("");
    let rule = reason.split([':', ' ']).next().unwrap_or("");
    (decision, rule.to_string())
}

/// The tmux-mcp binary in `env`, with the developer's `TMUX` and `TMUX_PANE`
/// gone.
fn tmux_mcp(env: &[(&'static str, String)]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tmux-mcp"));
    cmd.env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .envs(env.iter().map(|(name, value)| (name, value)));
    cmd
}

fn run(cmd: &mut Command) -> String {
    let output = cmd.output().expect("tmux");
    assert!(
        output.status.success(),
        "{cmd:?} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The `Shell`'s `.bashrc`: the product's own OSC 133 lines and a prompt a
/// test can find. `PS1` comes first, since the lines extend it. `set -b`
/// reports a finished job when it ends rather than at the next prompt, so a
/// test can wait for its "Done".
fn bashrc() -> String {
    let mut rc = String::from("PS1='$ '\nset -b\n");
    for line in OSC133_INJECT_LINES {
        rc.push_str(line);
        rc.push('\n');
    }
    rc
}
