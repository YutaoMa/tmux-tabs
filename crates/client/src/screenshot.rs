//! Offline screenshot generator for the README.
//!
//! Renders the real sidebar UI (`crate::ui`) into an off-screen ratatui buffer
//! with curated demo data, then serialises that buffer to SVG. Nothing here
//! touches tmux or the server, so screenshots are deterministic and can be
//! regenerated without a live session:
//!
//! ```sh
//! cargo run -p tmux-tabs-client --features screenshots -- __screenshot docs/images
//! ```
//!
//! Compiled out unless the `screenshots` feature is enabled.

use std::fmt::Write as _;
use std::path::Path;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use tmux_tabs_common::{AgentStatus, BrowserInfo, GitInfo, PrState, SessionEntry, TmuxSession};

use crate::app::{App, Link, Mode};
use crate::ui;

/// Spinner frame baked into every screenshot so regenerating produces an
/// identical file instead of whatever the wall clock happened to land on.
const FIXED_SPINNER: &str = ui::SPINNER_FRAMES[2];

/// Sidebar width in columns, matching `SIDEBAR_WIDTH` in
/// `scripts/tmux-tabs-sidebar.sh` so the shots clip exactly like the real pane.
const SIDEBAR_COLS: u16 = 24;

// Terminal metrics, in CSS pixels.
const FONT_SIZE: f32 = 15.0;
const CELL_W: f32 = 9.0;
const CELL_H: f32 = 20.0;
const PAD_X: f32 = 14.0;
const PAD_Y: f32 = 12.0;
const TITLEBAR_H: f32 = 30.0;
const RADIUS: f32 = 8.0;
/// Baseline offset inside a cell (approximates a monospace font's ascent).
const BASELINE: f32 = 14.8;

const FONT_STACK: &str = "ui-monospace,'SF Mono','JetBrains Mono','Fira Code',Menlo,Consolas,'DejaVu Sans Mono',monospace";

// Palette — a neutral dark terminal theme that reads well on both GitHub
// light and dark pages.
const BG: &str = "#181b21";
const TITLEBAR_BG: &str = "#262b33";
const BORDER: &str = "#333a45";
const FG: &str = "#d7dbe2";
const TITLE_FG: &str = "#8b93a1";

pub fn run(out_dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(out_dir)?;

    // Hero: the sidebar alongside a real captured tmux pane.
    write_svg(
        out_dir,
        "window.svg",
        &window_svg(
            &render(&overview_app(), SIDEBAR_COLS, 28)?,
            &parse_ansi(DEMO_PANE, 76, 28),
            "tmux — ~/dev/tmux-tabs",
        ),
    )?;

    // Close-up of the card anatomy.
    write_svg(
        out_dir,
        "sidebar.svg",
        &to_svg(
            &render(&overview_app(), SIDEBAR_COLS, 24)?,
            Some("tmux-tabs"),
        ),
    )?;

    // `/grab`: a failing test in one pane, pulled into the pane next door.
    write_svg(
        out_dir,
        "grab.svg",
        &window_svg(
            &parse_ansi(GRAB_SOURCE, 56, 26),
            &parse_ansi(GRAB_AI, 69, 26),
            "tmux — ~/dev/api-gateway",
        ),
    )?;

    write_svg(
        out_dir,
        "sidebar-states.svg",
        &strip_svg(&[
            (&render(&select_app(), SIDEBAR_COLS, 19)?, "j/k — select"),
            (&render(&rename_app(), SIDEBAR_COLS, 19)?, "r — rename"),
            (&render(&offline_app(), SIDEBAR_COLS, 19)?, "server offline"),
        ]),
    )?;

    Ok(())
}

fn write_svg(dir: &Path, name: &str, svg: &str) -> anyhow::Result<()> {
    let path = dir.join(name);
    std::fs::write(&path, svg)?;
    println!("wrote {}", path.display());
    Ok(())
}

fn session(name: &str, attached: bool) -> TmuxSession {
    TmuxSession {
        id: format!("${name}"),
        name: name.to_string(),
        windows: 1,
        attached,
        activity: 0,
        cwd: None,
    }
}

/// Declarative description of one sidebar card.
#[derive(Default)]
struct Card<'a> {
    name: &'a str,
    attached: bool,
    branch: Option<&'a str>,
    pr: Option<(u32, PrState)>,
    tabs: Option<u32>,
    topic: Option<&'a str>,
    context_pct: Option<u8>,
    agent: AgentStatus,
}

impl Card<'_> {
    fn build(self) -> SessionEntry {
        SessionEntry {
            session: session(self.name, self.attached),
            agent: self.agent,
            topic: self.topic.map(str::to_string),
            context_pct: self.context_pct,
            git: GitInfo {
                branch: self.branch.map(str::to_string),
                pr_number: self.pr.map(|(number, _)| number),
                pr_state: self.pr.map(|(_, state)| state),
            },
            browser: self.tabs.map(|tab_count| BrowserInfo {
                tab_count,
                collapsed: false,
            }),
        }
    }
}

fn processing(activity: &str) -> AgentStatus {
    AgentStatus::Processing {
        activity: Some(activity.to_string()),
    }
}

fn waiting(question: &str) -> AgentStatus {
    AgentStatus::WaitingForInput {
        question: Some(question.to_string()),
    }
}

/// The hero shot: four sessions covering every card element — PR states,
/// Chrome tab counts, context usage, a processing spinner and a prompt that
/// needs an answer.
fn overview_app() -> App {
    App {
        sessions: vec![
            Card {
                name: "api-gateway",
                branch: Some("feat/rate-limiting"),
                pr: Some((482, PrState::Open)),
                tabs: Some(4),
                topic: Some("token bucket limiter"),
                context_pct: Some(38),
                agent: processing("Edit: middleware.rs"),
                ..Card::default()
            }
            .build(),
            Card {
                name: "tmux-tabs",
                attached: true,
                branch: Some("main"),
                tabs: Some(2),
                topic: Some("sidebar screenshots"),
                context_pct: Some(61),
                agent: waiting("Run cargo test?"),
                ..Card::default()
            }
            .build(),
            Card {
                name: "docs-site",
                branch: Some("fix/nav-overflow"),
                pr: Some((91, PrState::Draft)),
                topic: Some("mobile nav clipping"),
                ..Card::default()
            }
            .build(),
            Card {
                name: "tonic",
                branch: Some("xds-routing"),
                pr: Some((2210, PrState::Merged)),
                tabs: Some(3),
                topic: Some("backport route filters"),
                context_pct: Some(12),
                agent: processing("Bash: cargo test"),
                ..Card::default()
            }
            .build(),
        ],
        current_session: "tmux-tabs".to_string(),
        selected: None,
        mode: Mode::Normal,
        running: true,
        link: Link::Up,
    }
}

/// Three sessions with the third highlighted, as `j`/`k` leaves it.
fn select_app() -> App {
    let mut app = compact_app();
    app.selected = Some(2);
    app
}

fn rename_app() -> App {
    let mut app = compact_app();
    app.mode = Mode::Rename {
        session_name: "docs-site".to_string(),
        input: "docs-v2".to_string(),
    };
    app
}

fn offline_app() -> App {
    let mut app = compact_app();
    app.link = Link::Down;
    app
}

/// `overview_app` minus the trailing card, so the state strip fits three
/// sessions per panel.
fn compact_app() -> App {
    let mut app = overview_app();
    app.sessions.truncate(3);
    app
}

fn render(app: &App, width: u16, height: u16) -> anyhow::Result<Buffer> {
    let mut terminal = Terminal::new(TestBackend::new(width, height))?;
    terminal.draw(|f| ui::render(f, app))?;
    let mut buf = terminal.backend().buffer().clone();
    freeze_spinner(&mut buf);
    Ok(buf)
}

/// Pin the animated spinner to a single frame so output is byte-stable.
fn freeze_spinner(buf: &mut Buffer) {
    let area = buf.area;
    for y in 0..area.height {
        for x in 0..area.width {
            if let Some(cell) = buf.cell_mut((x, y))
                && ui::SPINNER_FRAMES.contains(&cell.symbol())
            {
                cell.set_symbol(FIXED_SPINNER);
            }
        }
    }
}

/// A real `tmux capture-pane -e` dump, used as the right-hand pane of the
/// full-window shot so the surrounding terminal content isn't fabricated.
const DEMO_PANE: &str = include_str!("../fixtures/demo-pane.ansi");

/// The two panes of the `/grab` shot: a failing test run, and the pane next
/// door that pulled it in with `tmux-tabs capture`.
const GRAB_SOURCE: &str = include_str!("../fixtures/grab-source.ansi");
const GRAB_AI: &str = include_str!("../fixtures/grab-ai.ansi");

/// Parse SGR-coloured text (as produced by `tmux capture-pane -p -e`) into a
/// fixed-size buffer. Handles the subset the fixtures contain — bold, dim,
/// reverse, the 16 named colours and 24-bit `38;2`/`48;2` — and skips any
/// other escape rather than printing it.
fn parse_ansi(text: &str, width: u16, height: u16) -> Buffer {
    let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
    let mut style = Style::default();

    for (y, line) in (0..height).zip(text.lines()) {
        let mut x: u16 = 0;
        let mut chars = line.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                let mut seq = String::new();
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for c in chars.by_ref() {
                        if c.is_ascii_alphabetic() {
                            if c == 'm' {
                                style = apply_sgr(style, &seq);
                            }
                            break;
                        }
                        seq.push(c);
                    }
                } else {
                    // Non-CSI escape (e.g. OSC): drop the introducer only.
                    chars.next();
                }
                continue;
            }
            if x >= width {
                break;
            }
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_symbol(&ch.to_string());
                cell.set_style(style);
            }
            x += 1;
        }
    }
    buf
}

fn apply_sgr(mut style: Style, params: &str) -> Style {
    let parts: Vec<u8> = params
        .split(';')
        .map(|p| p.parse::<u8>().unwrap_or(0))
        .collect();
    let mut i = 0;
    while i < parts.len() {
        match parts[i] {
            0 => style = Style::default(),
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            7 => style = style.add_modifier(Modifier::REVERSED),
            22 => style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            27 => style = style.remove_modifier(Modifier::REVERSED),
            30..=37 => style = style.fg(basic_color(parts[i] - 30)),
            39 => style = style.fg(Color::Reset),
            40..=47 => style = style.bg(basic_color(parts[i] - 40)),
            49 => style = style.bg(Color::Reset),
            90..=97 => style = style.fg(basic_color(parts[i] - 90 + 8)),
            selector @ (38 | 48) => {
                let (color, consumed) = match parts.get(i + 1) {
                    Some(5) => (parts.get(i + 2).copied().map(Color::Indexed), 3),
                    Some(2) => (
                        match (parts.get(i + 2), parts.get(i + 3), parts.get(i + 4)) {
                            (Some(&red), Some(&green), Some(&blue)) => {
                                Some(Color::Rgb(red, green, blue))
                            }
                            _ => None,
                        },
                        5,
                    ),
                    _ => (None, 1),
                };
                if let Some(color) = color {
                    style = if selector == 38 {
                        style.fg(color)
                    } else {
                        style.bg(color)
                    };
                }
                i += consumed;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    style
}

fn basic_color(n: u8) -> Color {
    match n {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::Gray,
        8 => Color::DarkGray,
        9 => Color::LightRed,
        10 => Color::LightGreen,
        11 => Color::LightYellow,
        12 => Color::LightBlue,
        13 => Color::LightMagenta,
        14 => Color::LightCyan,
        _ => Color::White,
    }
}

/// Resolve a ratatui colour to a hex string, falling back to `reset_to` for
/// `Color::Reset`.
fn hex(color: Color, reset_to: &str) -> String {
    let s = match color {
        Color::Black => "#2b303b",
        Color::Red => "#e06c75",
        Color::Green => "#8fc866",
        Color::Yellow => "#e5c07b",
        Color::Blue => "#61afef",
        Color::Magenta => "#c678dd",
        Color::Cyan => "#4fb8c4",
        Color::Gray => "#abb2bf",
        Color::DarkGray => "#7f8794",
        Color::LightRed => "#ff7b86",
        Color::LightGreen => "#a5e075",
        Color::LightYellow => "#f0d399",
        Color::LightBlue => "#7cc4ff",
        Color::LightMagenta => "#d99ded",
        Color::LightCyan => "#6fd3de",
        Color::White => "#ffffff",
        Color::Rgb(r, g, b) => return format!("#{r:02x}{g:02x}{b:02x}"),
        // Reset takes the caller's default; 256-colour indices are unused by
        // the fixtures, so they fall back to it too.
        Color::Reset | Color::Indexed(_) => reset_to,
    };
    s.to_string()
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Style of a single cell after resolving REVERSED.
struct CellStyle {
    fg: String,
    bg: Option<String>,
    bold: bool,
}

fn cell_style(cell: &ratatui::buffer::Cell) -> CellStyle {
    let reversed = cell.modifier.contains(Modifier::REVERSED);
    let (fg, bg) = if reversed {
        (hex(cell.bg, BG), Some(hex(cell.fg, FG)))
    } else {
        let bg = match cell.bg {
            Color::Reset => None,
            other => Some(hex(other, BG)),
        };
        (hex(cell.fg, FG), bg)
    };
    CellStyle {
        fg,
        bg,
        bold: cell.modifier.contains(Modifier::BOLD),
    }
}

/// Emit the terminal contents of `buf` as SVG elements, offset by
/// (`ox`, `oy`). Runs of identically styled cells collapse into one `<text>`.
fn emit_cells(out: &mut String, buf: &Buffer, ox: f32, oy: f32) {
    let area = buf.area;

    // Background rects first so glyphs paint on top.
    for y in 0..area.height {
        let mut x = 0;
        while x < area.width {
            let Some(cell) = buf.cell((x, y)) else { break };
            let style = cell_style(cell);
            let Some(bg) = style.bg else {
                x += 1;
                continue;
            };
            let start = x;
            while x < area.width
                && buf
                    .cell((x, y))
                    .is_some_and(|c| cell_style(c).bg.as_deref() == Some(bg.as_str()))
            {
                x += 1;
            }
            let w = f32::from(x - start) * CELL_W;
            let px = ox + f32::from(start) * CELL_W;
            let py = oy + f32::from(y) * CELL_H;
            let _ = writeln!(
                out,
                r#"<rect x="{px:.1}" y="{py:.1}" width="{w:.1}" height="{CELL_H:.1}" fill="{bg}"/>"#
            );
        }
    }

    for y in 0..area.height {
        let mut x = 0;
        while x < area.width {
            let Some(cell) = buf.cell((x, y)) else { break };
            let style = cell_style(cell);
            let py = oy + f32::from(y) * CELL_H + BASELINE;
            let weight = if style.bold {
                r#" font-weight="700""#
            } else {
                ""
            };

            // Non-ASCII glyphs rarely honour the monospace advance in whatever
            // font the viewer has, so give each one its own centred cell
            // instead of letting it push the rest of the run out of the grid.
            if !cell.symbol().is_ascii() {
                if !cell.symbol().trim().is_empty() {
                    let cx = ox + (f32::from(x) + 0.5) * CELL_W;
                    let _ = writeln!(
                        out,
                        r#"<text x="{cx:.1}" y="{py:.1}" fill="{}"{weight} text-anchor="middle" xml:space="preserve">{}</text>"#,
                        style.fg,
                        escape(cell.symbol())
                    );
                }
                x += 1;
                continue;
            }

            let start = x;
            let mut text = String::new();
            while x < area.width {
                let Some(c) = buf.cell((x, y)) else { break };
                if !c.symbol().is_ascii() {
                    break;
                }
                let s = cell_style(c);
                if s.fg != style.fg || s.bold != style.bold {
                    break;
                }
                text.push_str(c.symbol());
                x += 1;
            }
            if text.trim().is_empty() {
                continue;
            }
            let px = ox + f32::from(start) * CELL_W;
            let len = f32::from(x - start) * CELL_W;
            let _ = writeln!(
                out,
                r#"<text x="{px:.1}" y="{py:.1}" fill="{}"{weight} textLength="{len:.1}" lengthAdjust="spacing" xml:space="preserve">{}</text>"#,
                style.fg,
                escape(&text)
            );
        }
    }
}

fn window_chrome(out: &mut String, x: f32, y: f32, w: f32, h: f32, title: Option<&str>) {
    let _ = writeln!(
        out,
        r#"<rect x="{x:.1}" y="{y:.1}" width="{w:.1}" height="{h:.1}" rx="{RADIUS}" fill="{BG}" stroke="{BORDER}"/>"#
    );
    let _ = writeln!(
        out,
        r#"<path d="M{x:.1} {ty:.1} v-{c} a{RADIUS} {RADIUS} 0 0 1 {RADIUS} -{RADIUS} h{inner:.1} a{RADIUS} {RADIUS} 0 0 1 {RADIUS} {RADIUS} v{c} z" fill="{TITLEBAR_BG}"/>"#,
        ty = y + TITLEBAR_H,
        c = TITLEBAR_H - RADIUS,
        inner = w - 2.0 * RADIUS,
    );
    let _ = writeln!(
        out,
        r#"<line x1="{x:.1}" y1="{ty:.1}" x2="{x2:.1}" y2="{ty:.1}" stroke="{BORDER}"/>"#,
        ty = y + TITLEBAR_H,
        x2 = x + w,
    );
    let mut cx = x + 17.0;
    for color in ["#ff5f57", "#febc2e", "#28c840"] {
        let _ = writeln!(
            out,
            r#"<circle cx="{cx:.1}" cy="{cy:.1}" r="5.5" fill="{color}"/>"#,
            cy = y + TITLEBAR_H / 2.0,
        );
        cx += 17.0;
    }
    if let Some(t) = title {
        let _ = writeln!(
            out,
            r#"<text x="{cx:.1}" y="{ty:.1}" fill="{TITLE_FG}" font-size="12" text-anchor="middle">{}</text>"#,
            escape(t),
            cx = x + w / 2.0,
            ty = y + TITLEBAR_H / 2.0 + 4.0,
        );
    }
}

fn svg_open(w: f32, h: f32) -> String {
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w:.0}" height="{h:.0}" viewBox="0 0 {w:.0} {h:.0}" font-family="{FONT_STACK}" font-size="{FONT_SIZE}">
"#
    )
}

/// A tmux window holding two panes side by side, split the way
/// `tmux-tabs-sidebar.sh` splits one.
fn window_svg(left: &Buffer, right: &Buffer, title: &str) -> String {
    let cols = f32::from(left.area.width + 1 + right.area.width);
    let rows = f32::from(left.area.height.max(right.area.height));
    let w = cols * CELL_W + PAD_X * 2.0;
    let h = rows * CELL_H + PAD_Y * 2.0 + TITLEBAR_H;
    let top = TITLEBAR_H + PAD_Y;

    let mut out = svg_open(w, h);
    window_chrome(&mut out, 0.5, 0.5, w - 1.0, h - 1.0, Some(title));
    emit_cells(&mut out, left, PAD_X, top);

    // tmux's pane divider occupies a full column between the two panes.
    let divider_x = PAD_X + (f32::from(left.area.width) + 0.5) * CELL_W;
    let _ = writeln!(
        &mut out,
        r#"<line x1="{divider_x:.1}" y1="{top:.1}" x2="{divider_x:.1}" y2="{y2:.1}" stroke="{BORDER}" stroke-width="1"/>"#,
        y2 = top + rows * CELL_H,
    );

    emit_cells(
        &mut out,
        right,
        PAD_X + f32::from(left.area.width + 1) * CELL_W,
        top,
    );
    out.push_str("</svg>\n");
    out
}

fn to_svg(buf: &Buffer, title: Option<&str>) -> String {
    let cols = f32::from(buf.area.width);
    let rows = f32::from(buf.area.height);
    let w = cols * CELL_W + PAD_X * 2.0;
    let h = rows * CELL_H + PAD_Y * 2.0 + TITLEBAR_H;

    let mut out = svg_open(w, h);
    window_chrome(&mut out, 0.5, 0.5, w - 1.0, h - 1.0, title);
    emit_cells(&mut out, buf, PAD_X, TITLEBAR_H + PAD_Y);
    out.push_str("</svg>\n");
    out
}

/// Several sidebars side by side with a caption under each.
fn strip_svg(panels: &[(&Buffer, &str)]) -> String {
    const GAP: f32 = 26.0;
    const CAPTION_H: f32 = 26.0;

    let count = u16::try_from(panels.len()).unwrap_or(1);
    let cols = panels.iter().map(|(b, _)| b.area.width).max().unwrap_or(1);
    let rows = panels.iter().map(|(b, _)| b.area.height).max().unwrap_or(1);
    let pw = f32::from(cols) * CELL_W + PAD_X * 2.0;
    let ph = f32::from(rows) * CELL_H + PAD_Y * 2.0 + TITLEBAR_H;
    let w = f32::from(count) * pw + f32::from(count.saturating_sub(1)) * GAP;

    let mut out = svg_open(w, ph + CAPTION_H);
    let mut x = 0.0;
    for (buf, caption) in panels {
        window_chrome(&mut out, x + 0.5, 0.5, pw - 1.0, ph - 1.0, None);
        emit_cells(&mut out, buf, x + PAD_X, TITLEBAR_H + PAD_Y);
        let _ = writeln!(
            &mut out,
            r#"<text x="{cx:.1}" y="{cy:.1}" fill="{TITLE_FG}" font-size="13" text-anchor="middle">{}</text>"#,
            escape(caption),
            cx = x + pw / 2.0,
            cy = ph + 18.0,
        );
        x += pw + GAP;
    }
    out.push_str("</svg>\n");
    out
}
