//! Minimal systemd `sd_notify` implementation.
//!
//! Sends state notifications to systemd via the `$NOTIFY_SOCKET` datagram
//! socket. This is a no-op when `$NOTIFY_SOCKET` is not set (i.e., when not
//! running under systemd with `Type=notify`).

/// Notify systemd about a service state change.
///
/// Common states:
/// - `"READY=1"` — service startup is complete
/// - `"RELOADING=1"` — service is reloading its configuration
/// - `"STOPPING=1"` — service is beginning its shutdown
/// - `"STATUS=..."` — free-form status string for `systemctl status`
///
/// This is a no-op on non-Linux platforms or when `$NOTIFY_SOCKET` is unset.
#[cfg(target_os = "linux")]
pub fn sd_notify(state: &str) {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::UnixDatagram;

    let Some(socket_path) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };

    let Ok(sock) = UnixDatagram::unbound() else {
        return;
    };

    let path_bytes = socket_path.as_os_str().as_bytes();

    if path_bytes.starts_with(b"@") {
        // Abstract socket (Linux-specific): replace leading '@' with '\0'.
        use std::os::linux::net::SocketAddrExt;
        use std::os::unix::net::SocketAddr;
        if let Ok(addr) = SocketAddr::from_abstract_name(&path_bytes[1..]) {
            let _ = sock.send_to_addr(state.as_bytes(), &addr);
        }
    } else {
        let _ = sock.send_to(state.as_bytes(), &socket_path);
    }
}

/// No-op on non-Linux platforms.
#[cfg(not(target_os = "linux"))]
pub fn sd_notify(_state: &str) {}
