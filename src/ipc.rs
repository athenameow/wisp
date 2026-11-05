//! Line-based protocol between the GUI and the daemon over a Unix socket.
//!
//! Commands: `copy <id>`, `reload`, `ping`. The daemon replies `ok` or `err <message>`.

use anyhow::{Context, bail};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

pub fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(dir).join("wisp.sock")
}

/// Sends one command to the daemon and waits for its reply.
pub fn send(command: &str) -> anyhow::Result<()> {
    let mut stream = UnixStream::connect(socket_path()).context("wispd is not running")?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(format!("{command}\n").as_bytes())?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    match reply.trim_end() {
        "ok" => Ok(()),
        other => bail!("{}", other.strip_prefix("err ").unwrap_or(other)),
    }
}
