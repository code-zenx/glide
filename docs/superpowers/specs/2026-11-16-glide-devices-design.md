# glide devices: pairing, on/off, forget, per-device extend, new menu

Date: 2026-11-16. Branch: `multi-client` (uncommitted work from 2026-11-15 included: X11 client support for `nullbyte`).

## 1. Intent

glide already lets one server (the Mac, `goldengate`) drive several clients (`anorak`, `nullbyte`). What is missing is the everyday handling of several devices:

- **Adding a device** today means `glide init` on the new box, then pasting fingerprints and addresses into *both* `config.toml` files by hand, then restarting the Mac app. The owner wants this to be easy.
- **Switching a device off** today only *pauses* it (link stays up). The owner wants a device switched off **completely**.
- **The menu** is built around one peer (three status lines about one "focus" peer, a Control/Extend pair hard-wired to `config.peers[0]`). The owner wants it redesigned around a list of devices.

Success: the owner adds a new Linux box with one paste, turns any device fully off and on from the menu, forgets a device, picks which device is the extended display, and reads every device's state at a glance, all without restarting Glide.app or editing config files.

## 2. Goals

1. **Invite pairing** — Mac menu *Add Device…* (or `glide invite`) produces a one-line `glide join …` command; pasting it on a new machine pairs it securely with no other step.
2. **Two states per device: On / Off.** Off = link fully closed, no input, no clipboard.
3. **Forget Device** — remove a device from the Mac.
4. **Extend per device** — choose which device shows the extended display.
5. **Menu option A** (picked from mockups, §7).
6. **No restarts on the server** for any of the above.

## 3. Non-goals

- Auto-discovery (mDNS / "Nearby devices"). Rejected for now; can be layered later.
- Code-comparison pairing (Bluetooth-style 6-digit codes).
- A third "Paused" state. Pause is **replaced** by Off.
- A pairing GUI on Linux. Linux side is CLI (`glide join`).
- Several servers, client-to-client chaining, renaming devices.
- Changing the client-side self-pause (`glide ctl pause` from a client's own bar). It stays as it is.

## 4. Anti-goals (failure even if it "works")

- **Any path for an unpaired machine on the LAN to receive input or clipboard.** A rogue "client" that links would get the owner's keystrokes on crossing and every copied password via clipboard sync. Trust-on-first-use is therefore rejected.
- Existing configs breaking: `anorak` and `nullbyte` must keep working after upgrade without re-pairing.
- Stuck cursor or stuck keys when a device is turned off or forgotten while it holds the cursor.
- An invite that works more than once, or forever.

## 5. Constraints (from the code as of 2026-11-16)

- Trust is certificate pinning: each machine has a self-signed cert (`~/.config/glide/{cert.der,key.der}`); `[[peer]].fingerprint` is the SHA-256 of the peer's cert. The same `Pinned` verifier runs both ways (`src/link.rs`: `ServerCertVerifier` / `ClientCertVerifier for Pinned`). After the handshake, `identify()` maps the cert to a configured peer name; unknown certs are dropped.
- Connections are told apart by ALPN: `glide/0` (control + input + clipboard) and `glide-video/0` (extended display).
- Unknown `Msg` tags are already ignored by receivers (`Msg::decode` errors are logged at debug and skipped in all three read loops), so new message tags are wire-compatible with older peers.
- Both ends dial each other every 2 s (`dial_forever`); first link wins; duplicates hang up. The Mac's application firewall permits inbound connections to `Glide.app` (checked 2026-11-16).
- Peer identity inside the daemon is the **index into `config.peers`** (`CaptureHandle`): the input session keeps `Vec`s sized to the peer count (`armed`, `linked`, `enabled`, `held_off`), capture/emulation handles are indices, `Control::SetEnabled(handle, …)` and `SetPosition(handle, …)` carry indices, the menu row tag is the index. The peer list is fixed at startup today.
- `config.peers` is also cloned into `link::run` (for `identify` and to spawn one `dial_forever` per peer) and the clipboard watcher (peer name list) and the control socket (name list).
- Extended display: `display::host` on macOS takes `config.peers[0]` at startup; `[display] mode = "control" | "extend"` is persisted. The receiving end (`display_linux.rs`) needs Hyprland (`hyprctl`, `waylandsink`); `nullbyte` (X11) cannot show it.
- Config is TOML; the Arrange window writes `position`/`display` back into it; `enabled = false` is written only when off (default `true` stays out of the file).
- macOS menu: `src/menubar.rs` (objc2, `NSMenu`, 1 s refresh timer). Status state: `src/status.rs` (`State`, `PeerState`).

## 6. Approaches considered

### 6.1 Pairing

| Option | What | Trade-off | Verdict |
|---|---|---|---|
| **A. Invite command** | Mac copies `glide join <token>`; token carries server name, addresses, server fingerprint and a one-time secret. New machine pastes it. | One paste, no codes. Security from the secret travelling over a channel the owner controls (their clipboard / ssh). ~1 day. | **Picked** by the owner |
| B. Matching codes | `glide pair <ip>`; both sides show a 6-digit code; owner clicks Pair on the Mac. | Nothing to copy; needs an alert on the Mac and a prompt on Linux. More work. | Not picked |
| C. Discovery + approve | mDNS announce; "Nearby" list in the menu, then codes. | Nicest; new dependency, discovery on X11 and Hyprland, more macOS Local Network friction. Biggest. | Not picked (later, on top of B) |
| — Trust first seen | Accept any new cert. | Rogue machine gets keystrokes and clipboard. | Rejected (anti-goal) |

### 6.2 Device states

| Option | Verdict |
|---|---|
| On / Paused / Off | Rejected by the owner: Pause adds little over Off (instant resume, latency visible). |
| **On / Off** (Off = completely off) | **Picked.** Old `enabled = false` reads as Off. |

### 6.3 Menu

Five mockups were shown (A, B, C1, C2, C3; temp HTML, not kept). **The owner picked A.** Details in §7.

## 7. Design

### 7.1 Invite pairing

**On the Mac**

1. Menu *Add Device…* (or `glide invite` in a terminal, which asks the running daemon over the control socket) creates an invite: a random 128-bit secret, valid **10 minutes**, **single use**. One outstanding invite at a time; a new one replaces the old.
2. The Mac copies `glide join <token>` to the pasteboard and shows a notification "Invite copied — valid for 10 minutes". `glide invite` prints the command instead.
3. The token carries: server name, the server's LAN address(es) with the listen port, the server's certificate fingerprint, the secret. It is one copy-pasteable word (no spaces, no shell-special characters). Exact encoding is the builder's call; it must carry a version prefix (e.g. `glide1:`).

**On the new machine**

4. `glide join <token> [--name <name>]`:
   - creates the certificate if missing;
   - creates a client config if missing (`name` = `--name` or the hostname, `role = "client"`); an existing config keeps its name and role (joining from a server config is refused with a clear message);
   - connects to the server on a new ALPN `glide-pair/0`, **pinning the server fingerprint from the token**, sends `{secret, name}`;
   - on success, writes/replaces the server as its `[[peer]]` (name, addresses from the token, fingerprint, the `position` the server sends back);
   - prints "Paired with goldengate" and, if the `glide.service` user unit is active, restarts it; otherwise prints how to start glide.

**Server side of the pairing connection**

5. Unknown client certificates pass the TLS handshake **only while an invite is outstanding**; at all other times the server's client verifier stays as strict as today. Whatever passes, a connection on `glide/0` or `glide-video/0` from an unknown cert is still dropped by `identify()`; only `glide-pair/0` treats unknown certs specially.
6. On `glide-pair/0` the server checks the secret (constant-time compare, unexpired, unused), then marks it used **before** saving. On success it:
   - takes the client's fingerprint from its TLS certificate (never from the message) and the address from the connection's remote address;
   - adds the device to `config.toml` — or, if a device with that **name already exists, replaces its fingerprint and address** (a reinstalled machine re-pairs; its position and display are kept);
   - places a new device on the first free edge (builder's call on order; it must not put two devices on the same edge of the same display);
   - replies with the client-side position (opposite of the server's) and closes;
   - adds the device to the running daemon (§7.4) and posts a notification "<name> added".
7. Any failure (bad/expired/used secret, malformed message) replies with an error string the client prints, and closes. Nothing is written.

### 7.2 On / Off

Config keeps the existing field: `enabled = false` on a `[[peer]]` now means **Off**. No new field; old configs read unchanged.

| | On | Off |
|---|---|---|
| Server dials it | yes | no |
| Server accepts its connection | yes | accepts, sends "you are off", closes |
| Screen edge | armed when linked | none |
| Clipboard | synced | none |
| Extended display | allowed | stopped / not offered |
| Client shows | linked | "turned off by goldengate" |

- **Turning a device Off** from the menu: if it holds the cursor, the server takes input back first and lifts held keys (existing `pause_takes_back` / `hand_back` path); stops the extended display if it is on that device; sends the "off" message; closes the link; stops dialing it. Persisted.
- **"Off" message:** reuse the existing `Msg::Paused(true)` wire tag with the new meaning, or add a new tag — builder's call; either is wire-compatible.
- **Client receiving "off":** shows "turned off by <server>" in its status (`glide status`, JSON class `off` for the Linux bar), drops its edge, and **slows its dialing of that server to one probe every 60 s** (builder may tune 30–60 s). Each probe gets "off" again and closes. *(Planner's call, from builder check: an Off client that stopped dialing entirely could never come back if its address changed or its inbound port is blocked.)*
- **The server records a device's address every time that device connects in**, including Off probes, and saves it to `addrs` when it changed. This also fixes `nullbyte`'s DHCP moves (vault known-issue #11). *(Planner's call, proposed in conversation, not objected to.)*
- **Turning a device On:** server dials its last known address at once (link up within ~2 s); if that fails, the device's next probe is accepted, so On takes effect within 60 s at worst.

### 7.3 Forget Device

- Menu: device submenu → *Forget <name>…* → confirmation alert ("Forget anorak? It will need a new invite to connect again." Forget / Cancel).
- Effect: same take-back/cleanup as Off, then the device is removed from `config.toml` and from the running daemon (menu row and Arrange window disappear). Its certificate is no longer trusted.
- The client is not told (it may be gone); it will just fail to connect.
- A CLI equivalent through the control socket: `glide peer forget <name>`.

### 7.4 Adding and removing devices at runtime

Pairing and Forget change the device list while the daemon runs. Today the list is fixed and index-addressed (§5).

**Requirement:** adding or forgetting a device must work without a restart and must never change *which* device another device's edge, capture handle, menu row or control command refers to.

How is the builder's call. Two acceptable shapes: stable per-device ids that are never reused (handles stop being raw indices), or an append-only list where forgotten slots become tombstones until the next start. Every holder of the peer list today — input session, `link::run` (`identify`, dialers), clipboard watcher, control socket name list, `display::host`, menu, Arrange window — must see additions and removals.

### 7.5 Extended display per device

- *Use as Extended Display* is per device (in its submenu). One device at a time; choosing another moves it.
- **Only the extended device's screen edge is disarmed** while extending; every other device stays controllable. (Today `extending` is one global flag that disarms all edges, because there was only one peer.) *(Planner's call.)*
- **Turning the extended device Off stops the display but keeps the saved target**; when it is On and linked again, extending resumes, as it already does after the device drops offline and reconnects. Unticking *Use as Extended Display* clears the target. Forget clears it. *(Planner's call.)*
- Persisted as the **name** of the device extended to (e.g. `[display] extend = "anorak"`; absent = not extending). Old configs with `mode = "extend"` read as extending to the first configured device; `mode = "control"` reads as not extending. Exact key name is the builder's call.
- A device can be extended to only if it can show it. Clients **advertise** that on connect (Hyprland Linux client: yes; X11 client: no); the menu greys the item out with "needs Hyprland" for others, and for Off or offline devices. **A client that advertises nothing (an old build) counts as able**: `anorak` keeps extending before it is rebuilt, and an old X11 client fails visibly through the existing `DisplayReady(Err)` path. *(Planner's call.)*
- `display::host` receives the target at runtime instead of `config.peers[0]` at startup.

### 7.6 Menu (option A)

```
┌────────────────────────────────────┐
│ Controlling nullbyte        3.5 ms │   ← one summary line
├────────────────────────────────────┤
│ Devices                            │
│ ● nullbyte        controlling    › │──┐
│ ● anorak          linked·1.7ms   › │  │  ┌───────────────────────────┐
│ ○ anton           off            › │  └─▶│ ✓ On                      │
│ Add Device…                        │     │   Off                     │
├────────────────────────────────────┤     ├───────────────────────────┤
│ Arrange Screens…                   │     │   Use as Extended Display │
│ Grant Input Permissions…  (if denied)    ├───────────────────────────┤
├────────────────────────────────────┤     │   Forget nullbyte…        │
│ Quit Glide                      ⌘Q │     └───────────────────────────┘
└────────────────────────────────────┘
```

- **Summary line** (one, not three), first match wins: "Input permission needed" when denied · "Controlling <device>" + latency while the cursor is in a device · "<device> is a second screen" while extending · "Linked to N devices" when idle · "Not connected". *(Order is the planner's call.)*
- **Device row:** status dot + name + right-aligned status text + submenu. Dot: blue = controlling, green = linked, amber = paused on the device itself (client self-pause, §3), grey = offline (on but unreachable), hollow = off, purple = extended display. Status text: `controlling`, `linked · 1.7 ms`, `paused on <name>`, `offline`, `off`, `extended display`. Off rows dimmed. *(The self-pause row is the planner's call: otherwise crossing to a self-paused device would silently do nothing.)*
- **Submenu:** *On* / *Off* (radio ticks) · separator · *Use as Extended Display* (ticked while it is; disabled with a "needs Hyprland" note when the device can't, disabled while off/offline) · separator · *Forget <name>…*.
- *Add Device…* sits at the end of the device list. *Arrange Screens…*, *Grant Input Permissions…* (only while input permission is denied, as today), *Quit Glide ⌘Q*.
- The old top-level *Control <peer>* / *Extend to <peer>* pair and the three status lines are removed.
- Rows refresh with the existing 1 s timer; rows are added/removed live after pairing/forget.
- Right-aligned status text: builder's call (attributed title with a tab stop, or a custom view).

### 7.7 Linux client status

- `glide status` / `--json` gain the "turned off by <server>" state (class `off`). ironbar/waybar styling for it is a dotfiles follow-up, not part of this work.

## 8. Rollout

- Mac: rebuild with `contrib/bundle.sh` (self-signed signing key keeps input permissions).
- `anorak`, `nullbyte`: rebuild and reinstall the client. Old clients keep working against the new server (unknown tags ignored), but only new clients show "turned off" and can `glide join`.
- Existing peers need no re-pairing.
- `anorak` was unreachable on 2026-11-16 (LAN and Tailscale), so it may be upgraded later than `nullbyte`; the "advertises nothing = able" rule (§7.5) keeps it working meanwhile.

## 9. Left to the builder

- Token encoding (beyond: versioned, one shell-safe word, carries the four items in §7.1).
- How the server finds its LAN addresses for the token.
- Pairing message format on `glide-pair/0`.
- Free-edge placement order for a new device.
- Stable ids vs. tombstones for runtime add/remove (§7.4).
- Reusing `Msg::Paused` vs. a new "off" tag; the capability message shape.
- Config key name for the extended-display target and its migration from `mode`.
- How right-aligned status text is drawn in the menu.
- Tests: at least — invite secret single-use + expiry; unknown cert refused on `glide/0` even during an invite window; re-join with an existing name replaces the fingerprint; Off while holding the cursor returns it with no held keys; forgetting one device leaves the others' edges and handles untouched; old config with `enabled = false` / `mode = "extend"` loads as Off / extend-to-first.
