//! A local control socket, so a status bar (or a person) can steer the running
//! daemon: `glide ctl pause`, `glide ctl peer off anton`. One line in, one line
//! back. Only this user can reach it: the socket is created mode 0600.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

use crate::input::Control;
use crate::link::Links;
use crate::pair::Me;

pub fn path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("glide.sock"),
        None => crate::status::path().with_file_name("glide.sock"),
    }
}

/// What a line asks for.
#[derive(Debug)]
pub enum Asked {
    Control(Control),
    /// A fresh invite; the reply is the command to run on the new machine.
    Invite,
}

const USAGE: &str = "pause, resume, release, peer on|off|forget <name>, extend <name>|off, invite";

/// `names` is the current device list, so a typo is refused with a reason
/// instead of silently doing nothing.
pub fn parse(line: &str, names: &[String]) -> Result<Asked> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let known = |name: &str| -> Result<String> {
        names.iter().find(|n| *n == name).cloned().with_context(|| format!("no peer named {name}"))
    };
    Ok(Asked::Control(match words[..] {
        ["pause"] => Control::Pause(true),
        ["resume"] => Control::Pause(false),
        ["release"] => Control::Release,
        ["peer", switch @ ("on" | "off"), name] => Control::SetEnabled(known(name)?, switch == "on"),
        ["peer", "forget", name] => Control::Forget(known(name)?),
        ["extend", "off"] => Control::Extend(None),
        ["extend", name] => Control::Extend(Some(known(name)?)),
        ["invite"] => return Ok(Asked::Invite),
        _ => bail!("unknown command {line:?} ({USAGE})"),
    }))
}

/// Serve the socket for as long as the daemon runs. `me` is set on a server,
/// which is the only end that can invite.
pub async fn serve(control: UnboundedSender<Control>, links: Arc<Links>, me: Option<Me>) -> Result<()> {
    serve_at(&path(), control, links, me).await
}

async fn serve_at(path: &std::path::Path, control: UnboundedSender<Control>, links: Arc<Links>, me: Option<Me>) -> Result<()> {
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
        let (control, links, me) = (control.clone(), links.clone(), me.clone());
        tokio::spawn(async move {
            let (read, mut write) = stream.into_split();
            let mut line = String::new();
            if tokio::io::BufReader::new(read).read_line(&mut line).await.is_err() {
                return;
            }
            let names: Vec<String> = links.peers().into_iter().map(|p| p.name).collect();
            let reply = match parse(line.trim(), &names) {
                Ok(Asked::Control(asked)) => match control.send(asked) {
                    Ok(()) => "ok".to_string(),
                    Err(_) => "error: the daemon is stopping".to_string(),
                },
                Ok(Asked::Invite) => match me.as_ref().map(crate::pair::invite) {
                    Some(Ok(command)) => command,
                    Some(Err(e)) => format!("error: {e}"),
                    None => "error: only a server can invite".to_string(),
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

    fn control(line: &str) -> Control {
        match parse(line, &names()).unwrap() {
            Asked::Control(c) => c,
            Asked::Invite => panic!("{line} is not an invite"),
        }
    }

    #[test]
    fn every_command_maps_to_its_control() {
        assert!(matches!(control("pause"), Control::Pause(true)));
        assert!(matches!(control("resume"), Control::Pause(false)));
        assert!(matches!(control("release"), Control::Release));
        assert!(matches!(control("peer off anton"), Control::SetEnabled(n, false) if n == "anton"));
        assert!(matches!(control("  peer on anorak \n"), Control::SetEnabled(n, true) if n == "anorak"));
        assert!(matches!(control("peer forget anton"), Control::Forget(n) if n == "anton"));
        assert!(matches!(control("extend anorak"), Control::Extend(Some(n)) if n == "anorak"));
        assert!(matches!(control("extend off"), Control::Extend(None)));
        assert!(matches!(parse("invite", &names()).unwrap(), Asked::Invite));
    }

    #[test]
    fn nonsense_is_refused_with_a_reason() {
        assert!(parse("peer off nobody", &names()).unwrap_err().to_string().contains("no peer named nobody"));
        assert!(parse("reboot", &names()).unwrap_err().to_string().contains("unknown command"));
        assert!(parse("", &names()).is_err());
        assert!(parse("peer maybe anton", &names()).is_err());
        assert!(parse("extend nobody", &names()).is_err());
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
            let links = Arc::new(Links::new(
                names()
                    .into_iter()
                    .map(|name| crate::Peer { name, addrs: vec![], fingerprint: String::new(), position: crate::Side::Left, display: None, enabled: true })
                    .collect(),
                true,
            ));
            async move { serve_at(&path, tx, links, None).await }
        });
        while !path.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let words = |w: &str| w.split(' ').map(String::from).collect::<Vec<_>>();
        let reply = { let path = path.clone(); tokio::task::spawn_blocking(move || send_to(&path, &words("peer off anton"))).await.unwrap() };
        assert_eq!(reply.unwrap(), "ok");
        assert!(matches!(rx.recv().await, Some(Control::SetEnabled(n, false)) if n == "anton"));
        let refused = { let path = path.clone(); tokio::task::spawn_blocking(move || send_to(&path, &words("peer off nobody"))).await.unwrap() };
        assert!(refused.unwrap_err().to_string().contains("no peer named nobody"));
        assert!(rx.try_recv().is_err(), "a refused command sends nothing");
        let invite = { let path = path.clone(); tokio::task::spawn_blocking(move || send_to(&path, &words("invite"))).await.unwrap() };
        assert!(invite.unwrap_err().to_string().contains("only a server"), "no identity, no invite");
        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
