use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, Paragraph};
use tmux_tabs_common::{AgentStatus, PrState, SessionEntry};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Link, Mode};

const TEXT_DIM: Color = Color::DarkGray;
const COLOR_ATTENTION: Color = Color::Rgb(255, 165, 0);
const COLOR_SPINNER: Color = Color::Green;
const COLOR_BRANCH: Color = Color::Cyan;
const COLOR_PR_OPEN: Color = Color::Green;
const COLOR_PR_DRAFT: Color = Color::DarkGray;
const COLOR_PR_MERGED: Color = Color::Magenta;
const COLOR_PR_CLOSED: Color = Color::Red;
const COLOR_CURRENT: Color = Color::Green;
/// External blockers are deliberately *not* [`COLOR_ATTENTION`]: orange means
/// "the agent needs you, go here", while a blocker means the opposite —
/// you're waiting on someone else, so don't bother going back.
const COLOR_BLOCKED: Color = Color::Rgb(170, 130, 255);

pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
pub const SPINNER_PERIOD_MS: u64 = 100;

/// Superscript digits 1-9 for the sidebar session-index prefix.
const SUPERSCRIPT_DIGITS: &[&str] = &["¹", "²", "³", "⁴", "⁵", "⁶", "⁷", "⁸", "⁹"];

/// Rows the blocker overlay occupies below the header: top border, two lines
/// of note, bottom border.
const BLOCKER_ROWS: usize = 4;
const BLOCKER_NOTE_LINES: usize = 2;
const BLOCKER_LABEL: &str = "BLOCKED";
/// Marks that at least one session has a Chrome tab group attached.
const BROWSER_INDICATOR: &str = "[]";

/// Narrowest pane that still fits `╭ BLOCKED ─✕╮`. Below this the overlay
/// drops the box rather than emit broken borders.
const BLOCKER_MIN_BOX_WIDTH: usize = BLOCKER_LABEL.len() + 6;

/// Note length accepted at the prompt, in terminal columns, sized to the two
/// note lines of a default 24-column sidebar (2 × 20). Capping input rather
/// than the rendered string keeps the prompt and the card in agreement about
/// how much text is worth typing; word wrapping can still elide a note that
/// fits the budget but not the line breaks, which [`wrap_note`] marks with an
/// ellipsis.
pub const BLOCKER_MAX_CELLS: usize = 40;

/// Cut a note down to [`BLOCKER_MAX_CELLS`]. Deliberately does not trim: it
/// runs on every keystroke at the prompt, where eating a trailing space would
/// make spaces impossible to type.
pub fn truncate_note(note: &str) -> String {
    split_at_width(note, BLOCKER_MAX_CELLS).0
}

/// Width budget reserved for the PR label after the `#N` segment. Sized for the
/// longest label string ("merged" / "closed").
const PR_LABEL_WIDTH: usize = 6;

pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();

    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);

    render_sessions(frame, app, chunks[0]);
    render_footer(frame, app, chunks[1]);
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    let mut end = max_len.saturating_sub(1);
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Trim whitespace, keep the first line, and truncate with an ellipsis.
fn summarize(s: &str, max_len: usize) -> String {
    let s = s.trim();
    let first_line = s.split('\n').next().unwrap_or(s);
    truncate(first_line, max_len)
}

/// Display width of a string in terminal columns.
///
/// Blocker notes are free text, so they can contain CJK or emoji that occupy
/// two columns. Counting chars (or bytes) would let those silently push the
/// overlay's borders out of alignment.
fn cells(s: &str) -> usize {
    s.width()
}

/// Split `s` at the last char boundary that keeps its display width within
/// `max` columns, returning the head and the remainder.
///
/// A double-width char that would straddle the limit is pushed to the
/// remainder rather than split, so the head never overruns `max`.
fn split_at_width(s: &str, max: usize) -> (String, String) {
    let mut head = String::new();
    let mut used = 0;
    let mut rest = String::new();
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if rest.is_empty() && used + w <= max {
            used += w;
            head.push(c);
        } else {
            rest.push(c);
        }
    }
    (head, rest)
}

/// Rows a card occupies, excluding the divider beneath it. Shared with mouse
/// hit-testing in `input.rs`: if the two ever disagree, clicks land on the
/// wrong session.
pub fn card_height(entry: &SessionEntry) -> usize {
    if entry.blocker.is_some() {
        return 1 + BLOCKER_ROWS;
    }
    let has_git = entry.git.branch.is_some() || entry.git.pr_number.is_some();
    3 + usize::from(has_git) + usize::from(entry.browser.is_some())
}

/// Whether `(row, column)` — relative to the top-left of a blocked card — hits
/// the overlay's `✕`. Owning the test here keeps it honest about the glyph
/// actually being drawn: narrow panes fall back to a boxless overlay, and a
/// click target with nothing under it would be a trap.
pub fn blocker_close_hit(width: usize, row: usize, column: usize) -> bool {
    width >= BLOCKER_MIN_BOX_WIDTH && row == 1 && column == width - 2
}

/// Word-wrap `note` into at most `max_lines` lines of `width` columns,
/// hard-splitting words too long to fit and ellipsizing any overflow.
fn wrap_note(note: &str, width: usize, max_lines: usize) -> Vec<String> {
    if width == 0 || max_lines == 0 {
        return Vec::new();
    }
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in note.split_whitespace() {
        let mut word = word.to_string();
        // A word wider than the line can never be placed by wrapping alone,
        // so break it across as many lines as it needs.
        while cells(&word) > width {
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            let (head, rest) = split_at_width(&word, width);
            // A single char wider than the whole line would loop forever.
            if head.is_empty() {
                break;
            }
            lines.push(head);
            word = rest;
        }
        if cur.is_empty() {
            cur = word;
        } else if cells(&cur) + 1 + cells(&word) <= width {
            cur.push(' ');
            cur.push_str(&word);
        } else {
            lines.push(std::mem::replace(&mut cur, word));
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        if let Some(last) = lines.last_mut() {
            let (mut head, _) = split_at_width(last, width.saturating_sub(1));
            head.push('…');
            *last = head;
        }
    }
    lines
}

/// The blocker overlay: a bordered box covering everything below the header,
/// so a session parked on an external dependency reads as a different kind of
/// object rather than one more line of text.
///
/// `agent_waiting` surfaces a `?` on the bottom border — the overlay hides the
/// status line, so without it a permission prompt raised while you were away
/// would be invisible.
fn blocker_overlay(note: &str, width: usize, agent_waiting: bool) -> Vec<Line<'static>> {
    let style = Style::default().fg(COLOR_BLOCKED);

    // Too narrow for a box: degrade to plain marked lines rather than emit
    // broken borders.
    if width < BLOCKER_MIN_BOX_WIDTH {
        let label: String = BLOCKER_LABEL.chars().take(width).collect();
        let mut lines = vec![Line::from(Span::styled(label, style))];
        lines.extend(
            wrap_note(note, width, BLOCKER_ROWS - 1)
                .into_iter()
                .map(|l| Line::from(Span::styled(l, style))),
        );
        lines.resize(BLOCKER_ROWS, Line::from(""));
        return lines;
    }

    let left = format!("╭ {BLOCKER_LABEL} ");
    let top = format!(
        "{left}{}✕╮",
        "─".repeat(width.saturating_sub(cells(&left) + 2))
    );

    let inner = width - 4;
    let mut lines = vec![Line::from(Span::styled(top, style))];
    let mut body = wrap_note(note, inner, BLOCKER_NOTE_LINES);
    body.resize(BLOCKER_NOTE_LINES, String::new());
    for text in body {
        let pad = " ".repeat(inner.saturating_sub(cells(&text)));
        lines.push(Line::from(Span::styled(format!("│ {text}{pad} │"), style)));
    }

    let bottom_left = if agent_waiting { "╰ ?" } else { "╰" };
    let bottom = format!(
        "{bottom_left}{}╯",
        "─".repeat(width.saturating_sub(cells(bottom_left) + 1))
    );
    lines.push(Line::from(Span::styled(bottom, style)));
    lines
}

fn spinner_frame() -> &'static str {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    SPINNER_FRAMES[((ms / u128::from(SPINNER_PERIOD_MS)) as usize) % SPINNER_FRAMES.len()]
}

fn render_sessions(frame: &mut Frame, app: &App, area: Rect) {
    let width = area.width as usize;
    let last_index = app.sessions.len().saturating_sub(1);
    let spinner = spinner_frame();

    let mut items: Vec<ListItem> = Vec::with_capacity(app.sessions.len() * 2);

    for (i, entry) in app.sessions.iter().enumerate() {
        let is_current = entry.session.name == app.current_session;
        let is_selected = app.selected == Some(i);
        items.push(render_card(
            entry,
            i,
            is_current,
            is_selected,
            width,
            spinner,
        ));
        if i < last_index {
            items.push(divider_item(width, TEXT_DIM));
        }
    }

    let list = List::new(items);
    frame.render_widget(list, area);
}

fn divider_item(width: usize, color: Color) -> ListItem<'static> {
    let rule: String = "─".repeat(width);
    ListItem::new(Line::from(Span::styled(rule, Style::default().fg(color))))
}

fn render_card(
    entry: &SessionEntry,
    index: usize,
    is_current: bool,
    is_selected: bool,
    width: usize,
    spinner: &'static str,
) -> ListItem<'static> {
    let mut name_style = Style::default();
    if is_current {
        name_style = name_style.add_modifier(Modifier::BOLD).fg(COLOR_CURRENT);
    }
    if is_selected {
        name_style = name_style.add_modifier(Modifier::REVERSED);
    }

    // Reserve 1 cell for the superscript index so layout stays aligned
    // even when the session count exceeds 9 (those get a blank).
    let index_prefix = SUPERSCRIPT_DIGITS.get(index).copied().unwrap_or(" ");
    let prefix_cells = 1;
    let pct_str = entry.context_pct.map(|p| format!("{p}%"));
    let pct_len = pct_str.as_deref().map_or(0, str::len);
    let trailing_reserve = if pct_len > 0 { pct_len + 1 } else { 0 };
    let name_max = width.saturating_sub(prefix_cells + trailing_reserve);
    let name = truncate(&entry.session.name, name_max);
    let pad = width.saturating_sub(prefix_cells + name.len() + pct_len);

    let mut header_spans = vec![
        Span::styled(index_prefix, Style::default().fg(TEXT_DIM)),
        Span::styled(name, name_style),
    ];
    header_spans.push(Span::raw(" ".repeat(pad)));
    if let Some(p) = pct_str {
        header_spans.push(Span::styled(p, Style::default().fg(TEXT_DIM)));
    }
    let header_line = Line::from(header_spans);

    let mut lines = vec![header_line];
    if let Some(note) = &entry.blocker {
        let agent_waiting = matches!(entry.agent, AgentStatus::WaitingForInput { .. });
        lines.extend(blocker_overlay(note, width, agent_waiting));
    } else {
        if let Some(gl) = git_line(entry, width) {
            lines.push(gl);
        }
        if let Some(bl) = browser_line(entry) {
            lines.push(bl);
        }
        lines.push(topic_line(entry, width));
        lines.push(status_line(entry, width, spinner));
    }

    ListItem::new(lines)
}

fn git_line(entry: &SessionEntry, width: usize) -> Option<Line<'static>> {
    let has_git = entry.git.branch.is_some() || entry.git.pr_number.is_some();
    if !has_git {
        return None;
    }
    let mut spans = Vec::new();
    if let Some(branch) = &entry.git.branch {
        let pr_width = entry
            .git
            .pr_number
            .map_or(0, |n| format!(" #{n} ").len() + PR_LABEL_WIDTH);
        let branch_max = width.saturating_sub(pr_width);
        let branch_display = summarize(branch, branch_max);
        spans.push(Span::styled(
            branch_display,
            Style::default().fg(COLOR_BRANCH),
        ));
    }
    if let Some(num) = entry.git.pr_number {
        let (pr_color, label) = match entry.git.pr_state {
            Some(PrState::Open) => (COLOR_PR_OPEN, "open"),
            Some(PrState::Draft) => (COLOR_PR_DRAFT, "draft"),
            Some(PrState::Merged) => (COLOR_PR_MERGED, "merged"),
            Some(PrState::Closed) => (COLOR_PR_CLOSED, "closed"),
            None => (TEXT_DIM, ""),
        };
        spans.push(Span::styled(
            format!(" #{num} {label}"),
            Style::default().fg(pr_color),
        ));
    }
    Some(Line::from(spans))
}

fn browser_line(entry: &SessionEntry) -> Option<Line<'static>> {
    entry.browser.as_ref().map(|b| {
        Line::from(vec![Span::styled(
            format!("[] {} tabs", b.tab_count),
            Style::default().fg(TEXT_DIM),
        )])
    })
}

fn topic_line(entry: &SessionEntry, width: usize) -> Line<'static> {
    if let Some(topic) = &entry.topic {
        let text = summarize(topic, width);
        Line::from(vec![Span::styled(text, Style::default().fg(TEXT_DIM))])
    } else {
        Line::from("")
    }
}

/// Status line priority: question (orange) > spinner+activity (processing) > blank.
fn status_line(entry: &SessionEntry, width: usize, spinner: &'static str) -> Line<'static> {
    match &entry.agent {
        AgentStatus::WaitingForInput { question } => {
            let Some(q) = question else {
                return Line::from("");
            };
            let text = summarize(q, width.saturating_sub(2));
            Line::from(vec![Span::styled(
                format!("? {text}"),
                Style::default().fg(COLOR_ATTENTION),
            )])
        }
        AgentStatus::Processing { activity } => {
            let activity = activity.as_deref().unwrap_or("");
            let text = summarize(activity, width.saturating_sub(2));
            let mut spans = vec![Span::styled(spinner, Style::default().fg(COLOR_SPINNER))];
            if !text.is_empty() {
                spans.push(Span::styled(
                    format!(" {text}"),
                    Style::default().fg(TEXT_DIM),
                ));
            }
            Line::from(spans)
        }
        AgentStatus::None => Line::from(""),
    }
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    if let Mode::Rename { input, .. } = &app.mode {
        let prompt = format!(" Rename: {input}█");
        let p = Paragraph::new(prompt).style(Style::default().fg(Color::Cyan));
        frame.render_widget(p, area);
        return;
    }

    if let Mode::Blocker { input, .. } = &app.mode {
        let prompt = format!(" Blocked: {input}█");
        let p = Paragraph::new(prompt).style(Style::default().fg(COLOR_BLOCKED));
        frame.render_widget(p, area);
        return;
    }

    // A dropped link leaves stale sessions on screen, so say so rather than
    // showing them as current.
    let notice = match app.link {
        Link::Up => None,
        Link::Connecting => Some(" ⋯ connecting to server…"),
        Link::Down => Some(" ⚠ server offline — retrying…"),
    };
    if let Some(notice) = notice {
        let p = Paragraph::new(notice).style(Style::default().fg(COLOR_ATTENTION));
        frame.render_widget(p, area);
        return;
    }

    // `b` toggles, so the hint has to name the direction it will actually go.
    let b_hint = if app.target_session_is_blocked() {
        "b:clear"
    } else {
        "b:block"
    };
    let has_browser = app.sessions.iter().any(|e| e.browser.is_some());
    let text = footer_hints(b_hint, has_browser, area.width as usize);
    let p = Paragraph::new(text).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(p, area);
}

/// Assemble the footer hints, dropping whole hints that don't fit.
///
/// Panes get resized, and a hint clipped mid-word (`b:blo`) reads as a bug
/// rather than as a pane that's simply too narrow.
fn footer_hints(b_hint: &str, has_browser: bool, width: usize) -> String {
    let mut text = String::from(" ");
    for hint in ["j/k", "↵:go", "r:name", b_hint] {
        let sep = usize::from(cells(&text) > 1);
        if cells(&text) + sep + cells(hint) > width {
            break;
        }
        if sep == 1 {
            text.push(' ');
        }
        text.push_str(hint);
    }
    // The browser indicator is right-aligned, and only earns its place once
    // the hints have taken theirs. The strict `<` leaves at least one column
    // of gap so it never abuts the last hint.
    let used = cells(&text) + cells(BROWSER_INDICATOR);
    if has_browser && used < width {
        text.push_str(&" ".repeat(width - used));
        text.push_str(BROWSER_INDICATOR);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use tmux_tabs_common::{GitInfo, TmuxSession};

    fn plain(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn entry(blocker: Option<&str>) -> SessionEntry {
        SessionEntry {
            session: TmuxSession {
                id: "$0".into(),
                name: "api-gateway".into(),
                windows: 1,
                attached: true,
                activity: 0,
                cwd: None,
            },
            agent: AgentStatus::None,
            topic: None,
            context_pct: None,
            git: GitInfo::default(),
            browser: None,
            blocker: blocker.map(str::to_string),
        }
    }

    #[test]
    fn wrap_note_breaks_on_word_boundaries() {
        assert_eq!(
            wrap_note("waiting on @jane re: cookie ttl", 20, 2),
            vec!["waiting on @jane re:", "cookie ttl"]
        );
    }

    #[test]
    fn wrap_note_hard_splits_words_longer_than_the_line() {
        assert_eq!(
            wrap_note("supercalifragilistic", 8, 3),
            vec!["supercal", "ifragili", "stic"]
        );
    }

    #[test]
    fn wrap_note_ellipsizes_beyond_the_line_budget() {
        let lines = wrap_note("one two three four five six seven eight", 10, 2);
        assert_eq!(lines.len(), 2);
        assert!(
            lines[1].ends_with('…'),
            "overflow must be visible, got {lines:?}"
        );
        assert!(lines.iter().all(|l| cells(l) <= 10));
    }

    #[test]
    fn blocker_overlay_matches_the_sidebar_width_exactly() {
        let lines = blocker_overlay("waiting on @jane re: cookie ttl", 24, false);
        assert_eq!(lines.len(), BLOCKER_ROWS);
        for line in &lines {
            assert_eq!(cells(&plain(line)), 24, "ragged line: {:?}", plain(line));
        }
        assert_eq!(plain(&lines[0]), "╭ BLOCKED ────────────✕╮");
        assert_eq!(plain(&lines[1]), "│ waiting on @jane re: │");
        assert_eq!(plain(&lines[2]), "│ cookie ttl           │");
        assert_eq!(plain(&lines[3]), "╰──────────────────────╯");
    }

    #[test]
    fn blocker_overlay_keeps_a_fixed_height_for_a_short_note() {
        let lines = blocker_overlay("ping bob", 24, false);
        assert_eq!(lines.len(), BLOCKER_ROWS);
        assert_eq!(plain(&lines[2]), "│                      │");
    }

    /// The overlay hides the status line, so a prompt raised while the user was
    /// away must still be advertised or the blocker becomes a blindfold.
    #[test]
    fn blocker_overlay_flags_an_agent_that_is_also_waiting() {
        let lines = blocker_overlay("ping bob", 24, true);
        let bottom = plain(&lines[3]);
        assert!(bottom.starts_with("╰ ?"), "got {bottom}");
        assert_eq!(cells(&bottom), 24);
    }

    #[test]
    fn blocker_overlay_survives_a_pane_too_narrow_for_a_box() {
        for width in 0..12 {
            let lines = blocker_overlay("waiting on jane", width, false);
            assert_eq!(lines.len(), BLOCKER_ROWS, "width {width}");
            for line in &lines {
                assert!(cells(&plain(line)) <= width.max(7), "width {width}");
            }
        }
    }

    #[test]
    fn blocker_overlay_handles_multibyte_notes_without_ragged_borders() {
        let lines = blocker_overlay("café ✕ naïve résumé blocké", 24, false);
        for line in &lines {
            assert_eq!(cells(&plain(line)), 24, "ragged line: {:?}", plain(line));
        }
    }

    #[test]
    fn a_note_at_the_input_cap_is_fully_visible() {
        let note = "a".repeat(BLOCKER_MAX_CELLS);
        let lines = blocker_overlay(&note, 24, false);
        let shown: String = lines[1..3].iter().map(plain).collect();
        assert!(!shown.contains('…'), "capped notes must not be elided");
    }

    #[test]
    fn card_height_is_fixed_while_blocked_regardless_of_git_or_browser() {
        let mut e = entry(Some("ping bob"));
        assert_eq!(card_height(&e), 1 + BLOCKER_ROWS);
        e.git.branch = Some("main".into());
        e.browser = Some(tmux_tabs_common::BrowserInfo {
            tab_count: 3,
            collapsed: false,
        });
        assert_eq!(card_height(&e), 1 + BLOCKER_ROWS);
    }

    #[test]
    fn card_height_grows_with_optional_rows_when_unblocked() {
        let mut e = entry(None);
        assert_eq!(card_height(&e), 3);
        e.git.branch = Some("main".into());
        assert_eq!(card_height(&e), 4);
    }

    #[test]
    fn close_affordance_sits_under_the_rendered_glyph() {
        let lines = blocker_overlay("ping bob", 24, false);
        let top: Vec<char> = plain(&lines[0]).chars().collect();
        let hit = (0..24)
            .find(|&c| blocker_close_hit(24, 1, c))
            .expect("a hit column");
        assert_eq!(top[hit], '✕');
    }

    /// The boxless fallback draws no ✕, so nothing in that pane may behave
    /// like one — an invisible click target that wipes state is worse than no
    /// affordance at all.
    #[test]
    fn no_close_target_exists_where_no_glyph_is_drawn() {
        for width in 0..BLOCKER_MIN_BOX_WIDTH {
            for column in 0..width.max(1) {
                assert!(
                    !blocker_close_hit(width, 1, column),
                    "phantom ✕ at width {width}, column {column}"
                );
            }
        }
    }

    /// Word boundaries can waste cells, so a note within the input cap is not
    /// guaranteed to survive wrapping intact. That's fine — but it has to be
    /// visibly elided rather than silently cut.
    #[test]
    fn a_capped_note_that_wraps_badly_is_elided_not_truncated_silently() {
        let note = format!("{} {}", "a".repeat(11), "b".repeat(28));
        assert_eq!(note.chars().count(), BLOCKER_MAX_CELLS);
        let lines = blocker_overlay(&note, 24, false);
        let shown: String = lines[1..3].iter().map(plain).collect();
        assert!(shown.contains('…'), "overflow must be marked: {shown:?}");
        for line in &lines {
            assert_eq!(cells(&plain(line)), 24);
        }
    }

    /// Notes come from a free-text prompt, so nothing about their shape is
    /// guaranteed. None of these may panic or break the borders.
    #[test]
    fn hostile_notes_neither_panic_nor_break_the_box() {
        let notes = [
            "",
            "   ",
            "\t\n",
            "…",
            "🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂",
            &"x".repeat(500),
            "a\u{7}b",
            "ありがとうございます",
        ];
        for note in notes {
            for width in 0..40 {
                let lines = blocker_overlay(note, width, width % 2 == 0);
                assert_eq!(lines.len(), BLOCKER_ROWS, "note {note:?} width {width}");
                if width >= BLOCKER_MIN_BOX_WIDTH {
                    for line in &lines {
                        assert_eq!(
                            cells(&plain(line)),
                            width,
                            "note {note:?} width {width} line {:?}",
                            plain(line)
                        );
                    }
                }
            }
        }
    }

    /// End-to-end through the real render path: the overlay must land on the
    /// actual terminal buffer, not just in the line builder.
    #[test]
    fn full_render_draws_the_overlay_at_sidebar_width() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = App::new();
        app.width = 24;
        app.current_session = "api-gateway".into();
        app.link = crate::app::Link::Up;
        let mut e = entry(Some("waiting on @jane re: cookie ttl"));
        e.context_pct = Some(45);
        app.sessions = vec![e];

        let mut terminal = Terminal::new(TestBackend::new(24, 10)).expect("terminal");
        terminal.draw(|f| render(f, &app)).expect("draw");

        let rows: Vec<String> = terminal
            .backend()
            .buffer()
            .content()
            .chunks(24)
            .map(|row| row.iter().map(ratatui::buffer::Cell::symbol).collect())
            .collect();

        assert_eq!(rows[1], "╭ BLOCKED ────────────✕╮");
        assert_eq!(rows[2], "│ waiting on @jane re: │");
        assert_eq!(rows[3], "│ cookie ttl           │");
        assert_eq!(rows[4], "╰──────────────────────╯");
        assert!(
            rows[0].contains("api-gateway"),
            "header must stay legible: {:?}",
            rows[0]
        );
        assert!(
            rows.last().unwrap().contains("b:clear"),
            "footer must offer the inverse action: {:?}",
            rows.last()
        );
    }

    /// Regression: counting chars instead of columns let a double-width glyph
    /// push the right border past the pane edge.
    #[test]
    fn double_width_glyphs_do_not_widen_the_box() {
        for note in ["🙂 ping bob", "確認中 @jane", "a🙂b🙂c🙂d🙂e🙂f🙂g🙂h🙂"]
        {
            let lines = blocker_overlay(note, 24, false);
            for line in &lines {
                let text = plain(line);
                assert_eq!(cells(&text), 24, "note {note:?} line {text:?}");
                assert!(
                    text.starts_with(['╭', '│', '╰']) && text.ends_with(['╮', '│', '╯']),
                    "borders lost on {text:?}"
                );
            }
        }
    }

    /// The prompt measures the same way the card does, so a wide glyph costs
    /// what it costs on screen.
    #[test]
    fn the_input_budget_is_measured_in_columns() {
        assert_eq!(cells(&truncate_note(&"a".repeat(80))), BLOCKER_MAX_CELLS);
        let wide = truncate_note(&"🙂".repeat(80));
        assert_eq!(cells(&wide), BLOCKER_MAX_CELLS);
        assert_eq!(wide.chars().count(), BLOCKER_MAX_CELLS / 2);
    }

    /// Spaces are typed, not trimmed, while the prompt is still open.
    #[test]
    fn truncating_live_input_keeps_trailing_spaces() {
        assert_eq!(truncate_note("ask jane "), "ask jane ");
    }

    /// Panes get resized; the footer must degrade by dropping hints, never by
    /// slicing one in half.
    #[test]
    fn footer_hints_drop_whole_hints_rather_than_clipping() {
        for width in 0..40 {
            for has_browser in [false, true] {
                let text = footer_hints("b:block", has_browser, width);
                assert!(
                    cells(&text) <= width.max(1),
                    "footer overflows at width {width}: {text:?}"
                );
                for hint in text.split_whitespace() {
                    assert!(
                        ["j/k", "↵:go", "r:name", "b:block", BROWSER_INDICATOR].contains(&hint),
                        "partial hint {hint:?} at width {width}"
                    );
                }
            }
        }
    }

    #[test]
    fn footer_shows_every_hint_at_the_default_width() {
        let text = footer_hints("b:block", false, 24);
        assert_eq!(text, " j/k ↵:go r:name b:block");
    }

    /// Regression: the previous footer was wider than the pane, so the browser
    /// indicator it advertised was never actually visible at 24 columns.
    #[test]
    fn the_browser_indicator_is_right_aligned_and_reachable() {
        let text = footer_hints("b:block", true, 30);
        assert!(text.ends_with(BROWSER_INDICATOR), "got {text:?}");
        assert_eq!(cells(&text), 30);
    }
}
