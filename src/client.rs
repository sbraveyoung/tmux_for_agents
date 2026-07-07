use crate::paths;
use crate::protocol::{Request, Response};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

const IO_TIMEOUT: Duration = Duration::from_millis(100);

fn connect() -> std::io::Result<UnixStream> {
    let stream = UnixStream::connect(paths::socket_path())?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(stream)
}

fn spawn_daemon() {
    let Ok(exe) = std::env::current_exe() else { return };
    let _ = std::process::Command::new(exe)
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

pub fn request(req: &Request) -> anyhow::Result<Response> {
    let mut stream = match connect() {
        Ok(s) => s,
        Err(_) if std::env::var("TFA_NO_SPAWN").as_deref() != Ok("1") => {
            spawn_daemon();
            std::thread::sleep(Duration::from_millis(50));
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
