//! Cross-process lock for the interactive OAuth flow.
//!
//! Atomic directory creation elects one owner. A unique owner file makes late
//! release safe after stale-lock recovery: an old owner only unlinks its own
//! file, never a successor's.

use super::token_store::{create_private_dir, resolve_token_directory, write_private_file};
use crate::crypto::uuid_v4;
use crate::logging::now_ms;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Lock directory name inside the token directory.
pub const OAUTH_FLOW_LOCK_DIRECTORY: &str = "oauth-flow.lock";
const DEFAULT_POLL_INTERVAL_MS: u64 = 100;
const DEFAULT_STALE_LOCK_MS: f64 = 5.0 * 60_000.0;

/// Tuning for [`acquire_oauth_flow_lock`].
#[derive(Debug, Clone, Copy)]
pub struct LockOptions {
    pub poll_interval_ms: u64,
    pub stale_lock_ms: f64,
}

impl Default for LockOptions {
    fn default() -> Self {
        Self {
            poll_interval_ms: DEFAULT_POLL_INTERVAL_MS,
            stale_lock_ms: DEFAULT_STALE_LOCK_MS,
        }
    }
}

/// A held OAuth flow lock.
#[derive(Debug)]
pub struct OAuthFlowLock {
    lock_directory: PathBuf,
    owner_path: PathBuf,
}

impl OAuthFlowLock {
    /// Release the lock (safe to call after a successor took over).
    pub fn release(&self) -> std::io::Result<()> {
        match std::fs::remove_file(&self.owner_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        }
        match std::fs::remove_dir(&self.lock_directory) {
            Ok(()) => Ok(()),
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    || error.kind() == std::io::ErrorKind::DirectoryNotEmpty =>
            {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
}

/// Machine hostname (`os.hostname()`).
pub fn hostname() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: the buffer is valid for `buf.len()` bytes and gethostname NUL-terminates on success.
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if rc == 0 {
            let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
            return String::from_utf8_lossy(&buf[..end]).into_owned();
        }
        String::new()
    }
    #[cfg(not(unix))]
    {
        std::env::var("COMPUTERNAME").unwrap_or_default()
    }
}

/// Whether a local process exists (`process.kill(pid, 0)`).
fn is_process_alive(pid: i64) -> Option<bool> {
    #[cfg(unix)]
    {
        let pid = libc::pid_t::try_from(pid).ok()?;
        // SAFETY: signal 0 performs only existence and permission checks.
        let rc = unsafe { libc::kill(pid, 0) };
        if rc == 0 {
            return Some(true);
        }
        Some(std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH))
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        None
    }
}

fn read_owner(lock_directory: &Path) -> Option<Value> {
    let entries = std::fs::read_dir(lock_directory).ok()?;
    let owner_file = entries
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .find(|name| {
            let name = name.to_string_lossy();
            name.starts_with("owner-") && name.ends_with(".json")
        })?;
    let raw = std::fs::read_to_string(lock_directory.join(owner_file)).ok()?;
    let owner: Value = serde_json::from_str(&raw).ok()?;
    let valid = owner.get("id").is_some_and(Value::is_string)
        && owner.get("pid").is_some_and(Value::is_number)
        && owner.get("hostname").is_some_and(Value::is_string)
        && owner.get("createdAt").is_some_and(Value::is_number);
    valid.then_some(owner)
}

fn is_stale(lock_directory: &Path, stale_lock_ms: f64) -> bool {
    if let Some(owner) = read_owner(lock_directory) {
        let created_at = owner["createdAt"].as_f64().unwrap_or(0.0);
        if owner["hostname"].as_str() == Some(hostname().as_str()) {
            let pid = owner["pid"].as_f64().unwrap_or(-1.0) as i64;
            if let Some(alive) = is_process_alive(pid) {
                return !alive;
            }
        }
        return now_ms() - created_at > stale_lock_ms;
    }
    match std::fs::metadata(lock_directory).and_then(|m| m.modified()) {
        Ok(modified) => {
            let age = modified
                .elapsed()
                .map(|d| d.as_millis() as f64)
                .unwrap_or(0.0);
            age > stale_lock_ms
        }
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

fn quarantine_stale_lock(lock_directory: &Path) -> std::io::Result<()> {
    let quarantine = PathBuf::from(format!("{}.stale-{}", lock_directory.display(), uuid_v4()));
    match std::fs::rename(lock_directory, &quarantine) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    let _ = std::fs::remove_dir_all(&quarantine);
    Ok(())
}

fn create_lock_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// Acquire the cross-process lock guarding an interactive OAuth flow.
pub async fn acquire_oauth_flow_lock(
    token_dir: Option<&Path>,
    options: LockOptions,
) -> std::io::Result<OAuthFlowLock> {
    let token_directory = resolve_token_directory(token_dir);
    let lock_directory = token_directory.join(OAUTH_FLOW_LOCK_DIRECTORY);
    create_private_dir(&token_directory)?;

    loop {
        let id = uuid_v4();
        let owner = json!({
            "id": id,
            "pid": std::process::id(),
            "hostname": hostname(),
            "createdAt": crate::js::num(now_ms()),
        });
        let owner_path = lock_directory.join(format!("owner-{id}.json"));
        match create_lock_dir(&lock_directory) {
            Ok(()) => {
                if let Err(error) = write_private_file(&owner_path, owner.to_string().as_bytes()) {
                    let _ = std::fs::remove_dir(&lock_directory);
                    return Err(error);
                }
                return Ok(OAuthFlowLock {
                    lock_directory,
                    owner_path,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        if is_stale(&lock_directory, options.stale_lock_ms) {
            quarantine_stale_lock(&lock_directory)?;
            continue;
        }
        tokio::time::sleep(Duration::from_millis(options.poll_interval_ms)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("whoop-mcp-lock-{}", uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn serializes_holders_and_releases() {
        let dir = temp_dir();
        let options = LockOptions {
            poll_interval_ms: 5,
            ..LockOptions::default()
        };
        let first = acquire_oauth_flow_lock(Some(&dir), options).await.unwrap();
        let dir2 = dir.clone();
        let waiter =
            tokio::spawn(async move { acquire_oauth_flow_lock(Some(&dir2), options).await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!waiter.is_finished());
        first.release().unwrap();
        let second = waiter.await.unwrap().unwrap();
        second.release().unwrap();
        first.release().unwrap();
        assert!(!dir.join(OAUTH_FLOW_LOCK_DIRECTORY).exists());
    }

    #[tokio::test]
    async fn recovers_locks_from_dead_processes() {
        let dir = temp_dir();
        let lock_dir = dir.join(OAUTH_FLOW_LOCK_DIRECTORY);
        std::fs::create_dir_all(&lock_dir).unwrap();
        let owner = json!({"id": "x", "pid": 999_999_999u64, "hostname": hostname(), "createdAt": now_ms()});
        std::fs::write(lock_dir.join("owner-x.json"), owner.to_string()).unwrap();
        let lock = acquire_oauth_flow_lock(Some(&dir), LockOptions::default())
            .await
            .unwrap();
        lock.release().unwrap();
    }
}
