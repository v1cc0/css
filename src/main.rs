use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{net::UnixListener, process::CommandExt},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Local};
use clap::{Parser, Subcommand};
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};

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
    /// Resume a session by session id, pid, or newest matching cwd.
    Continue {
        target: String,
        #[arg(last = true)]
        prompt: Vec<String>,
        /// Print the command instead of replacing this process.
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
    inferred: Option<Thread>,
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
    Resolve {
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
    Error {
        message: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let codex_home = cli.codex_home.unwrap_or_else(default_codex_home);

    match cli.command {
        Cmd::List { json } => {
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
                println!("{}", shell_words(&argv));
            } else {
                exec_resume(argv)?;
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

        out.push(RunningSession {
            pid,
            ppid,
            process_cwd,
            cmdline,
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

fn infer_thread_for_cwd<'a>(threads: &'a [Thread], cwd: &str) -> Option<&'a Thread> {
    threads.iter().find(|t| t.cwd == cwd)
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
        "{:<8} {:<8} {:<36} {:<20} CWD",
        "PID", "PPID", "SESSION", "UPDATED"
    );
    for s in sessions {
        if let Some(t) = &s.inferred {
            println!(
                "{:<8} {:<8} {:<36} {:<20} {}",
                s.pid,
                s.ppid,
                t.id,
                format_time(t.updated_at_ms.unwrap_or(t.updated_at * 1000)),
                t.cwd
            );
            println!("         title: {}", t.title);
        } else {
            println!(
                "{:<8} {:<8} {:<36} {:<20} {}",
                s.pid, s.ppid, "<unmatched>", "-", s.process_cwd
            );
        }
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

fn exec_resume(argv: Vec<String>) -> Result<()> {
    let program = argv
        .first()
        .ok_or_else(|| anyhow!("empty argv"))?
        .to_string();
    let err = Command::new(program).args(&argv[1..]).exec();
    Err(anyhow!(err).context("exec codex resume"))
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
            Ok(Request::List) => discover_running_sessions(codex_home)
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

fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "@%_+=:,./-".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}
