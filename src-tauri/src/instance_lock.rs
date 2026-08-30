use std::fs::{File, OpenOptions};
use std::path::Path;

/// A process-wide lock that uses the operating system's lifetime semantics.
/// Keeping the handle open means a crash releases the lock without leaving a
/// stale PID file that can strand the next launch.
pub struct InstanceLock {
    _file: File,
}

pub fn acquire(path: &Path) -> Result<InstanceLock, String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("Could not create instance-lock directory: {e}"))?;

    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .map_err(|e| format!("Could not open instance lock: {e}"))?;
        // flock is released by the kernel when this process exits, including
        // an abnormal exit. A second process gets EWOULDBLOCK immediately.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            return Err("Another rcsync instance is already running".into());
        }
        Ok(InstanceLock { _file: file })
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .share_mode(0)
            .open(path)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::PermissionDenied {
                    "Another rcsync instance is already running".to_string()
                } else {
                    format!("Could not open instance lock: {e}")
                }
            })?;
        Ok(InstanceLock { _file: file })
    }

    #[cfg(not(any(unix, windows)))]
    {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("Could not open instance lock: {e}"))?;
        Ok(InstanceLock { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_second_process_lock_handle_is_refused() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rcsync-instance-{}-{}.lock",
            std::process::id(),
            nonce
        ));
        let first = acquire(&path).unwrap();
        let second = acquire(&path);
        assert!(second.is_err());
        drop(second);
        drop(first);
        assert!(acquire(&path).is_ok());
        let _ = std::fs::remove_file(path);
    }
}
