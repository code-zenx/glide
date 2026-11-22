//! Wayland says when the clipboard changes: a data-control device gets a
//! `selection` event for every new copy. The clipboard watcher reads only
//! after one, instead of pulling the whole clipboard (a screenshot can be
//! megabytes) through a pipe every second.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{delegate_noop, event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_device_v1::{
    self, ZwlrDataControlDeviceV1,
};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1;
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_offer_v1::ZwlrDataControlOfferV1;

/// Watch on a thread of its own: `changed` turns true at every new copy, and
/// once at the start for what is already there. Err when the compositor has
/// no data-control, in which case the caller keeps reading every tick.
pub fn watch(changed: Arc<AtomicBool>) -> Result<()> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    let manager: ZwlrDataControlManagerV1 = globals.bind(&qh, 1..=2, ()).context("no data-control")?;
    let seat: WlSeat = globals.bind(&qh, 1..=1, ()).context("no seat")?;
    let device = manager.get_data_device(&seat, &qh, ());
    let mut state = State { changed, offer: None };
    queue.roundtrip(&mut state)?;
    std::thread::Builder::new().name("glide-clipboard".into()).spawn(move || {
        // Owned here so the device, and the connection behind it, live as long
        // as the thread does.
        let _kept = (conn, manager, device);
        while queue.blocking_dispatch(&mut state).is_ok() {}
    })?;
    Ok(())
}

struct State {
    changed: Arc<AtomicBool>,
    /// The offer behind the current selection; the next one replaces it.
    offer: Option<ZwlrDataControlOfferV1>,
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::Selection { id } => {
                // A spent offer must be destroyed, or every copy leaks one.
                if let Some(old) = std::mem::replace(&mut state.offer, id) {
                    old.destroy();
                }
                state.changed.store(true, Ordering::Relaxed);
            }
            // Middle-click selections are not synced; their offers just go.
            zwlr_data_control_device_v1::Event::PrimarySelection { id: Some(offer) } => offer.destroy(),
            _ => {}
        }
    }

    event_created_child!(State, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

/// `registry_queue_init` keeps the global list itself; nothing to add.
impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ZwlrDataControlManagerV1);
delegate_noop!(State: ignore WlSeat);
delegate_noop!(State: ignore ZwlrDataControlOfferV1);
