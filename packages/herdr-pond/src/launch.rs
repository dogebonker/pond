//! The desk's write legs: resume a stored session into its client's own
//! directory and start the client on it in a new tab, and park a live pane.
//! Design: `docs/plans/2610-05-herdr-pond-v2-resume-fork-handoff-park.md`.
//!
//! Both run detached from whatever spawned them: `pond resume` and `agent
//! start` take seconds, and neither the closed desk nor an action's herdr
//! command slot may wait on them. Failures go to `launch.log` and a toast.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, bail};
use serde::Deserialize;

use crate::config::{log_line, log_stdio};
use crate::herdr::{self, Herdr};
use crate::hook;

const LAUNCH_LOG: &str = "launch.log";

/// A client the desk can start on a resumed session. `{id}` is the resumed
/// session's id and `{path}` its first written file.
pub(crate) struct Client {
    pub adapter: &'static str,
    /// herdr's `agent start --kind`.
    kind: &'static str,
    resume: &'static [&'static str],
    fork: Option<&'static [&'static str]>,
}

/// The clients the desk knows how to start. Whether pond can resume into one
/// is pond's answer (`no_native_dir`), never this table's.
pub(crate) const CLIENTS: &[Client] = &[
    Client {
        adapter: "claude-code",
        kind: "claude",
        resume: &["--resume", "{id}"],
        fork: Some(&["--resume", "{id}", "--fork-session"]),
    },
    Client {
        adapter: "codex-cli",
        kind: "codex",
        resume: &["resume", "{id}"],
        fork: Some(&["fork", "{id}"]),
    },
    Client {
        adapter: "pi-coding-agent",
        kind: "pi",
        resume: &["--session", "{path}"],
        fork: Some(&["--fork", "{path}"]),
    },
];

/// The client for a session's `source_agent`; a subagent (`claude-code/x`)
/// belongs to its root client.
pub(crate) fn client(source_agent: &str) -> Option<&'static Client> {
    let root = source_agent.split('/').next().unwrap_or(source_agent);
    CLIENTS.iter().find(|client| client.adapter == root)
}

impl Client {
    pub(crate) fn can_fork(&self) -> bool {
        self.fork.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Launch {
    pub session_id: String,
    /// The client to resume into: the session's own, or a hand-off target.
    pub adapter: String,
    pub fork: bool,
}

impl Launch {
    fn to_args(&self) -> Vec<String> {
        let mut args = vec![
            "launch".to_owned(),
            self.session_id.clone(),
            self.adapter.clone(),
        ];
        if self.fork {
            args.push("--fork".to_owned());
        }
        args
    }

    fn from_args(args: &[String]) -> anyhow::Result<Self> {
        let (session_id, adapter, fork) = match args {
            [id, adapter] => (id, adapter, false),
            [id, adapter, flag] if flag == "--fork" => (id, adapter, true),
            _ => bail!(crate::USAGE),
        };
        Ok(Self {
            session_id: session_id.clone(),
            adapter: adapter.clone(),
            fork,
        })
    }
}

/// Called by the desk after it restored the terminal: hands the launch to a
/// detached `herdr-pond launch`, so the overlay closes at once.
pub(crate) fn spawn(launch: &Launch) -> anyhow::Result<()> {
    let log = herdr::state_dir()?.join(LAUNCH_LOG);
    let mut command = Command::new(std::env::current_exe()?);
    command.args(launch.to_args());
    herdr::spawn_detached(command, &log)
}

/// `herdr-pond launch <session-id> <adapter> [--fork]`.
pub(crate) fn run(args: &[String]) -> anyhow::Result<()> {
    herdr::detach();
    let launch = Launch::from_args(args)?;
    let state_dir = herdr::state_dir()?;
    let log = state_dir.join(LAUNCH_LOG);
    let Some(pond) = herdr::resolve_pond_or_toast(&herdr::config_dir()?, &state_dir, &log) else {
        return Ok(());
    };
    let herdr = Herdr::from_env();
    let workspace = std::env::var("HERDR_WORKSPACE_ID").ok();
    let fallback = herdr::context_project()
        .or_else(|| std::env::var("HOME").ok())
        .unwrap_or_else(|| "/".to_owned());
    if let Err(error) = launch_with(&launch, &pond, &herdr, workspace.as_deref(), &fallback) {
        log_line(&log, &format!("launch {}: {error:#}", launch.session_id));
        let _ = herdr.notify("pond: could not resume", &format!("{error:#}"));
    }
    Ok(())
}

fn launch_with(
    launch: &Launch,
    pond: &Path,
    herdr: &Herdr,
    workspace: Option<&str>,
    fallback_cwd: &str,
) -> anyhow::Result<()> {
    let client = client(&launch.adapter)
        .with_context(|| format!("the desk cannot start {} sessions", launch.adapter))?;
    let template = if launch.fork {
        client
            .fork
            .with_context(|| format!("{} has no native fork", client.adapter))?
    } else {
        client.resume
    };
    refuse_flag_shaped(&launch.session_id)?;
    let resumed = resume(pond, &launch.session_id, client.adapter)?;
    refuse_flag_shaped(&resumed.session_id)?;
    refuse_flag_shaped(&resumed.path)?;
    let args: Vec<String> = template
        .iter()
        .map(|arg| {
            arg.replace("{id}", &resumed.session_id)
                .replace("{path}", &resumed.path)
        })
        .collect();
    // A project from another machine has no directory here; the client is
    // then started where the desk was opened.
    let cwd = resumed
        .project
        .filter(|project| Path::new(project).is_dir())
        .unwrap_or_else(|| fallback_cwd.to_owned());
    let verb = if launch.fork { "fork" } else { "resume" };
    let short_id: String = launch.session_id.chars().take(8).collect();
    let label = format!("{} {verb} {short_id}", client.kind);
    let pane = herdr.tab_create(workspace, &cwd, &label)?;
    herdr.agent_start(
        &agent_name(client.kind, verb, &short_id),
        client.kind,
        &pane,
        &args,
    )
}

/// herdr agent names are `[a-z][a-z0-9_-]{0,31}`.
fn agent_name(kind: &str, verb: &str, short_id: &str) -> String {
    let id: String = short_id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    format!("{kind}-{verb}-{id}")
}

/// Session ids come from source files, possibly another machine's, and land
/// on a client's argv: one starting with `-` would parse as a flag (claude's
/// `--resume` takes an optional value).
fn refuse_flag_shaped(value: &str) -> anyhow::Result<()> {
    if value.starts_with('-') {
        bail!("refusing to pass {value:?} to a client: it would parse as a flag");
    }
    Ok(())
}

struct Resumed {
    session_id: String,
    path: String,
    project: Option<String>,
}

/// `pond resume --out-dir native`. Exit 3 ("already resumed") is the common
/// case for a session whose client file still exists, and launches the same.
fn resume(pond: &Path, session_id: &str, adapter: &str) -> anyhow::Result<Resumed> {
    #[derive(Deserialize)]
    struct Doc {
        project: Option<String>,
        #[serde(default)]
        sessions: Vec<Written>,
        #[serde(default)]
        existing: Vec<String>,
        error: Option<String>,
        reason: Option<String>,
        detail: Option<String>,
    }
    #[derive(Deserialize)]
    struct Written {
        session_id: String,
        files: Vec<String>,
    }
    let output = Command::new(pond)
        .args([
            "resume",
            session_id,
            "--to",
            adapter,
            "--out-dir",
            "native",
            "--format",
            "json",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("running {}", pond.display()))?;
    let doc: Doc = serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "pond resume printed no JSON ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
    })?;
    let first = |files: &[String]| {
        files
            .first()
            .cloned()
            .context("pond resume reported no file")
    };
    match output.status.code() {
        Some(0) => {
            let written = doc
                .sessions
                .into_iter()
                .next()
                .context("pond resume reported no session")?;
            Ok(Resumed {
                path: first(&written.files)?,
                session_id: written.session_id,
                project: doc.project,
            })
        }
        Some(3) => Ok(Resumed {
            path: first(&doc.existing)?,
            session_id: session_id.to_owned(),
            project: doc.project,
        }),
        _ => {
            let error = doc.error.unwrap_or_else(|| output.status.to_string());
            match doc.reason.or(doc.detail) {
                Some(why) => bail!("pond resume: {error}: {why}"),
                None => bail!("pond resume: {error}"),
            }
        }
    }
}

/// The `park` action: store the focused agent's session, then close its pane.
/// pond is the registry, so a parked session is just an idle row in the desk.
pub(crate) fn park() -> anyhow::Result<()> {
    let herdr = Herdr::from_env();
    let pane_id = herdr::context_pane().context("park needs a focused pane")?;
    let workspace = std::env::var("HERDR_WORKSPACE_ID").ok();
    let pane = herdr
        .pane_list(workspace.as_deref())?
        .into_iter()
        .find(|pane| pane.pane_id == pane_id)
        .with_context(|| format!("pane {pane_id} is gone"))?;
    let adapter = match park_check(&pane) {
        Ok(adapter) => adapter,
        Err(refusal) => return herdr.notify("pond: not parked", &refusal),
    };
    let log = herdr::state_dir()?.join(LAUNCH_LOG);
    let mut command = Command::new(std::env::current_exe()?);
    command.args(["park", "--worker", &pane_id, adapter]);
    herdr::spawn_detached(command, &log)
}

/// Parking a working agent would cut its turn off mid-write.
fn park_check(pane: &herdr::Pane) -> Result<&'static str, String> {
    let agent = pane
        .agent
        .as_deref()
        .ok_or("this pane runs no agent herdr recognizes")?;
    let adapter =
        hook::adapter_for(agent).ok_or_else(|| format!("pond does not read {agent} sessions"))?;
    if pane.agent_status.as_deref() == Some("working") {
        return Err(format!("{agent} is working - park it once it is idle"));
    }
    Ok(adapter)
}

/// `herdr-pond park --worker <pane> <adapter>`: the pane closes only once its
/// session is stored.
pub(crate) fn park_worker(args: &[String]) -> anyhow::Result<()> {
    let [pane_id, adapter] = args else {
        bail!(crate::USAGE);
    };
    herdr::detach();
    let state_dir = herdr::state_dir()?;
    let log = state_dir.join(LAUNCH_LOG);
    let Some(pond) = herdr::resolve_pond_or_toast(&herdr::config_dir()?, &state_dir, &log) else {
        return Ok(());
    };
    let herdr = Herdr::from_env();
    if let Err(error) = park_with(pane_id, adapter, &pond, &herdr, &log) {
        log_line(&log, &format!("park {pane_id}: {error:#}"));
        let _ = herdr.notify("pond: not parked", &format!("{error:#}"));
    }
    Ok(())
}

fn park_with(
    pane_id: &str,
    adapter: &str,
    pond: &Path,
    herdr: &Herdr,
    log: &Path,
) -> anyhow::Result<()> {
    let status = log_stdio(Command::new(pond).args(["sync", adapter, "-q"]), log)?
        .status()
        .with_context(|| format!("running {}", pond.display()))?;
    if !status.success() {
        bail!("pond sync {adapter} {status} - the pane stays open (see launch.log)");
    }
    herdr.pane_close(pane_id)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::fs;

    use super::*;
    use crate::fake_pond::{Sandbox, write_script};

    /// A fake herdr that records its argv and answers `tab create` with pane
    /// `wD:p9`.
    fn fake_herdr(sandbox: &Sandbox) -> Herdr {
        Herdr::new(write_script(
            &sandbox.path("bin/herdr"),
            &format!(
                r#"printf '%s\n' "$*" >> '{calls}'
case "$1 $2" in
  "tab create") echo '{{"result":{{"type":"tab_created","root_pane":{{"pane_id":"wD:p9"}}}}}}' ;;
  *) echo '{{"result":{{}}}}' ;;
esac"#,
                calls = sandbox.path("herdr-calls").display(),
            ),
        ))
    }

    /// A fake `pond` that records its argv, prints `stdout` and exits `code`.
    fn fake_pond(sandbox: &Sandbox, stdout: &str, code: i32) -> std::path::PathBuf {
        fs::write(sandbox.path("pond-out.json"), stdout).unwrap();
        write_script(
            &sandbox.path("bin/pond"),
            &format!(
                "printf '%s\\n' \"$*\" >> '{calls}'\ncat '{out}'\nexit {code}",
                calls = sandbox.path("pond-calls").display(),
                out = sandbox.path("pond-out.json").display(),
            ),
        )
    }

    fn launch(session_id: &str, adapter: &str, fork: bool) -> Launch {
        Launch {
            session_id: session_id.to_owned(),
            adapter: adapter.to_owned(),
            fork,
        }
    }

    #[test]
    fn launch_args_round_trip() {
        for request in [
            launch("s1", "codex-cli", false),
            launch("s1", "claude-code", true),
        ] {
            assert_eq!(Launch::from_args(&request.to_args()[1..]).unwrap(), request);
        }
        assert!(Launch::from_args(&["s1".to_owned()]).is_err());
    }

    #[test]
    fn subagents_belong_to_their_root_client() {
        assert_eq!(
            client("claude-code/general-purpose").unwrap().adapter,
            "claude-code"
        );
        assert!(client("openclaw").is_none());
    }

    #[test]
    fn a_resume_opens_a_tab_in_the_project_and_starts_the_client() {
        let sandbox = Sandbox::new();
        let project = sandbox.path("proj");
        fs::create_dir_all(&project).unwrap();
        let pond = fake_pond(
            &sandbox,
            &format!(
                r#"{{"project":"{}","sessions":[{{"session_id":"abc12345-x","files":["/c/p/abc12345-x.jsonl"]}}]}}"#,
                project.display()
            ),
            0,
        );
        launch_with(
            &launch("abc12345-x", "claude-code", false),
            &pond,
            &fake_herdr(&sandbox),
            Some("wD"),
            "/fallback",
        )
        .unwrap();
        assert_eq!(
            sandbox.lines("pond-calls"),
            ["resume abc12345-x --to claude-code --out-dir native --format json"]
        );
        assert_eq!(
            sandbox.lines("herdr-calls"),
            [
                format!(
                    "tab create --cwd {} --label claude resume abc12345 --focus --workspace wD",
                    project.display()
                ),
                "agent start claude-resume-abc12345 --kind claude --pane wD:p9 -- --resume abc12345-x"
                    .to_owned(),
            ]
        );
    }

    #[test]
    fn already_resumed_launches_on_the_existing_file() {
        let sandbox = Sandbox::new();
        let pond = fake_pond(
            &sandbox,
            r#"{"error":"already_exists","project":"/nowhere/here","existing":["/pi/sessions/s/f.jsonl"]}"#,
            3,
        );
        launch_with(
            &launch("s1", "pi-coding-agent", true),
            &pond,
            &fake_herdr(&sandbox),
            None,
            "/fallback",
        )
        .unwrap();
        let calls = sandbox.lines("herdr-calls");
        assert!(
            calls[0].starts_with("tab create --cwd /fallback "),
            "{calls:?}"
        );
        assert!(
            calls[1]
                == "agent start pi-fork-s1 --kind pi --pane wD:p9 -- --fork /pi/sessions/s/f.jsonl",
            "{calls:?}"
        );
    }

    #[test]
    fn a_failed_resume_names_pond_s_reason_and_opens_nothing() {
        let sandbox = Sandbox::new();
        let pond = fake_pond(
            &sandbox,
            r#"{"error":"no_native_dir","adapter":"codex-cli"}"#,
            2,
        );
        let error = launch_with(
            &launch("s1", "codex-cli", false),
            &pond,
            &fake_herdr(&sandbox),
            None,
            "/fallback",
        )
        .unwrap_err();
        assert!(error.to_string().contains("no_native_dir"), "{error}");
        assert!(sandbox.lines("herdr-calls").is_empty());
    }

    #[test]
    fn an_unknown_client_is_refused_before_pond_runs() {
        let sandbox = Sandbox::new();
        let pond = fake_pond(&sandbox, "{}", 0);
        let error = launch_with(
            &launch("s1", "openclaw", false),
            &pond,
            &fake_herdr(&sandbox),
            None,
            "/",
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("cannot start openclaw"),
            "{error}"
        );
        assert!(sandbox.lines("pond-calls").is_empty());
    }

    #[test]
    fn a_flag_shaped_session_id_never_reaches_a_client() {
        let sandbox = Sandbox::new();
        let pond = fake_pond(&sandbox, "{}", 0);
        let herdr = fake_herdr(&sandbox);
        let flag = "--dangerously-skip-permissions";
        let error = launch_with(
            &launch(flag, "claude-code", false),
            &pond,
            &herdr,
            None,
            "/",
        )
        .unwrap_err();
        assert!(error.to_string().contains("parse as a flag"), "{error}");
        assert!(sandbox.lines("pond-calls").is_empty());

        let pond = fake_pond(
            &sandbox,
            &format!(r#"{{"sessions":[{{"session_id":"{flag}","files":["/c/x.jsonl"]}}]}}"#),
            0,
        );
        assert!(
            launch_with(
                &launch("s1", "claude-code", false),
                &pond,
                &herdr,
                None,
                "/"
            )
            .is_err()
        );
        assert!(sandbox.lines("herdr-calls").is_empty());
    }

    fn pane(agent: Option<&str>, status: &str) -> herdr::Pane {
        serde_json::from_value(serde_json::json!({
            "pane_id": "wD:p1",
            "agent": agent,
            "agent_status": status,
        }))
        .unwrap()
    }

    #[test]
    fn park_refuses_a_working_or_unknown_agent() {
        assert_eq!(park_check(&pane(Some("claude"), "idle")), Ok("claude-code"));
        assert!(park_check(&pane(Some("claude"), "working")).is_err());
        assert!(park_check(&pane(Some("unknown-agent"), "idle")).is_err());
        assert!(park_check(&pane(None, "idle")).is_err());
    }

    #[test]
    fn park_closes_the_pane_only_after_a_good_sync() {
        for (code, closed) in [(0, true), (1, false)] {
            let sandbox = Sandbox::new();
            let pond = fake_pond(&sandbox, "", code);
            let result = park_with(
                "wD:p1",
                "codex-cli",
                &pond,
                &fake_herdr(&sandbox),
                &sandbox.path("launch.log"),
            );
            assert_eq!(result.is_ok(), closed);
            assert_eq!(sandbox.lines("pond-calls"), ["sync codex-cli -q"]);
            assert_eq!(
                sandbox.lines("herdr-calls") == ["pane close wD:p1"],
                closed,
                "code {code}"
            );
        }
    }
}
