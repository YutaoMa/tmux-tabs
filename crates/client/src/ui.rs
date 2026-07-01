use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, Paragraph};
use tmux_tabs_common::{AgentStatus, PrState, SessionEntry};

use crate::app::{App, Mode};

const TEXT_DIM: Color = Color::DarkGray;
const COLOR_ATTENTION: Color = Color::Rgb(255, 165, 0);
const COLOR_SPINNER: Color = Color::Green;
const COLOR_BRANCH: Color = Color::Cyan;
const COLOR_PR_OPEN: Color = Color::Green;
const COLOR_PR_DRAFT: Color = Color::DarkGray;
const COLOR_PR_MERGED: Color = Color::Magenta;
const COLOR_PR_CLOSED: Color = Color::Red;
const COLOR_CURRENT: Color = Color::Green;

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
pub const SPINNER_PERIOD_MS: u64 = 100;

/// Superscript digits 1-9 for the sidebar session-index prefix.
const SUPERSCRIPT_DIGITS: &[&str] = &["¹", "²", "³", "⁴", "⁵", "⁶", "⁷", "⁸", "⁹"];

/// Width budget reserved for the PR label after the `#N` segment. Sized for the
/// longest label string ("merged" / "closed").
const PR_LABEL_WIDTH: usize = 6;

pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();

    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(area);

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
        items.push(render_card(entry, i, is_current, is_selected, width, spinner));
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
    if let Some(gl) = git_line(entry, width) {
        lines.push(gl);
    }
    if let Some(bl) = browser_line(entry) {
        lines.push(bl);
    }
    lines.push(topic_line(entry, width));
    lines.push(status_line(entry, width, spinner));

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
        spans.push(Span::styled(branch_display, Style::default().fg(COLOR_BRANCH)));
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
    match &app.mode {
        Mode::Normal => {
            let has_browser = app.sessions.iter().any(|e| e.browser.is_some());
            let text = if has_browser {
                " j/k ↵:switch r:rename          []"
            } else {
                " j/k ↵:switch r:rename"
            };
            let p = Paragraph::new(text).style(Style::default().fg(Color::DarkGray));
            frame.render_widget(p, area);
        }
        Mode::Rename { input, .. } => {
            let prompt = format!(" Rename: {input}█");
            let p = Paragraph::new(prompt).style(Style::default().fg(Color::Cyan));
            frame.render_widget(p, area);
        }
    }
}
