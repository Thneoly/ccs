# ccs — Claude Code Sessions

A cross-project session finder + resumer for [Claude Code](https://code.claude.com/), for people who work on many projects in parallel.

**The problem:** Claude Code stores each session under `~/.claude/projects/<encoded-cwd>/`, scoped to the directory it started in. `claude --continue` only continues in the current directory, and the `--resume` picker assumes you already remember *which* project a session lived in. With 40+ project directories, "where was that session?" becomes its own task.

**ccs** lists recent sessions across **all** projects — time, project path, and the first user message as a topic — and reopens one in its original directory.

## Two modes, the claude cli pattern

| Invocation | Behavior |
|---|---|
| `ccs` (on a terminal) | Interactive picker: arrows to select, type to filter, `Enter` resumes in the session's original directory |
| `ccs` (piped), `ccs -n 40`, `ccs <keyword>`, `ccs open <n>`, `ccs --no-tui` | Plain, pipe-friendly, scriptable output |

```
  1 10-13 22:09  D:/Project/ccm        阅读下： README.md ↓ docs/DESIGN.md ↓ …
  2 10-13 22:09  D:/R2R-Agent/doc      阅读doc/document.txt, 拆分意图
  3 10-13 21:40  D:/Project            使用CDP 打开阅读 https://chatgpt.com/share/…
```

## Usage

```
ccs                     interactive picker over the newest 200 sessions
ccs -m glm              picker with a ccm route preset (Enter resumes via
                        `ccm run glm -- --resume <id>` — requires
                        [ccm](https://github.com/Thneoly/ccm), which also logs
                        each launch to its launches.jsonl ledger)
ccs <keyword>           full-text filter over the newest 500 transcripts
                        (path / topic / messages, case-insensitive)
ccs -n 40               list 40 non-interactively; grows the picker pool
                        past its 200-row floor on a terminal
ccs open <n>            resume session n from the last listing, spawning
                        `claude --resume <id>` with the child's cwd set to
                        the session's original directory
ccs open <n> -m glm     same, routed through ccm
ccs open <n> --print    print a paste-ready command (PowerShell syntax)
ccs id <n>              print "id<TAB>cwd" for scripting
ccs --no-tui            force the plain listing on a terminal
```

## How it works

- Sessions are read from `~/.claude/projects/<dir>/<uuid>.jsonl`. The real `cwd` always comes from the records inside the transcript; the encoded directory name is only a lossy fallback (hyphens are ambiguous), shown with a `~` mark — such rows are display-only and cannot be opened.
- The topic is the first user message that is not a harness wrapper (command output, caveats).
- Resume reuses the verified behavior that `claude --resume <id>` works from any directory; the gateway/model is whatever the launching environment provides — start it via ccm (`-m`) when you want routed credentials.
- On Windows, plain `claude` is spawned through `cmd /c` because npm installs ship `.cmd` shims that `CreateProcessW` cannot resolve.
- Reads only the transcripts directory; writes only a small listing cache (`%TEMP%/ccs-list.txt`). No config.

## Install

```sh
git clone https://github.com/Thneoly/ccs.git
cd ccs
cargo build --release
# put target/release/ccs[.exe] on your PATH
```

Dependencies: `serde_json`, `chrono`, `ratatui`, `crossterm`. Runs on Windows and Linux; `USERPROFILE`/`HOME` locates the projects directory.

## Notes & limits

- The keyword filter scans the newest `max(n, 500)` transcripts — matches older than that are invisible; raise with `-n`.
- `open <n>` numbers come from the machine-wide last **non-interactive** listing (bare `ccs` on a terminal opens the picker and does not refresh it); refresh with `ccs --no-tui`.
- Tested on Windows (Windows Terminal) against real transcript directories; interactive-mode key handling filters `KeyEventKind::Press` (crossterm emits Press+Release pairs on Windows).

## License

MIT
