mod frame;
mod model;
mod paths;
mod protocol;

pub use frame::{FrameError, encode_frame, read_frame, write_frame};
pub use model::{BrowserInfo, ClaudeStatus, GitInfo, PrState, SessionEntry, TmuxSession};
pub use paths::{pid_path, socket_dir, socket_path};
pub use protocol::{
    BridgeCommand, BridgeMessage, ClaudeEvent, ClientMessage, Envelope, HookNotification,
    ParseClaudeEventError, ServerMessage, TabGroupInfo,
};
