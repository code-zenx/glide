//! A small file the daemon keeps current so anything else — `glide status`, the
//! macOS menu bar, a waybar module — can show what is happening without talking
//! to the daemon or costing it a thread.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::Result;

use crate::screens::Screen;

#[derive(Clone, Debug, PartialEq)]
pub struct State {
    pub peer: String,
    /// One of: offline, local, sending, receiving.
    pub mode: String,
    pub rtt_ms: Option<f64>,
    /// This machine cannot capture or type. Kept apart from `mode` because a
    /// link that drops and reconnects must not erase a permission problem.
    pub denied: bool,
}

impl Default for State {
    fn default() -> Self {
        Self { peer: String::new(), mode: "offline".into(), rtt_ms: None, denied: false }
    }
}

impl State {
    /// One line fit for a menu bar or a status bar.
    pub fn summary(&self) -> String {
        if self.denied {
            return "⚠ needs permission".into();
        }
        match (self.mode.as_str(), self.rtt_ms) {
            ("sending", _) => format!("→ {}", self.peer),
            ("receiving", _) => format!("← {}", self.peer),
            ("local", Some(rtt)) => format!("● {rtt:.1}ms"),
            ("local", None) => "● linked".into(),
            _ => "✕ offline".into(),
        }
    }

    /// Who this machine is linked to, and whether it is linked at all.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn headline(&self) -> String {
        match self.mode.as_str() {
            _ if self.denied => "Input permission needed".into(),
            "offline" => "Not connected".into(),
            _ => format!("Linked to {}", self.peer),
        }
    }

    /// Which way input is flowing right now.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn flow(&self) -> String {
        if self.denied {
            return "Grant Input Monitoring and Device Control".into();
        }
        match self.mode.as_str() {
            "sending" => format!("You are controlling {}", self.peer),
            "receiving" => format!("{} is controlling this Mac", self.peer),
            "local" => "Input stays on this Mac".into(),
            _ => format!("Looking for {}", self.peer),
        }
    }

    /// The round trip, spelled out.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn speed(&self) -> String {
        match self.rtt_ms {
            Some(rtt) => format!("{rtt:.2} ms round trip"),
            None => "No timing yet".into(),
        }
    }

    pub fn tooltip(&self) -> String {
        let rtt = self.rtt_ms.map_or("no samples yet".into(), |r| format!("{r:.2} ms round trip"));
        if self.denied {
            return "glide: grant Input Monitoring and Device Control to Glide, then restart it".into();
        }
        match self.mode.as_str() {
            "sending" => format!("glide: controlling {}, {rtt}", self.peer),
            "receiving" => format!("glide: {} is controlling this machine, {rtt}", self.peer),
            "local" => format!("glide: linked to {}, input is local, {rtt}", self.peer),
            _ => "glide: no peer connected".into(),
        }
    }

    /// waybar and friends want one JSON object per line. `badge` swaps the
    /// wordy text for the app's letter, which a stylesheet can then draw as the
    /// same rounded square the Mac shows in its menu bar.
    pub fn json(&self, badge: bool) -> String {
        let text = if badge { "G".to_string() } else { self.summary() };
        format!(
            r#"{{"text":"{}","tooltip":"{}","class":"{}"}}"#,
            escape(&text),
            escape(&self.tooltip()),
            escape(&self.class())
        )
    }

    /// What a stylesheet keys off: the flow, or the problem.
    pub fn class(&self) -> String {
        if self.denied {
            "denied".into()
        } else {
            self.mode.clone()
        }
    }

    fn encode(&self) -> String {
        let rtt = self.rtt_ms.map_or(String::new(), |r| format!("{r:.3}"));
        format!(
            "mode={}\npeer={}\nrtt_ms={}\ndenied={}\n",
            self.mode, self.peer, rtt, self.denied
        )
    }

    fn decode(text: &str) -> State {
        let mut state = State::default();
        for (key, value) in text.lines().filter_map(|l| l.split_once('=')) {
            match key {
                "mode" => state.mode = value.to_string(),
                "peer" => state.peer = value.to_string(),
                "rtt_ms" => state.rtt_ms = value.parse().ok(),
                "denied" => state.denied = value == "true",
                _ => {}
            }
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
#[derive(Default)]
pub struct Status {
    current: Mutex<State>,
    /// What each peer says it has attached, for the arrange window. Not
    /// written to the status file: it changes rarely and nothing else wants it.
    peer_screens: Mutex<HashMap<String, Vec<Screen>>>,
}

impl Status {
    pub fn set(&self, change: impl FnOnce(&mut State)) {
        let mut current = self.current.lock().unwrap();
        let before = current.clone();
        change(&mut current);
        if *current != before {
            let _ = write(&current);
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

    pub fn offline(&self, peer: &str) {
        self.set(|s| {
            s.mode = "offline".into();
            s.peer = peer.to_string();
            s.rtt_ms = None;
        });
    }

    /// Whether this machine can capture and type at all.
    pub fn allowed(&self, allowed: bool) {
        self.set(|s| s.denied = !allowed);
    }
}

fn write(state: &State) -> Result<()> {
    let path = path();
    std::fs::create_dir_all(path.parent().unwrap())?;
    // Rename over the old file so a reader never sees half a line.
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, state.encode())?;
    std::fs::rename(&temp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn survives_a_round_trip() {
        let state =
            State { peer: "anorak".into(), mode: "sending".into(), rtt_ms: Some(1.844), denied: true };
        assert_eq!(State::decode(&state.encode()), state);
    }

    #[test]
    fn missing_fields_fall_back_to_offline() {
        assert_eq!(State::decode(""), State::default());
        assert_eq!(State::decode("mode=local\npeer=x\nrtt_ms=\n").rtt_ms, None);
        assert_eq!(State::decode("garbage").mode, "offline");
    }

    #[test]
    fn json_stays_valid_when_a_peer_name_is_hostile() {
        let state = State {
            peer: "an\"ora\\k".into(),
            mode: "sending".into(),
            rtt_ms: None,
            denied: false,
        };
        let json = state.json(false);
        assert!(json.contains(r#"an\"ora\\k"#), "{json}");
        assert_eq!(json.matches(r#"":""#).count(), 3, "{json}");
    }

    #[test]
    fn summaries_say_which_way_input_is_going() {
        let mut state =
            State { peer: "anorak".into(), mode: "local".into(), rtt_ms: Some(1.84), denied: false };
        assert_eq!(state.summary(), "● 1.8ms");
        state.mode = "sending".into();
        assert_eq!(state.summary(), "→ anorak");
        state.mode = "receiving".into();
        assert_eq!(state.summary(), "← anorak");
        state.mode = "offline".into();
        assert_eq!(state.summary(), "✕ offline");
    }

    #[test]
    fn the_badge_is_just_the_letter() {
        let state =
            State { peer: "anorak".into(), mode: "sending".into(), rtt_ms: Some(2.0), denied: false };
        assert!(state.json(true).contains(r#""text":"G""#), "{}", state.json(true));
        assert!(state.json(false).contains("anorak"), "{}", state.json(false));
        // A denied machine says so through the class, whatever the mode is.
        let denied = State { denied: true, ..state };
        assert!(denied.json(true).contains(r#""class":"denied""#), "{}", denied.json(true));
    }

    #[test]
    fn a_reconnect_cannot_erase_a_permission_problem() {
        let mut state =
            State { peer: "anorak".into(), mode: "local".into(), rtt_ms: Some(1.8), denied: true };
        assert_eq!(state.summary(), "⚠ needs permission");
        // The link dropping and coming back only ever touches `mode`.
        state.mode = "offline".into();
        state.mode = "local".into();
        assert_eq!(state.summary(), "⚠ needs permission", "denial must survive a reconnect");
    }
}
