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
//   ccs -a, --all        include the machine-generated agent workspaces
//                        (hex-id worktree/task dirs) the default views
//                        skip, so an agent storm cannot crowd real
//                        projects out of the newest-sessions view
//   ccs --since 7d       only sessions modified within the span (12h/7d/2w);
//                        works in every mode
//   picker extras        '/' searches message content (background scan,
//                        terms ANDed); Tab toggles an end-of-session
//                        preview (last input/reply, branch at end)
//   ccs <keyword>        filter across the newest max(n,500) transcripts;
//                        matches count only in conversation text (user /
//                        assistant messages), any case
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
// Machine-generated agent workspaces (a path segment splitting into a
// bare hex fragment of 8+ chars that contains a letter, e.g. multica's
// .../call-10-31bc8a6e6e05/workdir worktrees; and anything under the OS
// temp directory, e.g. %TEMP%/dsh-ccacp-probe-*) are skipped without
// consuming a pool slot in the default views AND out of the keyword scan
// window -- --all includes them everywhere. Keyword matching covers
// conversation text only (user/assistant). The hex signature also
// catches hex-named tool caches (cargo registry, bazel) -- --all is the
// escape hatch.
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
    let mut all = false;
    let mut since: Option<u64> = None;
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
            "-a" | "--all" => all = true,
            "--since" => {
                i += 1;
                match args.get(i).and_then(|v| parse_since(v)) {
                    Some(s) => since = Some(s),
                    None => {
                        eprintln!("ccs: --since expects a span like 12h, 7d, 2w");
                        return 2;
                    }
                }
            }
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

    // ── candidate selection, newest first ────────────────────────────────
    // Keyword mode scans the newest max(n,500) transcripts for CONVERSATION
    // hits (user/assistant text only — see human_text_hit) and keeps the
    // newest n matches; generated workspaces leave the window unless
    // --all, with the same skip-count hint as the listing. Every other
    // shape parses transcript heads newest first until it has `cap`
    // visible rows: machine-generated agent workspaces
    // (is_generated_workspace) are skipped without consuming a slot, so
    // an agent storm minting dozens of hash-named worktrees a day cannot
    // crowd real projects out of the view. Worst case (nearly everything
    // generated) parses a head per transcript — the same read order as
    // keyword mode's full-text scan.
    let cap = if interactive { n.max(200) } else { n };
    let hide_generated = keyword.is_none() && !all;
    let temp_root = env::temp_dir().to_string_lossy().into_owned();
    // clamp the span into i64 first: a saturating u64 span would wrap
    // negative `as i64` and invert the filter into "show nothing"
    let min_secs = since
        .map(|span| i64::try_from(span).unwrap_or(i64::MAX))
        .and_then(|span| chrono::Local::now().timestamp().checked_sub(span))
        .map(|t| t.max(0) as u64);

    let mut entries: Vec<picker::PickerRow> = Vec::new();
    // keyword-mode match context lines, parallel to entries (empty elsewhere)
    let mut snippets: Vec<String> = Vec::new();
    let mut hidden = 0usize;
    if let Some(kw) = &keyword {
        let needle = kw.to_lowercase();
        for (path, secs) in files.iter().take(n.max(500)) {
            if min_secs.is_some_and(|m| *secs < m) {
                continue;
            }
            let row = row_for(path, *secs);
            // generated workspaces leave the scan window unless --all:
            // they match their own harness boilerplate and would eat the
            // result quota before older real matches are ever reached
            if !all && is_generated_workspace(&row.cwd, &temp_root) {
                hidden += 1;
                continue;
            }
            let Ok(raw) = fs::read(path) else {
                continue;
            };
            // lossy decode: one bad byte must not hide a whole transcript.
            // C-speed prefilter on the whole file, then the hit must live
            // in conversation text — machine context (skill listings,
            // tool output) is identical across sessions and matches
            // every query, collapsing search into "list the newest n"
            let raw_s = String::from_utf8_lossy(&raw);
            // C-speed prefilter FIRST — human_text_hit parses every
            // message line, far too slow to run on non-matching files
            if !raw_s.to_lowercase().contains(&needle) {
                continue;
            }
            let Some(hit) = human_text_hit(&raw_s, &needle) else {
                continue;
            };
            entries.push(row);
            // whose words matched and where — the topic often has nothing
            // to do with the match point, which is why a hit list of bare
            // topics forced opening sessions one by one to tell them apart
            let who = if hit.role == "user" { "你" } else { "ai" };
            snippets.push(format!("{who}: {}", snippet_around(&hit.text, &needle, 56)));
            if entries.len() >= n {
                break;
            }
        }
    } else {
        for (path, secs) in files.iter() {
            if entries.len() >= cap {
                break;
            }
            if min_secs.is_some_and(|m| *secs < m) {
                continue;
            }
            let row = row_for(path, *secs);
            if hide_generated && is_generated_workspace(&row.cwd, &temp_root) {
                hidden += 1;
                continue;
            }
            entries.push(row);
        }
    }

    // ── interactive picker ─────────────────────────────────────────────────
    if interactive {
        match picker::run(&entries, model.as_deref(), hidden) {
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
        if hidden > 0 {
            out(&format!(
                "ccs: no sessions matched ({hidden} machine-generated agent workspaces hidden; ccs --all shows them)"
            ));
        } else {
            out("ccs: no sessions matched");
        }
        return 0;
    }
    for (idx, r) in entries.iter().enumerate() {
        let mark = if r.reliable { " " } else { "~" };
        let proj = picker::truncate_chars(&r.cwd.replace('\\', "/"), 38);
        let topic = picker::truncate_chars(&picker::squash(&r.topic), 56);
        out(&format!(
            "{:>3} {:<11} {}  {:<38}  {}",
            idx + 1,
            r.time,
            mark,
            proj,
            topic
        ));
        // the match context line: only when the topic does not already
        // contain the keyword (otherwise the snippet would just repeat
        // what the row above shows)
        if let Some(s) = snippets.get(idx)
            && !s.is_empty()
            && !keyword
                .as_ref()
                .is_some_and(|k| topic.to_lowercase().contains(&k.to_lowercase()))
        {
            out(&format!("      └ {s}"));
        }
    }
    out("");
    out("ccs open <n>          resume there (plain claude)");
    out("ccs open <n> -m glm   resume routed via ccm");
    if hidden > 0 {
        // "re-run", not "append": --all re-lists/re-searches the newest n
        // overall, so hidden rows displace older visible ones rather than
        // extend them
        out(&format!(
            "ccs --all             re-run with the {hidden} machine-generated agent workspace session(s) included"
        ));
    }
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

/// True for the known harness wrappers (command output / caveats /
/// continuation boilerplate) that never describe what a session is about
/// and must never be presented as a user's words. A genuine prompt that
/// merely starts with '<' (pasted HTML) stays a topic/hit. Pure —
/// pinned by tests.
fn is_wrapper(t: &str) -> bool {
    t.starts_with("<local-command-caveat")
        || t.starts_with("<local-command-stdout")
        || t.starts_with("<command-")
        || t.starts_with("Caveat: The messages")
        || t.starts_with("[Request interrupted")
        || t.starts_with("This session is being continued")
}

/// Signals collected from a transcript's head records (~200 lines):
/// cwd, the model-written session title (`ai-title`, recent CC versions
/// only), the first non-wrapper user text, and the first assistant text
/// as a last-resort topic.
struct HeadInfo {
    cwd: Option<String>,
    ai_title: Option<String>,
    user_topic: Option<String>,
    assistant_topic: Option<String>,
}

/// cwd + topic signals from the first ~200 parsable records. The user
/// topic is the first user text that is not a harness wrapper (command
/// output / caveats) — those never describe what the session is about.
fn head_info(path: &std::path::Path) -> HeadInfo {
    let Ok(file) = fs::File::open(path) else {
        return HeadInfo {
            cwd: None,
            ai_title: None,
            user_topic: Some("(unreadable)".to_string()),
            assistant_topic: None,
        };
    };
    let reader = BufReader::new(file);
    let mut info = HeadInfo {
        cwd: None,
        ai_title: None,
        user_topic: None,
        assistant_topic: None,
    };
    // map_while(Ok) BEFORE take(): unreadable/non-UTF8 lines must not eat
    // the 200-record budget (flatten would loop forever on repeated Err)
    for line in reader.lines().map_while(Result::ok).take(200) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        // ai-title lands ~line 18, AFTER the first user message — no
        // early break until it is seen (or the budget runs out)
        if info.ai_title.is_none()
            && v.get("type").and_then(|t| t.as_str()) == Some("ai-title")
            && let Some(t) = v.get("aiTitle").and_then(|t| t.as_str())
            && !t.trim().is_empty()
        {
            info.ai_title = Some(t.to_string());
        }
        if info.cwd.is_none()
            && let Some(c) = v.get("cwd").and_then(|c| c.as_str())
        {
            info.cwd = Some(c.to_string());
        }
        let msg = v.get("message");
        let role = msg.and_then(|m| m.get("role")).and_then(|r| r.as_str());
        if info.user_topic.is_none()
            && role == Some("user")
            && let Some(text) = user_text(msg)
        {
            let t = text.trim();
            if !t.is_empty() && !is_wrapper(t) {
                info.user_topic = Some(t.to_string());
            }
        }
        if info.assistant_topic.is_none()
            && role == Some("assistant")
            && let Some(text) = user_text(msg)
        {
            let t = text.trim();
            if !t.is_empty() {
                info.assistant_topic = Some(t.to_string());
            }
        }
        if info.cwd.is_some() && info.ai_title.is_some() && info.user_topic.is_some() {
            break;
        }
    }
    info
}

/// Topic priority: the model-written title (★) beats the opening user
/// message beats the session's LAST user input (≫, from a bounded tail
/// read — "where it ended" is often the memory hook) beats the first
/// assistant text. The markers keep the two title semantics from mixing
/// silently. Pure — pinned by tests.
fn choose_topic(head: &HeadInfo, tail_prompt: Option<&str>) -> Option<String> {
    if let Some(t) = &head.ai_title {
        return Some(format!("★ {t}"));
    }
    if let Some(t) = &head.user_topic {
        return Some(t.clone());
    }
    if let Some(p) = tail_prompt {
        return Some(format!("≫ {p}"));
    }
    head.assistant_topic.clone()
}

/// The final `last-prompt` record (the session's last user input), from a
/// 128KB tail read. Records append chronologically, so the LAST one in
/// file order is the first hit scanning backward. Pure core — pinned by
/// tests.
fn last_prompt_from_lines<'a>(lines: impl DoubleEndedIterator<Item = &'a str>) -> Option<String> {
    for line in lines.rev() {
        if !line.contains("\"last-prompt\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(p) = v.get("lastPrompt").and_then(|p| p.as_str()) {
            let p = p.trim();
            if !p.is_empty() {
                return Some(p.to_string());
            }
        }
    }
    None
}

/// Bounded tail read: the last 128KB of the transcript as whole lines
/// (first line dropped — it is likely partial; single records can exceed
/// the window). Shared by the last-prompt fallback and the preview pane.
fn read_tail(path: &std::path::Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(128 * 1024);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = vec![0u8; (len - start) as usize];
    f.read_exact(&mut buf).ok()?;
    let tail = String::from_utf8_lossy(&buf).to_string();
    let mut lines = tail.split('\n');
    if start > 0 {
        lines.next(); // drop the partial line
    }
    Some(lines.collect::<Vec<_>>().join("\n"))
}

/// The last `last-prompt` within a whole-line tail. All 216 top-level
/// transcripts on the author's machine hold theirs inside the window; a
/// miss falls back up the choose_topic chain.
fn tail_last_prompt(path: &std::path::Path) -> Option<String> {
    last_prompt_from_lines(read_tail(path)?.split('\n'))
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

/// A single-line, width-budgeted window around the first `needle` hit in
/// `text` — whitespace squashed, ellipsized at both ends when the window
/// cuts. The needle itself always survives whole: the window is grown
/// outward from the hit by DISPLAY width, never cut afterward (a
/// char-counted cut slices through the keyword on CJK-heavy lines).
/// Empty when the needle is not in the text at all. Pure — pinned by
/// tests.
fn snippet_around(text: &str, needle: &str, budget: usize) -> String {
    let flat = picker::squash(text);
    let chars: Vec<char> = flat.chars().collect();
    // lowercase per char with an index map: a byte offset found in the
    // LOWERCASED whole string is not a boundary in the original when a
    // char case-folds to a different length (İ, ß) — map low char
    // positions back to flat char positions instead
    let mut low = String::new();
    let mut map: Vec<usize> = Vec::new(); // map[low char idx] = flat char idx
    for (i, &c) in chars.iter().enumerate() {
        for _ in c.to_lowercase() {
            map.push(i);
        }
        low.extend(c.to_lowercase());
    }
    let Some(pos) = low.find(needle) else {
        return String::new();
    };
    let hit_cp = map[low[..pos].chars().count()];
    let needle_cp = needle.chars().count();
    let needle_w = picker::display_width(needle);
    // width each side may spend so needle + context + ellipses fit
    let side = budget.saturating_sub(needle_w + 2) / 2;

    let mut left = String::new();
    let mut w = 0;
    let mut i = hit_cp;
    while i > 0 {
        let c = chars[i - 1];
        if w + picker::char_width(c) > side {
            break;
        }
        left.insert(0, c);
        w += picker::char_width(c);
        i -= 1;
    }
    let cut_start = i > 0;

    let mut right = String::new();
    let mut w = 0;
    let mut j = hit_cp + needle_cp;
    while j < chars.len() {
        let c = chars[j];
        if w + picker::char_width(c) > side {
            break;
        }
        right.push(c);
        w += picker::char_width(c);
        j += 1;
    }
    let cut_end = j < chars.len();

    let mut window = String::new();
    if cut_start {
        window.push('…');
    }
    window.push_str(&left);
    window.extend(&chars[hit_cp..(hit_cp + needle_cp).min(chars.len())]);
    window.push_str(&right);
    if cut_end {
        window.push('…');
    }
    window
}

/// What the preview pane shows about a session's end: the last user
/// input, the last assistant reply, the branch at end (gitBranch changes
/// mid-session — this is where it ended), and the last-seen timestamp.
#[derive(Default, Debug, PartialEq)]
pub(crate) struct TailInfo {
    pub last_user: Option<String>,
    pub last_assistant: Option<String>,
    pub branch: Option<String>,
    pub last_seen: Option<String>,
}

/// Walk tail lines BACKWARD filling TailInfo — the first hit of each
/// field scanning from the end is its final value (records append
/// chronologically). Pure — pinned by tests.
fn tail_info_from_lines<'a>(lines: impl DoubleEndedIterator<Item = &'a str>) -> TailInfo {
    let mut info = TailInfo::default();
    for line in lines.rev() {
        let wants_any = info.last_user.is_none()
            || info.last_assistant.is_none()
            || info.branch.is_none()
            || info.last_seen.is_none();
        if !wants_any {
            break;
        }
        if !line.contains("\"message\"")
            && !line.contains("\"gitBranch\"")
            && !line.contains("\"timestamp\"")
        {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("isSidechain").and_then(|s| s.as_bool()) == Some(true)
            || v.get("isMeta").and_then(|s| s.as_bool()) == Some(true)
        {
            continue;
        }
        if let Some(ts) = v.get("timestamp").and_then(|t| t.as_str())
            && info.last_seen.is_none()
        {
            info.last_seen = Some(ts.to_string());
        }
        if let Some(b) = v.get("gitBranch").and_then(|t| t.as_str())
            && info.branch.is_none()
        {
            info.branch = Some(b.to_string());
        }
        let role = v
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(|r| r.as_str());
        if role == Some("user")
            && info.last_user.is_none()
            && let Some(text) = user_text(v.get("message"))
            && !text.trim().is_empty()
            && !is_wrapper(text.trim())
        {
            info.last_user = Some(text.trim().to_string());
        }
        if role == Some("assistant")
            && info.last_assistant.is_none()
            && let Some(text) = user_text(v.get("message"))
            && !text.trim().is_empty()
        {
            info.last_assistant = Some(text.trim().to_string());
        }
    }
    info
}

/// Preview lines for the picker's Tab pane: where the session ENDED —
/// often the actual memory hook when the topic is a generic opening.
pub(crate) fn preview_lines(path: &std::path::Path) -> Vec<String> {
    let info = read_tail(path)
        .map(|t| tail_info_from_lines(t.split('\n')))
        .unwrap_or_default();
    let mut lines = Vec::new();
    if let Some(u) = &info.last_user {
        lines.push(format!(
            "你: {}",
            picker::truncate_chars(&picker::squash(u), 72)
        ));
    }
    if let Some(a) = &info.last_assistant {
        lines.push(format!(
            "ai: {}",
            picker::truncate_chars(&picker::squash(a), 72)
        ));
    }
    let meta = [
        info.branch.as_deref().map(|b| format!("branch {b}")),
        info.last_seen
            .as_deref()
            .and_then(parse_iso_min)
            .map(|s| format!("last {s}")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    if !meta.is_empty() {
        lines.push(meta);
    }
    if lines.is_empty() {
        lines.push("(no conversation tail in the last 128KB)".to_string());
    }
    lines
}

/// ISO timestamp -> "MM-DD HH:mm" for the preview meta line; the raw
/// string back on any parse failure.
fn parse_iso_min(iso: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(iso).ok().map(|d| {
        d.with_timezone(&chrono::Local)
            .format("%m-%d %H:%M")
            .to_string()
    })
}

/// True when the transcript's conversation text contains EVERY term
/// (AND, case-insensitive, machine context excluded — same domain as
/// human_text_hit). Used by the picker's `/` content search; runs on a
/// background thread. Reads the file once.
pub(crate) fn conversation_matches(path: &std::path::Path, terms: &[String]) -> bool {
    if terms.is_empty() {
        return false;
    }
    let Ok(raw) = fs::read(path) else {
        return false;
    };
    let raw_s = String::from_utf8_lossy(&raw);
    let raw_l = raw_s.to_lowercase();
    if !terms.iter().all(|t| raw_l.contains(t.as_str())) {
        return false; // C-speed prefilter for the whole AND set
    }
    terms.iter().all(|t| human_text_hit(&raw_s, t).is_some())
}

/// A keyword hit inside human-visible conversation text.
#[derive(Debug, PartialEq)]
pub(crate) struct TextHit {
    /// "user" | "assistant" — whose words matched
    pub(crate) role: String,
    /// the matching text block, original casing, trimmed
    pub(crate) text: String,
}

/// First occurrence of `needle` (already lowercased) inside user /
/// assistant TEXT blocks, records in file order. Machine context — tool
/// results, thinking, attachments (skill listings!), system records,
/// harness-injected isMeta messages — never counts: it is dominated by
/// boilerplate identical across sessions, which made unrelated queries
/// return the same newest files. A user hit wins over an earlier
/// assistant hit (your own words are the stronger recall key).
/// Pure — pinned by tests.
fn human_text_hit(raw: &str, needle: &str) -> Option<TextHit> {
    let mut assistant_hit: Option<TextHit> = None;
    for line in raw.split('\n') {
        // cheap ASCII reject first; lowercase only message-carrying lines
        if !line.contains("\"message\"") {
            continue; // attachment/system/mode records carry no message
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("isSidechain").and_then(|s| s.as_bool()) == Some(true)
            || v.get("isMeta").and_then(|s| s.as_bool()) == Some(true)
        {
            continue;
        }
        let Some(msg) = v.get("message") else {
            continue;
        };
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        if role != "user" && role != "assistant" {
            continue;
        }
        let Some(text) = user_text(Some(msg)) else {
            continue;
        };
        if role == "user" && is_wrapper(text.trim()) {
            continue; // command output injected as a user record
        }
        if !text.to_lowercase().contains(needle) {
            continue;
        }
        let hit = TextHit {
            role: role.to_string(),
            text: text.trim().to_string(),
        };
        if role == "user" {
            return Some(hit);
        }
        assistant_hit = assistant_hit.or(Some(hit));
    }
    assistant_hit
}

/// One transcript file -> one picker row: id from the file stem, cwd and
/// topic signals from the transcript head (plus a tail read only when
/// the head yields no title at all), mtime formatted for display.
fn row_for(path: &std::path::Path, secs: u64) -> picker::PickerRow {
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("?")
        .to_string();
    let head = head_info(path);
    let reliable = head.cwd.is_some();
    let cwd = head.cwd.clone().unwrap_or_else(|| encoded_fallback(path));
    let tail_prompt = if head.ai_title.is_none() && head.user_topic.is_none() {
        tail_last_prompt(path)
    } else {
        None
    };
    picker::PickerRow {
        id,
        cwd,
        reliable,
        topic: choose_topic(&head, tail_prompt.as_deref()).unwrap_or_default(),
        time: format_mtime(secs),
        path: path.to_path_buf(),
    }
}

/// Machine-generated agent workspaces — worktrees and task dirs minted
/// with a hex id (`.../tuweiwu-7ef8b5c1bd5b/call-10-31bc8a6e6e05/workdir`,
/// `.../tuwe-5-a52f1d66d96d/worktree`) and anything spawned under the OS
/// temp directory (`%TEMP%/dsh-ccacp-probe-G1nBohF`) — are
/// indistinguishable by eye, never deliberately resumed, and an agent
/// storm can mint dozens a day, crowding real projects out of the
/// newest-sessions view. Signatures: a `/`-separated path segment which,
/// split on `-`/`_`, yields a bare hex fragment >= 8 chars with a letter
/// in it (the letter keeps digit-only names — compact dates like
/// `20251101` — visible), or a cwd anywhere under `temp_root` (temp is
/// ephemeral by contract). `--all` shows them anyway. Pure — pinned by
/// tests (temp_root passed in so the predicate stays
/// machine-independent).
fn is_generated_workspace(cwd: &str, temp_root: &str) -> bool {
    cwd.split(['/', '\\'])
        .flat_map(|seg| seg.split(['-', '_']))
        .any(|part| {
            part.len() >= 8
                && part.chars().all(|c| c.is_ascii_hexdigit())
                && part.chars().any(|c| c.is_ascii_alphabetic())
        })
        || is_under(cwd, temp_root)
}

/// Component-wise case-insensitive "cwd is under root" (Windows paths
/// disagree on case and separators between records and env). Pure —
/// pinned by tests.
fn is_under(cwd: &str, root: &str) -> bool {
    if root.is_empty() {
        return false;
    }
    let norm = |s: &str| s.replace('\\', "/").to_lowercase();
    let c = norm(cwd);
    let r = norm(root).trim_end_matches('/').to_string();
    c == r || c.starts_with(&format!("{r}/"))
}

/// Local time is what a human compares against their terminal history;
/// chrono::Local keeps that honest across DST and timezones. Rows from a
/// previous year swap the clock for the year — "MM-DD HH:mm" is
/// year-ambiguous on anything older than ~11 months.
fn format_mtime(secs: u64) -> String {
    mtime_display(secs, chrono::Local::now().timestamp().max(0) as u64)
}

/// Pure rendering core of format_mtime — pinned by tests.
fn mtime_display(secs: u64, now_secs: u64) -> String {
    let dt = chrono::DateTime::<chrono::Local>::from(
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs),
    );
    let now = chrono::DateTime::<chrono::Local>::from(
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(now_secs),
    );
    if dt.format("%Y").to_string() != now.format("%Y").to_string() {
        dt.format("%y-%m-%d").to_string()
    } else {
        dt.format("%m-%d %H:%M").to_string()
    }
}

/// "7d" / "12h" / "2w" -> seconds for the --since filter. Char-based
/// (never split_at on a byte length — a multibyte unit like `天` would
/// not be a char boundary and would panic). Pure — pinned by tests.
fn parse_since(s: &str) -> Option<u64> {
    let s = s.trim();
    let mut chars = s.chars();
    let unit = chars.next_back()?;
    let n: u64 = chars.as_str().parse().ok()?;
    match unit {
        'h' => Some(n.saturating_mul(3600)),
        'd' => Some(n.saturating_mul(86400)),
        'w' => Some(n.saturating_mul(7 * 86400)),
        _ => None,
    }
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
  ccs --since 7d        only sessions modified within the span (12h/7d/2w),
                        in every mode
  ccs -n 40             list 40 when non-interactive; on a terminal only
                        grows the picker pool past its 200-row floor
  ccs -a, --all         include the machine-generated agent workspaces
                        (hex-id worktree/task dirs) the default views
                        skip, so agent storms cannot crowd out real
                        projects
  ccs <keyword>         filter over the newest max(n,500) transcripts;
                        a match counts only inside conversation text
                        (user / assistant messages, any case) - tool
                        output and harness boilerplate never match. Hits
                        carry a 你:/ai: context line when the topic does
                        not already show the keyword
  ccs open <n>          resume session n in its original directory
  ccs open <n> -m glm   same, routed through ccm (ccm run glm -- --resume <id>)
  ccs open <n> --print  print a paste-ready command (PowerShell syntax)
  ccs id <n>            print \"id<TAB>cwd\" for scripting

NOTES
  Picker keys: '/' searches message content (terms ANDed, conversation
  text only, background scan); Tab toggles an end-of-session preview
  (last input / last reply / branch at end / last activity); Esc leaves
  the content query first, quits second.
  Bare ccs on a PIPE stays a plain listing (head/grep friendly). Sessions
  are read from ~/.claude/projects (never modified). open/id resolve
  against the machine-wide LAST listing - re-run ccs first if you listed
  with a different filter elsewhere. The ~ mark flags a cwd that was not
  on record (lossy directory-name decode); such rows cannot be opened.
  Topics: ★ marks a model-written session title (ai-title), ≫ the
  session's last user input (no opening line survived the wrapper
  filter); plain rows show the first user message.
  Machine-generated agent workspaces (a path segment splitting into a
  bare hex fragment of 8+ chars containing a letter, e.g.
  .../call-10-31bc8a6e6e05/workdir worktrees; and anything under the OS
  temp directory, e.g. %TEMP%/dsh-*) are skipped by default so they don't
  crowd out real projects; --all includes them and keyword search always
  scans them. Hex-named tool caches (cargo registry, bazel) match the
  same signature - use --all.",
    );
}

#[cfg(test)]
mod tests {
    use super::{choose_topic, is_generated_workspace, is_under, last_prompt_from_lines};

    const TMP: &str = "C:/Users/x/AppData/Local/Temp";

    fn head(
        ai_title: Option<&str>,
        user_topic: Option<&str>,
        assistant_topic: Option<&str>,
    ) -> super::HeadInfo {
        super::HeadInfo {
            cwd: Some("D:/Project/ccs".into()),
            ai_title: ai_title.map(String::from),
            user_topic: user_topic.map(String::from),
            assistant_topic: assistant_topic.map(String::from),
        }
    }

    #[test]
    fn topic_priority_ai_title_user_then_tail_then_assistant() {
        // ai-title wins and gets the star marker
        assert_eq!(
            choose_topic(
                &head(Some("修复输入"), Some("看下项目"), Some("好的")),
                None
            ),
            Some("★ 修复输入".into())
        );
        // opening user message is the default, unmarked
        assert_eq!(
            choose_topic(&head(None, Some("看下项目"), Some("好的")), None),
            Some("看下项目".into())
        );
        // no title in the head: the session's last input takes over, marked
        assert_eq!(
            choose_topic(&head(None, None, Some("好的")), Some("改成蓝绿部署")),
            Some("≫ 改成蓝绿部署".into())
        );
        // tail prompt still beats assistant text (assistant is last resort)
        assert_eq!(
            choose_topic(&head(None, None, Some("我先看看")), Some("改成蓝绿部署")),
            Some("≫ 改成蓝绿部署".into())
        );
        // nothing human anywhere: first assistant text, unmarked
        assert_eq!(
            choose_topic(&head(None, None, Some("我先看看")), None),
            Some("我先看看".into())
        );
        assert_eq!(choose_topic(&head(None, None, None), None), None);
    }

    #[test]
    fn keyword_hits_count_only_in_conversation_text() {
        use super::human_text_hit;
        let user_line = |t: &str| {
            format!("{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"{t}\"}}}}")
        };
        let assistant_line = |t: &str| {
            format!(
                "{{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"{t}\"}}]}}}}"
            )
        };
        // user string content hits, case-insensitively
        let raw = format!("{}\n{}", "{\"type\":\"mode\"}", user_line("把 Deploy 改掉"));
        let hit = human_text_hit(&raw, "deploy").unwrap();
        assert_eq!(hit.role, "user");
        assert_eq!(hit.text, "把 Deploy 改掉");
        // a user hit wins over an earlier assistant hit
        let raw = format!(
            "{}\n{}",
            assistant_line("deploy scripts"),
            user_line("Deploy 好了吗")
        );
        assert_eq!(human_text_hit(&raw, "deploy").unwrap().role, "user");
        // assistant text blocks are the fallback
        let raw = assistant_line("fixed the Deploy step");
        assert_eq!(human_text_hit(&raw, "deploy").unwrap().role, "assistant");
        // tool_result blocks, attachments, isMeta, sidechains never match
        let tool = "{\"type\":\"user\",\"isMeta\":false,\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"content\":\"deploy OK\"}]}}";
        let attachment = "{\"type\":\"attachment\",\"attachment\":{\"text\":\"deploy listing\"}}";
        let meta = "{\"type\":\"user\",\"isMeta\":true,\"message\":{\"role\":\"user\",\"content\":\"deploy meta\"}}";
        let side = "{\"type\":\"user\",\"isSidechain\":true,\"message\":{\"role\":\"user\",\"content\":\"deploy side\"}}";
        let raw = format!("{tool}\n{attachment}\n{meta}\n{side}");
        assert_eq!(human_text_hit(&raw, "deploy"), None);
        // thinking blocks are not visible text
        let think = "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"thinking\",\"thinking\":\"deploy maybe\"}]}}";
        assert_eq!(human_text_hit(&think, "deploy"), None);
    }

    #[test]
    fn snippet_windows_around_the_first_hit() {
        use super::snippet_around;
        // hit in the middle: window on both sides, ellipsized
        let s = snippet_around(
            "先把环境检查一遍 然后 我们聊聊 deploy 的事情 再说",
            "deploy",
            20,
        );
        assert!(s.contains("deploy"));
        assert!(s.starts_with('…') && s.ends_with('…'));
        // hit at the very start: no leading ellipsis
        let s = snippet_around("deploy 在开头", "deploy", 20);
        assert!(s.starts_with("deploy"));
        assert!(!s.starts_with('…'));
        // no hit at all
        assert_eq!(snippet_around("nothing here", "deploy", 20), "");
        // CJK budget is display columns: 10-col budget holds 4 CJK chars
        let s = snippet_around("甲乙丙丁deploy戊己庚辛", "deploy", 12);
        assert!(crate::picker::display_width(&s) <= 12);
        assert!(s.contains("deploy"));
    }

    #[test]
    fn since_spans_parse() {
        use super::parse_since;
        assert_eq!(parse_since("7d"), Some(7 * 86400));
        assert_eq!(parse_since("12h"), Some(12 * 3600));
        assert_eq!(parse_since("2w"), Some(14 * 86400));
        assert_eq!(parse_since("0h"), Some(0));
        assert_eq!(parse_since("7x"), None);
        assert_eq!(parse_since("d"), None);
        assert_eq!(parse_since(""), None);
        assert_eq!(parse_since("-3d"), None);
        // multibyte unit chars are not byte-boundary panics, just spans
        // we do not understand
        assert_eq!(parse_since("7天"), None);
        assert_eq!(parse_since("3µd"), None);
    }

    #[test]
    fn snippet_survives_case_folding_length_changes() {
        use super::snippet_around;
        // 'İ' lowercases to TWO chars; a byte offset taken from the
        // lowercased string would slice mid-char in the original
        let s = snippet_around("İxdeploy 关于部署", "deploy", 20);
        assert!(s.contains("deploy"));
        // the hit position still maps back through the fold
        let s = snippet_around("aßİ deploy 件事", "deploy", 20);
        assert!(s.contains("deploy"));
    }

    #[test]
    fn wrappers_never_pose_as_user_words() {
        use super::{human_text_hit, is_wrapper};
        assert!(is_wrapper("<command-name>/effort</command-name>"));
        assert!(is_wrapper("<local-command-stdout>1"));
        assert!(is_wrapper("Caveat: The messages below"));
        assert!(!is_wrapper("<pasted HTML the user actually sent>"));
        assert!(!is_wrapper("普通的问题"));
        // a keyword hitting only a command wrapper is not a conversation
        // match — the user never typed it
        let raw = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"<command-name>/effort设置</command-name>\"}}";
        assert_eq!(human_text_hit(raw, "effort"), None);
    }

    #[test]
    fn cross_year_rows_swap_the_clock_for_the_year() {
        use super::mtime_display;
        let now = 1_735_689_600_u64; // 2025-01-01Z
        let old = now - 400 * 86400; // safely a previous year in any tz
        assert_eq!(mtime_display(now, now).len(), 11); // "MM-DD HH:mm"
        assert_eq!(mtime_display(old, now).len(), 8); // "YY-MM-DD"
    }

    #[test]
    fn tail_info_reads_the_session_end() {
        use super::tail_info_from_lines;
        let mk_user = |t: &str| {
            format!("{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"{t}\"}}}}")
        };
        let mk_ai = |t: &str| {
            format!(
                "{{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"{t}\"}}]}}}}"
            )
        };
        let sys = |b: &str, ts: &str| {
            format!("{{\"type\":\"system\",\"gitBranch\":\"{b}\",\"timestamp\":\"{ts}\"}}")
        };
        // reverse walk: latest of each field wins; sidechain/meta skipped
        let lines = vec![
            sys("main", "2026-11-01T10:00:00Z"),
            mk_user("第一个问题"),
            mk_ai("第一版答复"),
            mk_user("最后的问题"),
            "{\"type\":\"user\",\"isSidechain\":true,\"message\":{\"role\":\"user\",\"content\":\"sidechain 不算\"}}".to_string(),
            mk_ai("最终答复"),
            sys("feat/findability", "2026-11-01T12:00:00Z"),
        ];
        let info = tail_info_from_lines(lines.iter().map(|s| s.as_str()));
        assert_eq!(info.last_user.as_deref(), Some("最后的问题"));
        assert_eq!(info.last_assistant.as_deref(), Some("最终答复"));
        assert_eq!(info.branch.as_deref(), Some("feat/findability"));
        assert_eq!(info.last_seen.as_deref(), Some("2026-11-01T12:00:00Z"));
    }

    #[test]
    fn conversation_and_matching_reads_real_files() {
        use super::conversation_matches;
        let dir = std::env::temp_dir().join("ccs-test-and-match");
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("s.jsonl");
        std::fs::write(
            &f,
            concat!(
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"把 Deploy 脚本改掉\"}}\n",
                "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"done\"}]}}\n",
                "{\"type\":\"attachment\",\"attachment\":{\"text\":\"deploy listing noise\"}}\n",
            ),
        )
        .unwrap();
        // AND across terms, case-insensitive, conversation text only
        assert!(conversation_matches(&f, &["deploy".into(), "脚本".into()]));
        assert!(conversation_matches(&f, &["done".into()]));
        // a term only present in an attachment record must not match
        assert!(!conversation_matches(&f, &["listing".into()]));
        assert!(!conversation_matches(&f, &[]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn last_prompt_takes_the_latest_record_from_the_tail() {
        let mk = |p: &str| format!("{{\"type\":\"last-prompt\",\"lastPrompt\":\"{p}\"}}");
        let find = |v: &[String]| last_prompt_from_lines(v.iter().map(|s| s.as_str()));
        // scanning backward: the newest (last) prompt wins
        let lines = vec![
            mk("第一个"),
            mk("第二个"),
            "{\"type\":\"mode\"}".to_string(),
        ];
        assert_eq!(find(&lines), Some("第二个".into()));
        // empty prompts are skipped, not returned
        let lines = vec![mk("  "), mk("有效")];
        assert_eq!(find(&lines), Some("有效".into()));
        // nothing there
        assert_eq!(find(&["{\"type\":\"user\"}".to_string()]), None);
    }

    #[test]
    fn hex_id_segments_flag_generated_workspaces() {
        // real shapes seen under ~/.claude/projects
        assert!(is_generated_workspace(
            "D:\\multica-workspaces\\tuweiwu-7ef8b5c1bd5b\\tuwe-5-a52f1d66d96d\\worktree",
            TMP
        ));
        assert!(is_generated_workspace(
            "D:/multica-workspaces/callisto-zg-deacf13231f4/call-10-31bc8a6e6e05/workdir",
            TMP
        ));
        assert!(is_generated_workspace(
            "C:/Users/x/multica/workspaces/desktop/api/multica-ai/proteus/vgm-a313551a4e5f/task-24fc52e85ec9/workdir",
            TMP
        ));
        // mixed-case hex counts
        assert!(is_generated_workspace("D:/x/A52F1D66D96D/y", TMP));
    }

    #[test]
    fn temp_dir_cwds_flag_generated_workspaces() {
        // the real dsh scratch shape (7-char mixed-case id, no hex hit)
        assert!(is_generated_workspace(
            "C:\\Users\\45110\\AppData\\Local\\Temp\\dsh-real-acp-gate-cOQZOV",
            "C:\\Users\\45110\\AppData\\Local\\Temp"
        ));
        // separator + case disagreement between cwd and env root
        assert!(is_under(
            "C:\\users/x\\AppData/local\\Temp\\dsh-ccacp-probe-G1nBohF",
            "C:/Users/x/AppData/Local/Temp/"
        ));
        assert!(is_under("/tmp/foo/bar", "/tmp"));
        assert!(is_under("/tmp", "/tmp/"));
        // component boundary: /tmpfoo is NOT under /tmp
        assert!(!is_under("/tmpfoo", "/tmp"));
        assert!(!is_under("D:/Project/ccs", TMP));
        // degenerate empty root matches nothing
        assert!(!is_under("C:/anything", ""));
    }

    #[test]
    fn human_named_dirs_stay_visible() {
        assert!(!is_generated_workspace("D:/Project/ccs", TMP));
        assert!(!is_generated_workspace("D:/Project/r2r-multica", TMP));
        assert!(!is_generated_workspace("D:/FDE/fde-student-w01-0-2-0", TMP));
        // a hex fragment inside a hyphenated name is human naming
        assert!(!is_generated_workspace("D:/Project/proj-2b3c4d5", TMP));
        // compact dates are pure digits — no letter, stays visible
        assert!(!is_generated_workspace("D:/notes/20251101", TMP));
        assert!(!is_generated_workspace("D:/notes/2026-11-01", TMP));
        // too short to be a machine id, and not under TMP
        assert!(!is_generated_workspace(
            "D:/workspaces/dsh/probe/G1nBohF",
            TMP
        ));
        assert!(!is_generated_workspace("", TMP));
    }
}
