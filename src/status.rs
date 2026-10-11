//! A small file the daemon keeps current so anything else — `glide status`, the
//! macOS menu bar, a waybar module — can show what is happening without talking
//! to the daemon or costing it a thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Result;

use crate::screens::Screen;

/// One peer as this machine sees it: is the link up, how fast, is it paused.
#[derive(Clone, Debug, PartialEq)]
pub struct PeerState {
    pub name: String,
    /// The link: `offline` or `local` (linked, input not flowing to or from it).
    pub mode: String,
    pub rtt_ms: Option<f64>,
    /// The server has switched this peer off; the link stays up so both ends
    /// can say so, but no input or clipboard crosses.
    pub paused: bool,
}

impl PeerState {
    pub fn new(name: &str) -> Self {
        Self { name: name.to_string(), mode: "offline".into(), rtt_ms: None, paused: false }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct State {
    /// Every configured peer, in config order.
    pub peers: Vec<PeerState>,
    /// Which way input flows right now: local, sending or receiving. Only one
    /// peer can hold the cursor, so this is per machine, not per peer.
    pub flow: String,
    /// The peer input is flowing to or from; empty while it stays local.
    pub active: String,
    /// This machine cannot capture or type. Kept apart from `mode` because a
    /// link that drops and reconnects must not erase a permission problem.
    pub denied: bool,
    /// What the extended display is doing, in words; empty when it is off.
    pub display: String,
}

impl Default for State {
    fn default() -> Self {
        Self { peers: Vec::new(), flow: "local".into(), active: String::new(), denied: false, display: String::new() }
    }
}

/// How much a peer's state matters when one line has to stand for all of them.
fn rank(mode: &str, paused: bool) -> u8 {
    match mode {
        "sending" | "receiving" => 3,
        "local" if !paused => 2,
        "local" => 1,
        _ => 0,
    }
}

impl State {
    /// A peer's mode with the flow folded in: `sending`/`receiving` for the
    /// active peer, its link mode for the rest.
    pub fn mode_of(&self, peer: &PeerState) -> String {
        if peer.name == self.active && self.flow != "local" && peer.mode != "offline" {
            self.flow.clone()
        } else {
            peer.mode.clone()
        }
    }

    /// The peer a one-line summary talks about: the one holding the cursor,
    /// else a linked one, else a paused one, else the first configured.
    pub fn focus(&self) -> Option<&PeerState> {
        self.peers.iter().enumerate().max_by_key(|(i, p)| (rank(&self.mode_of(p), p.paused), std::cmp::Reverse(*i))).map(|(_, p)| p)
    }

    pub fn peer_mut(&mut self, name: &str) -> &mut PeerState {
        let i = match self.peers.iter().position(|p| p.name == name) {
            Some(i) => i,
            None => {
                self.peers.push(PeerState::new(name));
                self.peers.len() - 1
            }
        };
        &mut self.peers[i]
    }

    /// Mode of the focus peer; offline with no peers at all.
    pub fn mode(&self) -> String {
        self.focus().map_or("offline".into(), |p| self.mode_of(p))
    }

    fn peer(&self) -> String {
        self.focus().map_or(String::new(), |p| p.name.clone())
    }

    fn rtt_ms(&self) -> Option<f64> {
        self.focus().and_then(|p| p.rtt_ms)
    }

    fn paused(&self) -> bool {
        self.focus().is_some_and(|p| p.paused) && self.mode() == "local"
    }

    /// One line fit for a menu bar or a status bar.
    pub fn summary(&self) -> String {
        if self.denied {
            return "⚠ needs permission".into();
        }
        if self.paused() {
            return "Ⅱ paused".into();
        }
        match (self.mode().as_str(), self.rtt_ms()) {
            ("sending", _) => format!("→ {}", self.peer()),
            ("receiving", _) => format!("← {}", self.peer()),
            ("local", Some(rtt)) => format!("● {rtt:.1}ms"),
            ("local", None) => "● linked".into(),
            _ => "✕ offline".into(),
        }
    }

    /// Who this machine is linked to, and whether it is linked at all.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn headline(&self) -> String {
        match self.mode().as_str() {
            _ if self.denied => "Input permission needed".into(),
            "offline" => "Not connected".into(),
            _ if self.paused() => format!("{} is paused", self.peer()),
            _ => format!("Linked to {}", self.peer()),
        }
    }

    /// Which way input is flowing right now.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn flow(&self) -> String {
        if self.denied {
            return "Grant Input Monitoring and Device Control".into();
        }
        match self.mode().as_str() {
            "sending" => format!("You are controlling {}", self.peer()),
            "receiving" => format!("{} is controlling this Mac", self.peer()),
            "local" => "Input stays on this Mac".into(),
            _ => format!("Looking for {}", self.peer()),
        }
    }

    /// The round trip, spelled out.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn speed(&self) -> String {
        match self.rtt_ms() {
            Some(rtt) => format!("{rtt:.2} ms round trip"),
            None => "No timing yet".into(),
        }
    }

    pub fn tooltip(&self) -> String {
        let rtt = self.rtt_ms().map_or("no samples yet".into(), |r| format!("{r:.2} ms round trip"));
        if self.denied {
            return "glide: grant Input Monitoring and Device Control to Glide, then restart it".into();
        }
        if self.paused() {
            return format!("glide: {} is paused", self.peer());
        }
        match self.mode().as_str() {
            "sending" => format!("glide: controlling {}, {rtt}", self.peer()),
            "receiving" => format!("glide: {} is controlling this machine, {rtt}", self.peer()),
            "local" => format!("glide: linked to {}, input is local, {rtt}", self.peer()),
            _ => "glide: no peer connected".into(),
        }
    }

    /// waybar and friends want one JSON object per line. `badge` swaps the
    /// wordy text for the app's letter, which a stylesheet can then draw as the
    /// same rounded square the Mac shows in its menu bar. `peers` lists every
    /// peer for a popover.
    pub fn json(&self, badge: bool) -> String {
        let text = if badge { "G".to_string() } else { self.summary() };
        let peers: Vec<String> = self
            .peers
            .iter()
            .map(|p| {
                let rtt = p.rtt_ms.map_or("null".into(), |r| format!("{r:.2}"));
                format!(
                    r#"{{"name":"{}","mode":"{}","rtt_ms":{rtt},"paused":{}}}"#,
                    escape(&p.name),
                    escape(&self.mode_of(p)),
                    p.paused
                )
            })
            .collect();
        format!(
            r#"{{"text":"{}","tooltip":"{}","class":"{}","peers":[{}]}}"#,
            escape(&text),
            escape(&self.tooltip()),
            escape(&self.class()),
            peers.join(",")
        )
    }

    /// What a stylesheet keys off: the flow, the problem, or the pause.
    pub fn class(&self) -> String {
        if self.denied {
            "denied".into()
        } else if self.paused() {
            "paused".into()
        } else {
            self.mode()
        }
    }

    /// One `key=value` per line; a peer is `peer=<mode>\t<rtt>\t<paused>\t<name>`,
    /// name last so it may hold anything but a newline.
    fn encode(&self) -> String {
        let mut out = format!(
            "flow={}\nactive={}\ndenied={}\ndisplay={}\n",
            self.flow,
            self.active.replace('\n', " "),
            self.denied,
            self.display.replace('\n', " ")
        );
        for p in &self.peers {
            let rtt = p.rtt_ms.map_or(String::new(), |r| format!("{r:.3}"));
            out += &format!("peer={}\t{rtt}\t{}\t{}\n", p.mode, p.paused, p.name.replace('\n', " "));
        }
        out
    }

    /// Also reads the old single-peer file (`mode=`, `peer=<name>`, `rtt_ms=`)
    /// a daemon from before multi-peer support still writes.
    fn decode(text: &str) -> State {
        let mut state = State::default();
        let (mut old_mode, mut old_rtt, mut old_peer) = (None, None, None);
        for (key, value) in text.lines().filter_map(|l| l.split_once('=')) {
            match key {
                "flow" => state.flow = value.to_string(),
                "active" => state.active = value.to_string(),
                "denied" => state.denied = value == "true",
                "display" => state.display = value.to_string(),
                "peer" => match value.splitn(4, '\t').collect::<Vec<_>>()[..] {
                    [mode, rtt, paused, name] => state.peers.push(PeerState {
                        name: name.to_string(),
                        mode: mode.to_string(),
                        rtt_ms: rtt.parse().ok(),
                        paused: paused == "true",
                    }),
                    _ => old_peer = Some(value.to_string()),
                },
                "mode" => old_mode = Some(value.to_string()),
                "rtt_ms" => old_rtt = value.parse().ok(),
                _ => {}
            }
        }
        if let (Some(name), Some(mode)) = (old_peer, old_mode) {
            let link = if mode == "offline" { "offline" } else { "local" };
            if mode == "sending" || mode == "receiving" {
                state.flow = mode.clone();
                state.active = name.clone();
            }
            state.peers.push(PeerState { name, mode: link.into(), rtt_ms: old_rtt, paused: false });
        }
        state
    }
}

fn escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

pub fn path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".cache/glide/status")
}

/// What the daemon last wrote. A missing file means nothing is running.
pub fn read() -> State {
    std::fs::read_to_string(path()).map(|t| State::decode(&t)).unwrap_or_default()
}

/// The daemon's handle on that file. Writes only when something actually
/// changed, so a quiet session touches the disk twice a minute at most.
pub struct Status {
    current: Mutex<State>,
    /// What each peer says it has attached, for the arrange window. Not
    /// written to the status file: it changes rarely and nothing else wants it.
    peer_screens: Mutex<HashMap<String, Vec<Screen>>>,
    /// Where `set` writes. The default is the file everything else reads; a
    /// test points it somewhere disposable so `cargo test` cannot clobber the
    /// state of a daemon that happens to be running.
    file: PathBuf,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            current: Mutex::default(),
            peer_screens: Mutex::default(),
            file: path(),
        }
    }
}

impl Status {
    /// A status that writes somewhere harmless.
    #[cfg(test)]
    pub fn at(file: PathBuf) -> Self {
        Self { file, ..Default::default() }
    }

    pub fn set(&self, change: impl FnOnce(&mut State)) {
        let mut current = self.current.lock().unwrap();
        let before = current.clone();
        change(&mut current);
        if *current != before {
            let _ = write(&self.file, &current);
        }
    }

    /// What the daemon knows right now, for an in-process reader: the macOS
    /// menu bar. Elsewhere the status file is the only reader.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn snapshot(&self) -> State {
        self.current.lock().unwrap().clone()
    }

    pub fn set_screens(&self, peer: &str, screens: Vec<Screen>) {
        self.peer_screens.lock().unwrap().insert(peer.to_string(), screens);
    }

    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn screens_of(&self, peer: &str) -> Vec<Screen> {
        self.peer_screens.lock().unwrap().get(peer).cloned().unwrap_or_default()
    }

    /// Change one peer, adding it (at the end) if this is the first word of it.
    pub fn set_peer(&self, peer: &str, change: impl FnOnce(&mut PeerState)) {
        self.set(|s| change(s.peer_mut(peer)));
    }

    pub fn offline(&self, peer: &str) {
        self.set(|s| {
            let p = s.peer_mut(peer);
            p.mode = "offline".into();
            p.rtt_ms = None;
            if s.active == peer {
                s.flow = "local".into();
                s.active.clear();
            }
        });
    }

    /// Whether this machine can capture and type at all.
    pub fn allowed(&self, allowed: bool) {
        self.set(|s| s.denied = !allowed);
    }
}

fn write(path: &Path, state: &State) -> Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    // Rename over the old file so a reader never sees half a line.
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, state.encode())?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(name: &str, mode: &str, rtt_ms: Option<f64>) -> PeerState {
        PeerState { name: name.into(), mode: mode.into(), rtt_ms, paused: false }
    }

    fn one(mode: &str, rtt_ms: Option<f64>, denied: bool) -> State {
        let (link, flow, active) = match mode {
            "sending" | "receiving" => ("local", mode, "mybox"),
            other => (other, "local", ""),
        };
        State { peers: vec![peer("mybox", link, rtt_ms)], flow: flow.into(), active: active.into(), denied, display: String::new() }
    }

    #[test]
    fn survives_a_round_trip() {
        let mut state = one("sending", Some(1.844), true);
        state.display = "Extended: 1920×1080 at 120 Hz".into();
        state.peers.push(PeerState { name: "anton".into(), mode: "local".into(), rtt_ms: None, paused: true });
        assert_eq!(State::decode(&state.encode()), state);
    }

    #[test]
    fn missing_fields_fall_back_to_offline() {
        assert_eq!(State::decode(""), State::default());
        assert_eq!(State::decode("").mode(), "offline");
        assert_eq!(State::decode("garbage").mode(), "offline");
    }

    #[test]
    fn reads_the_old_single_peer_file() {
        let old = State::decode("mode=sending\npeer=anorak\nrtt_ms=1.500\ndenied=false\ndisplay=\n");
        assert_eq!(old.peers, vec![peer("anorak", "local", Some(1.5))]);
        assert_eq!(old.mode(), "sending");
        assert_eq!(old.summary(), "→ anorak");
        let offline = State::decode("mode=offline\npeer=anorak\nrtt_ms=\n");
        assert_eq!(offline.mode(), "offline");
    }

    #[test]
    fn the_peer_holding_the_cursor_speaks_for_all() {
        let mut state = State {
            peers: vec![peer("anorak", "local", Some(1.0)), peer("anton", "local", Some(2.0))],
            ..State::default()
        };
        // Both linked, nothing flowing: the first configured one.
        assert_eq!(state.summary(), "● 1.0ms");
        state.flow = "sending".into();
        state.active = "anton".into();
        assert_eq!(state.summary(), "→ anton");
        // A linked peer beats an offline one listed first.
        let state = State { peers: vec![peer("anorak", "offline", None), peer("anton", "local", None)], ..State::default() };
        assert_eq!(state.summary(), "● linked");
        assert_eq!(state.headline(), "Linked to anton");
    }

    #[test]
    fn a_paused_peer_says_so() {
        let mut state = one("local", Some(1.0), false);
        state.peers[0].paused = true;
        assert_eq!(state.summary(), "Ⅱ paused");
        assert_eq!(state.class(), "paused");
        assert!(state.json(false).contains(r#""paused":true"#), "{}", state.json(false));
        // A linked, unpaused peer outranks a paused one.
        state.peers.push(peer("anton", "local", None));
        assert_eq!(state.class(), "local");
    }

    #[test]
    fn json_stays_valid_when_a_peer_name_is_hostile() {
        let mut state = one("sending", None, false);
        state.peers[0].name = "an\"ora\\k".into();
        state.active = "an\"ora\\k".into();
        let json = state.json(false);
        assert!(json.contains(r#"an\"ora\\k"#), "{json}");
        assert!(json.ends_with("}]}"), "{json}");
    }

    #[test]
    fn summaries_say_which_way_input_is_going() {
        assert_eq!(one("local", Some(1.84), false).summary(), "● 1.8ms");
        assert_eq!(one("sending", None, false).summary(), "→ mybox");
        assert_eq!(one("receiving", None, false).summary(), "← mybox");
        assert_eq!(one("offline", None, false).summary(), "✕ offline");
    }

    #[test]
    fn the_badge_is_just_the_letter() {
        let state = one("sending", Some(2.0), false);
        assert!(state.json(true).contains(r#""text":"G""#), "{}", state.json(true));
        assert!(state.json(false).contains("mybox"), "{}", state.json(false));
        // A denied machine says so through the class, whatever the mode is.
        let denied = State { denied: true, ..state };
        assert!(denied.json(true).contains(r#""class":"denied""#), "{}", denied.json(true));
    }

    #[test]
    fn a_reconnect_cannot_erase_a_permission_problem() {
        let mut state = one("local", Some(1.8), true);
        assert_eq!(state.summary(), "⚠ needs permission");
        // The link dropping and coming back only ever touches the peer's mode.
        state.peers[0].mode = "offline".into();
        state.peers[0].mode = "local".into();
        assert_eq!(state.summary(), "⚠ needs permission", "denial must survive a reconnect");
    }

    #[test]
    fn going_offline_ends_the_flow_with_that_peer() {
        let status = Status::at(std::env::temp_dir().join(format!("glide-status-test-{}", std::process::id())));
        status.set(|s| {
            s.flow = "sending".into();
            s.active = "anton".into();
        });
        status.set_peer("anton", |p| p.mode = "local".into());
        status.set_peer("anorak", |p| p.mode = "local".into());
        status.offline("anton");
        let now = status.snapshot();
        assert_eq!((now.flow.as_str(), now.active.as_str()), ("local", ""));
        assert_eq!(now.peers.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["anton", "anorak"]);
        let _ = std::fs::remove_file(&status.file);
    }
}
