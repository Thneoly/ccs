// ccs — Claude Code Sessions: cross-project session finder + resumer.
//
// Claude Code stores every session as <id>.jsonl under
// ~/.claude/projects/<encoded-cwd>/, with the real cwd inside the records.
// `claude --continue` is cwd-scoped and the --resume picker assumes you
// already know the project; ccs closes that gap: it lists recent sessions
// across ALL projects (time / project / topic) and reopens one in its
// original directory.
//
// Two modes, the claude cli pattern:
//   a terminal with no keyword/subcommand and no --no-tui -> interactive
//   picker (arrows / type-to-filter / Enter resumes in the original dir)
//   every other shape (pipes, keyword, open/id, --no-tui) -> plain,
//   pipe-friendly listing
//
// Usage:
//   ccs                  picker (TUI) over the newest max(n,200) sessions
//   ccs -n 40            list 40 when non-interactive (piped, keyword,
//                        or --no-tui); on a terminal it only grows the
//                        picker pool past its 200-row floor
//   ccs <keyword>        full-text filter across the newest max(n,500)
//                        transcripts (path / topic / messages, any case)
//   ccs open <n>         resume session n from the last listing: spawn
//                        `claude --resume <id>` with the child's cwd set to
//                        the session's original project directory
//   ccs open <n> -m glm  same, routed through ccm: `ccm run glm -- --resume <id>`
//   ccs -m glm           picker with the ccm route preset (Enter routes)
//   ccs open <n> --print print a paste-ready command (PowerShell syntax)
//                        instead of running it
//   ccs id <n>           print "id<TAB>cwd" for scripting
//   ccs --no-tui         force the non-interactive listing on a terminal
//
// The listing cache (%TEMP%/ccs-list.txt) maps numbers to ids and is
// rewritten by every non-interactive listing (including empty ones);
// `open <n>` resolves against the machine-wide LAST listing — which only
// a NON-interactive listing refreshes (bare `ccs` on a terminal opens the
// picker and never rewrites it); refresh with `ccs --no-tui`.
// Reads only the transcripts directory; writes only the cache file.

mod picker;

use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::Command;
use std::time::UNIX_EPOCH;

use crossterm::tty::IsTty;

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let code = run(&args);
    std::process::exit(code);
}

/// println! panics on a closed pipe (Windows os error 232 / EPIPE) — a
/// listing tool must just stop quietly, so every stdout line goes through
/// here with the write error ignored.
fn out(line: &str) {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = writeln!(lock, "{line}");
}

fn run(args: &[String]) -> i32 {
    // ── argument parsing (hand-rolled; the dep count stays tiny) ──────────
    let mut n: usize = 20;
    let mut keyword: Option<String> = None;
    let mut model: Option<String> = None;
    let mut no_tui = false;
    let mut positional: Vec<String> = Vec::new();
    let mut sub_idx: Option<usize> = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "-n" | "--num" => {
                i += 1;
                match args
                    .get(i)
                    .and_then(|v| v.parse::<usize>().ok())
                    .filter(|v| *v > 0)
                {
                    Some(v) => n = v,
                    None => {
                        eprintln!("ccs: -n expects a positive number");
                        return 2;
                    }
                }
            }
            "-m" | "--model" => {
                i += 1;
                match args.get(i) {
                    Some(m) => model = Some(m.clone()),
                    None => {
                        eprintln!("ccs: -m expects a ccm target name");
                        return 2;
                    }
                }
            }
            "--no-tui" => no_tui = true,
            "-h" | "--help" | "help" => {
                print_usage();
                return 0;
            }
            _ => {
                if positional.is_empty() {
                    sub_idx = Some(i);
                }
                positional.push(a.clone());
            }
        }
        i += 1;
    }

    // subcommands: open / id consume the args after the sub word. The
    // words "open"/"id"/"help" are therefore not searchable keywords.
    if let (Some(sub), Some(idx)) = (positional.first().cloned(), sub_idx)
        && (sub == "open" || sub == "id")
    {
        return subcommand(&sub, &args[idx + 1..]);
    }
    if positional.len() > 1 {
        eprintln!("ccs: unexpected arguments (see `ccs help`)");
        return 2;
    }
    if let Some(k) = positional.first() {
        keyword = Some(k.clone());
    }

    // Dual-mode dispatch: the picker only ever starts on a real terminal,
    // with no keyword and no subcommand — every other shape (pipes, flags,
    // scripts) gets the plain listing.
    let interactive = keyword.is_none() && !no_tui && std::io::stdout().is_tty();

    // ── collect candidates, newest first ──────────────────────────────────
    let root = match sessions_root() {
        Some(p) => p,
        None => {
            eprintln!("ccs: cannot resolve the claude projects directory (no USERPROFILE/HOME)");
            return 1;
        }
    };
    if !root.is_dir() {
        eprintln!("ccs: no claude projects directory at {}", root.display());
        return 1;
    }

    let mut files: Vec<(PathBuf, u64)> = Vec::new();
    if let Ok(project_dirs) = fs::read_dir(&root) {
        for dir in project_dirs.flatten() {
            let dir_path = dir.path();
            if !dir_path.is_dir() {
                continue;
            }
            for f in fs::read_dir(&dir_path).into_iter().flatten() {
                let f = match f {
                    Ok(f) => f,
                    Err(_) => continue,
                };
                let path = f.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let secs = f
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                files.push((path, secs));
            }
        }
    }
    files.sort_by_key(|f| std::cmp::Reverse(f.1));

    // pool sizing: picker browses max(n,200); keyword mode full-text scans
    // the newest max(n,500); plain listing shows exactly n.
    let pool = if interactive {
        n.max(200)
    } else if keyword.is_some() {
        n.max(500)
    } else {
        n
    };
    let mut selected: Vec<&(PathBuf, u64)> = files.iter().take(pool).collect();
    if let Some(kw) = &keyword {
        let needle = kw.to_lowercase();
        selected.retain(|(path, _)| {
            let Ok(raw) = fs::read(path) else {
                return false;
            };
            // lossy decode: one bad byte must not hide a whole transcript
            String::from_utf8_lossy(&raw)
                .to_lowercase()
                .contains(&needle)
        });
    }
    // the picker browses its whole pool; non-interactive shapes show n
    let cap = if interactive { pool } else { n };
    let selected: Vec<&(PathBuf, u64)> = selected.into_iter().take(cap).collect();

    // ── per-session metadata from the transcript head ─────────────────────
    let mut entries: Vec<picker::PickerRow> = Vec::new();
    for (path, secs) in selected.iter() {
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        let info = head_info(path);
        let reliable = info.0.is_some();
        let cwd = info.0.unwrap_or_else(|| encoded_fallback(path));
        entries.push(picker::PickerRow {
            id,
            cwd,
            reliable,
            topic: info.1,
            time: format_mtime(*secs),
        });
    }

    // ── interactive picker ─────────────────────────────────────────────────
    if interactive {
        match picker::run(&entries, model.as_deref()) {
            picker::Pick::Chosen(idx) => {
                let row = &entries[idx];
                return open_entry(&row.id, &row.cwd, row.reliable, model.as_deref(), false);
            }
            picker::Pick::Aborted => return 0, // q/Esc — nothing launched
            picker::Pick::Failed => {}         // fall through to the plain listing
        }
    }

    // ── non-interactive listing ───────────────────────────────────────────
    // The cache is rewritten on EVERY listing — including empty ones, so a
    // zero-match run can never leave a stale numbering behind for `open`.
    let body: String = entries
        .iter()
        .enumerate()
        .map(|(idx, r)| {
            // control chars would corrupt the line-oriented cache
            let safe_cwd = r.cwd.replace(['\n', '\r'], " ");
            format!(
                "{}\t{}\t{}\t{}\n",
                idx + 1,
                r.id,
                safe_cwd,
                if r.reliable { 1 } else { 0 }
            )
        })
        .collect();
    let _ = fs::write(cache_path(), body);

    if entries.is_empty() {
        out("ccs: no sessions matched");
        return 0;
    }
    for (idx, r) in entries.iter().enumerate() {
        let mark = if r.reliable { " " } else { "~" };
        let proj = picker::truncate_chars(&r.cwd.replace('\\', "/"), 38);
        let topic = picker::truncate_chars(&picker::squash(&r.topic), 56);
        out(&format!(
            "{:>3} {} {}  {:<38}  {}",
            idx + 1,
            r.time,
            mark,
            proj,
            topic
        ));
    }
    out("");
    out("ccs open <n>          resume there (plain claude)");
    out("ccs open <n> -m glm   resume routed via ccm");
    0
}

fn subcommand(sub: &str, rest: &[String]) -> i32 {
    let mut number: Option<usize> = None;
    let mut model: Option<String> = None;
    let mut print_only = false;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "-m" | "--model" => {
                i += 1;
                match rest.get(i) {
                    Some(m) => model = Some(m.clone()),
                    None => {
                        eprintln!("ccs: -m expects a ccm target name");
                        return 2;
                    }
                }
            }
            "--print" => print_only = true,
            "-h" | "--help" => {
                print_usage();
                return 0;
            }
            v => match v.parse::<usize>() {
                Ok(v) => {
                    if number.is_some() {
                        eprintln!("ccs: two session numbers given (`{v}`) - exactly one expected");
                        return 2;
                    }
                    number = Some(v);
                }
                Err(_) => {
                    eprintln!("ccs: unrecognized argument `{v}`");
                    return 2;
                }
            },
        }
        i += 1;
    }
    let Some(number) = number else {
        eprintln!("ccs: `{sub}` needs a session number from the last listing");
        return 2;
    };

    let Some((id, cwd, reliable)) = lookup(number) else {
        eprintln!("ccs: no session #{number} in the last listing (refresh with `ccs --no-tui`)");
        return 1;
    };

    if sub == "id" {
        out(&format!("{}\t{}", id, cwd));
        return 0;
    }
    open_entry(&id, &cwd, reliable, model.as_deref(), print_only)
}

/// Validate one entry and resume it: spawn claude (or ccm run) with the
/// child's cwd set to the session's original directory. Shared by
/// `ccs open <n>` and the picker's Enter.
fn open_entry(id: &str, cwd: &str, reliable: bool, model: Option<&str>, print_only: bool) -> i32 {
    // refuse the unreliable (~) decoded cwd — hyphens in real path segments
    // make the decode a guess.
    if !reliable {
        eprintln!(
            "ccs: session has no cwd on record; its directory could not be decoded \
             reliably ({cwd}) - open it manually with `claude --resume {id}`"
        );
        return 1;
    }
    if !PathBuf::from(cwd).is_dir() {
        eprintln!("ccs: original directory no longer exists: {cwd}");
        return 1;
    }

    if print_only {
        // PowerShell-pasteable (the primary shell); works in pwsh/cmd too.
        if let Some(m) = model {
            out(&format!("cd \"{cwd}\"; ccm run {m} -- --resume {id}"));
        } else {
            out(&format!("cd \"{cwd}\"; claude --resume {id}"));
        }
        return 0;
    }

    let mut cmd = resume_command(id, model);
    cmd.current_dir(cwd);
    match cmd.status() {
        // propagate the child's fate: `ccs open n && ...` must not fire
        // when the session died nonzero or was killed (code() == None)
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("ccs: failed to launch: {e}");
            1
        }
    }
}

/// The spawn shape for resuming. Windows routes plain `claude` through
/// `cmd /c` — npm installs ship only claude.cmd shims and CreateProcessW
/// resolves bare names to .exe, so Command::new("claude") fails with os
/// error 2 (the same reason ccm's launcher goes through cmd). `ccm` is a
/// real .exe and spawns directly on every platform.
fn resume_command(id: &str, model: Option<&str>) -> Command {
    if let Some(m) = model {
        let mut c = Command::new("ccm");
        c.arg("run").arg(m).arg("--").arg("--resume").arg(id);
        c
    } else if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/c").arg("claude").arg("--resume").arg(id);
        c
    } else {
        let mut c = Command::new("claude");
        c.arg("--resume").arg(id);
        c
    }
}

fn lookup(number: usize) -> Option<(String, String, bool)> {
    let cache = fs::read_to_string(cache_path()).ok()?;
    for line in cache.lines() {
        let mut parts = line.splitn(4, '\t');
        if let (Some(n), Some(id), Some(cwd), Some(reliable)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
            && n.parse::<usize>().ok() == Some(number)
        {
            return Some((id.to_string(), cwd.to_string(), reliable == "1"));
        }
    }
    None
}

fn cache_path() -> PathBuf {
    std::env::temp_dir().join("ccs-list.txt")
}

fn sessions_root() -> Option<PathBuf> {
    let home = env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)?;
    Some(home.join(".claude").join("projects"))
}

/// cwd + topic from the first ~200 parsable records. The topic is the
/// first user text that is not a harness wrapper (command output /
/// caveats) — those never describe what the session is about.
fn head_info(path: &std::path::Path) -> (Option<String>, String) {
    let Ok(file) = fs::File::open(path) else {
        return (None, "(unreadable)".to_string());
    };
    let reader = BufReader::new(file);
    let mut cwd = None;
    let mut topic = String::new();
    // map_while(Ok) BEFORE take(): unreadable/non-UTF8 lines must not eat
    // the 200-record budget (flatten would loop forever on repeated Err)
    for line in reader.lines().map_while(Result::ok).take(200) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if cwd.is_none()
            && let Some(c) = v.get("cwd").and_then(|c| c.as_str())
        {
            cwd = Some(c.to_string());
        }
        if topic.is_empty() {
            let msg = v.get("message");
            let role = msg.and_then(|m| m.get("role")).and_then(|r| r.as_str());
            if role == Some("user")
                && let Some(text) = user_text(msg)
            {
                let t = text.trim();
                // skip the known harness wrappers ONLY — a genuine prompt
                // that merely starts with '<' (pasted HTML) stays a topic
                let wrapper = t.starts_with("<local-command-caveat")
                    || t.starts_with("<local-command-stdout")
                    || t.starts_with("<command-")
                    || t.starts_with("Caveat: The messages")
                    || t.starts_with("[Request interrupted");
                if !t.is_empty() && !wrapper {
                    topic = t.to_string();
                }
            }
        }
        if cwd.is_some() && !topic.is_empty() {
            break;
        }
    }
    (cwd, topic)
}

fn user_text(msg: Option<&serde_json::Value>) -> Option<String> {
    let content = msg?.get("content")?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = content.as_array() {
        let joined = arr
            .iter()
            .filter_map(|b| {
                if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                    b.get("text").and_then(|t| t.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        if !joined.is_empty() {
            return Some(joined);
        }
    }
    None
}

/// Best-effort decode of the encoded directory name, shown with the `~`
/// mark by the caller: `D--Project-ccm` -> `D:/Project/ccm`, but a real
/// hyphen in a path segment is indistinguishable, so decoded rows are
/// display-only and cannot be `open`ed.
fn encoded_fallback(path: &std::path::Path) -> String {
    let dir = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("?");
    let re = if let Some((prefix, rest)) = dir.split_once("--") {
        format!("{}:/{}", prefix, rest)
    } else {
        dir.to_string()
    };
    re.replace(['-', '\\'], "/")
}

/// Local time is what a human compares against their terminal history;
/// chrono::Local keeps that honest across DST and timezones.
fn format_mtime(secs: u64) -> String {
    chrono::DateTime::<chrono::Local>::from(
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs),
    )
    .format("%m-%d %H:%M")
    .to_string()
}

fn print_usage() {
    out(
        "ccs - Claude Code Sessions: cross-project session finder + resumer

USAGE
  ccs                   interactive picker on a terminal (newest max(n,200)
                        sessions; arrows to select, type to filter, Enter
                        resumes in the session's original directory)
  ccs -m glm            picker with the ccm route preset (Enter routes)
  ccs --no-tui          force the plain listing on a terminal
  ccs -n 40             list 40 when non-interactive; on a terminal only
                        grows the picker pool past its 200-row floor
  ccs <keyword>         full-text filter over the newest max(n,500)
                        transcripts (path / topic / messages, any case)
  ccs open <n>          resume session n in its original directory
  ccs open <n> -m glm   same, routed through ccm (ccm run glm -- --resume <id>)
  ccs open <n> --print  print a paste-ready command (PowerShell syntax)
  ccs id <n>            print \"id<TAB>cwd\" for scripting

NOTES
  Bare ccs on a PIPE stays a plain listing (head/grep friendly). Sessions
  are read from ~/.claude/projects (never modified). open/id resolve
  against the machine-wide LAST listing - re-run ccs first if you listed
  with a different filter elsewhere. The ~ mark flags a cwd that was not
  on record (lossy directory-name decode); such rows cannot be opened.",
    );
}
