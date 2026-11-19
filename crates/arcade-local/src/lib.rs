//! Where an arcade keeps its data, and the socket the CLI and the desktop app use to
//! reach a running arcaded.
//!
//! Linux: `~/.local/share/arcade` (or `$XDG_DATA_HOME/arcade`), and a Unix socket.
//! macOS: `~/Library/Application Support/arcade`, and a Unix socket.
//! Windows: `%LOCALAPPDATA%\arcade`, and the same socket kind (Windows 10 1803 and
//! later speak Unix sockets). `$ARCADE_HOME` / `%ARCADE_HOME%` wins on all.

use std::path::{Path, PathBuf};

#[cfg(unix)]
pub use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(windows)]
pub use uds_windows::{UnixListener, UnixStream};

/// The arcade data folder: `$ARCADE_HOME`, otherwise the per-user default.
///
/// On Linux a system install in `/var/lib/arcade` (its socket or database is there) is
/// used before the home-folder default. Windows has no system location; the default is
/// always under the user's local app data.
pub fn default_home() -> PathBuf {
    resolve(true)
}

/// Same as [`default_home`], except a Linux system folder counts only when its API
/// socket is present. The desktop app uses this: a database with no running service
/// is not the arcade to show.
pub fn default_home_for_app() -> PathBuf {
    resolve(false)
}

fn resolve(db_counts: bool) -> PathBuf {
    if let Some(h) = std::env::var_os("ARCADE_HOME") {
        if !h.is_empty() {
            return h.into();
        }
    }
    #[cfg(windows)]
    {
        let _ = db_counts;
        windows_home()
    }
    #[cfg(target_os = "macos")]
    {
        let _ = db_counts;
        macos_home()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        unix_home(db_counts)
    }
}

/// macOS keeps per-user app data in ~/Library/Application Support (no system install yet).
#[cfg(target_os = "macos")]
fn macos_home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Library/Application Support/arcade")
}

#[cfg(not(any(windows, target_os = "macos")))]
fn unix_home(db_counts: bool) -> PathBuf {
    let system = Path::new("/var/lib/arcade");
    if system.join("arcaded.sock").exists() || (db_counts && system.join("arcade.mdbx").exists()) {
        return system.to_path_buf();
    }
    let base = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
    });
    base.join("arcade")
}

#[cfg(windows)]
fn windows_home() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("USERPROFILE").unwrap_or_default())
                .join("AppData")
                .join("Local")
        });
    base.join("arcade")
}

/// `home/arcaded.sock`. Windows rejects a Unix-socket path longer than about 100 bytes;
/// [`check_socket_path`] turns that into a clear error at bind time.
pub fn socket_path(home: &Path) -> PathBuf {
    home.join("arcaded.sock")
}

/// Windows AF_UNIX paths are limited to 108 bytes including the trailing NUL; macOS's
/// sun_path is 104 bytes, so 103 before the NUL.
pub fn check_socket_path(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let n = path.as_os_str().len();
        if n > MACOS_SOCKET_MAX {
            return Err(format!(
                "the API socket path is {n} bytes long, and macOS allows {MACOS_SOCKET_MAX}. Set ARCADE_HOME to a shorter folder. Path: {}",
                path.display()
            ));
        }
    }
    #[cfg(windows)]
    {
        let n = path.to_string_lossy().len();
        if n > 100 {
            return Err(format!(
                "the API socket path is {n} bytes long, and Windows allows about 100. Set ARCADE_HOME to a shorter folder. Path: {}",
                path.display()
            ));
        }
    }
    let _ = path;
    Ok(())
}

/// The longest Unix socket path macOS takes (sun_path is 104 bytes with its NUL).
#[cfg(target_os = "macos")]
const MACOS_SOCKET_MAX: usize = 103;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_home_is_the_arcade_folder() {
        if std::env::var_os("ARCADE_HOME").is_some_and(|v| !v.is_empty()) {
            assert_eq!(default_home(), PathBuf::from(std::env::var_os("ARCADE_HOME").unwrap()));
            return;
        }
        assert_eq!(default_home().file_name().and_then(|n| n.to_str()), Some("arcade"));
    }
}
