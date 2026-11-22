//! Whole-link tests over real loopback endpoints.

use super::*;
use crate::proto::Route;

/// A clipboard image must never be able to claim the reliable stream, or a
/// key press waits behind sixteen megabytes.
#[test]
fn the_clipboard_never_shares_a_lane_with_input() {
    use crate::clipboard::Clip;
    use input_event::{Event, KeyboardEvent, PointerEvent};

    let image = Msg::Clipboard(Clip::Image { mime: "image/png".into(), bytes: vec![0; 64] });
    assert!(matches!(image.route(), Route::Bulk));
    assert!(matches!(Msg::Clipboard(Clip::Text("hi".into())).route(), Route::Bulk));

    let key = Msg::Input(Event::Keyboard(KeyboardEvent::Key { time: 0, key: 1, state: 1 }));
    assert!(matches!(key.route(), Route::Stream));
    assert!(matches!(Msg::Enter.route(), Route::Stream));

    let motion = Msg::Input(Event::Pointer(PointerEvent::Motion { time: 0, dx: 1.0, dy: 0.0 }));
    assert!(matches!(motion.route(), Route::Datagram));
    assert!(matches!(Msg::Ping(0).route(), Route::Datagram));
}

/// Off means off: a server refuses a switched-off device's link with a
/// reason, and the device learns it is off instead of retrying blindly.
#[tokio::test(flavor = "multi_thread")]
async fn a_switched_off_device_is_refused_and_told_why() -> Result<()> {
    use crate::{Role, Side};
    let dir = std::env::temp_dir().join(format!("glide-off-{}", std::process::id()));
    let (id_server, id_client) = (Identity::load_or_create(&dir.join("s"))?, Identity::load_or_create(&dir.join("c"))?);
    let config = |peer: &Identity, role, enabled| Config {
        name: "me".into(),
        role,
        listen: "127.0.0.1:0".parse().unwrap(),
        display: Default::default(),
        peers: vec![Peer { name: "peer".into(), addrs: vec![], fingerprint: peer.fingerprint(), position: Side::Right, display: None, enabled, control: false }],
    };
    let (server_cfg, client_cfg) = (config(&id_client, Role::Server, false), config(&id_server, Role::Client, true));
    let server_links = Arc::new(Links::new(server_cfg.peers.clone(), true));
    let client_links = Arc::new(Links::new(client_cfg.peers.clone(), false));
    assert!(server_links.is_off("peer"), "enabled = false in the config reads as off");
    let server = endpoint(&server_cfg, &id_server, server_links.clone())?.endpoint;
    let client = endpoint(&client_cfg, &id_client, client_links.clone())?.endpoint;
    let status = Status::at(dir.join("status"));
    let (to_session, _inbox) = unbounded_channel();
    let (linked, _states) = unbounded_channel();

    let (accepted, dialed) = tokio::join!(
        async { server.accept().await.unwrap().await.unwrap() },
        async { client.connect(server.local_addr().unwrap(), "glide").unwrap().await.unwrap() },
    );
    serve(accepted, "peer", false, &server_links, &status, &to_session, &linked).await?;
    assert!(!server_links.connected("peer"), "the server never registers it");
    let _ = serve(dialed.clone(), "peer", true, &client_links, &status, &to_session, &linked).await;
    check_told_off(&dialed, "peer", &client_links, &status);
    assert!(client_links.is_off("peer"), "the client knows it was switched off");
    assert_eq!(status.snapshot().peers[0].mode, "off");
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// The bug this whole split exists for. A clipboard image spans thousands
/// of packets, so while it was framed on the input stream every ping that
/// landed mid-frame cancelled a `read_exact` that had already swallowed
/// bytes, desynchronising the stream for the rest of the link — and until
/// it finished, every key press waited behind the whole image.
///
/// Two real endpoints over loopback, because nothing smaller would have
/// caught it.
#[tokio::test(flavor = "multi_thread")]
async fn a_big_clipboard_item_neither_corrupts_nor_delays_input() -> Result<()> {
    use crate::clipboard::Clip;
    use crate::{Role, Side};
    use input_event::{Event, KeyboardEvent};

    const KEYS: u32 = 200;
    let dir = std::env::temp_dir().join(format!("glide-link-{}", std::process::id()));
    let (here, there) = (dir.join("here"), dir.join("there"));
    let (id_here, id_there) = (Identity::load_or_create(&here)?, Identity::load_or_create(&there)?);

    let config = |peer: &Identity, name: &str| Config {
        name: name.to_string(),
        role: Role::Server,
        listen: "127.0.0.1:0".parse().unwrap(),
        display: Default::default(),
        peers: vec![Peer {
            name: "peer".into(),
            addrs: vec![],
            fingerprint: peer.fingerprint(),
            position: Side::Right,
            display: None,
            enabled: true,
            control: false,
        }],
    };
    let (cfg_here, cfg_there) = (config(&id_there, "here"), config(&id_here, "there"));
    let out = Arc::new(Links::new(cfg_here.peers.clone(), false));
    let links = Arc::new(Links::new(cfg_there.peers.clone(), false));
    let sender = endpoint(&cfg_here, &id_here, out.clone())?.endpoint;
    let receiver = endpoint(&cfg_there, &id_there, links.clone())?.endpoint;
    let addr = receiver.local_addr()?;

    let (to_session, mut inbox) = unbounded_channel();
    let (unused, _drop) = unbounded_channel();
    let (linked, mut link_state) = unbounded_channel();
    let status = Arc::new(Status::at(dir.join("status")));

    // Both connections are built here rather than inside the tasks, so the
    // test keeps a handle it can close: aborting a `serve` leaves the
    // writer and pinger it spawned holding the connection open.
    let (accepted, dialed) = tokio::join!(
        async { receiver.accept().await.unwrap().await.unwrap() },
        async { sender.connect(addr, "glide").unwrap().await.unwrap() },
    );
    let hang_up = dialed.clone();
    let accepting = tokio::spawn({
        let (links, status, linked) = (links.clone(), status.clone(), linked.clone());
        async move {
            let _ = serve(accepted, "peer", false, &links, &status, &to_session, &linked).await;
        }
    });
    let dialing = tokio::spawn({
        let (out, status, linked) = (out.clone(), status.clone(), linked.clone());
        async move {
            let _ = serve(dialed, "peer", true, &out, &status, &unused, &linked).await;
        }
    });
    while !out.connected("peer") {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Two megabytes of clipboard, then a burst of key presses right behind
    // it — the order that used to wedge the link.
    let bytes: Vec<u8> = (0..2_000_000u32).map(|i| i as u8).collect();
    let image = Clip::Image { mime: "image/png".into(), bytes: bytes.clone() };
    out.send("peer", Msg::Clipboard(image.clone()));
    for time in 0..KEYS {
        out.send("peer", Msg::Input(Event::Keyboard(KeyboardEvent::Key { time, key: 30, state: 1 })));
    }

    let (mut keys, mut clip) = (0u32, None);
    let collect = async {
        while keys < KEYS || clip.is_none() {
            match inbox.recv().await.expect("link died").1 {
                Msg::Input(Event::Keyboard(KeyboardEvent::Key { time, .. })) => {
                    assert_eq!(time, keys, "a key arrived out of order or was lost");
                    keys += 1;
                }
                Msg::Clipboard(got) => {
                    // The lanes are independent, so keys sent after the
                    // image still overtake it. Sharing one stream put this
                    // at exactly zero; how far past zero is a race between
                    // two sockets and not worth pinning down.
                    assert!(keys > 0, "every key queued behind the clipboard");
                    clip = Some(got);
                }
                _ => {} // the screen list each side sends on connect
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(20), collect).await.expect("timed out");
    assert_eq!(clip, Some(image), "the clipboard arrived changed");

    // The session arms its screen edge on the first of these and drops it
    // again on the second. Without the second, an edge outlives the link
    // that justified it and starts grabbing the cursor for nobody.
    assert_eq!(link_state.recv().await, Some(("peer".to_string(), true)));
    hang_up.close(0u32.into(), b"test over");
    let down = async {
        while link_state.recv().await != Some(("peer".to_string(), false)) {}
    };
    tokio::time::timeout(Duration::from_secs(20), down).await.expect("no link-down signal");

    accepting.abort();
    dialing.abort();
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
