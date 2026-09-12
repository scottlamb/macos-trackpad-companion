# Icons

**Option B (`bridge.svg`) is the chosen direction** and is wired into the
build: `bundle.sh` copies `bridge.icns` into the app bundle, and
`status_item.rs` embeds `bridge-template-40.png` via `include_bytes!`.
Option A is parked here as an alternative to revisit.

| File | Use |
| --- | --- |
| `bridge.svg` | **Chosen** app icon — PTP pad, translation chevron, native macOS pointer. Leads with what the program *does*. |
| `bridge.icns` | Generated from `bridge.svg` at all ten standard sizes; copied into the bundle's `Resources`. |
| `bridge-template.svg` | **Chosen** menu-bar template. Redrawn to fill its canvas — the first cut kept the app icon's thin stroke and read as visibly smaller than neighbouring menu-bar icons. |
| `bridge-template-20.png` | 1x render of the template. |
| `bridge-template-40.png` | 2x render, embedded in the binary. |
| `pinch-arcs.svg` | Option A app icon — two fingertips closing on each other. Leads with the gesture engine. Parked. |
| `pinch-arcs-template.svg` | Option A menu-bar template. Parked. |

## Conventions

- **App icons** are drawn on a 1024 canvas with the content in an
  824x824 squircle (`rx=185`) inset 100px on every side, matching the
  Big Sur+ icon grid. Export to `.icns` with all the standard sizes.
- **Rasterize templates with Chrome headless**, not `qlmanage`: Quick
  Look flattens transparency onto white, which silently ruins a template
  image. `--default-background-color=00000000` preserves alpha, and the
  SVG's `width`/`height` must be scaled to the target size first or the
  glyph renders at natural size in the corner of a larger canvas.
- **Templates** are pure black on transparency at 20x20. Load them with
  `isTemplate = true` and AppKit handles inversion for dark menu bars,
  selection, and Reduce Transparency — don't ship a separate white copy.
- Templates are **redrawn, not scaled down**, and sized to fill their
  canvas. App-icon geometry (bowed shafts, the chevron, thin strokes)
  turns to mush at menu-bar size — and artwork that only fills the middle
  of its box reads as smaller than neighbouring icons even at the same
  point size, which is what happened to the first cut of the chosen
  template.
- The shared accent is `#5AB2FF` on a `#3A4049 → #15181C` graphite
  gradient, so whichever option wins, the pair still reads as a family.
