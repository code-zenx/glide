//! What screens each machine has, so the arrange window can draw the real
//! layout instead of four abstract compass points.

#[derive(Clone, Debug, PartialEq)]
pub struct Screen {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Screen {
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.x.to_le_bytes());
        out.extend_from_slice(&self.y.to_le_bytes());
        out.extend_from_slice(&self.w.to_le_bytes());
        out.extend_from_slice(&self.h.to_le_bytes());
        let name = self.name.as_bytes();
        out.push(name.len().min(u8::MAX as usize) as u8);
        out.extend_from_slice(&name[..name.len().min(u8::MAX as usize)]);
    }

    /// Returns the screen and how many bytes it used.
    pub fn decode(b: &[u8]) -> Option<(Screen, usize)> {
        let x = i32::from_le_bytes(b.get(0..4)?.try_into().ok()?);
        let y = i32::from_le_bytes(b.get(4..8)?.try_into().ok()?);
        let w = u32::from_le_bytes(b.get(8..12)?.try_into().ok()?);
        let h = u32::from_le_bytes(b.get(12..16)?.try_into().ok()?);
        let len = *b.get(16)? as usize;
        let name = String::from_utf8_lossy(b.get(17..17 + len)?).into_owned();
        Some((Screen { name, x, y, w, h }, 17 + len))
    }
}

/// The vendor number glide's own virtual display reports, so it can be told
/// apart from a real monitor.
#[cfg(target_os = "macos")]
pub const GLIDE_VENDOR: u32 = 0x676c;

/// The screens attached to this machine, left to right as the system sees them.
/// Leaves out glide's own virtual display: it stands in for the peer's screen,
/// and counting it would renumber the real displays the config refers to.
#[cfg(target_os = "macos")]
pub fn local() -> Vec<Screen> {
    use objc2_core_foundation::CGRect;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
        fn CGDisplayBounds(display: u32) -> CGRect;
        fn CGDisplayVendorNumber(display: u32) -> u32;
    }

    let mut ids = [0u32; 8];
    let mut found = 0u32;
    if unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut found) } != 0 {
        return Vec::new();
    }
    ids.iter()
        .take(found as usize)
        .filter(|&&id| unsafe { CGDisplayVendorNumber(id) } != GLIDE_VENDOR)
        .enumerate()
        .map(|(i, &id)| {
            let bounds = unsafe { CGDisplayBounds(id) };
            Screen {
                name: format!("Display {}", i + 1),
                x: bounds.origin.x as i32,
                y: bounds.origin.y as i32,
                w: bounds.size.width as u32,
                h: bounds.size.height as u32,
            }
        })
        .collect()
}

/// Hyprland already knows the layout; asking it beats talking Wayland again.
/// Anything else (nullbyte's Xfce) is X11, where xrandr knows.
#[cfg(not(target_os = "macos"))]
pub fn local() -> Vec<Screen> {
    let hyprland = hyprland();
    if hyprland.is_empty() { xrandr() } else { hyprland }
}

#[cfg(not(target_os = "macos"))]
fn hyprland() -> Vec<Screen> {
    let output = std::process::Command::new("hyprctl").args(["-j", "monitors"]).output();
    let Ok(output) = output else { return Vec::new() };
    let Ok(monitors) = serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout) else {
        return Vec::new();
    };
    monitors
        .iter()
        .map(|m| Screen {
            name: m["name"].as_str().unwrap_or("display").to_string(),
            x: m["x"].as_i64().unwrap_or(0) as i32,
            y: m["y"].as_i64().unwrap_or(0) as i32,
            w: m["width"].as_u64().unwrap_or(0) as u32,
            h: m["height"].as_u64().unwrap_or(0) as u32,
        })
        .collect()
}

#[cfg(not(target_os = "macos"))]
fn xrandr() -> Vec<Screen> {
    let Ok(output) = std::process::Command::new("xrandr").arg("--listmonitors").output() else { return Vec::new() };
    xrandr_monitors(&String::from_utf8_lossy(&output.stdout))
}

/// Mirrored outputs (a laptop panel cloned to HDMI) are one screen to the
/// cursor, so each spot on the desktop is listed once.
#[cfg(not(target_os = "macos"))]
fn xrandr_monitors(text: &str) -> Vec<Screen> {
    let mut screens: Vec<Screen> = Vec::new();
    for s in text.lines().skip(1).filter_map(xrandr_monitor) {
        if !screens.iter().any(|o| (o.x, o.y, o.w, o.h) == (s.x, s.y, s.w, s.h)) {
            screens.push(s);
        }
    }
    screens
}

/// One line of `xrandr --listmonitors`: ` 0: +*HDMI-1 1920/528x1080/297+0+0  HDMI-1`.
#[cfg(not(target_os = "macos"))]
fn xrandr_monitor(line: &str) -> Option<Screen> {
    let mut parts = line.split_whitespace();
    let geometry = parts.nth(2)?;
    let name = parts.last()?.to_string();
    let (size, at) = geometry.split_once('+')?;
    let (w, h) = size.split_once('x')?;
    let (x, y) = at.split_once('+')?;
    let pixels = |s: &str| s.split('/').next()?.parse().ok();
    Some(Screen { name, x: x.parse().ok()?, y: y.parse().ok()?, w: pixels(w)?, h: pixels(h)? })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screens_survive_the_wire() {
        let screen = Screen { name: "DP-1".into(), x: -1920, y: 0, w: 2560, h: 1440 };
        let mut bytes = Vec::new();
        screen.encode(&mut bytes);
        let (decoded, used) = Screen::decode(&bytes).unwrap();
        assert_eq!(decoded, screen);
        assert_eq!(used, bytes.len());
    }

    #[test]
    fn a_truncated_screen_is_refused() {
        let mut bytes = Vec::new();
        Screen { name: "x".into(), x: 0, y: 0, w: 1, h: 1 }.encode(&mut bytes);
        for len in 0..bytes.len() {
            assert!(Screen::decode(&bytes[..len]).is_none(), "accepted {len} bytes");
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn xrandr_lines_parse() {
        let screen = xrandr_monitor(" 1: +eDP-1 1920/309x1080/173+-1920+0  eDP-1").unwrap();
        assert_eq!(screen, Screen { name: "eDP-1".into(), x: -1920, y: 0, w: 1920, h: 1080 });
        assert!(xrandr_monitor("Monitors: 2").is_none());
        let mirrored = "Monitors: 2\n 0: +*HDMI-1 1920/528x1080/297+0+0  HDMI-1\n 1: +eDP-1 1920/309x1080/173+0+0  eDP-1\n";
        assert_eq!(xrandr_monitors(mirrored).len(), 1, "a mirror is one screen");
    }

    #[test]
    fn every_screen_reported_is_one_that_could_be_drawn() {
        let screens = local();
        assert!(screens.iter().all(|s| s.w > 0 && s.h > 0), "{screens:?}");
        // Asking for the screens needs a session to ask. Over SSH or in CI
        // there is no compositor and reporting nothing is the right answer;
        // where there is one, a machine with no screen would make the arrange
        // window lie.
        let session = cfg!(target_os = "macos")
            || std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var_os("DISPLAY").is_some();
        assert!(!session || !screens.is_empty(), "a machine with a display must report one");
    }
}
