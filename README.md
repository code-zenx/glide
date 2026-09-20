# glide

Share one keyboard, mouse and clipboard between a Mac and a Linux box on the same
network. Either machine can drive the other: push the cursor off the edge of one
screen and it appears on the other, and the keyboard follows it. Copy text or an
image on either machine and it is on the other's clipboard a second later.

Tested between macOS (`goldengate`) and Arch with Hyprland (`anorak`), linked by
Ethernet: round trip 2.4 ms.

## How it is built

- **Transport** — QUIC (`quinn`). Pointer motion travels as unreliable datagrams, so
  a lost packet never stalls the cursor; keys, buttons, clipboard and cursor
  handover go on a reliable stream, so nothing gets stuck down.
- **Trust** — each machine generates a self-signed certificate on first run and
  pins the other's fingerprint, checked in both directions. No CA, no password;
  nothing else on the network can connect or inject input.
- **Input** — the `input-capture` and `input-emulation` crates from
  [lan-mouse](https://github.com/feschber/lan-mouse): layer-shell capture and
  wlroots emulation on Hyprland, native event taps on macOS.
- **Handover** — both machines watch their own screen edges, so control simply
  follows the cursor. Ctrl+Shift+Alt+Meta together yank input back if a peer
  stops answering.
- **Addresses** — a peer may list several (Ethernet, Wi-Fi, Tailscale). Both ends
  dial; the first connection that lands is used and the duplicate hangs up, so one
  end being firewalled costs nothing.

## Install

You need [Rust](https://rustup.rs) on both machines. Build takes about a minute.

### 1. Build, on each machine

```sh
git clone https://github.com/code-zenx/glide.git
cd glide
cargo build --release
```

**macOS** — build the app bundle instead, so macOS has one thing to trust:

```sh
bash contrib/bundle.sh          # builds and installs /Applications/Glide.app
```

### 2. Create a config, on each machine

```sh
./target/release/glide init --name mymac      # any name you like
./target/release/glide fingerprint            # note this down
```

### 3. Tell each machine about the other

Edit `~/.config/glide/config.toml` on both. Each one lists the *other* machine:

```toml
name = "mymac"                 # this machine
listen = "0.0.0.0:4242"

[[peer]]
name = "mybox"                 # the other machine
addrs = ["192.168.1.50:4242"]  # its IP
position = "right"             # where its screen sits: left, right, top, bottom
fingerprint = "<the fingerprint printed on the other machine>"
```

Set `position` to opposites — if the peer is `right` of this machine, this machine
is `left` of the peer. Rearranging from the macOS app fixes the other end
automatically.

### 4. Run it

**Linux:**

```sh
cp contrib/glide.service ~/.config/systemd/user/
systemctl --user enable --now glide
```

Edit the unit first if you cloned somewhere other than `~/projects/glide`. Under
Wayland it also needs `XDG_RUNTIME_DIR` and `WAYLAND_DISPLAY`, which the unit sets.

**macOS:** open Glide from Applications, then grant it two permissions in
System Settings → Privacy & Security:

- **Input Monitoring** — add `/Applications/Glide.app`, then Quit & Reopen when asked
- **Device Control and Data Access** (called Accessibility before macOS 27) — add the same app
- **Local Network** — allow it when the prompt appears, or the peer is unreachable

Add Glide to Login Items to have it start with the Mac.

That is it. Push the cursor off the edge you configured and it appears on the
other machine.

### Keeping macOS permissions across rebuilds

macOS ties a permission grant to the app's code signature. An ad-hoc signature
changes on every build, so every rebuild silently loses the grant. Sign with a
certificate — any certificate, including a self-signed one — and the grant sticks:

```sh
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
  -keyout key.pem -out cert.pem -subj "/CN=Glide Self Signed/O=Glide" \
  -addext "basicConstraints=critical,CA:false" \
  -addext "keyUsage=critical,digitalSignature" \
  -addext "extendedKeyUsage=critical,codeSigning"
openssl pkcs12 -export -legacy -out glide.p12 -inkey key.pem -in cert.pem \
  -passout pass:glide -name "Glide Self Signed"
security import glide.p12 -k ~/Library/Keychains/login.keychain-db \
  -T /usr/bin/codesign -P glide
```

`contrib/bundle.sh` picks that certificate up on its own from then on. Keep the
key somewhere safe: rebuild with a different certificate and the grants reset.

## Configuration

```toml
name = "goldengate"
listen = "0.0.0.0:4242"

[[peer]]
name = "anorak"
addrs = ["192.168.2.3:4242"]   # several are allowed: Ethernet, Wi-Fi, Tailscale
position = "top"               # which edge of this machine the peer sits on
display = 1                    # optional: which of this machine's displays owns
                               # that edge, numbered as the Arrange window shows
fingerprint = "8afca615..."    # from `glide fingerprint` on the other machine
```

Every ten seconds each side logs the round trip to the other:

```
INFO glide::link: rtt p50 2.37ms  p99 3.01ms  min 1.41ms  max 3.01ms  n=10 peer=anorak
```

### Per-machine notes

- **macOS** — the app bundle holds the daemon and the menu bar in one process.
  Its menu shows who is connected and which way input is flowing, and opens a
  window that draws both machines' real displays for arranging them.
- **Hyprland** — `contrib/waybar-glide.md` has a waybar module and matching CSS.
- **lan-mouse** — it holds the same port and grabs the same screen edges, so the
  two cannot run together. Its XDG autostart entry has to go.

## Seeing what it is doing

The daemon keeps a few lines in `~/.cache/glide/status` — mode, peer, round trip —
and rewrites them only when something changes. Anything can read it without
touching the daemon:

```sh
glide status            # → ● 1.8ms   or   → anorak   or   ✕ offline
glide status --json     # {"text":"● 1.8ms","tooltip":"...","class":"local"}
glide status --watch    # keep printing, once a second
```

- **macOS** — the menu bar item in Glide.app reads it directly, in process. It
  shows the app's badge, and its menu says who is connected, which way input is
  flowing and the round trip.
- **waybar** — point a custom module at the JSON:

  ```jsonc
  "custom/glide": { "exec": "glide status --json --icon --watch", "return-type": "json" }
  ```

  `--icon` prints the letter instead of words, so a stylesheet can draw the same
  rounded-square badge the Mac shows.

  The `class` field is `local`, `sending`, `receiving`, `offline` or `denied`, so
  the badge can be coloured per state. `contrib/waybar-glide.md` has the module and the CSS
  as installed on anorak.

## Clipboard

Text and images, both directions, capped at 1 MB and 16 MB.

Images cross as the bytes the clipboard already holds — `image/png`, `image/jpeg`
or `image/gif` — rather than being decoded to pixels and re-encoded. A screenshot
is PNG on both platforms already, so it arrives lossless and at its original size:
a terminal screenshot is a few hundred kilobytes, where the same image as raw
pixels would be eleven megabytes. That also keeps change detection cheap, since
the daemon hashes compressed bytes once a second rather than a full bitmap.

macOS reads and writes the pasteboard by UTI, Hyprland by MIME type, so neither
side needs an image library.

## Not done yet

- **The cursor does not arrive at the matching spot.** Leave the top edge on the
  left and you arrive wherever the other machine's cursor happened to be. The
  input crates carry relative motion only; landing in the right place means
  sending the crossing point and warping the remote cursor to it.
- **Cmd and Super are not remapped.** Scancodes pass through as they are, so
  shortcuts land as the other platform's keys.
- **Latency is round trip only** — no capture-to-inject measurement, which needs
  clock-offset estimation.
- **No pixel-accurate offsets along an edge.** A barrier is a whole side of a
  display, so two screens of different heights meet at whichever part overlaps.
- Preferring the lowest-latency address when several are up; today the first to
  answer wins.
- mDNS discovery, and an arrange window on Linux — there, screens are arranged by
  editing the config.

## License

GPL-3.0, because the lan-mouse input crates are.
