//! Socket and PID paths, namespaced by UID to avoid conflicts between users.

use std::path::PathBuf;

#[must_use]
pub fn socket_dir() -> PathBuf {
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/tmux-tabs-{uid}"))
}

#[must_use]
pub fn socket_path() -> PathBuf {
    socket_dir().join("tmux-tabs.sock")
}

#[must_use]
pub fn pid_path() -> PathBuf {
    socket_dir().join("tmux-tabs.pid")
}
