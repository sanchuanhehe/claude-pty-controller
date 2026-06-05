//! Single-instance lock (ARCHITECTURE §12.4, review OPS-1/2/5).
//!
//! A host-level `flock` that prevents a SECOND controller process from driving
//! the same session (a manually-launched one colliding with the supervised one).
//! This is NOT the §13 single-DRIVER lock (which arbitrates N dashboards on ONE
//! controller) — different layer, different mechanism.
//!
//! The lock lives on a namespace-stable path: `RUNTIME_DIRECTORY` (systemd, set
//! by `RuntimeDirectory=`) → `XDG_RUNTIME_DIR` → temp dir. Avoid `PrivateTmp=yes`
//! for the lock dir, or two instances lock in different mount namespaces and the
//! mutual exclusion silently fails. The holder's PID is written into the lock body
//! so a contender can report (or signal) it.

use anyhow::{bail, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct InstanceLock {
    _file: File, // held for process lifetime; drop releases the flock
    path: PathBuf,
}

impl InstanceLock {
    /// Try to acquire the lock for `session`. Errors (with the holder's PID) if
    /// another live instance holds it.
    pub fn acquire(session: &str) -> Result<Self> {
        let dir = lock_dir();
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("claude-pty-controller-{session}.lock"));

        let file = OpenOptions::new().create(true).read(true).write(true).truncate(false).open(&path)?;
        // SAFETY: valid fd; LOCK_NB makes this non-blocking.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let mut held = String::new();
            let _ = (&file).read_to_string(&mut held);
            let pid = held.trim();
            let pid = if pid.is_empty() { "unknown" } else { pid };
            bail!("another controller already holds {} (pid {pid})", path.display());
        }

        // Record our PID in the body (for the preempt path; not for liveness).
        let mut f = &file;
        let _ = f.set_len(0);
        let _ = f.seek(SeekFrom::Start(0));
        let _ = write!(f, "{}", std::process::id());
        let _ = f.flush();

        Ok(Self { _file: file, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn lock_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("RUNTIME_DIRECTORY") {
        // systemd may give a colon-separated list; take the first.
        let s = d.to_string_lossy();
        if let Some(first) = s.split(':').next() {
            return PathBuf::from(first);
        }
    }
    if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(d);
    }
    std::env::temp_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_is_rejected_with_pid() {
        let session = format!("unit-{}", std::process::id());
        let a = InstanceLock::acquire(&session).expect("first acquire");
        let err = InstanceLock::acquire(&session).expect_err("second must fail");
        let msg = err.to_string();
        assert!(msg.contains("already holds"));
        assert!(msg.contains(&std::process::id().to_string()), "should report holder pid: {msg}");
        drop(a);
        // After release, acquire succeeds again.
        let _c = InstanceLock::acquire(&session).expect("re-acquire after drop");
        let _ = std::fs::remove_file(_c.path());
    }
}
