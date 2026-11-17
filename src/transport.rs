//! Three ways for the Electron app to reach the same handler:
//!   stdio - JSON lines on the child's stdin/stdout (parent death = EOF = exit)
//!   uds   - Unix domain socket, mode 0600 (on Windows this would be a named pipe)
//!   tcp   - 127.0.0.1 on an ephemeral port, TCP_NODELAY, token-checked in hello
//! Socket transports print one ready line on stdout with the address.

use crate::server::{serve_conn, State};
use anyhow::Result;
use std::io::{BufReader, Write};
use std::sync::Arc;

pub fn stdio(state: &Arc<State>) -> Result<()> {
    let stdin = std::io::stdin();
    serve_conn(state, stdin.lock(), std::io::stdout());
    Ok(())
}

fn ready(transport: &str, addr: &str) {
    let mut o = std::io::stdout();
    let _ = writeln!(
        o,
        "{}",
        serde_json::json!({"event": "ready", "transport": transport, "addr": addr})
    );
    let _ = o.flush();
}

pub fn tcp(state: &Arc<State>) -> Result<()> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    ready("tcp", &l.local_addr()?.to_string());
    for s in l.incoming() {
        let s = s?;
        s.set_nodelay(true)?;
        if serve_conn(state, BufReader::new(s.try_clone()?), s) {
            break;
        }
    }
    Ok(())
}

#[cfg(unix)]
pub fn uds(state: &Arc<State>, path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::remove_file(path);
    let l = std::os::unix::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    ready("uds", &path.to_string_lossy());
    for s in l.incoming() {
        let s = s?;
        if serve_conn(state, BufReader::new(s.try_clone()?), s) {
            break;
        }
    }
    let _ = std::fs::remove_file(path);
    Ok(())
}

#[cfg(not(unix))]
pub fn uds(_: &Arc<State>, _: &std::path::Path) -> Result<()> {
    anyhow::bail!(
        "uds transport is Unix-only; a Windows build would use a named pipe (not implemented)"
    )
}
