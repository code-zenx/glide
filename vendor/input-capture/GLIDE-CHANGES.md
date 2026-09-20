# What glide changed in this crate

Vendored from `input-capture` 0.4.0 (GPL-3.0, from lan-mouse). One change:

## A capture barrier can belong to a single display

Upstream folds every display into one rectangle (`update_bounds`, "Get the max
bounds of all displays") and arms an edge on that. With two displays of
different heights, "top" means the topmost point across both — so a peer can
only ever sit off the outermost screen. A machine parked above the *lower*
display is unreachable: pushing up from it never crosses the union's top edge.

Added, macOS only:

- `InputCaptureState::only_display: Option<usize>` — an index into the active
  display list. `None` keeps upstream behaviour.
- `update_bounds` narrows to that display when set, and resets the bounds first
  so repeated calls do not accumulate.
- `crossed` additionally requires the cursor to be *along* the barrier's own
  screen, so a taller neighbour cannot trip a barrier that is not its own.
- `Capture::restrict_to_display` on the trait, implemented for macOS and a no-op
  for every other backend. The libei portal and layer-shell hand over
  whole-desktop capture zones, so there is nothing to narrow there.

Everything else is upstream. Diff against the crates.io copy to see it all.
