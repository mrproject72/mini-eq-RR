//! Single-instance guard.
//!
//! The previous `InstanceGuard` was an in-process `Arc<Mutex<bool>>` that was
//! never even instantiated, so it could not prevent anything: two instances
//! could run at once, and both would try to own `mini_eq_sink`.
//!
//! This is a real lock — an exclusive `flock` on a file under
//! `$XDG_RUNTIME_DIR`. The kernel releases it when the holder dies, so a
//! crashed instance never blocks the next launch and no stale-lock reaping is
//! needed.
//!
//! ## Why there is no orphaned-filter-chain reaping here
//!
//! Upstream's `instance.py` also walks `/proc` and kills orphaned
//! `pipewire -c ...` child processes, because the *Python* app spawns the
//! filter chain as a child process and a crash would orphan it.
//!
//! This port has no such child: `PipeWireBackend` loads the module with
//! `pw_context_load_module`, so the module lives in the PipeWire **daemon** and
//! is owned by our client connection. Verified empirically — after `kill -9` of
//! a running instance, `mini_eq_sink` and `mini_eq_sink_output` are both gone
//! within a few seconds because the daemon drops the module when the client
//! disconnects. Reaping is therefore unnecessary, and a `/proc` scanner that
//! kills processes would be a liability rather than a safety net.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use nix::fcntl::{Flock, FlockArg};

/// Namespaced to this app so a running upstream Python install is not mistaken
/// for us, and vice versa.
const LOCK_FILE_NAME: &str = "mini-eq-rr.lock";

/// Why the lock could not be taken.
#[derive(Debug)]
pub enum AcquireError {
    /// Another live instance holds the lock. Carries its PID when the lock file
    /// held a readable one.
    AlreadyRunning(Option<u32>),
    Io(std::io::Error),
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcquireError::AlreadyRunning(Some(pid)) => {
                write!(f, "another instance is already running (pid {pid})")
            }
            AcquireError::AlreadyRunning(None) => {
                write!(f, "another instance is already running")
            }
            AcquireError::Io(e) => write!(f, "instance lock error: {e}"),
        }
    }
}

/// `$XDG_RUNTIME_DIR/mini-eq-rr.lock`, falling back to a uid-scoped directory
/// under the system temp dir when `XDG_RUNTIME_DIR` is unset (mirrors upstream
/// `runtime_lock_path()`).
pub fn runtime_lock_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR")
        && !dir.is_empty()
    {
        return Path::new(&dir).join(LOCK_FILE_NAME);
    }
    // SAFETY: `getuid` is always safe and cannot fail.
    let uid = unsafe { nix::libc::getuid() };
    std::env::temp_dir()
        .join(format!("mini-eq-rr-{uid}"))
        .join(LOCK_FILE_NAME)
}

/// Holds the exclusive lock for as long as it is alive. The lock is released
/// when this is dropped.
pub struct InstanceGuard {
    lock: Flock<File>,
    path: PathBuf,
}

impl InstanceGuard {
    /// Try to become the single running instance.
    ///
    /// On success the guard holds the lock and its PID has been written into
    /// the file for diagnostics. On failure another live instance holds it.
    pub fn try_acquire() -> Result<Self, AcquireError> {
        Self::try_acquire_at(runtime_lock_path())
    }

    /// Lock an explicit path. Split out so tests can use a private lock file
    /// instead of the real one — otherwise the suite fails whenever the app
    /// happens to be running, which is exactly when it is least useful.
    pub fn try_acquire_at(path: PathBuf) -> Result<Self, AcquireError> {
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return Err(AcquireError::Io(e));
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(AcquireError::Io)?;

        // LOCK_EX|LOCK_NB: fail immediately rather than hanging if another
        // instance already has it.
        let lock = match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => lock,
            Err((_file, nix::errno::Errno::EWOULDBLOCK)) => {
                return Err(AcquireError::AlreadyRunning(read_holder_pid(&path)));
            }
            Err((_, e)) => return Err(AcquireError::Io(std::io::Error::from(e))),
        };

        // Record our PID for diagnostics only; the lock itself is the kernel's.
        let mut lock = lock;
        let _ = lock.seek(SeekFrom::Start(0));
        let _ = lock.set_len(0);
        let _ = writeln!(lock, "{}", std::process::id());

        Ok(Self { lock, path })
    }

    /// Path of the lock file (for messages).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// PID of the process holding the lock, if the file names a readable one.
    pub fn holder_pid(&self) -> Option<u32> {
        read_holder_pid(&self.path)
    }

    /// Release the lock early. `Drop` does this too; this exists for clarity at
    /// shutdown and for tests.
    pub fn release(self) {
        // Flock's Drop issues LOCK_UN; consuming `self` here makes the intent
        // explicit at the call site.
        drop(self.lock);
    }
}

/// The file intentionally stays on disk after release: unlinking would let a
/// concurrent process lock the old inode while a third creates and locks a new
/// one (upstream notes the same hazard). The kernel lock enforces exclusion and
/// the file itself is harmless.
fn read_holder_pid(path: &Path) -> Option<u32> {
    let mut file = File::open(path).ok()?;
    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;
    buf.trim().parse::<u32>().ok()
}

/// True when some live instance currently holds the lock. For diagnostics.
pub fn is_lock_held() -> bool {
    is_lock_held_at(&runtime_lock_path())
}

/// As [`is_lock_held`] for an explicit lock-file path.
pub fn is_lock_held_at(path: &Path) -> bool {
    let Ok(file) = OpenOptions::new().read(true).write(true).open(path) else {
        return false;
    };
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        // We got it, so nobody was holding it.
        Ok(lock) => {
            drop(lock);
            false
        }
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A private lock file per test, so the suite is independent of whether the
    /// real app is running and of test ordering.
    fn temp_lock(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mini-eq-rr-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join("test.lock")
    }

    /// Two open file descriptions have independent `flock` state even inside one
    /// process, so this exercises the same path a second *process* takes.
    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let path = temp_lock("excl");

        {
            let first = InstanceGuard::try_acquire_at(path.clone()).expect("first acquire");
            assert!(is_lock_held_at(&path), "guard should report the lock held");

            assert_eq!(
                first.holder_pid(),
                Some(std::process::id()),
                "lock file should name the holding process"
            );

            let second = InstanceGuard::try_acquire_at(path.clone());
            assert!(
                matches!(second, Err(AcquireError::AlreadyRunning(_))),
                "second acquire must be refused, got {:?}",
                second.err()
            );
            assert!(
                is_lock_held_at(&path),
                "a refused attempt must not steal or drop the lock"
            );
        }

        assert!(!is_lock_held_at(&path), "drop should free the lock");
        assert!(
            InstanceGuard::try_acquire_at(path.clone()).is_ok(),
            "lock must be re-acquirable after release"
        );
    }

    /// A crash-free exit that simply returns must not leave the app
    /// permanently unlaunchable.
    #[test]
    fn explicit_release_frees_the_lock() {
        let path = temp_lock("release");
        let guard = InstanceGuard::try_acquire_at(path.clone()).expect("acquire");
        assert!(is_lock_held_at(&path));
        guard.release();
        assert!(!is_lock_held_at(&path));
    }

    #[test]
    fn lock_path_is_absolute_and_namespaced() {
        let path = runtime_lock_path();
        assert!(path.is_absolute(), "lock path must be absolute");
        assert_eq!(path.file_name().unwrap(), LOCK_FILE_NAME);
    }

    /// A missing lock file cannot be held.
    #[test]
    fn no_lock_file_means_not_held() {
        let path = temp_lock("absent").with_extension("definitely-absent");
        let _ = std::fs::remove_file(&path);
        assert!(!is_lock_held_at(&path));
    }
}
