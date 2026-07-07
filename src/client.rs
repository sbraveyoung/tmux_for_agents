use crate::paths;
use crate::protocol::{Request, Response};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::time::Duration;

const IO_TIMEOUT: Duration = Duration::from_millis(100);
const SPAWN_RETRY_DELAY: Duration = Duration::from_millis(50);

fn connect() -> std::io::Result<UnixStream> {
    let stream = UnixStream::connect(paths::socket_path())?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(stream)
}

#[allow(clippy::zombie_processes)] // intentional detach: daemon outlives this short-lived client, never wait()ed
fn spawn_daemon() {
    let Ok(exe) = std::env::current_exe() else { return };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Detach from the birth pane's session so closing the pane that triggered
    // the autospawn doesn't SIGHUP the resident daemon: without this, the
    // daemon inherits the hook's process group and controlling TTY.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let _ = cmd.spawn();
}

pub fn request(req: &Request) -> anyhow::Result<Response> {
    let mut stream = match connect() {
        Ok(s) => s,
        Err(_) if std::env::var("TFA_NO_SPAWN").as_deref() != Ok("1") => {
            spawn_daemon();
            std::thread::sleep(SPAWN_RETRY_DELAY);
            connect()?
        }
        Err(e) => return Err(e.into()),
    };
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let mut resp = String::new();
    BufReader::new(stream).read_line(&mut resp)?;
    Ok(serde_json::from_str(&resp)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_discipline_timeouts_are_pinned() {
        // This is the product's core non-blocking invariant: a hook must never
        // let a stuck socket read/write or a slow daemon spawn block the calling
        // agent. The generous 2000ms envelope asserted in tests/hook_cmd.rs
        // guards against hangs, not scheduler jitter — it only holds if these
        // constants stay small. Pin them so a well-meaning "just bump the
        // timeout" edit trips a test instead of silently widening the hang
        // window an agent can be stuck behind.
        assert_eq!(IO_TIMEOUT, Duration::from_millis(100));
        assert_eq!(SPAWN_RETRY_DELAY, Duration::from_millis(50));
    }
}
