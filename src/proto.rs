//! What one machine says to another, and how it goes on the wire.
//!
//! Pointer motion and pings travel as QUIC datagrams: they are worthless once
//! late, so a lost one is better dropped than retransmitted. Key presses,
//! buttons and who has the cursor go on a reliable stream, because a dropped
//! key-up leaves a modifier stuck down on the peer. The clipboard gets a
//! stream to itself: it is the only message that can run to megabytes, and
//! anything queued behind it would wait for all of them.

use anyhow::{bail, Context, Result};
use input_event::{Event, KeyboardEvent, PointerEvent};

use crate::clipboard::Clip;
use crate::screens::Screen;
use crate::Side;

#[derive(Debug, PartialEq, Clone)]
pub enum Msg {
    /// Latency probe carrying the sender's monotonic nanoseconds.
    Ping(u64),
    /// The same number, echoed back.
    Pong(u64),
    /// "I have the cursor now, here comes input."
    Enter,
    /// "You have the cursor back."
    Leave,
    Input(Event),
    /// Whatever was copied: text, or an image in the bytes the clipboard
    /// already held.
    Clipboard(Clip),
    /// The screens this machine has, sent once per connection so the other end
    /// can draw the real layout.
    Screens(Vec<Screen>),
    /// "You are on this side of me" — sent when one end is rearranged, so both
    /// ends agree which edges face each other.
    Arrange(Side),
}

/// Which of the connection's three channels a message belongs on.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Route {
    /// Droppable and worthless once late.
    Datagram,
    /// Must arrive, and small enough that the next one is never held up.
    Stream,
    /// Must arrive, may be megabytes: gets a stream of its own.
    Bulk,
}

impl Msg {
    pub fn route(&self) -> Route {
        match self {
            Msg::Ping(_) | Msg::Pong(_) => Route::Datagram,
            Msg::Input(Event::Pointer(PointerEvent::Motion { .. })) => Route::Datagram,
            Msg::Clipboard(_) => Route::Bulk,
            _ => Route::Stream,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(24);
        match self {
            Msg::Ping(t) => {
                out.push(0);
                out.extend_from_slice(&t.to_le_bytes());
            }
            Msg::Pong(t) => {
                out.push(1);
                out.extend_from_slice(&t.to_le_bytes());
            }
            Msg::Enter => out.push(2),
            Msg::Leave => out.push(3),
            Msg::Input(event) => {
                out.push(4);
                encode_event(event, &mut out);
            }
            Msg::Clipboard(Clip::Text(text)) => {
                out.push(5);
                out.extend_from_slice(text.as_bytes());
            }
            Msg::Clipboard(Clip::Image { mime, bytes }) => {
                out.push(8);
                out.push(mime.len().min(u8::MAX as usize) as u8);
                out.extend_from_slice(&mime.as_bytes()[..mime.len().min(u8::MAX as usize)]);
                out.extend_from_slice(bytes);
            }
            Msg::Arrange(side) => {
                out.push(7);
                out.push(match side {
                    Side::Left => 0,
                    Side::Right => 1,
                    Side::Top => 2,
                    Side::Bottom => 3,
                });
            }
            Msg::Screens(screens) => {
                out.push(6);
                out.push(screens.len().min(u8::MAX as usize) as u8);
                for screen in screens.iter().take(u8::MAX as usize) {
                    screen.encode(&mut out);
                }
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Msg> {
        let (&tag, rest) = bytes.split_first().ok_or_else(|| anyhow::anyhow!("empty message"))?;
        Ok(match tag {
            0 => Msg::Ping(u64_at(rest, 0)?),
            1 => Msg::Pong(u64_at(rest, 0)?),
            2 => Msg::Enter,
            3 => Msg::Leave,
            4 => Msg::Input(decode_event(rest)?),
            5 => Msg::Clipboard(Clip::Text(String::from_utf8(rest.to_vec())?)),
            8 => {
                let (&len, rest) = rest.split_first().context("no mime length")?;
                let mime = rest.get(..len as usize).context("truncated mime type")?;
                Msg::Clipboard(Clip::Image {
                    mime: String::from_utf8(mime.to_vec())?,
                    bytes: rest[len as usize..].to_vec(),
                })
            }
            6 => {
                let (&count, mut rest) = rest.split_first().context("no screen count")?;
                let mut screens = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    let (screen, used) = Screen::decode(rest).context("truncated screen")?;
                    screens.push(screen);
                    rest = &rest[used..];
                }
                Msg::Screens(screens)
            }
            7 => Msg::Arrange(match rest.first().context("no side")? {
                0 => Side::Left,
                1 => Side::Right,
                2 => Side::Top,
                3 => Side::Bottom,
                other => bail!("unknown side {other}"),
            }),
            _ => bail!("unknown message tag {tag}"),
        })
    }
}

fn encode_event(event: &Event, out: &mut Vec<u8>) {
    match event {
        Event::Pointer(PointerEvent::Motion { time, dx, dy }) => {
            out.push(0);
            out.extend_from_slice(&time.to_le_bytes());
            out.extend_from_slice(&dx.to_le_bytes());
            out.extend_from_slice(&dy.to_le_bytes());
        }
        Event::Pointer(PointerEvent::Button { time, button, state }) => {
            out.push(1);
            out.extend_from_slice(&time.to_le_bytes());
            out.extend_from_slice(&button.to_le_bytes());
            out.extend_from_slice(&state.to_le_bytes());
        }
        Event::Pointer(PointerEvent::Axis { time, axis, value }) => {
            out.push(2);
            out.extend_from_slice(&time.to_le_bytes());
            out.push(*axis);
            out.extend_from_slice(&value.to_le_bytes());
        }
        Event::Pointer(PointerEvent::AxisDiscrete120 { axis, value }) => {
            out.push(3);
            out.push(*axis);
            out.extend_from_slice(&value.to_le_bytes());
        }
        Event::Keyboard(KeyboardEvent::Key { time, key, state }) => {
            out.push(4);
            out.extend_from_slice(&time.to_le_bytes());
            out.extend_from_slice(&key.to_le_bytes());
            out.push(*state);
        }
        Event::Keyboard(KeyboardEvent::Modifiers { depressed, latched, locked, group }) => {
            out.push(5);
            for field in [depressed, latched, locked, group] {
                out.extend_from_slice(&field.to_le_bytes());
            }
        }
    }
}

fn decode_event(b: &[u8]) -> Result<Event> {
    let (&kind, b) = b.split_first().ok_or_else(|| anyhow::anyhow!("empty event"))?;
    Ok(match kind {
        0 => Event::Pointer(PointerEvent::Motion {
            time: u32_at(b, 0)?,
            dx: f64_at(b, 4)?,
            dy: f64_at(b, 12)?,
        }),
        1 => Event::Pointer(PointerEvent::Button {
            time: u32_at(b, 0)?,
            button: u32_at(b, 4)?,
            state: u32_at(b, 8)?,
        }),
        2 => Event::Pointer(PointerEvent::Axis {
            time: u32_at(b, 0)?,
            axis: *b.get(4).ok_or_else(|| anyhow::anyhow!("short axis event"))?,
            value: f64_at(b, 5)?,
        }),
        3 => Event::Pointer(PointerEvent::AxisDiscrete120 {
            axis: *b.first().ok_or_else(|| anyhow::anyhow!("short axis event"))?,
            value: i32::from_le_bytes(array_at(b, 1)?),
        }),
        4 => Event::Keyboard(KeyboardEvent::Key {
            time: u32_at(b, 0)?,
            key: u32_at(b, 4)?,
            state: *b.get(8).ok_or_else(|| anyhow::anyhow!("short key event"))?,
        }),
        5 => Event::Keyboard(KeyboardEvent::Modifiers {
            depressed: u32_at(b, 0)?,
            latched: u32_at(b, 4)?,
            locked: u32_at(b, 8)?,
            group: u32_at(b, 12)?,
        }),
        _ => bail!("unknown event kind {kind}"),
    })
}

fn array_at<const N: usize>(b: &[u8], at: usize) -> Result<[u8; N]> {
    let slice = b.get(at..at + N).ok_or_else(|| anyhow::anyhow!("message too short"))?;
    Ok(slice.try_into()?)
}

fn u32_at(b: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(array_at(b, at)?))
}

fn u64_at(b: &[u8], at: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(array_at(b, at)?))
}

fn f64_at(b: &[u8], at: usize) -> Result<f64> {
    Ok(f64::from_le_bytes(array_at(b, at)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_message_survives_the_wire() {
        let messages = [
            Msg::Ping(12345),
            Msg::Pong(u64::MAX),
            Msg::Enter,
            Msg::Leave,
            Msg::Clipboard(Clip::Text("hello wörld".into())),
            Msg::Clipboard(Clip::Image { mime: "image/png".into(), bytes: vec![0x89, b'P', 0, 255] }),
            Msg::Clipboard(Clip::Image { mime: "image/jpeg".into(), bytes: Vec::new() }),
            Msg::Screens(vec![
                Screen { name: "Display 1".into(), x: 0, y: 0, w: 1512, h: 982 },
                Screen { name: "DP-1".into(), x: 1512, y: -200, w: 2560, h: 1440 },
            ]),
            Msg::Screens(vec![]),
            Msg::Arrange(Side::Left),
            Msg::Arrange(Side::Bottom),
            Msg::Input(Event::Pointer(PointerEvent::Motion { time: 7, dx: -1.5, dy: 2.25 })),
            Msg::Input(Event::Pointer(PointerEvent::Button { time: 8, button: 0x110, state: 1 })),
            Msg::Input(Event::Pointer(PointerEvent::Axis { time: 9, axis: 1, value: -3.5 })),
            Msg::Input(Event::Pointer(PointerEvent::AxisDiscrete120 { axis: 0, value: -120 })),
            Msg::Input(Event::Keyboard(KeyboardEvent::Key { time: 10, key: 30, state: 1 })),
            Msg::Input(Event::Keyboard(KeyboardEvent::Modifiers {
                depressed: 4,
                latched: 0,
                locked: 2,
                group: 0,
            })),
        ];
        for msg in messages {
            assert_eq!(Msg::decode(&msg.encode()).unwrap(), msg, "{msg:?}");
        }
    }

    #[test]
    fn every_message_goes_down_the_right_channel() {
        let motion = Event::Pointer(PointerEvent::Motion { time: 0, dx: 1.0, dy: 0.0 });
        assert_eq!(Msg::Ping(1).route(), Route::Datagram);
        assert_eq!(Msg::Input(motion).route(), Route::Datagram);
        let key = Event::Keyboard(KeyboardEvent::Key { time: 0, key: 1, state: 0 });
        assert_eq!(Msg::Input(key).route(), Route::Stream);
        assert_eq!(Msg::Enter.route(), Route::Stream);
        assert_eq!(Msg::Arrange(Side::Left).route(), Route::Stream);
        // Both kinds of clipboard, because text can be a megabyte too.
        assert_eq!(Msg::Clipboard(Clip::Text(String::new())).route(), Route::Bulk);
        let image = Clip::Image { mime: "image/png".into(), bytes: vec![0; 8] };
        assert_eq!(Msg::Clipboard(image).route(), Route::Bulk);
    }

    #[test]
    fn truncated_messages_are_refused_not_panicked() {
        let full = Msg::Input(Event::Pointer(PointerEvent::Motion { time: 1, dx: 1.0, dy: 1.0 })).encode();
        for len in 0..full.len() {
            assert!(Msg::decode(&full[..len]).is_err(), "accepted {len} bytes");
        }
        assert!(Msg::decode(&[9, 9, 9]).is_err());
    }
}
