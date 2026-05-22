use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use tmux_tabs_common::ClientMessage;

use crate::app::{App, Mode};

pub enum Action {
    None,
    Send(ClientMessage),
    Quit,
}

pub fn handle_key(app: &mut App, key: KeyEvent) -> Action {
    match &app.mode {
        Mode::Normal => handle_normal(app, key),
        Mode::Rename { .. } => handle_rename(app, key),
    }
}

fn handle_normal(app: &mut App, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Char('j') | KeyCode::Down => {
            app.select_next();
            Action::None
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.select_prev();
            Action::None
        }
        KeyCode::Esc => {
            app.selected = None;
            Action::None
        }
        KeyCode::Enter => {
            if let Some(name) = app.selected_session_name() {
                app.selected = None;
                Action::Send(ClientMessage::SwitchSession { session_name: name })
            } else {
                Action::None
            }
        }
        KeyCode::Char('r') => {
            let name = app
                .selected_session_name()
                .unwrap_or_else(|| app.current_session.clone());
            if !name.is_empty() {
                app.mode = Mode::Rename {
                    session_name: name.clone(),
                    input: name,
                };
            }
            Action::None
        }
        _ => Action::None,
    }
}

fn handle_rename(app: &mut App, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => {
            app.mode = Mode::Normal;
            Action::None
        }
        KeyCode::Enter => {
            if let Mode::Rename {
                session_name,
                input,
            } = &app.mode
            {
                let old_name = session_name.clone();
                let new_name = input.clone();
                app.mode = Mode::Normal;
                if !new_name.is_empty() && new_name != old_name {
                    Action::Send(ClientMessage::RenameSession { old_name, new_name })
                } else {
                    Action::None
                }
            } else {
                Action::None
            }
        }
        KeyCode::Backspace => {
            if let Mode::Rename { input, .. } = &mut app.mode {
                input.pop();
            }
            Action::None
        }
        KeyCode::Char(c) => {
            if let Mode::Rename { input, .. } = &mut app.mode {
                input.push(c);
            }
            Action::None
        }
        _ => Action::None,
    }
}

/// Switch one session forward or backward from the current one. No wrap.
fn scroll_switch(app: &mut App, forward: bool) -> Action {
    if app.sessions.is_empty() {
        return Action::None;
    }
    let cur = app
        .sessions
        .iter()
        .position(|e| e.session.name == app.current_session)
        .unwrap_or(0);
    let target = if forward {
        (cur + 1).min(app.sessions.len() - 1)
    } else {
        cur.saturating_sub(1)
    };
    if target == cur {
        return Action::None;
    }
    let name = app.sessions[target].session.name.clone();
    app.selected = None;
    Action::Send(ClientMessage::SwitchSession { session_name: name })
}

/// Handle mouse events in Normal mode: left click to switch, wheel to navigate.
pub fn handle_mouse(app: &mut App, mouse: MouseEvent) -> Action {
    if !matches!(app.mode, Mode::Normal) {
        return Action::None;
    }
    match mouse.kind {
        MouseEventKind::ScrollDown => return scroll_switch(app, true),
        MouseEventKind::ScrollUp => return scroll_switch(app, false),
        MouseEventKind::Down(MouseButton::Left) => {}
        _ => return Action::None,
    }

    // Sessions render flush at row 0 — no surrounding block.
    let content_row = mouse.row as usize;

    // Cards have variable height depending on optional git/browser lines,
    // plus 1 divider line between cards.
    let mut cumulative = 0;
    for (i, entry) in app.sessions.iter().enumerate() {
        let has_git = entry.git.branch.is_some() || entry.git.pr_number.is_some();
        let has_browser = entry.browser.is_some();
        let content_lines = 3 + usize::from(has_git) + usize::from(has_browser);
        let is_last = i == app.sessions.len() - 1;
        let card_height = content_lines + usize::from(!is_last);
        if content_row < cumulative + card_height {
            let name = entry.session.name.clone();
            app.selected = None;
            return Action::Send(ClientMessage::SwitchSession { session_name: name });
        }
        cumulative += card_height;
    }

    Action::None
}
