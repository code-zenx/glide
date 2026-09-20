# Personal project icon theme

One visual system for every icon in this collection of personal projects. The
point is that siblings look like siblings: same plate, same weights, same mark
style — only the hue and the glyph change. `contrib/icon.swift` is the
reference implementation; copy it, not a description of it.

All numbers are in a **1024 × 1024 design space** and scale linearly.

## 1. Canvas and plate

| | value |
|---|---|
| Canvas | 1024 × 1024, fully transparent outside the plate |
| Plate | 824 × 824, centred (inset 100 on all four sides) |
| Corner radius | 22.37% of the plate side = 184.3 |
| Safe area for the mark | 720 × 720 centred — 87% of the plate |

The plate is inset, not full-bleed. That 100-unit margin is the macOS Dock
grid; a full-bleed square sits visibly larger than every Apple icon next to it.

No baked drop shadow. macOS composites its own.

*Known simplification:* `CGPath(roundedRect:)` draws circular corners, not
Apple's continuous ("squircle") curve. At 22.37% the difference is a few units
of curvature and invisible below 256px. Not worth hand-rolling the superellipse.

## 2. Colour

One hue per project. Everything else is fixed, so the family reads as a family.

* **Gradient**: linear, **vertical only**, light at the top. Never diagonal —
  a diagonal wash is the tell of a generic gradient square.
* **Top stop**: the hue at S 88%, lightness tuned to **3.7 : 1 contrast against
  white** (±0.2).
* **Bottom stop**: same hue at S 60%, tuned to **11.4 : 1 against white** (±0.5).
* **Mark**: pure `#FFFFFF` for the primary element, white at **86% alpha** for
  secondary elements. No third value, no colour in the mark.

Contrast-anchoring rather than fixed lightness is what keeps a teal icon and an
amber icon equally readable — equal HSL lightness across hues is not equal
perceived lightness.

| Hue | Top | Bottom | Claimed by |
|-----|-----|--------|-----------|
| 250 | `#866FF6` | `#372593` | **Glide** |
| 206 | `#0F87E4` | `#163C5A` | free |
| 178 | `#09948F` | `#10403F` | free |
| 146 | `#0A9948` | `#114326` | free |
| 36  | `#BA750C` | `#4D3613` | free |
| 346 | `#F23B66` | `#6D1B2F` | free |
| 292 | `#D830F2` | `#5F1B6A` | free |

Pick hues at least 30° apart. Claim a row here when you use it.

## 3. Stroke weights

As a fraction of the **canvas** (1024), not the plate:

| Role | Fraction | Units |
|---|---|---|
| Primary mark stroke | 6.25% | 64 |
| Secondary mark stroke | 5.1% | 52 |
| Floor — never go below | 5.0% | 51 |
| Compact mark (≤ 32px) | 12.5% | 128 |

The floor exists because 5% of the canvas is ~1.3px at 32px; anything thinner
dissolves. The compact weight is deliberately `1/8` of the canvas so it lands
on whole pixels at every power-of-two size (2px at 16, 4px at 32).

Round caps and round joins everywhere. Corner radius on mark rectangles:
16–23% of their own short side.

## 4. The mark

* **Flat and monoline.** Strokes, not fills. No gradients, no shadows, no
  bevels, no inner glow inside the mark.
* **Three elements maximum.** If it needs four, the idea is too complicated for
  an icon.
* **Nothing overlaps.** Keep ≥ 28 units of clear space between any two
  elements. Overlap forces a knockout, and a knockout is the first thing to
  turn to mush when the icon is downscaled.
* **If two elements must overlap**, knock the lower one out with a plain slot
  (a rounded rect), never with a fattened copy of the upper path — a fattened
  copy shaves irregular slivers off corners. Slot height = upper stroke +
  2 × 22 clearance, and the slot must clear the lower element's own edges by
  ≥ 40 so it cuts one clean channel instead of severing the shape.
* **Hierarchy through alpha, not size**: the verb is 100% white, the nouns are
  86%.
* Keep the mark inside the 720 safe area; it does not have to fill it.

## 5. Small sizes

Design for 64px. Then give 16px and 32px their own artwork.

At 32px the plate is ~26 usable pixels wide. Three elements do not fit — the
detail collapses and the icon becomes a coloured smudge. So **at ≤ 32px, drop
everything but the single most meaningful element and draw it at the compact
stroke weight.** One threshold, one alternate drawing; not a per-size art
program.

This only works if each `.iconset` slot is rendered from the vector source at
its own pixel size. `make-icon.sh` does exactly that — do not downsample a
single 1024 PNG with `sips`, which is both blurrier and blind to the threshold.

## 6. Never

* No text, no letterforms, no version numbers, no wordmarks.
* No photographic or noise textures, no material simulation.
* No hairlines — nothing below the 5% stroke floor.
* No inner shadows, no emboss, no glass highlight, no baked drop shadow.
* No diagonal gradients, no multi-hue gradients.
* No full-bleed plate.
* No mark that only works at 512px.

## 7. Applying this to a new project

1. Copy `contrib/icon.swift`. It already encodes sections 1–5.
2. Claim a hue in the table above and set `accentTop` / `accentBottom`.
3. Replace the mark paths only. Leave `plateInset`, `cornerRatio`,
   `screenStroke`, `arrowStroke` and the `compact` threshold alone.
4. Decide the one element that survives in `compact` mode before you draw the
   full mark. If you cannot name it, the mark is wrong.
5. Copy `contrib/make-icon.sh`, change the output name, run it.
6. Look at 16, 32, 64 and 128px at 1:1 before you believe any of it.
