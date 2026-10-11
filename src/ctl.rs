//! A local control socket, so a status bar (or a person) can steer the running
//! daemon: `glide ctl pause`, `glide ctl peer off anton`. One line in, one line
//! back. Only this user can reach it: the socket is created mode 0600.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

use crate::input::Control;

pub fn path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("glide.sock"),
        None => crate::status::path().with_file_name("glide.sock"),
    }
}

/// What a line asks for. `names` is the config's peer list, in order, so a
/// peer is addressed by name and sent on by its handle.
pub fn parse(line: &str, names: &[String]) -> Result<Control> {
    let words: Vec<&str> = line.split_whitespace().collect();
    Ok(match words[..] {
        ["pause"] => Control::Pause(true),
        ["resume"] => Control::Pause(false),
        ["release"] => Control::Release,
        ["peer", switch @ ("on" | "off"), name] => {
            let handle = names.iter().position(|n| n == name).with_context(|| format!("no peer named {name}"))?;
            Control::SetEnabled(handle as _, switch == "on")
        }
        _ => bail!("unknown command {line:?} (pause, resume, release, peer on|off <name>)"),
    })
}

/// Serve the socket for as long as the daemon runs.
pub async fn serve(control: UnboundedSender<Control>, names: Vec<String>) -> Result<()> {
    serve_at(&path(), control, names).await
}

async fn serve_at(path: &std::path::Path, control: UnboundedSender<Control>, names: Vec<String>) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // A daemon that died without cleaning up leaves the file behind; binding
    // over it would fail forever.
    let _ = std::fs::remove_file(path);
    let listener = tokio::net::UnixListener::bind(path).with_context(|| format!("cannot listen on {}", path.display()))?;
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    info!(path = %path.display(), "control socket ready");
    loop {
        let (stream, _) = listener.accept().await?;
        let (control, names) = (control.clone(), names.clone());
        tokio::spawn(async move {
            let (read, mut write) = stream.into_split();
            let mut line = String::new();
            if tokio::io::BufReader::new(read).read_line(&mut line).await.is_err() {
                return;
            }
            let reply = match parse(line.trim(), &names) {
                Ok(asked) => match control.send(asked) {
                    Ok(()) => "ok".to_string(),
                    Err(_) => "error: the daemon is stopping".to_string(),
                },
                Err(e) => {
                    warn!("control socket: {e}");
                    format!("error: {e}")
                }
            };
            let _ = write.write_all(format!("{reply}\n").as_bytes()).await;
        });
    }
}

/// `glide ctl …`: send one command to the running daemon and return its reply.
pub fn send(words: &[String]) -> Result<String> {
    send_to(&path(), words)
}

fn send_to(path: &std::path::Path, words: &[String]) -> Result<String> {
    let mut stream = UnixStream::connect(path).with_context(|| format!("glide is not running ({} missing)", path.display()))?;
    stream.write_all(format!("{}\n", words.join(" ")).as_bytes())?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    let reply = reply.trim().to_string();
    if let Some(why) = reply.strip_prefix("error: ") {
        bail!("{why}");
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        vec!["anorak".into(), "anton".into()]
    }

    #[test]
    fn every_command_maps_to_its_control() {
        assert!(matches!(parse("pause", &names()).unwrap(), Control::Pause(true)));
        assert!(matches!(parse("resume", &names()).unwrap(), Control::Pause(false)));
        assert!(matches!(parse("release", &names()).unwrap(), Control::Release));
        assert!(matches!(parse("peer off anton", &names()).unwrap(), Control::SetEnabled(1, false)));
        assert!(matches!(parse("  peer on anorak \n", &names()).unwrap(), Control::SetEnabled(0, true)));
    }

    #[test]
    fn nonsense_is_refused_with_a_reason() {
        assert!(parse("peer off nobody", &names()).unwrap_err().to_string().contains("no peer named nobody"));
        assert!(parse("reboot", &names()).unwrap_err().to_string().contains("unknown command"));
        assert!(parse("", &names()).is_err());
        assert!(parse("peer maybe anton", &names()).is_err());
    }

    #[test]
    fn the_socket_lives_in_the_runtime_dir() {
        // Read the variable rather than set it: tests run in parallel.
        match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) => assert_eq!(path(), PathBuf::from(dir).join("glide.sock")),
            None => assert!(path().ends_with(".cache/glide/glide.sock"), "{}", path().display()),
        }
    }

    #[tokio::test]
    async fn a_command_reaches_the_daemon_and_nonsense_does_not() {
        let dir = std::env::temp_dir().join(format!("glide-ctl-test-{}", std::process::id()));
        let path = dir.join("glide.sock");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn({
            let path = path.clone();
            async move { serve_at(&path, tx, names()).await }
        });
        while !path.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let words = |w: &str| w.split(' ').map(String::from).collect::<Vec<_>>();
        let reply = { let path = path.clone(); tokio::task::spawn_blocking(move || send_to(&path, &words("peer off anton"))).await.unwrap() };
        assert_eq!(reply.unwrap(), "ok");
        assert!(matches!(rx.recv().await, Some(Control::SetEnabled(1, false))));
        let refused = { let path = path.clone(); tokio::task::spawn_blocking(move || send_to(&path, &words("peer off nobody"))).await.unwrap() };
        assert!(refused.unwrap_err().to_string().contains("no peer named nobody"));
        assert!(rx.try_recv().is_err(), "a refused command sends nothing");
        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
