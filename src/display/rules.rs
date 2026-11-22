//! The server's extended-display state: given what just happened, whether to
//! ask the client, start, stop now, or stop after a grace period.

use anyhow::Result;

use super::Mode;
use crate::proto::{ScreenMode, Show};

/// What the server does next, decided apart from the frameworks so the rules
/// can be tested without a screen.
#[derive(Debug, PartialEq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) enum Step {
    Ask(Show),
    Start(ScreenMode),
    Stop,
    /// Stop in this long unless the link comes back first.
    StopLater,
}

/// A link that blips must not throw every window off the virtual display, so a
/// running display outlives its link by this long.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) const GRACE: std::time::Duration = std::time::Duration::from_secs(10);

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) struct Host {
    pub(super) mode: Mode,
    pub(super) linked: bool,
    pub(super) workspace: u8,
    pub(super) running: Option<ScreenMode>,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl Host {
    fn ask(&self) -> Step {
        Step::Ask(match self.mode {
            Mode::Extend => Show::On { workspace: self.workspace },
            Mode::Control => Show::Off,
        })
    }

    pub(super) fn mode(&mut self, mode: Mode) -> Vec<Step> {
        self.mode = mode;
        let mut steps = Vec::new();
        if mode == Mode::Control && self.running.take().is_some() {
            steps.push(Step::Stop);
        }
        if self.linked {
            steps.push(self.ask());
        }
        steps
    }

    pub(super) fn link(&mut self, up: bool) -> Vec<Step> {
        self.linked = up;
        match (up, self.mode, self.running) {
            (true, Mode::Extend, _) => vec![self.ask()],
            (false, _, Some(_)) => vec![Step::StopLater],
            _ => vec![],
        }
    }

    pub(super) fn ready(&mut self, answer: Result<ScreenMode, String>) -> Vec<Step> {
        match answer {
            _ if self.mode != Mode::Extend => vec![],
            // Same monitor as before: the display and its windows stay put,
            // and the sender reconnects on its own.
            Ok(screen) if self.running == Some(screen) => vec![],
            Ok(screen) => {
                let mut steps = if self.running.is_some() { vec![Step::Stop] } else { vec![] };
                self.running = Some(screen);
                steps.push(Step::Start(screen));
                steps
            }
            Err(_) => self.running.take().map(|_| vec![Step::Stop]).unwrap_or_default(),
        }
    }

    /// The grace period ran out. Only stops if the link is still down.
    pub(super) fn grace_over(&mut self) -> Vec<Step> {
        if self.linked || self.running.take().is_none() {
            return vec![];
        }
        vec![Step::Stop]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FHD: ScreenMode = ScreenMode { w: 1920, h: 1080, hz: 120 };

    fn host(mode: Mode) -> Host {
        Host { mode, linked: false, workspace: 9, running: None }
    }

    #[test]
    fn extend_asks_the_client_then_starts_at_its_mode() {
        let mut h = host(Mode::Control);
        assert_eq!(h.link(true), vec![], "control mode asks for nothing on connect");
        assert_eq!(h.mode(Mode::Extend), vec![Step::Ask(Show::On { workspace: 9 })]);
        assert_eq!(h.ready(Ok(FHD)), vec![Step::Start(FHD)]);
        assert_eq!(h.mode(Mode::Control), vec![Step::Stop, Step::Ask(Show::Off)]);
    }

    #[test]
    fn switching_modes_offline_waits_for_the_link() {
        let mut h = host(Mode::Control);
        assert_eq!(h.mode(Mode::Extend), vec![], "nobody to ask yet");
        assert_eq!(h.link(true), vec![Step::Ask(Show::On { workspace: 9 })]);
    }

    /// A blip must not tear down the display and scatter its windows.
    #[test]
    fn a_short_link_drop_keeps_the_display() {
        let mut h = host(Mode::Extend);
        h.link(true);
        h.ready(Ok(FHD));
        assert_eq!(h.link(false), vec![Step::StopLater]);
        assert_eq!(h.link(true), vec![Step::Ask(Show::On { workspace: 9 })]);
        assert_eq!(h.ready(Ok(FHD)), vec![], "same monitor: nothing to rebuild");
        assert_eq!(h.grace_over(), vec![], "a stale timer must not stop a live display");
    }

    #[test]
    fn a_long_link_drop_removes_the_display_and_comes_back_on_reconnect() {
        let mut h = host(Mode::Extend);
        h.link(true);
        h.ready(Ok(FHD));
        h.link(false);
        assert_eq!(h.grace_over(), vec![Step::Stop]);
        assert_eq!(h.link(true), vec![Step::Ask(Show::On { workspace: 9 })], "the saved mode is applied again");
        assert_eq!(h.ready(Ok(FHD)), vec![Step::Start(FHD)]);
    }

    #[test]
    fn a_new_monitor_mode_rebuilds_the_display() {
        let mut h = host(Mode::Extend);
        h.link(true);
        h.ready(Ok(FHD));
        let slower = ScreenMode { hz: 60, ..FHD };
        assert_eq!(h.ready(Ok(slower)), vec![Step::Stop, Step::Start(slower)]);
    }

    #[test]
    fn a_client_that_cannot_show_it_stops_the_display() {
        let mut h = host(Mode::Extend);
        h.link(true);
        assert_eq!(h.ready(Err("install gst-plugin-va".into())), vec![]);
        h.ready(Ok(FHD));
        assert_eq!(h.ready(Err("gone".into())), vec![Step::Stop]);
        assert_eq!(host(Mode::Control).ready(Ok(FHD)), vec![], "an answer to a stale request starts nothing");
    }
}
