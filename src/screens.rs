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

/// The screens attached to this machine, left to right as the system sees them.
#[cfg(target_os = "macos")]
pub fn local() -> Vec<Screen> {
    use objc2_core_foundation::CGRect;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
        fn CGDisplayBounds(display: u32) -> CGRect;
    }

    let mut ids = [0u32; 8];
    let mut found = 0u32;
    if unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut found) } != 0 {
        return Vec::new();
    }
    ids.iter()
        .take(found as usize)
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
#[cfg(not(target_os = "macos"))]
pub fn local() -> Vec<Screen> {
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

    #[test]
    fn this_machine_reports_at_least_one_screen() {
        // A machine with no screen at all would make the arrange window lie.
        let screens = local();
        assert!(!screens.is_empty(), "no screens found");
        assert!(screens.iter().all(|s| s.w > 0 && s.h > 0), "{screens:?}");
    }
}
