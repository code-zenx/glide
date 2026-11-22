//! An extended display: the server makes a virtual screen, and the client shows
//! it fullscreen on one of its own monitors.
//!
//! The server decides everything — whether the display is on, which workspace
//! the client shows it on, how many bits it gets. The client answers with the
//! mode of its monitor and does as it is told.
//!
//! Two modes, switched from the macOS menu bar:
//!
//! - **Control**: today's KVM. No video code runs at all.
//! - **Extend**: the client is an extra monitor. The KVM edge is disarmed, so
//!   the cursor walks onto the virtual display instead of into the client.
//!
//! Video never shares a connection with input. It rides a connection of its
//! own (see `link.rs`), and the sender keeps at most one frame waiting: when it
//! falls behind it throws frames away and asks the encoder for a keyframe,
//! trading one stuttered frame for latency that never builds up.

use quinn::Connection;
use serde::{Deserialize, Serialize};

use crate::proto::Msg;

#[cfg(not(target_os = "macos"))]
mod client;
mod frames;
mod rules;
#[cfg(target_os = "macos")]
mod server;

#[cfg(not(target_os = "macos"))]
pub use client::show;
#[cfg(test)]
pub(crate) use frames::write_frame;
pub use frames::{recv_frames, send_frames, Frame, Slot};
#[cfg(target_os = "macos")]
pub use server::host;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// The KVM: the cursor crosses into the peer and drives it.
    #[default]
    Control,
    /// The peer's monitor is an extra screen for this machine.
    Extend,
}

/// `[display]` in the server's config. A client ignores it: it is told.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Settings {
    /// Old configs say `mode = "extend"` and mean the first peer; new ones
    /// name the device in `extend`. Read through `crate::extend_target`.
    #[serde(default)]
    pub mode: Mode,
    /// The device showing the extended display, by name; absent when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extend: Option<String>,
    /// Which of the client's workspaces shows the display.
    #[serde(default = "default_workspace")]
    pub workspace: u8,
    #[serde(default = "default_bitrate")]
    pub bitrate_mbps: u32,
    /// Offer Retina modes on the virtual display. The stream stays at the
    /// client's pixels either way; HiDPI only changes what macOS draws.
    /// Off by default: on a 1080p client, drawing at 2x and shrinking every
    /// frame back down leaves text softer than drawing it 1:1.
    #[serde(default)]
    pub hidpi: bool,
}

fn default_workspace() -> u8 {
    9
}

fn default_bitrate() -> u32 {
    30
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: Mode::Control,
            extend: None,
            workspace: default_workspace(),
            bitrate_mbps: default_bitrate(),
            hidpi: false,
        }
    }
}

/// Everything the display side of the daemon hears about, funnelled through
/// the input session, which already sees every link change and message.
pub enum Event {
    /// Show the display on this device now, or on none.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Target(Option<crate::Peer>),
    Link(#[cfg_attr(not(target_os = "macos"), allow(dead_code))] String, bool),
    Msg(String, Msg),
    /// A peer opened a video connection to us.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Video(String, Connection),
}
