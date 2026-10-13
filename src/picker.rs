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

/// One selectable session row. Built by main's scan; the picker owns it
/// for the duration of the interaction.
pub struct PickerRow {
    pub id: String,
    pub cwd: String,
    pub reliable: bool,
    pub topic: String,
    /// preformatted "MM-DD HH:mm" (local) — date math stays in the scan
    pub time: String,
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
/// topic, case-insensitive. Pure — pinned by tests.
pub fn matches_filter(row: &PickerRow, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let needle = needle.to_lowercase();
    row.id.to_lowercase().contains(&needle)
        || row.cwd.to_lowercase().contains(&needle)
        || row.topic.to_lowercase().contains(&needle)
}

/// Selection clamped into [0, len). Zero-length handled by the caller.
/// Pure — pinned by tests.
pub fn clamp_sel(sel: usize, len: usize) -> usize {
    if len == 0 { 0 } else { sel.min(len - 1) }
}

/// Run the picker on a TTY stdout. A piped stdout reports Failed so the
/// caller can list plainly.
pub fn run(rows: &[PickerRow], model: Option<&str>) -> Pick {
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

    let result = pick(&mut terminal, rows, model);

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

fn pick(terminal: &mut DefaultTerminal, rows: &[PickerRow], model: Option<&str>) -> Option<usize> {
    let mut filter = String::new();
    let mut filtered = refilter(rows, &filter);
    // The TableState lives HERE (not in the render fn) so its scroll
    // offset survives frames — ratatui's own state.rs contract.
    let mut state = TableState::default().with_selected(0);
    loop {
        let sel = clamp_sel(state.selected().unwrap_or(0), filtered.len());
        state.select(Some(sel));

        terminal
            .draw(|frame| draw(frame, rows, &filtered, &mut state, &filter, model))
            .ok()?;

        // Windows emits TWO Key events per physical press (Press + Release);
        // without this guard every key would act twice.
        match event::read() {
            Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Esc => return None,
                // plain q quits only with an empty filter, so the letter
                // stays typeable; Esc/Ctrl-C always abort
                KeyCode::Char('q') if key.modifiers.is_empty() && filter.is_empty() => {
                    return None;
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return None;
                }
                KeyCode::Char('j') if key.modifiers.is_empty() => {
                    if !filtered.is_empty() {
                        state.select(Some(clamp_sel(sel + 1, filtered.len())));
                    }
                }
                KeyCode::Char('k') if key.modifiers.is_empty() => {
                    state.select(Some(sel.saturating_sub(1)));
                }
                KeyCode::Up => {
                    state.select(Some(sel.saturating_sub(1)));
                }
                KeyCode::Down => {
                    if !filtered.is_empty() {
                        state.select(Some(clamp_sel(sel + 1, filtered.len())));
                    }
                }
                KeyCode::PageUp => {
                    let page = 10.min(sel);
                    state.select(Some(sel - page));
                }
                KeyCode::PageDown => {
                    if !filtered.is_empty() {
                        state.select(Some(clamp_sel(sel + 10, filtered.len())));
                    }
                }
                KeyCode::Home => state.select(Some(0)),
                KeyCode::End => {
                    if !filtered.is_empty() {
                        state.select(Some(filtered.len() - 1));
                    }
                }
                KeyCode::Backspace => {
                    filter.pop();
                    filtered = refilter(rows, &filter);
                }
                KeyCode::Enter => {
                    if let Some(&row_idx) = filtered.get(sel) {
                        return Some(row_idx);
                    }
                }
                // unmodified typing only — Ctrl/Alt combos must not inject
                // bare letters into the filter
                KeyCode::Char(c) if key.modifiers.is_empty() => {
                    filter.push(c);
                    filtered = refilter(rows, &filter);
                }
                _ => {}
            },
            Ok(_) => {} // Release events, resize, mouse: redraw at loop top
            Err(_) => return None,
        }
    }
}

fn draw(
    frame: &mut ratatui::Frame,
    rows: &[PickerRow],
    filtered: &[usize],
    state: &mut TableState,
    filter: &str,
    model: Option<&str>,
) {
    let [header, table_area, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let shown = if filtered.is_empty() {
        " · no matches".to_string()
    } else {
        format!(" · {}/{}", filtered.len(), rows.len())
    };
    let route = model.map(|m| format!(" · ccm: {m}")).unwrap_or_default();
    let header_line = Line::from(vec![
        Span::styled(" ccs", Style::default().fg(Color::Cyan).bold()),
        Span::raw(shown),
        Span::raw(route),
        Span::raw(" · filter: "),
        Span::styled(format!("{filter}█"), Style::default().fg(Color::Yellow)),
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

    let enter_hint = match model {
        Some(m) => format!("Enter resume via ccm {m}"),
        None => "Enter resume".to_string(),
    };
    let footer_line = Line::from(vec![
        Span::styled(
            " ↑↓/jk select · type filter · ⌫ edit · ",
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
/// the two views.
pub(crate) fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
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
    fn selection_clamps_into_range() {
        assert_eq!(clamp_sel(0, 3), 0);
        assert_eq!(clamp_sel(2, 3), 2);
        assert_eq!(clamp_sel(9, 3), 2);
        assert_eq!(clamp_sel(0, 0), 0); // caller guards empty separately
    }
}
