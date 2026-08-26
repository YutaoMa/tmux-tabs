use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use tmux_tabs_common::ClientMessage;

use crate::app::{App, Mode};
use crate::ui;

pub enum Action {
    None,
    Send(ClientMessage),
    Quit,
}

pub fn handle_key(app: &mut App, key: KeyEvent) -> Action {
    match &app.mode {
        Mode::Normal => handle_normal(app, key),
        Mode::Rename { .. } => handle_rename(app, key),
        Mode::Blocker { .. } => handle_blocker(app, key),
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
            if let Some(name) = app.target_session_name() {
                app.mode = Mode::Rename {
                    session_name: name.clone(),
                    input: name,
                };
            }
            Action::None
        }
        // `b` toggles: marking is the common case, but clearing a stale note
        // shouldn't cost a second keystroke once the blocker is resolved.
        KeyCode::Char('b') => {
            let Some(name) = app.target_session_name() else {
                return Action::None;
            };
            if app.blocker_of(&name).is_some() {
                return Action::Send(ClientMessage::SetBlocker {
                    session_name: name,
                    note: None,
                });
            }
            app.mode = Mode::Blocker {
                session_name: name,
                input: String::new(),
            };
            Action::None
        }
        _ => Action::None,
    }
}

fn handle_blocker(app: &mut App, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => {
            app.mode = Mode::Normal;
            Action::None
        }
        KeyCode::Enter => {
            let Mode::Blocker {
                session_name,
                input,
            } = &app.mode
            else {
                return Action::None;
            };
            let session_name = session_name.clone();
            let note = input.trim().to_string();
            app.mode = Mode::Normal;
            // An empty note carries no reminder, so treat Enter on it as a
            // cancel rather than marking a card with a blank overlay.
            if note.is_empty() {
                return Action::None;
            }
            Action::Send(ClientMessage::SetBlocker {
                session_name,
                note: Some(note),
            })
        }
        KeyCode::Backspace => {
            if let Mode::Blocker { input, .. } = &mut app.mode {
                input.pop();
            }
            Action::None
        }
        KeyCode::Char(c) => {
            if let Mode::Blocker { input, .. } = &mut app.mode {
                // Cap on input rather than on render, so the prompt never
                // accepts more than the card has room to show.
                input.push(c);
                *input = ui::truncate_note(input);
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

    // Cards have variable height depending on optional git/browser lines (or a
    // fixed-height blocker overlay), plus 1 divider line between cards.
    let mut cumulative = 0;
    for (i, entry) in app.sessions.iter().enumerate() {
        let content_lines = ui::card_height(entry);
        let is_last = i == app.sessions.len() - 1;
        let card_height = content_lines + usize::from(!is_last);
        if content_row < cumulative + card_height {
            let name = entry.session.name.clone();
            // The overlay's ✕ clears the blocker instead of switching, so a
            // resolved dependency can be dismissed without leaving the pane.
            if entry.blocker.is_some()
                && ui::blocker_close_hit(
                    app.width as usize,
                    content_row - cumulative,
                    mouse.column as usize,
                )
            {
                return Action::Send(ClientMessage::SetBlocker {
                    session_name: name,
                    note: None,
                });
            }
            app.selected = None;
            return Action::Send(ClientMessage::SwitchSession { session_name: name });
        }
        cumulative += card_height;
    }

    Action::None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use tmux_tabs_common::{AgentStatus, GitInfo, SessionEntry, TmuxSession};

    use crate::ui;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn app_with(blocker: Option<&str>) -> App {
        let mut app = App::new();
        app.width = 24;
        app.current_session = "api".into();
        app.sessions = vec![SessionEntry {
            session: TmuxSession {
                id: "$0".into(),
                name: "api".into(),
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
        }];
        app
    }

    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            handle_key(app, key(c));
        }
    }

    #[test]
    fn b_opens_the_blocker_prompt_and_enter_commits() {
        let mut app = app_with(None);
        handle_key(&mut app, key('b'));
        assert!(matches!(app.mode, Mode::Blocker { .. }));

        type_str(&mut app, "ask jane");
        let action = handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.mode, Mode::Normal));
        match action {
            Action::Send(ClientMessage::SetBlocker { session_name, note }) => {
                assert_eq!(session_name, "api");
                assert_eq!(note.as_deref(), Some("ask jane"));
            }
            _ => panic!("Enter must commit the note"),
        }
    }

    /// Once the dependency is resolved, clearing shouldn't cost a round trip
    /// through the editor.
    #[test]
    fn b_on_a_blocked_session_clears_it_outright() {
        let mut app = app_with(Some("ask jane"));
        let action = handle_key(&mut app, key('b'));
        assert!(
            matches!(app.mode, Mode::Normal),
            "clearing must not open the prompt"
        );
        match action {
            Action::Send(ClientMessage::SetBlocker { session_name, note }) => {
                assert_eq!(session_name, "api");
                assert!(note.is_none());
            }
            _ => panic!("b must clear an existing blocker"),
        }
    }

    #[test]
    fn esc_abandons_the_blocker_prompt_without_marking() {
        let mut app = app_with(None);
        handle_key(&mut app, key('b'));
        type_str(&mut app, "oops");
        let action = handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(matches!(action, Action::None));
    }

    /// A blank overlay conveys nothing, so an empty commit is a cancel.
    #[test]
    fn an_empty_note_cancels_instead_of_marking() {
        let mut app = app_with(None);
        handle_key(&mut app, key('b'));
        type_str(&mut app, "   ");
        let action = handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(matches!(action, Action::None));
    }

    /// Capping at input time is what makes the "nothing you typed is hidden"
    /// guarantee in the overlay hold.
    #[test]
    fn the_prompt_stops_accepting_past_the_display_budget() {
        let mut app = app_with(None);
        handle_key(&mut app, key('b'));
        type_str(&mut app, &"x".repeat(ui::BLOCKER_MAX_CELLS + 20));
        let Mode::Blocker { input, .. } = &app.mode else {
            panic!("still expected the blocker prompt")
        };
        assert_eq!(input.chars().count(), ui::BLOCKER_MAX_CELLS);
    }

    #[test]
    fn typing_b_while_renaming_is_literal_text() {
        let mut app = app_with(None);
        handle_key(&mut app, key('r'));
        type_str(&mut app, "b");
        let Mode::Rename { input, .. } = &app.mode else {
            panic!("expected rename mode")
        };
        assert!(input.ends_with('b'));
    }

    fn click(row: u16, column: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row,
            column,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn clicking_the_overlay_close_glyph_clears_the_blocker() {
        let mut app = app_with(Some("ask jane"));
        let col = app.width - 2;
        let row = 1;
        let action = handle_mouse(&mut app, click(row, col));
        match action {
            Action::Send(ClientMessage::SetBlocker { note, .. }) => assert!(note.is_none()),
            _ => panic!("✕ must clear the blocker"),
        }
    }

    #[test]
    fn clicking_elsewhere_on_a_blocked_card_still_switches() {
        let mut app = app_with(Some("ask jane"));
        let action = handle_mouse(&mut app, click(2, 4));
        assert!(matches!(
            action,
            Action::Send(ClientMessage::SwitchSession { .. })
        ));
    }

    /// The overlay is taller than a bare card; hit-testing must follow it or
    /// clicks land on the wrong session.
    #[test]
    fn hit_testing_accounts_for_the_taller_blocked_card() {
        let mut app = app_with(Some("ask jane"));
        let second = {
            let mut e = app.sessions[0].clone();
            e.session.name = "web".into();
            e.session.id = "$1".into();
            e.blocker = None;
            e
        };
        app.sessions.push(second);

        let first_height = u16::try_from(ui::card_height(&app.sessions[0])).unwrap();
        // Last row of the blocked card (before the divider) is still card one.
        let action = handle_mouse(&mut app, click(first_height - 1, 4));
        match action {
            Action::Send(ClientMessage::SwitchSession { session_name }) => {
                assert_eq!(session_name, "api");
            }
            _ => panic!("expected a switch"),
        }
        // Past the card and its divider we're into the second session.
        let action = handle_mouse(&mut app, click(first_height + 1, 4));
        match action {
            Action::Send(ClientMessage::SwitchSession { session_name }) => {
                assert_eq!(session_name, "web");
            }
            _ => panic!("expected a switch"),
        }
    }
}
