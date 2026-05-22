use std::io;

use crossterm::event::{Event, EventStream};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use tmux_tabs_common::{
    ClientMessage, Envelope, ServerMessage, SessionEntry, read_frame, write_frame,
};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;

use crate::input::{self, Action};
use crate::ui;

pub enum Mode {
    Normal,
    Rename { session_name: String, input: String },
}

pub struct App {
    pub sessions: Vec<SessionEntry>,
    pub current_session: String,
    /// Transient selection highlight. None = hidden (default).
    /// Shown when user navigates with j/k, dismissed when session detaches.
    pub selected: Option<usize>,
    pub mode: Mode,
    pub running: bool,
}

impl App {
    pub fn new() -> Self {
        Self {
            sessions: Vec::new(),
            current_session: String::new(),
            selected: None,
            mode: Mode::Normal,
            running: true,
        }
    }

    pub fn select_next(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        self.selected = Some(if let Some(i) = self.selected {
            (i + 1).min(self.sessions.len() - 1)
        } else {
            let cur = self.current_session_index().unwrap_or(0);
            (cur + 1).min(self.sessions.len() - 1)
        });
    }

    pub fn select_prev(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        self.selected = Some(if let Some(i) = self.selected {
            i.saturating_sub(1)
        } else {
            let cur = self.current_session_index().unwrap_or(0);
            cur.saturating_sub(1)
        });
    }

    pub fn selected_session_name(&self) -> Option<String> {
        self.selected
            .and_then(|i| self.sessions.get(i).map(|e| e.session.name.clone()))
    }

    fn current_session_index(&self) -> Option<usize> {
        self.sessions
            .iter()
            .position(|e| e.session.name == self.current_session)
    }

    fn apply_state_update(&mut self, sessions: Vec<SessionEntry>, current_session: String) {
        let was_attached = self
            .sessions
            .iter()
            .find(|e| e.session.name == self.current_session)
            .is_some_and(|e| e.session.attached);

        let prev_selected_name = self.selected_session_name();

        self.sessions = sessions;
        self.current_session = current_session;

        let is_attached = self
            .sessions
            .iter()
            .find(|e| e.session.name == self.current_session)
            .is_some_and(|e| e.session.attached);

        if was_attached && !is_attached {
            self.selected = None;
            return;
        }

        if let Some(name) = prev_selected_name {
            self.selected = self.sessions.iter().position(|e| e.session.name == name);
        }
    }
}

/// Run the TUI event loop.
///
/// # Errors
/// Returns an error if the terminal frame draw fails.
pub async fn run(
    terminal: &mut DefaultTerminal,
    mut reader: OwnedReadHalf,
    mut writer: OwnedWriteHalf,
    pane_id: String,
) -> io::Result<()> {
    let mut app = App::new();

    let reg = Envelope::Client(ClientMessage::Register {
        pane_id: pane_id.clone(),
    });
    write_frame(&mut writer, &reg)
        .await
        .map_err(io::Error::other)?;

    let (srv_tx, mut srv_rx) = mpsc::channel::<ServerMessage>(16);

    tokio::spawn(async move {
        while let Ok(Some(msg)) = read_frame::<_, ServerMessage>(&mut reader).await {
            if srv_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    // OSC 2 + ST: set the containing tmux pane title to "tmux-tabs".
    print!("\x1b]2;tmux-tabs\x1b\\");

    let mut event_stream = EventStream::new();

    while app.running {
        terminal.draw(|f| ui::render(f, &app))?;

        tokio::select! {
            evt = event_stream.next() => {
                let Some(Ok(event)) = evt else { break };
                let action = match event {
                    Event::Key(key) => input::handle_key(&mut app, key),
                    Event::Mouse(mouse) => input::handle_mouse(&mut app, mouse),
                    _ => Action::None,
                };
                match action {
                    Action::None => {}
                    Action::Quit => {
                        app.running = false;
                    }
                    Action::Send(cmd) => {
                        let envelope = Envelope::Client(cmd);
                        let _ = write_frame(&mut writer, &envelope).await;
                    }
                }
            }
            msg = srv_rx.recv() => {
                match msg {
                    Some(ServerMessage::StateUpdate { sessions, current_session }) => {
                        app.apply_state_update(sessions, current_session);
                    }
                    Some(ServerMessage::Shutdown) | None => {
                        app.running = false;
                    }
                }
            }
        }
    }

    Ok(())
}
