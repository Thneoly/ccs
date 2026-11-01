// The interactive picker (ratatui + crossterm). Entered when ccs runs on
// a TTY with no keyword/subcommand and no --no-tui; every other shape
// stays non-interactive, the claude cli dual-mode pattern. The picker only
// SELECTS - launching happens in main after the terminal is restored, so
// the resumed claude inherits a clean console.

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen};
use crossterm::tty::IsTty;
use ratatui::DefaultTerminal;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Row, Table, TableState};
use std::time::Duration;

/// One selectable session row. Built by main's scan; the picker owns it
/// for the duration of the interaction.
pub struct PickerRow {
    pub id: String,
    pub cwd: String,
    pub reliable: bool,
    pub topic: String,
    /// preformatted time (the year is prepended for cross-year rows)
    pub time: String,
    /// transcript source path — the preview pane reads it on demand
    pub path: std::path::PathBuf,
}

/// What the picker decided.
pub enum Pick {
    /// index into the rows slice of the confirmed selection
    Chosen(usize),
    /// user aborted (q/Esc/Ctrl-C) — nothing launched
    Aborted,
    /// the TUI could not start (raw mode / alternate screen / not a TTY) —
    /// the caller should fall back to the plain listing
    Failed,
}

/// True when the row matches the live filter: substring over id, cwd, and
/// topic, case-insensitive. The cwd is matched with separators normalized
/// to `/` — the table DISPLAYS forward slashes, so what you see must be
/// what you can type (the raw record cwd carries backslashes on Windows).
/// Pure — pinned by tests.
pub fn matches_filter(row: &PickerRow, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let needle = needle.to_lowercase().replace('\\', "/");
    row.id.to_lowercase().contains(&needle)
        || row.cwd.replace('\\', "/").to_lowercase().contains(&needle)
        || row.topic.to_lowercase().contains(&needle)
}

/// Selection clamped into [0, len). Zero-length handled by the caller.
/// Pure — pinned by tests.
pub fn clamp_sel(sel: usize, len: usize) -> usize {
    if len == 0 { 0 } else { sel.min(len - 1) }
}

/// True when a Char key with these modifiers still means "the user typed
/// this character": SHIFT alone passes (it is how uppercase arrives —
/// Shift+A is `Char('A')` + SHIFT; caps-lock uppercase arrives with no
/// modifiers at all), but any Ctrl/Alt/Super/Meta combo must not inject a
/// bare letter into the filter. Known gap, unchanged from before: on
/// layouts needing AltGr for common characters, Windows delivers them as
/// CONTROL|ALT and they are dropped too. Pure — pinned by tests.
pub fn typeable_modifiers(m: KeyModifiers) -> bool {
    m.intersection(
        KeyModifiers::CONTROL
            | KeyModifiers::ALT
            | KeyModifiers::SUPER
            | KeyModifiers::HYPER
            | KeyModifiers::META,
    )
    .is_empty()
}

/// Run the picker on a TTY stdout. A piped stdout reports Failed so the
/// caller can list plainly. `hidden` is the count of machine-generated
/// agent-workspace sessions the scan skipped (0 = none) — shown in the
/// header so the smaller pool is not a mystery.
pub fn run(rows: &[PickerRow], model: Option<&str>, hidden: usize) -> Pick {
    if !std::io::stdout().is_tty() {
        return Pick::Failed;
    }
    let mut terminal = match setup() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("ccs: cannot start the picker ({e}); falling back to the listing");
            return Pick::Failed;
        }
    };
    // restore the console even on panic inside the loop
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = teardown();
        default_panic(info);
    }));

    let result = pick(&mut terminal, rows, model, hidden);

    let _ = teardown();
    // the panic hook stays installed: after the picker, teardown is
    // idempotent and any later panic still deserves a restored console
    match result {
        Some(idx) => Pick::Chosen(idx),
        None => Pick::Aborted,
    }
}

fn refilter(rows: &[PickerRow], filter: &str) -> Vec<usize> {
    (0..rows.len())
        .filter(|&i| matches_filter(&rows[i], filter))
        .collect()
}

fn pick(
    terminal: &mut DefaultTerminal,
    rows: &[PickerRow],
    model: Option<&str>,
    hidden: usize,
) -> Option<usize> {
    let mut filter = String::new();
    // '/' content search: the query, plus background-scan state (the
    // picker is a single-threaded loop; the scan runs on a thread and its
    // result arrives through the channel, latest generation wins)
    let mut content_mode = false;
    let mut content_query = String::new();
    let mut content_hits: Option<Vec<usize>> = None;
    let mut scanning = false;
    let mut scan_gen = 0u64;
    let (scan_tx, scan_rx) = std::sync::mpsc::channel::<(u64, Vec<usize>)>();
    // Tab preview of the selected row, recomputed when the row changes
    let mut preview_on = false;
    let mut preview: Option<(usize, Vec<String>)> = None;

    // the metadata filter applies on top of whatever base set the content
    // search produced (all rows when it never ran)
    let apply = |filter: &str, base: &Option<Vec<usize>>| -> Vec<usize> {
        match base {
            Some(hits) => hits
                .iter()
                .copied()
                .filter(|&i| matches_filter(&rows[i], filter))
                .collect(),
            None => refilter(rows, filter),
        }
    };
    let mut filtered = apply(&filter, &content_hits);
    // The TableState lives HERE (not in the render fn) so its scroll
    // offset survives frames — ratatui's own state.rs contract.
    let mut state = TableState::default().with_selected(0);
    loop {
        let sel = clamp_sel(state.selected().unwrap_or(0), filtered.len());
        state.select(Some(sel));

        // the preview follows the selection: a 128KB tail read is a few
        // milliseconds, fine on the UI thread
        if preview_on {
            if let Some(&row_idx) = filtered.get(sel)
                && preview.as_ref().map(|(i, _)| *i) != Some(row_idx)
            {
                preview = Some((row_idx, crate::preview_lines(&rows[row_idx].path)));
            }
        } else {
            preview = None;
        }

        terminal
            .draw(|frame| {
                draw(
                    frame,
                    rows,
                    &filtered,
                    &mut state,
                    &filter,
                    model,
                    hidden,
                    content_mode,
                    &content_query,
                    scanning,
                    content_hits.as_ref().map(|h| h.len()),
                    preview_on,
                    preview.as_ref().map(|(_, l)| l),
                )
            })
            .ok()?;

        // wait for a key but wake up regularly to collect scan results —
        // this used to block on event::read() forever
        // fast poll only while a scan can land; the preview has no
        // async producer (selection changes interrupt poll instantly)
        let timeout = if scanning { 80 } else { 250 };
        if event::poll(Duration::from_millis(timeout)).unwrap_or(false) {
            // Windows emits TWO Key events per physical press (Press +
            // Release); without this guard every key would act twice.
            if let Ok(Event::Key(key)) = event::read()
                && key.kind == KeyEventKind::Press
            {
                match key.code {
                    KeyCode::Esc => {
                        if content_mode {
                            // leave the query input, keep any results
                            content_mode = false;
                        } else {
                            return None;
                        }
                    }
                    KeyCode::Tab => {
                        preview_on = !preview_on;
                        preview = None;
                    }
                    // '/' enters content search: the metadata filter can
                    // only see id/cwd/topic, never message text
                    KeyCode::Char('/') if !content_mode && typeable_modifiers(key.modifiers) => {
                        // typeable_modifiers, not is_empty: on layouts
                        // where '/' needs Shift, the press arrives with
                        // SHIFT set
                        content_mode = true;
                    }
                    // Enter in content mode re-scans (terms are the
                    // whitespace-split query, ANDed)
                    KeyCode::Enter if content_mode => {
                        // Enter on an EMPTY query is the "clear" gesture:
                        // back to the full list, not a scan that matches
                        // nothing and bricks the picker
                        if content_query.trim().is_empty() {
                            content_mode = false;
                            content_hits = None;
                            state.select(Some(0));
                            // filtered re-applies after the match, like
                            // every other edit
                        } else {
                            scan_gen += 1;
                            scanning = true;
                            let paths: Vec<std::path::PathBuf> =
                                rows.iter().map(|r| r.path.clone()).collect();
                            let terms: Vec<String> = content_query
                                .to_lowercase()
                                .split_whitespace()
                                .map(String::from)
                                .collect();
                            let generation = scan_gen;
                            let tx = scan_tx.clone();
                            std::thread::spawn(move || {
                                let hits: Vec<usize> = paths
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, p)| crate::conversation_matches(p, &terms))
                                    .map(|(i, _)| i)
                                    .collect();
                                let _ = tx.send((generation, hits));
                            });
                        }
                    }
                    KeyCode::Backspace if content_mode => {
                        content_query.pop();
                    }
                    KeyCode::Char(c) if content_mode && typeable_modifiers(key.modifiers) => {
                        content_query.push(c);
                    }
                    // plain q quits only with an empty filter, so the letter
                    // stays typeable; Esc/Ctrl-C always abort. crossterm
                    // uppercases the char when caps-lock is on, so the abort
                    // arm must take both cases
                    KeyCode::Char('q')
                        if !content_mode && key.modifiers.is_empty() && filter.is_empty() =>
                    {
                        return None;
                    }
                    KeyCode::Char('c' | 'C') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return None;
                    }
                    KeyCode::Up => {
                        state.select(Some(sel.saturating_sub(1)));
                    }
                    KeyCode::Down if !filtered.is_empty() => {
                        state.select(Some(clamp_sel(sel + 1, filtered.len())));
                    }
                    KeyCode::PageUp => {
                        let page = 10.min(sel);
                        state.select(Some(sel - page));
                    }
                    KeyCode::PageDown if !filtered.is_empty() => {
                        state.select(Some(clamp_sel(sel + 10, filtered.len())));
                    }
                    KeyCode::Home => state.select(Some(0)),
                    KeyCode::End if !filtered.is_empty() => {
                        state.select(Some(filtered.len() - 1));
                    }
                    KeyCode::Backspace => {
                        filter.pop();
                    }
                    KeyCode::Enter => {
                        if let Some(&row_idx) = filtered.get(sel) {
                            return Some(row_idx);
                        }
                    }
                    // plain typing only — Ctrl/Alt combos must not inject bare
                    // letters into the filter, but SHIFT passes so uppercase
                    // stays typeable (Shift+A arrives as Char('A') + SHIFT)
                    KeyCode::Char(c) if typeable_modifiers(key.modifiers) => {
                        filter.push(c);
                    }
                    _ => {}
                }
                // any edit re-filters over the current base set
                filtered = apply(&filter, &content_hits);
            }
        }
        // latest-wins: a slow older scan must not clobber a newer one
        while let Ok((generation, hits)) = scan_rx.try_recv() {
            if generation == scan_gen {
                content_hits = Some(hits);
                scanning = false;
                // results are in: leave the input so Enter resumes the
                // selection instead of silently rescanning
                content_mode = false;
                state.select(Some(0));
                filtered = apply(&filter, &content_hits);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw(
    frame: &mut ratatui::Frame,
    rows: &[PickerRow],
    filtered: &[usize],
    state: &mut TableState,
    filter: &str,
    model: Option<&str>,
    hidden: usize,
    content_mode: bool,
    content_query: &str,
    scanning: bool,
    content_n: Option<usize>,
    preview_on: bool,
    preview: Option<&Vec<String>>,
) {
    let (header, table_area, preview_area, footer) = if preview_on {
        let [h, t, p, f] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(5),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        (h, t, Some(p), f)
    } else {
        let [h, t, f] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        (h, t, None, f)
    };

    let shown = if filtered.is_empty() {
        " · no matches".to_string()
    } else {
        format!(" · {}/{}", filtered.len(), rows.len())
    };
    let skipped = if hidden > 0 {
        format!(" · {hidden} hidden (--all)")
    } else {
        String::new()
    };
    let content = match (content_n, scanning) {
        (_, true) => " · scanning…".to_string(),
        (Some(n), false) if !content_mode => format!(" · {n} content hits"),
        _ => String::new(),
    };
    let route = model.map(|m| format!(" · ccm: {m}")).unwrap_or_default();
    let (prompt_label, prompt_value) = if content_mode {
        (" · content: ", content_query)
    } else {
        (" · filter: ", filter)
    };
    let header_line = Line::from(vec![
        Span::styled(" ccs", Style::default().fg(Color::Cyan).bold()),
        Span::raw(shown),
        Span::styled(content, Style::default().fg(Color::Cyan)),
        Span::styled(skipped, Style::default().fg(Color::DarkGray)),
        Span::raw(route),
        Span::raw(prompt_label),
        Span::styled(
            format!("{prompt_value}█"),
            Style::default().fg(Color::Yellow),
        ),
    ]);
    frame.render_widget(Paragraph::new(header_line), header);

    let rows_widget = filtered.iter().map(|&i| {
        let r = &rows[i];
        let base = if r.reliable {
            Style::default()
        } else {
            Style::default().fg(Color::DarkGray)
        };
        Row::new(vec![
            r.time.clone(),
            if r.reliable { " ".into() } else { "~".into() },
            truncate_chars(&r.cwd.replace('\\', "/"), 38),
            truncate_chars(&squash(&r.topic), 64),
        ])
        .style(base)
    });
    let table = Table::new(
        rows_widget,
        [
            Constraint::Length(11),
            Constraint::Length(1),
            Constraint::Length(38),
            Constraint::Min(20),
        ],
    )
    .header(
        Row::new(vec!["time", "", "project", "topic"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    // an explicit fg keeps dimmed (~) rows readable when highlighted —
    // their DarkGray fg on a DarkGray bg would be invisible
    .row_highlight_style(
        Style::default()
            .fg(Color::White)
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("▶ ");
    frame.render_stateful_widget(table, table_area, state);

    // the preview pane: where the session ended (last input, last reply,
    // branch-at-end, last activity) — a bounded tail read per selection
    if let (Some(area), Some(lines)) = (preview_area, preview) {
        let text: Vec<Line> = lines
            .iter()
            .map(|l| Line::styled(l.clone(), Style::default().fg(Color::Gray)))
            .collect();
        frame.render_widget(
            Paragraph::new(text).block(
                ratatui::widgets::Block::bordered()
                    .border_style(Style::default().fg(Color::DarkGray))
                    .title(" end of session "),
            ),
            area,
        );
    }

    let enter_hint = match model {
        Some(m) => format!("Enter resume via ccm {m}"),
        None => "Enter resume".to_string(),
    };
    let footer_line = Line::from(vec![
        Span::styled(
            " ↑↓ select · type filter · / content · Tab preview · ",
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(enter_hint, Style::default().fg(Color::Green)),
        Span::styled(
            " · Esc quit · ~ = cwd unknown",
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(Paragraph::new(footer_line), footer);
}

fn setup() -> Result<DefaultTerminal, Box<dyn std::error::Error>> {
    terminal::enable_raw_mode()?;
    crossterm::execute!(std::io::stdout(), EnterAlternateScreen)?;
    Ok(ratatui::init()) // hides the cursor
}

fn teardown() -> Result<(), Box<dyn std::error::Error>> {
    ratatui::restore(); // restores the cursor + leaves the alternate screen
    Ok(terminal::disable_raw_mode()?)
}

/// Shared with main.rs's plain listing so truncation cannot drift between
/// the two views. Cuts by DISPLAY width — a CJK char occupies two columns
/// and a char-counted cut left Chinese topics running far past the column
/// (the corpus median topic is 200 display columns wide).
pub(crate) fn truncate_chars(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }
    let mut cut = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = char_width(c);
        if w + cw > max.saturating_sub(1) {
            break;
        }
        cut.push(c);
        w += cw;
    }
    format!("{cut}…")
}

/// Terminal cell count of a char: East-Asian Wide/Fullwidth and emoji
/// blocks take two columns, everything else one. A pragmatic East-Asian
/// subset (no unicode-width dep) — good enough to keep columns honest.
/// Pure — pinned by tests.
pub(crate) fn char_width(c: char) -> usize {
    let u = c as u32;
    if matches!(u,
        0x1100..=0x115F          // Hangul Jamo
        | 0x2E80..=0x303E         // CJK Radicals .. CJK Symbols
        | 0x3041..=0x33FF         // Kana .. CJK Compat
        | 0x3400..=0x4DBF         // CJK Ext A
        | 0x4E00..=0x9FFF         // CJK Unified
        | 0xA000..=0xA4CF         // Yi
        | 0xAC00..=0xD7A3         // Hangul Syllables
        | 0xF900..=0xFAFF         // CJK Compat Ideographs
        | 0xFE30..=0xFE4F         // CJK Compat Forms
        | 0xFF00..=0xFF60         // Fullwidth Forms
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F       // emoji
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x2FFFD       // CJK Ext B..
        | 0x30000..=0x3FFFD
    ) {
        2
    } else {
        1
    }
}

/// Total display width of a string. Pure — pinned by tests.
pub(crate) fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

pub(crate) fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(cwd: &str, topic: &str) -> PickerRow {
        PickerRow {
            id: "abc123".into(),
            cwd: cwd.into(),
            reliable: true,
            topic: topic.into(),
            time: "10-13 21:00".into(),
            path: std::path::PathBuf::from("x.jsonl"),
        }
    }

    #[test]
    fn filter_matches_id_cwd_and_topic_case_insensitively() {
        let r = row("D:/Project/ccm", "给 Claude Code 写路由");
        assert!(matches_filter(&r, ""));
        assert!(matches_filter(&r, "ccm"));
        assert!(matches_filter(&r, "CCM"));
        assert!(matches_filter(&r, "路由"));
        assert!(matches_filter(&r, "abc123"));
        assert!(!matches_filter(&r, "zzz"));
    }

    #[test]
    fn filter_matches_the_cwd_as_displayed() {
        // raw Windows cwd carries backslashes but the table shows '/'
        let r = row("D:\\Project\\ccm", "topic");
        assert!(matches_filter(&r, "d:/project"));
        assert!(matches_filter(&r, "D:\\Project"));
        assert!(matches_filter(&r, "project/ccm"));
    }

    #[test]
    fn truncation_counts_display_width_not_chars() {
        // 8 CJK chars = 16 columns: a 10-wide cut keeps 4 chars + ellipsis
        assert_eq!(truncate_chars("一二三四五六七八", 10), "一二三四…");
        assert_eq!(truncate_chars("一二三四", 10), "一二三四");
        // ASCII is unaffected
        assert_eq!(truncate_chars("abcdef", 4), "abc…");
        assert_eq!(display_width("你好ab"), 6);
        assert_eq!(display_width("🎬"), 2); // emoji occupies two cells
        assert_eq!(display_width("a"), 1);
    }

    #[test]
    fn selection_clamps_into_range() {
        assert_eq!(clamp_sel(0, 3), 0);
        assert_eq!(clamp_sel(2, 3), 2);
        assert_eq!(clamp_sel(9, 3), 2);
        assert_eq!(clamp_sel(0, 0), 0); // caller guards empty separately
    }

    #[test]
    fn shift_and_locks_type_but_ctrl_alt_combos_do_not() {
        // Shift+A arrives as Char('A') + SHIFT — must type
        assert!(typeable_modifiers(KeyModifiers::SHIFT));
        assert!(typeable_modifiers(KeyModifiers::NONE));
        // caps-lock uppercase arrives as a plain Char('A') with NONE —
        // crossterm 0.29 carries no lock-state modifiers to admit here
        // Ctrl/Alt combos must not inject their bare control char
        assert!(!typeable_modifiers(KeyModifiers::CONTROL));
        assert!(!typeable_modifiers(KeyModifiers::ALT));
        assert!(!typeable_modifiers(KeyModifiers::SUPER));
        assert!(!typeable_modifiers(KeyModifiers::META));
        assert!(!typeable_modifiers(
            KeyModifiers::SHIFT | KeyModifiers::CONTROL
        ));
        assert!(!typeable_modifiers(
            KeyModifiers::CONTROL | KeyModifiers::ALT
        ));
    }
}
