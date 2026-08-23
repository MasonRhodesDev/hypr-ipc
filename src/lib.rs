//! Fail-closed Hyprland IPC: instance discovery, socket2 events, hyprctl.
//!
//! Discovery: `HYPRLAND_INSTANCE_SIGNATURE` if that instance's `.socket2.sock`
//! exists; otherwise scan `$XDG_RUNTIME_DIR/hypr/*/hyprland.lock` for a live
//! Hyprland PID. There is no `/run/user/<uid>` fallback.

use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::net::UnixStream;

/// Event socket filename inside an instance directory.
pub const SOCKET2_NAME: &str = ".socket2.sock";
/// Command socket filename. Prefer `hyprctl` over framing this yourself.
pub const COMMAND_SOCKET_NAME: &str = ".socket.sock";
/// Lock file whose first line is the compositor PID.
pub const LOCK_NAME: &str = "hyprland.lock";
/// Lua-dialect marker under `$XDG_CONFIG_HOME/hypr/`.
pub const LUA_CONFIG: &str = "hyprland.lua";

/// Default `hyprctl` subprocess timeout.
pub const HYPRCTL_TIMEOUT: Duration = Duration::from_secs(5);
/// Suggested reconnect backoff after a dropped socket2 stream.
pub const RECONNECT: Duration = Duration::from_secs(2);

/// Why Hyprland IPC could not be used.
#[derive(Debug)]
pub enum Error {
    Paths(xdg_paths::Error),
    Io(std::io::Error),
    NoInstance,
    Timeout,
    Json(serde_json::Error),
    NotOk {
        stdout: String,
        stderr: String,
        status: Option<i32>,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Paths(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
            Self::NoInstance => write!(f, "no live Hyprland instance socket found"),
            Self::Timeout => write!(f, "hyprctl timed out"),
            Self::Json(e) => write!(f, "{e}"),
            Self::NotOk {
                stdout,
                stderr,
                status,
            } => {
                let detail = if stdout.is_empty() { stderr } else { stdout };
                write!(f, "hyprctl failed (rc={status:?}): {detail}")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Paths(e) => Some(e),
            Self::Io(e) => Some(e),
            Self::Json(e) => Some(e),
            _ => None,
        }
    }
}

impl From<xdg_paths::Error> for Error {
    fn from(value: xdg_paths::Error) -> Self {
        Self::Paths(value)
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

/// One `event>>payload` line from socket2.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame {
    pub event: String,
    pub payload: String,
}

/// Parse a socket2 line. Missing `>>` or an empty event name is `None`.
pub fn parse_line(line: &str) -> Option<Frame> {
    let (event, payload) = line.split_once(">>")?;
    if event.is_empty() {
        return None;
    }
    Some(Frame {
        event: event.to_string(),
        payload: payload.to_string(),
    })
}

/// Instance directory from the process environment.
pub fn instance_dir() -> Result<PathBuf, Error> {
    let dirs = xdg_paths::BaseDirs::from_env()?;
    instance_dir_from(
        dirs.runtime_dir(),
        env::var_os("HYPRLAND_INSTANCE_SIGNATURE").as_deref(),
    )
}

/// Instance directory from explicit runtime + optional HIS.
pub fn instance_dir_from(runtime_dir: &Path, his: Option<&OsStr>) -> Result<PathBuf, Error> {
    instance_dir_from_with(runtime_dir, his, pid_is_hyprland)
}

/// `is_live` is true when `/proc/<pid>/comm` is Hyprland. Tests inject a stub.
pub fn instance_dir_from_with(
    runtime_dir: &Path,
    his: Option<&OsStr>,
    is_live: impl Fn(u32) -> bool,
) -> Result<PathBuf, Error> {
    let hypr = runtime_dir.join("hypr");
    if let Some(sig) = his {
        if !sig.is_empty() {
            let candidate = hypr.join(sig);
            if candidate.join(SOCKET2_NAME).exists() {
                return Ok(candidate);
            }
        }
    }
    let rd = match std::fs::read_dir(&hypr) {
        Ok(rd) => rd,
        Err(_) => return Err(Error::NoInstance),
    };
    for entry in rd.flatten() {
        let dir = entry.path();
        let Ok(text) = std::fs::read_to_string(dir.join(LOCK_NAME)) else {
            continue;
        };
        let Some(pid) = text
            .lines()
            .next()
            .and_then(|line| line.trim().parse::<u32>().ok())
        else {
            continue;
        };
        if !is_live(pid) {
            continue;
        }
        if dir.join(SOCKET2_NAME).exists() {
            return Ok(dir);
        }
    }
    Err(Error::NoInstance)
}

fn pid_is_hyprland(pid: u32) -> bool {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
    comm.trim() == "Hyprland"
}

/// Event socket path from the process environment.
pub fn socket2_path() -> Result<PathBuf, Error> {
    Ok(instance_dir()?.join(SOCKET2_NAME))
}

/// Event socket path from explicit runtime + optional HIS.
pub fn socket2_path_from(runtime_dir: &Path, his: Option<&OsStr>) -> Result<PathBuf, Error> {
    Ok(instance_dir_from(runtime_dir, his)?.join(SOCKET2_NAME))
}

/// Command socket path from the process environment.
pub fn command_socket_path() -> Result<PathBuf, Error> {
    Ok(instance_dir()?.join(COMMAND_SOCKET_NAME))
}

/// True when `$XDG_CONFIG_HOME/hypr/hyprland.lua` exists.
pub fn lua_dialect() -> Result<bool, Error> {
    let dirs = xdg_paths::ConfigDirs::from_env()?;
    Ok(lua_dialect_from(dirs.config_home()))
}

/// True when `{config_home}/hypr/hyprland.lua` is a file.
pub fn lua_dialect_from(config_home: &Path) -> bool {
    config_home.join("hypr").join(LUA_CONFIG).is_file()
}

/// Connect to the live instance's socket2. Callers reconnect with re-resolve.
pub async fn connect_socket2() -> Result<UnixStream, Error> {
    let path = socket2_path()?;
    Ok(UnixStream::connect(path).await?)
}

/// Run `hyprctl` with a timeout. Does not require a successful exit.
pub async fn hyprctl_output(
    args: &[&str],
    timeout: Duration,
) -> Result<std::process::Output, Error> {
    let mut cmd = tokio::process::Command::new("hyprctl");
    cmd.args(args).kill_on_drop(true);
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(out)) => Ok(out),
        Ok(Err(e)) => Err(Error::Io(e)),
        Err(_) => Err(Error::Timeout),
    }
}

/// Parse `hyprctl` stdout as JSON.
pub async fn hyprctl_json(args: &[&str], timeout: Duration) -> Result<serde_json::Value, Error> {
    let out = hyprctl_output(args, timeout).await?;
    Ok(serde_json::from_slice(&out.stdout)?)
}

/// Success is exit 0 and stdout exactly `ok`.
pub async fn hyprctl_ok(args: &[&str], timeout: Duration) -> Result<(), Error> {
    let out = hyprctl_output(args, timeout).await?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() && stdout == "ok" {
        return Ok(());
    }
    Err(Error::NotOk {
        stdout,
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        status: out.status.code(),
    })
}

/// Blocking `hyprctl` JSON with no timeout. Safe outside a tokio runtime.
pub fn hyprctl_json_std(args: &[&str]) -> Result<serde_json::Value, Error> {
    let out = std::process::Command::new("hyprctl").args(args).output()?;
    Ok(serde_json::from_slice(&out.stdout)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "hypr-ipc-{}-{}-{name}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(path.join("hypr")).unwrap();
        path
    }

    fn write_instance(runtime: &Path, name: &str, pid: u32, socket: bool) {
        let dir = runtime.join("hypr").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(LOCK_NAME), format!("{pid}\n")).unwrap();
        if socket {
            std::fs::write(dir.join(SOCKET2_NAME), []).unwrap();
        }
    }

    #[test]
    fn parse_line_splits_event_and_payload() {
        assert_eq!(
            parse_line("focusedmon>>HDMI-A-1,1").unwrap(),
            Frame {
                event: "focusedmon".into(),
                payload: "HDMI-A-1,1".into(),
            }
        );
        assert_eq!(
            parse_line("configreloaded>>").unwrap(),
            Frame {
                event: "configreloaded".into(),
                payload: String::new(),
            }
        );
        assert!(parse_line("configreloaded").is_none());
        assert!(parse_line(">>payload").is_none());
    }

    #[test]
    fn his_wins_when_socket2_exists() {
        let runtime = scratch("his");
        write_instance(&runtime, "stale", 1, true);
        write_instance(&runtime, "live", 2, true);
        let dir = instance_dir_from_with(&runtime, Some(OsStr::new("live")), |_| {
            panic!("HIS hit must not scan")
        })
        .unwrap();
        assert_eq!(dir.file_name().unwrap(), "live");
        let _ = std::fs::remove_dir_all(&runtime);
    }

    #[test]
    fn stale_his_rescans_live_lock() {
        let runtime = scratch("rescan");
        write_instance(&runtime, "gone", 9, false);
        write_instance(&runtime, "current", 42, true);
        let dir =
            instance_dir_from_with(&runtime, Some(OsStr::new("gone")), |pid| pid == 42).unwrap();
        assert_eq!(dir.file_name().unwrap(), "current");
        let _ = std::fs::remove_dir_all(&runtime);
    }

    #[test]
    fn dead_pid_is_not_an_instance() {
        let runtime = scratch("dead");
        write_instance(&runtime, "dead", 7, true);
        let err = instance_dir_from_with(&runtime, None, |_| false).unwrap_err();
        assert!(matches!(err, Error::NoInstance));
        let _ = std::fs::remove_dir_all(&runtime);
    }

    #[test]
    fn lua_dialect_is_the_hyprland_lua_file() {
        let config = scratch("lua");
        assert!(!lua_dialect_from(&config));
        let hypr = config.join("hypr");
        std::fs::create_dir_all(&hypr).unwrap();
        std::fs::write(hypr.join(LUA_CONFIG), "return {}").unwrap();
        assert!(lua_dialect_from(&config));
        let _ = std::fs::remove_dir_all(&config);
    }

    #[test]
    fn missing_runtime_hypr_is_no_instance() {
        let runtime = scratch("empty");
        let err = instance_dir_from_with(&runtime, None, |_| true).unwrap_err();
        assert!(matches!(err, Error::NoInstance));
        let _ = std::fs::remove_dir_all(&runtime);
    }
}
