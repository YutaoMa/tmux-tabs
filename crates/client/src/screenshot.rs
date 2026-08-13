//! Offline screenshot generator for the README.
//!
//! Renders the real sidebar UI (`crate::ui`) into an off-screen ratatui buffer
//! with curated demo data, then serialises that buffer to SVG. Nothing here
//! touches tmux or the server, so screenshots are deterministic and can be
//! regenerated without a live session.
//!
//! Stills are written as one SVG each. Animations are written as a directory
//! of numbered SVGs plus a manifest, where every frame after the first holds
//! only the region that changed. Turning either into a PNG needs a rasteriser,
//! so this is driven by the script rather than run directly:
//!
//! ```sh
//! ./scripts/gen-screenshots.sh
//! ```
//!
//! Compiled out unless the `screenshots` feature is enabled.

use std::fmt::Write as _;
use std::ops::RangeInclusive;
use std::path::Path;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use tmux_tabs_common::{AgentStatus, BrowserInfo, GitInfo, PrState, SessionEntry, TmuxSession};

use crate::app::{App, Link, Mode};
use crate::ui;

/// Spinner frame baked into every still so regenerating produces an identical
/// file instead of whatever the wall clock happened to land on. Animations
/// step through the frames instead.
const FIXED_SPINNER: usize = 2;

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

    // Close-up of the card anatomy, animated: agents work, one stops to ask a
    // question, a draft PR opens.
    write_animation(
        out_dir,
        "cards",
        &ambient_frames()?,
        &Stage::Window(Some("tmux-tabs")),
    )?;

    // `/grab`: a failing test in one pane, pulled into the pane next door.
    write_animation(
        out_dir,
        "grab",
        &grab_frames(),
        &Stage::RightPane {
            left: &parse_ansi(GRAB_SOURCE, 56, 26),
            title: "tmux — ~/dev/api-gateway",
        },
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

    write_animation(
        out_dir,
        "rename",
        &rename_frames()?,
        &Stage::Window(Some("tmux-tabs")),
    )?;

    write_animation(
        out_dir,
        "switch",
        &switch_frames()?,
        &Stage::Window(Some("tmux-tabs")),
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

/// Rows used by the animated sidebar: enough for four cards and their
/// dividers, cropped close so the cards fill the frame.
const CARD_ROWS: u16 = 24;

/// One frame of an animation: the buffer to show, and how long to hold it.
struct Frame {
    buf: Buffer,
    delay_ms: u16,
    pointer: Option<Pointer>,
}

impl Frame {
    fn new(buf: Buffer, delay_ms: u16) -> Self {
        Self {
            buf,
            delay_ms,
            pointer: None,
        }
    }

    fn with_pointer(buf: Buffer, delay_ms: u16, pointer: Pointer) -> Self {
        Self {
            buf,
            delay_ms,
            pointer: Some(pointer),
        }
    }
}

/// What the mouse is doing, so the pointer can show the gesture rather than
/// leaving the reader to infer it from the result.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Gesture {
    Idle,
    Scroll,
    Click,
}

/// A mouse pointer drawn over the terminal. It is anchored to a cell so its
/// dirty rectangle lives on the same grid as the buffer's.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Pointer {
    col: u16,
    row: u16,
    gesture: Gesture,
}

impl Pointer {
    fn at(col: u16, row: u16) -> Self {
        Self {
            col,
            row,
            gesture: Gesture::Idle,
        }
    }

    fn gesturing(self, gesture: Gesture) -> Self {
        Self { gesture, ..self }
    }

    /// The cells the pointer art can touch, padded for the click ring and the
    /// scroll chevrons and clamped to the buffer.
    fn rect(self, area: Rect) -> Rect {
        let x0 = self.col.saturating_sub(1);
        let y0 = self.row.saturating_sub(1);
        let x1 = (self.col + 2).min(area.width.saturating_sub(1));
        let y1 = (self.row + 2).min(area.height.saturating_sub(1));
        Rect::new(x0, y0, x1 - x0 + 1, y1 - y0 + 1)
    }
}

/// Where an animation's changing buffer sits in the canvas, and how the parts
/// that never change are drawn for the first frame.
enum Stage<'a> {
    /// A lone sidebar window.
    Window(Option<&'a str>),
    /// The right-hand pane of a two-pane tmux window; the left pane is static.
    RightPane { left: &'a Buffer, title: &'a str },
}

impl Stage<'_> {
    /// Top-left corner of the animated buffer within the canvas, in CSS px.
    fn origin(&self) -> (f32, f32) {
        let x = match self {
            Self::Window(_) => PAD_X,
            Self::RightPane { left, .. } => PAD_X + f32::from(left.area.width + 1) * CELL_W,
        };
        (x, TITLEBAR_H + PAD_Y)
    }

    fn canvas(&self, buf: &Buffer) -> String {
        match self {
            Self::Window(title) => to_svg(buf, *title),
            Self::RightPane { left, title } => window_svg(left, buf, title),
        }
    }
}

/// A frame after diffing: where its rectangle lands in the canvas, in CSS px.
struct Emitted {
    x: f32,
    y: f32,
    delay_ms: u16,
}

/// `overview_app` rearranged so the *current* session is quiet and the
/// background ones carry the story.
fn ambient_app() -> App {
    let mut app = overview_app();
    app.sessions[1].agent = AgentStatus::None;
    app.sessions[3].agent = processing("Bash: cargo test");
    app
}

fn hold(
    frames: &mut Vec<Frame>,
    app: &App,
    spinner: &mut usize,
    count: usize,
    delay_ms: u16,
) -> anyhow::Result<()> {
    for _ in 0..count {
        frames.push(Frame::new(
            render_frame(app, SIDEBAR_COLS, CARD_ROWS, *spinner)?,
            delay_ms,
        ));
        *spinner += 1;
    }
    Ok(())
}

/// Scene: you are working in one session while the others move on their own —
/// an agent switches files, another stops to ask permission, a draft PR opens.
fn ambient_frames() -> anyhow::Result<Vec<Frame>> {
    const BEAT: u16 = 140;

    let mut app = ambient_app();
    let mut frames = Vec::new();
    let mut spinner = 0;

    hold(&mut frames, &app, &mut spinner, 4, BEAT)?;

    app.sessions[0].agent = processing("Edit: routes.rs");
    app.sessions[0].context_pct = Some(44);
    hold(&mut frames, &app, &mut spinner, 4, BEAT)?;

    app.sessions[3].agent = waiting("Apply patch?");
    hold(&mut frames, &app, &mut spinner, 4, BEAT)?;

    app.sessions[2].git.pr_state = Some(PrState::Open);
    hold(&mut frames, &app, &mut spinner, 3, BEAT)?;

    if let Some(last) = frames.last_mut() {
        last.delay_ms = 2600;
    }
    Ok(frames)
}

/// Scene: `r` opens the rename prompt pre-filled with the current name, which
/// is edited a keystroke at a time and committed with Enter.
fn rename_frames() -> anyhow::Result<Vec<Frame>> {
    const BACKSPACE: u16 = 110;
    const KEYPRESS: u16 = 170;

    let mut app = ambient_app();
    app.selected = Some(2);

    // The spinner stays pinned here so each frame's changed region is just the
    // prompt, keeping the animation small.
    let mut frames = vec![Frame::new(render(&app, SIDEBAR_COLS, CARD_ROWS)?, 900)];

    let push = |app: &App, delay_ms: u16, frames: &mut Vec<Frame>| -> anyhow::Result<()> {
        frames.push(Frame::new(render(app, SIDEBAR_COLS, CARD_ROWS)?, delay_ms));
        Ok(())
    };

    let typed = [
        "docs-site",
        "docs-sit",
        "docs-si",
        "docs-s",
        "docs-",
        "docs-v",
        "docs-v2",
    ];
    for (i, input) in typed.iter().enumerate() {
        app.mode = Mode::Rename {
            session_name: "docs-site".to_string(),
            input: (*input).to_string(),
        };
        let delay = match i {
            0 => 600,
            _ if input.len() < typed[i - 1].len() => BACKSPACE,
            _ => KEYPRESS,
        };
        push(&app, delay, &mut frames)?;
    }

    if let Some(last) = frames.last_mut() {
        last.delay_ms = 600;
    }

    app.mode = Mode::Normal;
    app.sessions[2].session.name = "docs-v2".to_string();
    push(&app, 2600, &mut frames)?;

    Ok(frames)
}

/// Row each card's header sits on, mirroring the height arithmetic in
/// `input::handle_mouse` so the pointer lands where a real click would.
fn card_rows(app: &App) -> Vec<u16> {
    let mut rows = Vec::with_capacity(app.sessions.len());
    let mut top = 0;
    for entry in &app.sessions {
        rows.push(top);
        let has_git = entry.git.branch.is_some() || entry.git.pr_number.is_some();
        let lines = 3 + u16::from(has_git) + u16::from(entry.browser.is_some());
        top += lines + 1;
    }
    rows
}

/// Switch to a session the way the server does once it echoes the change back.
fn make_current(app: &mut App, index: usize) {
    let name = app.sessions[index].session.name.clone();
    for (i, entry) in app.sessions.iter_mut().enumerate() {
        entry.session.attached = i == index;
    }
    app.current_session = name;
}

/// Scene: the mouse drives the sidebar — the wheel steps through sessions one
/// at a time, then a click jumps straight to one.
fn switch_frames() -> anyhow::Result<Vec<Frame>> {
    const GLIDE: u16 = 55;
    const SETTLE: u16 = 620;

    let mut app = ambient_app();
    let rows = card_rows(&app);
    let mut frames = Vec::new();

    // The spinner is pinned throughout: letting it tick would stretch every
    // dirty rectangle across the whole sidebar and swamp the pointer.
    let mut push = |app: &App, pointer: Pointer, delay_ms: u16| -> anyhow::Result<()> {
        frames.push(Frame::with_pointer(
            render(app, SIDEBAR_COLS, CARD_ROWS)?,
            delay_ms,
            pointer,
        ));
        Ok(())
    };

    let track = |row: u16| Pointer::at(15, row);

    // Slide in from below the last card, then step down through the sessions.
    let mut row = rows[3] + 3;
    push(&app, track(row), 700)?;
    while row > rows[1] + 2 {
        row -= 2;
        push(&app, track(row), GLIDE)?;
    }
    push(&app, track(row), 420)?;

    for target in [2, 3] {
        push(&app, track(row).gesturing(Gesture::Scroll), 240)?;
        make_current(&mut app, target);
        push(&app, track(row).gesturing(Gesture::Scroll), 360)?;
        push(&app, track(row), SETTLE)?;
    }

    // Then jump straight to the top card with a click.
    while row > rows[0] + 2 {
        row -= 2;
        push(&app, track(row), GLIDE)?;
    }
    push(&app, track(row), 380)?;
    push(&app, track(row).gesturing(Gesture::Click), 280)?;
    make_current(&mut app, 0);
    push(&app, track(row).gesturing(Gesture::Click), 460)?;
    push(&app, track(row), 2400)?;

    Ok(frames)
}

/// The transcript viewport of the Copilot pane in the `/grab` fixture.
const GRAB_TRANSCRIPT: RangeInclusive<u16> = 2..=19;
/// The fixture's prompt line, and the column its text starts at.
const GRAB_PROMPT_ROW: u16 = 22;
const GRAB_PROMPT_COL: u16 = 2;

/// The finished `/grab` pane rewound: the transcript is revealed only as far
/// as `revealed`, and the prompt carries whatever has been typed so far.
fn grab_pane(revealed: Option<u16>, typed: &str) -> Buffer {
    let mut buf = parse_ansi(GRAB_AI, 69, 26);
    let width = buf.area.width;

    for row in GRAB_TRANSCRIPT {
        if revealed.is_some_and(|last| row <= last) {
            continue;
        }
        // Clearing the scrollbar column too lets it grow with the transcript
        // instead of hanging over an empty pane.
        for col in 0..width {
            if let Some(cell) = buf.cell_mut((col, row)) {
                cell.reset();
            }
        }
    }

    let mut col = GRAB_PROMPT_COL;
    for ch in typed.chars() {
        if let Some(cell) = buf.cell_mut((col, GRAB_PROMPT_ROW)) {
            cell.set_char(ch);
        }
        col += 1;
    }
    if let Some(cell) = buf.cell_mut((col, GRAB_PROMPT_ROW)) {
        cell.set_char(' ')
            .set_style(Style::default().add_modifier(Modifier::REVERSED));
    }

    buf
}

/// Scene: a test fails in one pane, and `/grab` hands it to the agent next
/// door without anyone copying a line of it.
fn grab_frames() -> Vec<Frame> {
    const KEYPRESS: u16 = 145;

    let mut frames = vec![Frame::new(grab_pane(None, ""), 900)];

    let command = "/grab";
    for end in 1..=command.len() {
        frames.push(Frame::new(grab_pane(None, &command[..end]), KEYPRESS));
    }
    if let Some(last) = frames.last_mut() {
        last.delay_ms = 700;
    }

    // Enter: the prompt clears and the answer arrives a block at a time. The
    // last step fills the viewport, matching the fixture exactly.
    for (revealed, delay) in [(4, 620), (7, 780), (12, 900), (19, 2600)] {
        frames.push(Frame::new(grab_pane(Some(revealed), ""), delay));
    }

    frames
}

/// Serialise an animation as one SVG per frame plus a manifest of
/// dirty-rectangle offsets, which `apng::main` stitches into an APNG.
/// Only the region that changed since the previous frame is emitted, so a
/// spinner tick costs a sliver of a frame rather than a whole one.
fn write_animation(
    dir: &Path,
    name: &str,
    frames: &[Frame],
    stage: &Stage<'_>,
) -> anyhow::Result<()> {
    let dir = dir.join(name);
    std::fs::create_dir_all(&dir)?;

    let (ox, oy) = stage.origin();
    let mut emitted: Vec<Emitted> = Vec::new();

    for (i, frame) in frames.iter().enumerate() {
        let (svg, x, y) = if i == 0 {
            (
                with_pointer(stage.canvas(&frame.buf), frame.pointer, ox, oy),
                0.0,
                0.0,
            )
        } else if let Some(rect) = changed_rect(&frames[i - 1], frame) {
            (
                sub_svg(&frame.buf, rect, frame.pointer),
                ox + f32::from(rect.x) * CELL_W,
                oy + f32::from(rect.y) * CELL_H,
            )
        } else {
            // Nothing moved; fold the time into the frame already on screen.
            if let Some(last) = emitted.last_mut() {
                last.delay_ms = last.delay_ms.saturating_add(frame.delay_ms);
            }
            continue;
        };
        std::fs::write(dir.join(format!("{:03}.svg", emitted.len())), svg)?;
        emitted.push(Emitted {
            x,
            y,
            delay_ms: frame.delay_ms,
        });
    }

    let mut manifest = String::new();
    for (i, frame) in emitted.iter().enumerate() {
        let _ = writeln!(
            &mut manifest,
            "{i:03} {x:.0} {y:.0} {delay}",
            x = frame.x,
            y = frame.y,
            delay = frame.delay_ms,
        );
    }

    std::fs::write(dir.join("frames.txt"), manifest)?;
    println!("wrote {} ({} frames)", dir.display(), emitted.len());
    Ok(())
}

/// Everything that has to be repainted between two frames: the cells that
/// differ, plus the pointer's old and new positions when it moved.
fn changed_rect(prev: &Frame, next: &Frame) -> Option<Rect> {
    let mut rect = dirty_rect(&prev.buf, &next.buf);
    if prev.pointer != next.pointer {
        for pointer in [prev.pointer, next.pointer].into_iter().flatten() {
            let moved = pointer.rect(next.buf.area);
            rect = Some(rect.map_or(moved, |acc| acc.union(moved)));
        }
    }
    rect
}

/// Bounding box of the cells that differ between two frames, padded by one
/// cell so glyphs that overhang their cell can't leave a seam.
fn dirty_rect(prev: &Buffer, next: &Buffer) -> Option<Rect> {
    let changed = prev.diff(next);
    if changed.is_empty() {
        return None;
    }
    let (mut x0, mut y0) = (u16::MAX, u16::MAX);
    let (mut x1, mut y1) = (0u16, 0u16);
    for (x, y, _) in &changed {
        x0 = x0.min(*x);
        y0 = y0.min(*y);
        x1 = x1.max(*x);
        y1 = y1.max(*y);
    }
    let area = next.area;
    let x0 = x0.saturating_sub(1);
    let y0 = y0.saturating_sub(1);
    let x1 = (x1 + 1).min(area.width - 1);
    let y1 = (y1 + 1).min(area.height - 1);
    Some(Rect::new(x0, y0, x1 - x0 + 1, y1 - y0 + 1))
}

/// A rectangular slice of a frame, sized to the slice rather than the window,
/// for compositing over the previous frame.
fn sub_svg(buf: &Buffer, rect: Rect, pointer: Option<Pointer>) -> String {
    let w = f32::from(rect.width) * CELL_W;
    let h = f32::from(rect.height) * CELL_H;

    let mut sub = Buffer::empty(Rect::new(0, 0, rect.width, rect.height));
    for y in 0..rect.height {
        for x in 0..rect.width {
            if let Some(src) = buf.cell((rect.x + x, rect.y + y))
                && let Some(dst) = sub.cell_mut((x, y))
            {
                *dst = src.clone();
            }
        }
    }

    let mut out = svg_open(w, h);
    let _ = writeln!(
        &mut out,
        r#"<rect width="{w:.0}" height="{h:.0}" fill="{BG}"/>"#
    );
    emit_cells(&mut out, &sub, 0.0, 0.0);
    out.push_str("</svg>\n");
    with_pointer(
        out,
        pointer,
        -f32::from(rect.x) * CELL_W,
        -f32::from(rect.y) * CELL_H,
    )
}

/// Draw the pointer into an already-closed SVG document, so the still-image
/// builders stay unaware of it.
fn with_pointer(svg: String, pointer: Option<Pointer>, ox: f32, oy: f32) -> String {
    let (Some(pointer), Some(body)) = (pointer, svg.strip_suffix("</svg>\n")) else {
        return svg;
    };
    let mut out = String::with_capacity(svg.len() + 512);
    out.push_str(body);
    draw_pointer(&mut out, pointer, ox, oy);
    out.push_str("</svg>\n");
    out
}

/// A macOS-style arrow whose tip sits on the pointer's cell, plus an
/// indicator for whatever gesture is under way.
fn draw_pointer(out: &mut String, pointer: Pointer, ox: f32, oy: f32) {
    const ACCENT: &str = "#7aa2f7";
    const FILL: &str = "#ffffff";
    const OUTLINE: &str = "#11131a";

    let x = ox + f32::from(pointer.col) * CELL_W;
    let y = oy + f32::from(pointer.row) * CELL_H;

    if pointer.gesture == Gesture::Click {
        let _ = writeln!(
            out,
            r#"<circle cx="{x:.1}" cy="{y:.1}" r="11" fill="none" stroke="{ACCENT}" stroke-width="2.5" opacity="0.85"/>"#
        );
    }

    let _ = writeln!(
        out,
        r#"<path d="M{x:.1},{y:.1} l0,17 l4.5,-4.1 l2.8,6.2 l3,-1.3 l-2.8,-6 l5,-0.3 Z" fill="{FILL}" stroke="{OUTLINE}" stroke-width="1.1" stroke-linejoin="round"/>"#
    );

    if pointer.gesture == Gesture::Scroll {
        for (i, dy) in [-9.0_f32, -2.0].into_iter().enumerate() {
            let _ = writeln!(
                out,
                r#"<path d="M{cx:.1},{cy:.1} l4,4.4 l4,-4.4" fill="none" stroke="{ACCENT}" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" opacity="{opacity}"/>"#,
                cx = x + 12.0,
                cy = y + dy,
                opacity = if i == 0 { "0.55" } else { "1" },
            );
        }
    }
}

fn render(app: &App, width: u16, height: u16) -> anyhow::Result<Buffer> {
    render_frame(app, width, height, FIXED_SPINNER)
}

fn render_frame(app: &App, width: u16, height: u16, spinner: usize) -> anyhow::Result<Buffer> {
    let mut terminal = Terminal::new(TestBackend::new(width, height))?;
    terminal.draw(|f| ui::render(f, app))?;
    let mut buf = terminal.backend().buffer().clone();
    set_spinner(&mut buf, spinner);
    Ok(buf)
}

/// Pin the wall-clock-driven spinner to a chosen frame so output is stable.
fn set_spinner(buf: &mut Buffer, frame: usize) {
    let symbol = ui::SPINNER_FRAMES[frame % ui::SPINNER_FRAMES.len()];
    let area = buf.area;
    for y in 0..area.height {
        for x in 0..area.width {
            if let Some(cell) = buf.cell_mut((x, y))
                && ui::SPINNER_FRAMES.contains(&cell.symbol())
            {
                cell.set_symbol(symbol);
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
