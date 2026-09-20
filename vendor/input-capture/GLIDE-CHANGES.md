# What glide changed in this crate

Vendored from `input-capture` 0.4.0 (GPL-3.0, from lan-mouse). Two changes:

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

## The macOS event tap never waits

A `CGEventTap` at session level sits in the input path of *every* application.
While the callback has not returned, nobody's keyboard or mouse works. Upstream
holds the state mutex for the whole callback and then, still holding it, pushes
each captured event into a 32-slot channel with `blocking_send`, and sends
`ProducerEvent::Grab` the same way with an `.expect` on the result.

So a consumer that falls a fraction of a second behind fills the channel, the
callback blocks holding the lock, and the task that would process a release
cannot take that lock to do it. The tap never returns, the machine stops
responding to input, and the only way out is a power cycle. Short of that it is
the everyday cause of input lag: the tap waits for the consumer every 32 events.

Changed, macOS only:

- The event channel holds 1024 and the notify channel 256, about a second of
  pointer motion rather than thirty milliseconds.
- The state guard is dropped before anything is sent.
- Every `blocking_send` is a `try_send`: on a full channel the event is dropped
  and logged. Losing a motion event costs a little cursor accuracy; waiting
  costs the user their keyboard.
- The `Grab` send no longer panics on failure, which used to kill the tap
  thread and leave the cursor hidden.
- The display-reconfiguration callback, which runs on the same run loop, uses
  `try_send` for the same reason.
