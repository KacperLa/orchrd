//! Single-instance guard.
//!
//! The systemd unit already prevents a second daemon, but a manually started
//! one would publish a duplicate uinput device and a second microphone node,
//! doubling every input event. The guard is an `flock` on a per-user file in
//! the runtime directory, so it disappears with the session and never blocks a
//! different user.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Lock file stem; the user id and `.lock` are appended.
pub const NAME: &str = "apple-tv-remote";

/// Held for the process lifetime; dropping the file releases the lock.
pub struct InstanceLock {
    _file: File,
}

/// Take the single-instance lock in the runtime directory.
pub fn acquire() -> Result<InstanceLock> {
    let directory = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    acquire_in(&directory, NAME)
}

/// Take the lock in `directory` instead of the runtime directory.
pub fn acquire_in(directory: &Path, name: &str) -> Result<InstanceLock> {
    let path = directory.join(format!("{name}-{}.lock", unsafe { libc::getuid() }));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        let error = io::Error::last_os_error();
        return match error.kind() {
            io::ErrorKind::WouldBlock => {
                Err(anyhow::anyhow!("another {name} daemon is already running"))
            }
            _ => Err(error).with_context(|| format!("lock {}", path.display())),
        };
    }
    Ok(InstanceLock { _file: file })
}
