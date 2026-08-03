//! Wayland connection setup with a session-start race fallback.

use std::{
    env, fs, io,
    os::unix::{ffi::OsStrExt, fs::FileTypeExt, net::UnixStream},
    path::{Path, PathBuf},
};

use wayland_client::Connection;

/// Connects using the standard Wayland environment, falling back to the only
/// compositor socket in `XDG_RUNTIME_DIR` when `WAYLAND_DISPLAY` has not been
/// propagated to this long-lived process yet.
pub(super) fn connect_wayland() -> Result<Connection, String> {
    let has_explicit_selection =
        env::var_os("WAYLAND_DISPLAY").is_some() || env::var_os("WAYLAND_SOCKET").is_some();
    let environment_error = match Connection::connect_to_env() {
        Ok(connection) => return Ok(connection),
        Err(error) => error,
    };

    // Never override an explicit display/socket selection. A stale or invalid
    // value should remain visible to the operator instead of silently choosing
    // a different session.
    if has_explicit_selection {
        return Err(environment_error.to_string());
    }

    let Some(runtime_dir) = env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) else {
        return Err(environment_error.to_string());
    };
    if !runtime_dir.is_absolute() {
        return Err(environment_error.to_string());
    }

    let candidates = discover_wayland_sockets(&runtime_dir).map_err(|error| {
        format!(
            "{environment_error}; could not inspect {} for a Wayland socket: {error}",
            runtime_dir.display()
        )
    })?;
    let [socket_path] = candidates.as_slice() else {
        return if candidates.is_empty() {
            Err(environment_error.to_string())
        } else {
            Err(format!(
                "{environment_error}; WAYLAND_DISPLAY is unset and multiple compositor sockets were found in {}",
                runtime_dir.display()
            ))
        };
    };

    let stream = UnixStream::connect(socket_path).map_err(|error| {
        format!(
            "connect to discovered Wayland socket {}: {error}",
            socket_path.display()
        )
    })?;
    Connection::from_socket(stream).map_err(|error| error.to_string())
}

fn discover_wayland_sockets(runtime_dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut candidates = Vec::new();
    for entry in fs::read_dir(runtime_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(suffix) = name.as_bytes().strip_prefix(b"wayland-") else {
            continue;
        };
        if suffix.is_empty() || !suffix.iter().all(u8::is_ascii_digit) {
            continue;
        }
        if entry.file_type()?.is_socket() {
            candidates.push(entry.path());
        }
    }
    candidates.sort_unstable();
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::net::UnixListener};

    use super::*;

    #[test]
    fn discovery_accepts_only_numbered_wayland_sockets() {
        let temporary = tempfile::tempdir().expect("temporary runtime directory");
        let expected = temporary.path().join("wayland-1");
        let _listener = UnixListener::bind(&expected).expect("Wayland-shaped socket");
        let _sidecar = UnixListener::bind(temporary.path().join("wayland-1-helper.sock"))
            .expect("sidecar socket");
        fs::write(temporary.path().join("wayland-2"), b"not a socket")
            .expect("socket-shaped regular file");

        assert_eq!(
            discover_wayland_sockets(temporary.path()).expect("discover sockets"),
            vec![expected]
        );
    }
}
