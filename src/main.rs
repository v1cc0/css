use std::{
    ffi::OsString,
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    os::unix::{net::UnixListener, process::CommandExt},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Local};
use clap::{Parser, Subcommand};
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Parser, Debug)]
#[command(
    name = "codexdaemon",
    version,
    about = "Discover and resume current user's Codex CLI sessions"
)]
struct Cli {
    #[arg(long, env = "CODEX_HOME", value_name = "DIR")]
    codex_home: Option<PathBuf>,

    #[arg(long, default_value = "codex", value_name = "BIN")]
    codex_bin: String,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// List running Codex processes and infer their session metadata.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Same as list; kept as the user-facing verb for remote checks.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Resume a session by session id, pid, or newest matching cwd.
    Continue {
        target: String,
        #[arg(last = true)]
        prompt: Vec<String>,
        /// Print the command instead of replacing this process.
        #[arg(long)]
        dry_run: bool,
    },
    /// Kill the original Codex process, then resume the session in this terminal.
    Takeover {
        target: String,
        #[arg(last = true)]
        prompt: Vec<String>,
        /// Print actions instead of killing/executing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Run a tiny local Unix-socket daemon for list/resolve requests.
    Serve {
        #[arg(long, value_name = "PATH")]
        socket: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Serialize)]
struct RunningSession {
    pid: u32,
    ppid: u32,
    process_cwd: String,
    cmdline: String,
    status: SessionStatus,
    status_detail: String,
    last_event_at_ms: Option<i64>,
    inferred: Option<Thread>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum SessionStatus {
    Working,
    WaitingUserInput,
    Malfunction,
}

#[derive(Debug, Clone, Serialize)]
struct Thread {
    id: String,
    rollout_path: String,
    cwd: String,
    title: String,
    updated_at: i64,
    updated_at_ms: Option<i64>,
    model: Option<String>,
    model_provider: String,
    sandbox_policy: String,
    approval_mode: String,
    cli_version: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Request {
    List,
    Status,
    Resolve {
        target: String,
        prompt: Option<String>,
    },
    Takeover {
        target: String,
        prompt: Option<String>,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Response {
    Sessions {
        sessions: Vec<RunningSession>,
    },
    ResumeCommand {
        argv: Vec<String>,
        session: Box<Thread>,
    },
    TakeoverCommand {
        pid: u32,
        argv: Vec<String>,
        session: Box<Thread>,
    },
    Error {
        message: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let codex_home = cli.codex_home.unwrap_or_else(default_codex_home);

    match cli.command {
        Cmd::List { json } | Cmd::Status { json } => {
            let sessions = discover_running_sessions(&codex_home)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&sessions)?);
            } else {
                print_sessions(&sessions);
            }
        }
        Cmd::Continue {
            target,
            prompt,
            dry_run,
        } => {
            let prompt = join_prompt(prompt);
            let thread = resolve_target(&codex_home, &target)?;
            let argv = resume_argv(&cli.codex_bin, &thread, prompt.as_deref());
            if dry_run {
                println!("{}", resume_shell_command(&argv, &thread.cwd));
            } else {
                exec_resume(argv, &thread.cwd)?;
            }
        }
        Cmd::Takeover {
            target,
            prompt,
            dry_run,
        } => {
            let prompt = join_prompt(prompt);
            let (pid, thread) = resolve_running_target(&codex_home, &target)?;
            let argv = resume_argv(&cli.codex_bin, &thread, prompt.as_deref());
            if dry_run {
                println!("kill {}", pid);
                println!("{}", resume_shell_command(&argv, &thread.cwd));
            } else {
                validate_resume_cwd(&thread.cwd)?;
                terminate_process(pid)?;
                exec_resume(argv, &thread.cwd)?;
            }
        }
        Cmd::Serve { socket } => serve(&codex_home, &cli.codex_bin, socket)?,
    }

    Ok(())
}

fn default_codex_home() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
}

fn state_db(codex_home: &Path) -> PathBuf {
    codex_home.join("state_5.sqlite")
}

fn open_state(codex_home: &Path) -> Result<Connection> {
    Connection::open_with_flags(state_db(codex_home), OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("open Codex state database")
}

fn discover_running_sessions(codex_home: &Path) -> Result<Vec<RunningSession>> {
    let threads = load_recent_threads(codex_home, 500)?;
    let mut out = Vec::new();

    for entry in fs::read_dir("/proc").context("read /proc")? {
        let entry = entry?;
        let Some(pid) = entry.file_name().to_string_lossy().parse::<u32>().ok() else {
            continue;
        };
        let proc_dir = entry.path();
        let comm = fs::read_to_string(proc_dir.join("comm")).unwrap_or_default();
        if comm.trim() != "codex" {
            continue;
        }

        let cmdline = read_cmdline(&proc_dir.join("cmdline"));
        if !cmdline.starts_with("codex") && !cmdline.contains("/codex") {
            continue;
        }

        let process_cwd = fs::read_link(proc_dir.join("cwd"))
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "<unknown>".to_string());
        let ppid = read_ppid(&proc_dir.join("status")).unwrap_or_default();
        let inferred = infer_thread_for_cwd(&threads, &process_cwd).cloned();
        let (status, status_detail, last_event_at_ms) =
            inferred.as_ref().map(assess_thread_status).unwrap_or((
                SessionStatus::Malfunction,
                "running codex process could not be matched to a saved thread".to_string(),
                None,
            ));

        out.push(RunningSession {
            pid,
            ppid,
            process_cwd,
            cmdline,
            status,
            status_detail,
            last_event_at_ms,
            inferred,
        });
    }

    out.sort_by_key(|s| s.pid);
    Ok(out)
}

fn load_recent_threads(codex_home: &Path, limit: usize) -> Result<Vec<Thread>> {
    let conn = open_state(codex_home)?;
    let mut stmt = conn.prepare(
        r#"
        SELECT id, rollout_path, cwd, title, updated_at, updated_at_ms, model,
               model_provider, sandbox_policy, approval_mode, cli_version
        FROM threads
        WHERE archived = 0 AND source = 'cli'
        ORDER BY COALESCE(updated_at_ms, updated_at * 1000) DESC
        LIMIT ?1
        "#,
    )?;

    let rows = stmt.query_map(params![limit as i64], |row| {
        Ok(Thread {
            id: row.get(0)?,
            rollout_path: row.get(1)?,
            cwd: row.get(2)?,
            title: row.get(3)?,
            updated_at: row.get(4)?,
            updated_at_ms: row.get(5)?,
            model: row.get(6)?,
            model_provider: row.get(7)?,
            sandbox_policy: row.get(8)?,
            approval_mode: row.get(9)?,
            cli_version: row.get(10)?,
        })
    })?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn resolve_target(codex_home: &Path, target: &str) -> Result<Thread> {
    if let Ok(pid) = target.parse::<u32>() {
        let cwd = fs::read_link(format!("/proc/{pid}/cwd"))
            .with_context(|| format!("read cwd for pid {pid}"))?
            .display()
            .to_string();
        let threads = load_recent_threads(codex_home, 500)?;
        return infer_thread_for_cwd(&threads, &cwd)
            .cloned()
            .ok_or_else(|| anyhow!("no Codex thread found for pid {pid} cwd {cwd}"));
    }

    if target == ":cwd" {
        let cwd = std::env::current_dir()?.display().to_string();
        let threads = load_recent_threads(codex_home, 500)?;
        return infer_thread_for_cwd(&threads, &cwd)
            .cloned()
            .ok_or_else(|| anyhow!("no Codex thread found for current cwd {cwd}"));
    }

    let conn = open_state(codex_home)?;
    let mut stmt = conn.prepare(
        r#"
        SELECT id, rollout_path, cwd, title, updated_at, updated_at_ms, model,
               model_provider, sandbox_policy, approval_mode, cli_version
        FROM threads
        WHERE archived = 0 AND (id = ?1 OR title = ?1)
        ORDER BY COALESCE(updated_at_ms, updated_at * 1000) DESC
        LIMIT 1
        "#,
    )?;

    stmt.query_row(params![target], |row| {
        Ok(Thread {
            id: row.get(0)?,
            rollout_path: row.get(1)?,
            cwd: row.get(2)?,
            title: row.get(3)?,
            updated_at: row.get(4)?,
            updated_at_ms: row.get(5)?,
            model: row.get(6)?,
            model_provider: row.get(7)?,
            sandbox_policy: row.get(8)?,
            approval_mode: row.get(9)?,
            cli_version: row.get(10)?,
        })
    })
    .with_context(|| format!("resolve Codex session {target}"))
}

fn resolve_running_target(codex_home: &Path, target: &str) -> Result<(u32, Thread)> {
    let sessions = discover_running_sessions(codex_home)?;

    if let Ok(pid) = target.parse::<u32>() {
        let session = sessions
            .into_iter()
            .find(|s| s.pid == pid)
            .ok_or_else(|| anyhow!("pid {pid} is not a running Codex process"))?;
        let thread = session
            .inferred
            .ok_or_else(|| anyhow!("pid {pid} is not matched to a Codex session"))?;
        return Ok((pid, thread));
    }

    if target == ":cwd" {
        let cwd = std::env::current_dir()?.display().to_string();
        let session = sessions
            .into_iter()
            .find(|s| s.inferred.as_ref().is_some_and(|t| t.cwd == cwd))
            .ok_or_else(|| anyhow!("no running Codex session found for current cwd {cwd}"))?;
        let pid = session.pid;
        let thread = session.inferred.expect("checked above");
        return Ok((pid, thread));
    }

    let session = sessions
        .into_iter()
        .find(|s| {
            s.inferred
                .as_ref()
                .is_some_and(|t| t.id == target || t.title == target)
        })
        .ok_or_else(|| anyhow!("no running Codex session matched {target}"))?;
    let pid = session.pid;
    let thread = session.inferred.expect("checked above");
    Ok((pid, thread))
}

fn infer_thread_for_cwd<'a>(threads: &'a [Thread], cwd: &str) -> Option<&'a Thread> {
    threads.iter().find(|t| t.cwd == cwd)
}

fn assess_thread_status(thread: &Thread) -> (SessionStatus, String, Option<i64>) {
    let Ok(events) = read_recent_events(Path::new(&thread.rollout_path)) else {
        return (
            SessionStatus::Malfunction,
            "cannot read rollout jsonl".to_string(),
            None,
        );
    };

    let mut active_turn: Option<String> = None;
    let mut last_payload_type: Option<String> = None;
    let mut last_item_name: Option<String> = None;
    let mut last_event_at_ms = None;

    for event in events {
        last_event_at_ms = event.timestamp_ms.or(last_event_at_ms);
        if let Some(payload_type) = event.payload_type.as_deref() {
            last_payload_type = Some(payload_type.to_string());
            match payload_type {
                "task_started" => active_turn = event.turn_id,
                "task_complete" => active_turn = None,
                _ => {}
            }
        }
        if let Some(item_name) = event.item_name {
            last_item_name = Some(item_name);
        }
    }

    if active_turn.is_some() {
        let age_ms = last_event_at_ms
            .and_then(|ts| now_ms().checked_sub(ts))
            .unwrap_or_default();
        if age_ms > 30 * 60 * 1000 {
            return (
                SessionStatus::Malfunction,
                format!(
                    "active turn has had no rollout event for {}s",
                    age_ms / 1000
                ),
                last_event_at_ms,
            );
        }

        let detail = match (last_payload_type.as_deref(), last_item_name.as_deref()) {
            (Some("token_count"), _) | (Some("task_started"), _) | (Some("user_message"), _) => {
                "working: likely waiting on model/api response"
            }
            (_, Some("function_call")) => "working: model requested a tool call",
            (_, Some("function_call_output")) => {
                "working: tool output recorded, likely returning to model/api"
            }
            (_, Some("reasoning")) => "working: model/api response in progress",
            (_, Some("message")) => "working: assistant message is being recorded",
            _ => "working: active turn is not complete",
        };
        return (SessionStatus::Working, detail.to_string(), last_event_at_ms);
    }

    match last_payload_type.as_deref() {
        Some("task_complete") | Some("agent_message") => (
            SessionStatus::WaitingUserInput,
            "last turn completed; Codex TUI is waiting for user input".to_string(),
            last_event_at_ms,
        ),
        Some(other) => (
            SessionStatus::WaitingUserInput,
            format!("no active turn; last event is {other}"),
            last_event_at_ms,
        ),
        None => (
            SessionStatus::Malfunction,
            "rollout has no readable events".to_string(),
            last_event_at_ms,
        ),
    }
}

#[derive(Debug)]
struct RecentEvent {
    timestamp_ms: Option<i64>,
    payload_type: Option<String>,
    turn_id: Option<String>,
    item_name: Option<String>,
}

fn read_recent_events(path: &Path) -> Result<Vec<RecentEvent>> {
    const MAX_TAIL: u64 = 512 * 1024;

    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(MAX_TAIL);
    file.seek(SeekFrom::Start(start))?;

    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    if start > 0
        && let Some(pos) = buf.find('\n')
    {
        buf = buf[pos + 1..].to_string();
    }

    let mut out = Vec::new();
    for line in buf.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let timestamp_ms = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp_ms);
        let payload = value.get("payload").unwrap_or(&Value::Null);
        let payload_type = payload
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string);
        let turn_id = payload
            .get("turn_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let item_name = if value.get("type").and_then(Value::as_str) == Some("response_item") {
            payload_type.clone()
        } else {
            None
        };

        out.push(RecentEvent {
            timestamp_ms,
            payload_type,
            turn_id,
            item_name,
        });
    }

    Ok(out)
}

fn parse_timestamp_ms(raw: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

fn read_cmdline(path: &Path) -> String {
    fs::read(path)
        .map(|bytes| {
            bytes
                .split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

fn read_ppid(path: &Path) -> Option<u32> {
    let status = fs::read_to_string(path).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:")?.trim().parse().ok())
}

fn print_sessions(sessions: &[RunningSession]) {
    if sessions.is_empty() {
        println!("No running Codex CLI processes found.");
        return;
    }

    println!(
        "{:<8} {:<8} {:<18} {:<36} {:<20} CWD",
        "PID", "PPID", "STATUS", "SESSION", "UPDATED"
    );
    for s in sessions {
        if let Some(t) = &s.inferred {
            println!(
                "{:<8} {:<8} {:<18} {:<36} {:<20} {}",
                s.pid,
                s.ppid,
                status_label(s.status),
                t.id,
                format_time(t.updated_at_ms.unwrap_or(t.updated_at * 1000)),
                t.cwd
            );
            println!("         status: {}", s.status_detail);
            println!("         title: {}", t.title);
        } else {
            println!(
                "{:<8} {:<8} {:<18} {:<36} {:<20} {}",
                s.pid,
                s.ppid,
                status_label(s.status),
                "<unmatched>",
                "-",
                s.process_cwd
            );
            println!("         status: {}", s.status_detail);
        }
    }
}

fn status_label(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Working => "working",
        SessionStatus::WaitingUserInput => "waiting_user",
        SessionStatus::Malfunction => "malfunction",
    }
}

fn format_time(epoch_ms: i64) -> String {
    DateTime::from_timestamp_millis(epoch_ms)
        .map(|dt| {
            dt.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "<invalid>".to_string())
}

fn join_prompt(prompt: Vec<String>) -> Option<String> {
    let joined = prompt.join(" ");
    (!joined.is_empty()).then_some(joined)
}

fn resume_argv(codex_bin: &str, thread: &Thread, prompt: Option<&str>) -> Vec<String> {
    let mut argv = vec![
        codex_bin.to_string(),
        "resume".to_string(),
        thread.id.clone(),
        "--cd".to_string(),
        thread.cwd.clone(),
    ];
    if let Some(prompt) = prompt {
        argv.push(prompt.to_string());
    }
    argv
}

fn exec_resume(argv: Vec<String>, cwd: &str) -> Result<()> {
    validate_resume_cwd(cwd)?;
    let program = argv
        .first()
        .ok_or_else(|| anyhow!("empty argv"))?
        .to_string();
    let program = exec_program(&program)?;
    let err = Command::new(program)
        .args(&argv[1..])
        .current_dir(cwd)
        .env("PWD", cwd)
        .exec();
    Err(anyhow!(err).context(format!("exec codex resume from cwd {cwd}")))
}

fn validate_resume_cwd(cwd: &str) -> Result<()> {
    let metadata = fs::metadata(cwd).with_context(|| format!("stat resume cwd {cwd}"))?;
    if metadata.is_dir() {
        Ok(())
    } else {
        Err(anyhow!("resume cwd {cwd} is not a directory"))
    }
}

fn exec_program(program: &str) -> Result<OsString> {
    let path = Path::new(program);
    if path.is_relative() && program.contains('/') {
        Ok(std::env::current_dir()?.join(path).into_os_string())
    } else {
        Ok(OsString::from(program))
    }
}

fn terminate_process(pid: u32) -> Result<()> {
    send_signal(pid, libc::SIGTERM)?;
    for _ in 0..50 {
        if !Path::new(&format!("/proc/{pid}")).exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }

    send_signal(pid, libc::SIGKILL)?;
    for _ in 0..20 {
        if !Path::new(&format!("/proc/{pid}")).exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }

    Err(anyhow!("pid {pid} did not exit after SIGKILL"))
}

fn send_signal(pid: u32, signal: i32) -> Result<()> {
    let rc = unsafe { libc::kill(pid as libc::pid_t, signal) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
            .with_context(|| format!("send signal {signal} to pid {pid}"))
    }
}

fn serve(codex_home: &Path, codex_bin: &str, socket: Option<PathBuf>) -> Result<()> {
    let socket = socket.unwrap_or_else(default_socket_path);
    if socket.exists() {
        fs::remove_file(&socket)
            .with_context(|| format!("remove stale socket {}", socket.display()))?;
    }
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;
    println!("codexdaemon listening on {}", socket.display());

    for stream in listener.incoming() {
        let mut stream = stream?;
        let mut line = String::new();
        BufReader::new(stream.try_clone()?).read_line(&mut line)?;
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(Request::List | Request::Status) => discover_running_sessions(codex_home)
                .map(|sessions| Response::Sessions { sessions })
                .unwrap_or_else(|e| Response::Error {
                    message: e.to_string(),
                }),
            Ok(Request::Resolve { target, prompt }) => resolve_target(codex_home, &target)
                .map(|session| Response::ResumeCommand {
                    argv: resume_argv(codex_bin, &session, prompt.as_deref()),
                    session: Box::new(session),
                })
                .unwrap_or_else(|e| Response::Error {
                    message: e.to_string(),
                }),
            Ok(Request::Takeover { target, prompt }) => resolve_running_target(codex_home, &target)
                .map(|(pid, session)| Response::TakeoverCommand {
                    pid,
                    argv: resume_argv(codex_bin, &session, prompt.as_deref()),
                    session: Box::new(session),
                })
                .unwrap_or_else(|e| Response::Error {
                    message: e.to_string(),
                }),
            Err(e) => Response::Error {
                message: e.to_string(),
            },
        };
        writeln!(stream, "{}", serde_json::to_string(&response)?)?;
    }
    Ok(())
}

fn default_socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("codexdaemon.sock")
}

fn shell_words(argv: &[String]) -> String {
    argv.iter()
        .map(|s| shell_quote(s))
        .collect::<Vec<_>>()
        .join(" ")
}

fn resume_shell_command(argv: &[String], cwd: &str) -> String {
    format!("cd {} && {}", shell_quote(cwd), shell_words(argv))
}

fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "@%_+=:,./-".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread_with_cwd(cwd: &str) -> Thread {
        Thread {
            id: "00000000-0000-0000-0000-000000000000".to_string(),
            rollout_path: "/tmp/rollout.jsonl".to_string(),
            cwd: cwd.to_string(),
            title: "test".to_string(),
            updated_at: 0,
            updated_at_ms: None,
            model: None,
            model_provider: "openai".to_string(),
            sandbox_policy: "workspace-write".to_string(),
            approval_mode: "on-request".to_string(),
            cli_version: "0.0.0".to_string(),
        }
    }

    #[test]
    fn dry_run_resume_command_changes_directory_first() {
        let thread = thread_with_cwd("/home/vc/project with space");
        let argv = resume_argv("codex", &thread, Some("continue"));

        assert_eq!(
            resume_shell_command(&argv, &thread.cwd),
            "cd '/home/vc/project with space' && codex resume 00000000-0000-0000-0000-000000000000 --cd '/home/vc/project with space' continue"
        );
    }

    #[test]
    fn bare_program_still_uses_path_lookup_after_chdir() {
        assert_eq!(exec_program("codex").unwrap(), OsString::from("codex"));
    }

    #[test]
    fn relative_program_path_is_resolved_before_chdir() {
        let expected = std::env::current_dir()
            .unwrap()
            .join("target/debug/codex")
            .into_os_string();

        assert_eq!(exec_program("target/debug/codex").unwrap(), expected);
    }
}
