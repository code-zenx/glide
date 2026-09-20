//! The clipboard is kept the same on both machines, in both directions: copy
//! here, paste there, and the other way round.
//!
//! Images travel as whatever bytes the clipboard already holds — a screenshot
//! is PNG on both platforms, and passing it through untouched is smaller,
//! faster and lossless compared with decoding it to pixels and re-encoding.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::debug;

use crate::link::Links;
use crate::proto::Msg;

/// Text past this is a file dump, not something worth syncing on every copy.
const MAX_TEXT: usize = 1 << 20;
/// Images get more room: a retina screenshot is a couple of megabytes.
pub const MAX_IMAGE: usize = 16 << 20;
/// The image types worth carrying, in the order they are asked for.
const IMAGE_TYPES: [&str; 3] = ["image/png", "image/jpeg", "image/gif"];
/// ponytail: polling, because neither platform gives a portable change
/// notification. A second is under human reaction time and costs one clipboard
/// read; macOS could use NSPasteboard's changeCount if this ever shows up in a
/// profile.
const POLL: Duration = Duration::from_secs(1);

/// What sat on the clipboard last, whether we read it locally or wrote it from
/// a peer. Without this, writing a peer's clipboard looks like a local copy and
/// gets sent straight back.
static LAST_SEEN: Mutex<Option<u64>> = Mutex::new(None);

#[derive(Debug, Clone, PartialEq)]
pub enum Clip {
    Text(String),
    Image { mime: String, bytes: Vec<u8> },
}

impl Clip {
    /// A cheap identity for change detection. Image bytes are already
    /// compressed, so hashing them costs little.
    fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        match self {
            Clip::Text(text) => text.hash(&mut hasher),
            Clip::Image { mime, bytes } => {
                mime.hash(&mut hasher);
                bytes.hash(&mut hasher);
            }
        }
        hasher.finish()
    }

    fn describe(&self) -> String {
        match self {
            Clip::Text(text) => format!("{} bytes of text", text.len()),
            Clip::Image { mime, bytes } => format!("{} of {mime}", bytes.len()),
        }
    }
}

/// Read the clipboard: an image if one is there, otherwise text. `None` when it
/// holds neither, is too big, or the platform refuses — none of which are worth
/// interrupting input sharing for.
pub async fn read() -> Option<Clip> {
    tokio::task::spawn_blocking(|| read_image().or_else(read_text)).await.ok()?
}

/// Put a peer's clipboard on this machine's.
pub async fn write(clip: Clip) {
    // Remember it before it lands, so the watcher does not mistake it for a
    // local copy and bounce it back to the peer it came from.
    *LAST_SEEN.lock().unwrap() = Some(clip.fingerprint());
    let _ = tokio::task::spawn_blocking(move || match clip {
        Clip::Text(text) => write_text(text),
        Clip::Image { mime, bytes } => write_image(&mime, bytes),
    })
    .await;
}

/// Send every local copy to the peers, for as long as the daemon runs.
pub async fn watch(links: Arc<Links>, peers: Vec<String>) {
    // Whatever is on the clipboard at startup was not copied just now.
    *LAST_SEEN.lock().unwrap() = read().await.map(|clip| clip.fingerprint());
    loop {
        tokio::time::sleep(POLL).await;
        let Some(clip) = read().await else { continue };
        let now = clip.fingerprint();
        let changed = {
            let mut last = LAST_SEEN.lock().unwrap();
            let changed = *last != Some(now);
            if changed {
                *last = Some(now);
            }
            changed
        };
        if changed {
            debug!("clipboard changed: {}", clip.describe());
            for peer in &peers {
                links.send(peer, Msg::Clipboard(clip.clone()));
            }
        }
    }
}

fn read_text() -> Option<Clip> {
    let text = arboard::Clipboard::new().and_then(|mut c| c.get_text());
    match text {
        Ok(text) if text.is_empty() => None,
        Ok(text) if text.len() > MAX_TEXT => {
            debug!("clipboard holds {} bytes of text, not sending", text.len());
            None
        }
        Ok(text) => Some(Clip::Text(text)),
        Err(e) => {
            debug!("cannot read clipboard text: {e}");
            None
        }
    }
}

fn write_text(text: String) {
    if let Err(e) = arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
        debug!("cannot set clipboard text: {e}");
    }
}

fn too_big(mime: &str, len: usize) -> bool {
    if len > MAX_IMAGE {
        debug!("clipboard holds {len} bytes of {mime}, too big to send");
        return true;
    }
    false
}

/// macOS keeps the original bytes on the pasteboard under a UTI, so a
/// screenshot can be moved across without ever becoming pixels.
#[cfg(target_os = "macos")]
fn read_image() -> Option<Clip> {
    use objc2_app_kit::NSPasteboard;
    use objc2_foundation::NSString;

    let pasteboard = NSPasteboard::generalPasteboard();
    for mime in IMAGE_TYPES {
        let uti = NSString::from_str(uti_for(mime));
        let Some(data) = pasteboard.dataForType(&uti) else { continue };
        let bytes = data.to_vec();
        if bytes.is_empty() || too_big(mime, bytes.len()) {
            return None;
        }
        return Some(Clip::Image { mime: mime.to_string(), bytes });
    }
    None
}

#[cfg(target_os = "macos")]
fn write_image(mime: &str, bytes: Vec<u8>) {
    use objc2_app_kit::NSPasteboard;
    use objc2_foundation::{NSData, NSString};

    let pasteboard = NSPasteboard::generalPasteboard();
    pasteboard.clearContents();
    let uti = NSString::from_str(uti_for(mime));
    let data = NSData::with_bytes(&bytes);
    if !pasteboard.setData_forType(Some(&data), &uti) {
        debug!("cannot put {mime} on the pasteboard");
    }
}

/// macOS names types by UTI, not by MIME type.
#[cfg(target_os = "macos")]
fn uti_for(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "public.jpeg",
        "image/gif" => "com.compuserve.gif",
        _ => "public.png",
    }
}

/// Wayland offers the clipboard by MIME type directly, so the bytes come
/// across as they are.
#[cfg(not(target_os = "macos"))]
fn read_image() -> Option<Clip> {
    use std::io::Read;
    use wl_clipboard_rs::paste::{get_contents, ClipboardType, MimeType, Seat};

    for mime in IMAGE_TYPES {
        let Ok((mut pipe, _)) =
            get_contents(ClipboardType::Regular, Seat::Unspecified, MimeType::Specific(mime))
        else {
            continue;
        };
        let mut bytes = Vec::new();
        // Read one byte past the cap so an oversized image is refused rather
        // than silently truncated.
        if pipe.by_ref().take(MAX_IMAGE as u64 + 1).read_to_end(&mut bytes).is_err() {
            continue;
        }
        if bytes.is_empty() || too_big(mime, bytes.len()) {
            return None;
        }
        return Some(Clip::Image { mime: mime.to_string(), bytes });
    }
    None
}

#[cfg(not(target_os = "macos"))]
fn write_image(mime: &str, bytes: Vec<u8>) {
    use wl_clipboard_rs::copy::{copy, MimeType, Options, Source};

    let mut options = Options::new();
    // Serve paste requests on a thread of this process, so `copy` returns at
    // once. Blocking here instead would hold the thread until someone else
    // copies something, and the thread dies with the daemon either way.
    options.foreground(false);
    if let Err(e) = copy(
        options,
        Source::Bytes(bytes.into_boxed_slice()),
        MimeType::Specific(mime.to_string()),
    ) {
        debug!("cannot put {mime} on the clipboard: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_of_any_kind_is_noticed() {
        let text = Clip::Text("hello".into());
        let same = Clip::Text("hello".into());
        let other = Clip::Text("hello!".into());
        assert_eq!(text.fingerprint(), same.fingerprint());
        assert_ne!(text.fingerprint(), other.fingerprint());

        let png = Clip::Image { mime: "image/png".into(), bytes: vec![1, 2, 3] };
        let jpeg = Clip::Image { mime: "image/jpeg".into(), bytes: vec![1, 2, 3] };
        assert_ne!(png.fingerprint(), jpeg.fingerprint(), "the type is part of the identity");
        assert_ne!(png.fingerprint(), text.fingerprint());
    }

    #[test]
    fn oversized_images_are_refused() {
        assert!(too_big("image/png", MAX_IMAGE + 1));
        assert!(!too_big("image/png", MAX_IMAGE));
    }
}
