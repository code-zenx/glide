# glide multi-client Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One server (the Mac, `goldengate`) drives several Linux clients (`anorak`, `anton`, …): a per-client on/off toggle on the server, one combined desk the cursor moves across (client to client), and a proper badge + popover in each client's ironbar.

**Architecture:** The server stays the hub that owns the keyboard, mouse and layout. Clients still never capture input; when the cursor leaves a client by an edge, the client tells the server *which* edge, and the server routes it to whoever sits there (itself or another client). Clients get a local control socket so the bar can show state and pause/release.

**Tech Stack:** Rust 2021 single crate, quinn/rustls (link), vendored `input-capture`, objc2 (macOS menu bar + Arrange window), ironbar 0.19 (Linux bar, in `code-zenx/dotfiles`).

**Spec:** this file's Decisions section (no separate spec; decisions confirmed with the owner before Task 1).

## Global Constraints

- Wire format stays backward compatible within a phase: a new `Msg` tag must be ignored, not fatal, by an older peer (`proto.rs` decode returns an error today → make unknown tags skip).
- Config stays readable: every new `[[peer]]` field has a default (`enabled = true`, `attach` falls back to `position`/`display`).
- Status file `~/.cache/glide/status`: readers must accept the old single-peer format (a running old daemon during upgrade).
- Clients never capture and never send `Msg::Input` (unchanged rule, `Handover.driven`).
- Ctrl+Shift+Alt+Meta always takes input back on the server, whatever any client state is.
- Icons follow `contrib/ICON-THEME.md` (hue 250, `#866FF6` → `#372593`, squircle plate, ≤32 px = simplified art).
- Linux side ships through `code-zenx/dotfiles` (`--with-glide` opt-in); no hard dependency added to the dotfiles for machines without glide.

## Decisions (confirm before Task 1)

| # | Question | Options | Recommended |
|---|----------|---------|-------------|
| D1 | What does the server's per-client toggle do? | **Pause**: link stays up, no input/clipboard, client shows "Paused by goldengate" · **Disconnect**: link closed, client shows offline | Pause (instant resume, client knows why) |
| D2 | Combined desk shape | **Hub**: every client on an edge of a Mac display (works mostly today) · **Chain**: clients may sit next to each other (anton right of anorak), server routes client→client | Hub in Phase 1, Chain in Phase 2 |
| D3 | Client bar icon | **Glide badge** (squircle G SVG, coloured by state) · **Tabler glyph** (matches the rest of the bar) | Glide badge, 18 px, state = plate colour |
| D4 | Client popover contents | Header (badge, server name, state pill) · latency · switches: *Allow control*, *Clipboard sync* · buttons: *Release*, *Reconnect* | All of it |

## Review Focus

1. Server pauses a client **while that client holds the cursor** → server takes input back and lifts every key the client saw go down (no stuck Shift). Test in Task 3.
2. Client→client crossing while the target's link just died → input stays on the source client's server side, never captured into nothing. Test in Task 7.
3. Two peers configured on the same edge of the same display → config load fails with a message naming both. Test in Task 6.
4. Status reader meets an old single-peer status file → shows that one peer, doesn't crash the bar. Test in Task 1.
5. Client bar running with no daemon (glide stopped, as on anton today) → badge hidden, popover not reachable, no error spam in ironbar's log. Test in Task 10.

---

## File Structure

| File | Change | Responsibility |
|------|--------|----------------|
| `src/status.rs` | modify | `State` → per-peer list (`peers: Vec<PeerState>`), JSON v2, old-format reader |
| `src/main.rs` | modify | drop first-peer assumptions; `glide peer on|off`, `glide ctl …` subcommands |
| `src/ctl.rs` | **create** | local control socket (`$XDG_RUNTIME_DIR/glide.sock`, macOS `~/Library/Caches/glide.sock`): `pause`, `resume`, `release`, `reconnect`, `status` |
| `src/proto.rs` | modify | `Msg::Paused(bool)`, `Msg::Leave { side, display, along }`, `Msg::Enter { along }`, `Msg::Edges(Vec<Edge>)`; unknown tags skipped |
| `src/layout.rs` | **create** | pure layout model: machines, displays, attachments, `neighbor_at()` |
| `src/input.rs` | modify | per-peer enabled flag, routing on `Leave{side}`, client arms edges from `Msg::Edges` |
| `src/menubar.rs` | modify | one row per client with toggle + state |
| `src/arrange.rs` | modify | draw and drag N peers, snap any machine to any display edge |
| `contrib/make-icon.sh` | modify | render state badges `glide-{linked,active,paused,offline,denied}.svg` |
| dotfiles `ironbar/glide.sh`, `ironbar/config.toml`, `ironbar/style.css` | modify | badge image + popover driven by `glide status --watch` / `glide ctl` |

---

## Phase 0 — Clean base

### Task 0: Settle branches

**Files:** none new.

- [ ] Mac (`~/personal/projects/glide`, branch `display`): commit or stash the uncommitted `Cargo.toml`, `Cargo.lock`, `src/input.rs` change (37 lines) after reading it.
- [ ] anorak: commit or drop its uncommitted `src/clipboard.rs` change.
- [ ] Merge `display` into `main` (5 commits), `cargo test` on Mac and anorak, push.
- [ ] Commit: `Merge display branch`.

## Phase 1 — Several clients on one server (hub)

### Task 1: Per-peer status

**Files:** Modify `src/status.rs`, `src/link.rs:463-484,603`, `src/input.rs:255-269`, `src/display.rs:291,416,469`. Test: `src/status.rs` tests.

**Interfaces:**
- Produces: `struct PeerState { name, mode, rtt_ms, paused, denied }`; `State { me, role, peers: Vec<PeerState>, display }`; `Status::set_peer(&self, name, impl FnOnce(&mut PeerState))`; `State::json(badge) -> String` with `{"text","tooltip","class","peers":[…]}`; `State::active() -> Option<&PeerState>` (the one holding the cursor).

- [ ] Test: two peers encode → decode round-trip equal; `class` of the aggregate is the "busiest" peer (`active` > `linked` > `paused` > `offline`).
- [ ] Test: decoding an old single-peer file yields one `PeerState` (Review Focus 4).
- [ ] Test: hostile peer names stay valid JSON (extend the existing test).
- [ ] Implement; switch every `status.set(|s| s.rtt_ms…)` call to `set_peer(name, …)`.
- [ ] `cargo test`, commit `status: one entry per peer`.

### Task 2: Remove first-peer assumptions

**Files:** Modify `src/main.rs:165,293,308,330`.

- [ ] `set_position(path, peer_name, side, display)` takes the name (Arrange passes it).
- [ ] `status.offline` for every configured peer at start.
- [ ] `glide ping`/`glide test` take an optional `<peer>`; default = all peers.
- [ ] Test: `set_position` on a two-peer config edits only the named peer.
- [ ] Commit `cli: address peers by name`.

### Task 3: Per-client toggle (D1 = Pause)

**Files:** Modify `src/main.rs` (Peer `enabled: bool`, default true; `set_enabled(path, name, bool)`), `src/proto.rs` (`Msg::Paused(bool)`), `src/input.rs` (`Control::SetEnabled{peer, on}`, `crossing()` gains `enabled`), `src/clipboard.rs` (skip paused peers), `src/menubar.rs`.

**Interfaces:**
- Consumes: `Status::set_peer` (Task 1).
- Produces: `Control::SetEnabled { peer: String, on: bool }`; `fn crossing(server: bool, linked: bool, enabled: bool) -> Crossing`.

- [ ] Test: `crossing(true, true, false)` → `Crossing::Release` (edge does nothing when paused).
- [ ] Test (Review Focus 1): Handover forwarding to peer P, `SetEnabled{P,false}` → returns cleanup that sends key-ups for every held key, mode back to Local.
- [ ] Test: config round-trip keeps `enabled = false`; missing field reads as true.
- [ ] Implement: disarm/re-arm that peer's edge; send `Msg::Paused(on)` so the client can show it; persist via `set_enabled`.
- [ ] Menu bar: a section per client — `● anorak  1.4 ms` with a checkmark item *Connected* that toggles; paused rows dimmed.
- [ ] Manual check: pause anton from the menu while the cursor is on anton → cursor back on Mac, no stuck keys.
- [ ] Commit `server: pause and resume each client`.

### Task 4: Local control socket + CLI

**Files:** Create `src/ctl.rs`; modify `src/main.rs`, `src/input.rs`.

**Interfaces:**
- Produces: `glide ctl pause|resume|release|reconnect|status` (client and server); `glide peer on|off <name>` (server = `Control::SetEnabled`). Line protocol over a Unix socket, one command → one JSON reply.
- Client `pause` = local refusal to be driven (`Handover.driven = false` until `resume`), shown to the server as `Msg::Paused(true)` coming the other way.

- [ ] Test: parser maps each command string to its enum; unknown → error reply, socket stays up.
- [ ] Test: socket path uses `XDG_RUNTIME_DIR`, falls back to `~/.cache/glide/`.
- [ ] Implement with `tokio::net::UnixListener`, mode 0600.
- [ ] Commit `ctl: local control socket`.

### Task 5: Arrange window for N peers

**Files:** Modify `src/arrange.rs` (`ViewState.peer: Vec<Screen>` → `peers: Vec<(String, Vec<Screen>)>`, drag one group at a time).

- [ ] Test (pure helper): `landed_on()` for a peer group dropped right of display 2 → `(Right, Some(2))`, independent of other groups.
- [ ] Implement; each drop calls `set_position(path, name, side, display)`.
- [ ] Manual check: anorak top, anton right, both saved in `config.toml`.
- [ ] Commit `arrange: several peers`.

## Phase 2 — One combined desk (chain)

### Task 6: Layout model

**Files:** Create `src/layout.rs`; modify `src/main.rs` (config `attach = { to = "anorak", side = "right", display = 1 }`, default `to` = this server).

**Interfaces:**
- Produces: `Layout::from_config(&Config, screens: &HashMap<String, Vec<Screen>>) -> Result<Layout>`; `Layout::neighbor_at(machine, side, display) -> Option<(String, Side, usize)>`; `Layout::edges_of(machine) -> Vec<Edge>`; `struct Edge { side: Side, display: usize }`.

- [ ] Test: hub (anorak top, anton right of Mac) → `neighbor_at("anorak", Bottom, 1) == goldengate`.
- [ ] Test: chain (anton right of anorak) → `neighbor_at("anorak", Right, 1) == anton`, `neighbor_at("anton", Left, 1) == anorak`.
- [ ] Test (Review Focus 3): two peers on the same edge+display → `Err` naming both.
- [ ] Commit `layout: one model for every machine`.

### Task 7: Routing client → client

**Files:** Modify `src/proto.rs` (`Leave { side, display, along: f32 }`, `Edges(Vec<Edge>)`), `src/input.rs` (server: on `Leave` ask `Layout::neighbor_at`, switch forwarding target; client: arm every edge from `Msg::Edges`).

**Interfaces:**
- Consumes: `Layout` (Task 6), `Control::SetEnabled` (Task 3).

- [ ] Test: server forwarding to anorak, receives `Leave{Right,1,…}` with anton linked → sends `Leave` to anorak's handover, `Enter` to anton, clipboard pushed to anton.
- [ ] Test (Review Focus 2): same but anton not linked or paused → no target change, anorak keeps the cursor.
- [ ] Test: `Leave` toward the server itself → take back (today's behaviour).
- [ ] Loopback test: server + two fake clients on 127.0.0.1, cursor walks Mac → anorak → anton → Mac.
- [ ] Commit `route the cursor between clients`.

### Task 8: Land the cursor where it crossed (known issue #4)

**Files:** Modify `src/proto.rs` (`Enter { along: f32 }`), `src/input.rs` (client warps with absolute motion before typing).

- [ ] Test: `along` 0.25 on a 1080 px tall edge → warp y = 270.
- [ ] Commit `enter where the cursor left`.

## Phase 3 — Client bar: badge + popover

### Task 9: State badges

**Files:** Modify `contrib/make-icon.sh`, `contrib/ICON-THEME.md`; create `contrib/badges/glide-{linked,active,paused,offline,denied}.svg`; `Makefile`/install step copies them to `~/.local/share/icons/hicolor/scalable/status/`.

- [ ] Render: squircle plate + knocked-out G, plate colour per state (linked = hue 250 gradient, active = brighter, paused = amber, offline = grey, denied = red), 18 px simplified art.
- [ ] Check: `gtk4-icon-browser` / `ls` shows all five; `xmllint --noout` each SVG.
- [ ] Commit `badges for the Linux bar`.

### Task 10: ironbar badge + popover (dotfiles)

**Files (code-zenx/dotfiles):** Modify `ironbar/glide.sh` (→ sets ironvars `#glide_show`, `#glide_icon`, `#glide_state`, `#glide_server`, `#glide_rtt`, `#glide_allow`), `ironbar/config.toml` (custom module: image button `#glide_icon`, `show_if = "#glide_show"`, popup), `ironbar/style.css`.

**Interfaces:**
- Consumes: `glide status --json --watch` v2 (Task 1), `glide ctl …` (Task 4), badges (Task 9).

- [ ] Popup layout (D4): header row (badge 32 px, server name, state pill) · "Latency 1.4 ms" · switch *Allow control* (`glide ctl pause|resume`) · switch *Clipboard sync* · buttons *Release* (`glide ctl release`), *Reconnect*.
- [ ] Test (Review Focus 5): with no `~/.local/bin/glide` or no daemon, `glide.sh` sets `#glide_show=false` once and exits quietly; ironbar log shows no errors for 60 s.
- [ ] Manual check on anton: badge colours follow pause/linked/active; every popover control works.
- [ ] Commit (dotfiles) `ironbar: glide badge and popover`.

## Phase 4 — Roll out and document

### Task 11: Ship

- [ ] Tag `v0.2.0`; release workflow builds Mac + Linux artifacts.
- [ ] Mac: `contrib/bundle.sh`; anorak + anton: `./setup.sh --with-glide` from dotfiles.
- [ ] Add anton to the Mac `config.toml` (fingerprint swap both ways), arrange it.
- [ ] README: multi-client config, `attach`, `glide peer on|off`, `glide ctl`.
- [ ] Vault: `glide.md` (now:, features), close known-issues #10 (one peer only) and #4 if Task 8 shipped, decision note for D1–D4.

---

## Execution order and size

| Phase | Tasks | Ships on its own |
|-------|-------|------------------|
| 0 | 0 | clean `main` |
| 1 | 1–5 | Mac drives anorak **and** anton, per-client pause, multi-peer arrange |
| 2 | 6–8 | cursor walks client → client, lands where it crossed |
| 3 | 9–10 | badge + popover on Linux clients |
| 4 | 11 | release, docs, vault |

Phase 3 depends only on Tasks 1 and 4, so it can start right after Phase 1.
