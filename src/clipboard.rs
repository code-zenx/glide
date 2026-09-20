//! The clipboard is kept the same on both machines, in both directions: copy
//! here, paste there, and the other way round.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::debug;

use crate::link::Links;
use crate::proto::Msg;

/// Anything bigger is almost certainly an image or a file dump, not something
/// worth pushing across on every copy.
const MAX_BYTES: usize = 1 << 20;
/// ponytail: polling, because neither macOS nor wlr-data-control gives a
/// portable change notification. A second is under human reaction time and
/// costs one clipboard read; move to NSPasteboard changeCount plus a Wayland
/// listener if it ever shows up in a profile.
const POLL: Duration = Duration::from_secs(1);

/// The last text we knew about, whether we read it locally or wrote it from a
/// peer. Without this, writing a peer's clipboard looks like a local copy and
/// gets sent straight back.
static LAST_SEEN: Mutex<Option<String>> = Mutex::new(None);

/// Read the local clipboard as text. `None` when it is empty, holds something
/// other than text, is too big, or the platform refuses — none of which are
/// worth interrupting input sharing for.
pub async fn read() -> Option<String> {
    // arboard's handle is not Send, and on Wayland each read is a fresh
    // connection anyway, so open and drop one inside the blocking thread.
    let text = tokio::task::spawn_blocking(|| {
        arboard::Clipboard::new().and_then(|mut c| c.get_text())
    })
    .await
    .ok()?;
    match text {
        Ok(text) if text.is_empty() => None,
        Ok(text) if text.len() > MAX_BYTES => {
            debug!("clipboard is {} bytes, not sending", text.len());
            None
        }
        Ok(text) => Some(text),
        Err(e) => {
            debug!("cannot read clipboard: {e}");
            None
        }
    }
}

/// Put the peer's clipboard text on this machine's clipboard.
pub async fn write(text: String) {
    if text.len() > MAX_BYTES {
        return;
    }
    // Remember it before it lands, so the watcher does not mistake it for a
    // local copy and bounce it back to the peer it came from.
    *LAST_SEEN.lock().unwrap() = Some(text.clone());
    let done = tokio::task::spawn_blocking(move || {
        arboard::Clipboard::new().and_then(|mut c| c.set_text(text))
    })
    .await;
    if let Ok(Err(e)) = done {
        debug!("cannot set clipboard: {e}");
    }
}

/// Send every local copy to the peers, for as long as the daemon runs.
pub async fn watch(links: Arc<Links>, peers: Vec<String>) {
    // Whatever is on the clipboard at startup was not copied just now.
    *LAST_SEEN.lock().unwrap() = read().await;
    loop {
        tokio::time::sleep(POLL).await;
        let Some(text) = read().await else { continue };
        let changed = {
            let mut last = LAST_SEEN.lock().unwrap();
            let changed = last.as_deref() != Some(text.as_str());
            if changed {
                *last = Some(text.clone());
            }
            changed
        };
        if changed {
            debug!(bytes = text.len(), "clipboard changed, sending to peers");
            for peer in &peers {
                links.send(peer, Msg::Clipboard(text.clone()));
            }
        }
    }
}
