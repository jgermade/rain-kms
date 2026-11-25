# rain-kms

Full-screen digital rain, drawn **straight to the display through DRM/KMS** — no
display server, no compositor, no browser. Built to run a kiosk screen with as
little RAM as possible.

Two styles:

- **`film`** (default, v2): rain modelled on the monitors in *The Matrix* (1999),
  with depth, in the colours of v1: a random hue per column (`--colors full`),
  v1's vertical rainbow bands (`--colors rainbow`), or the film's green
  (`--colors green`). Mirrored half-width katakana, digits and a few symbols in
  narrow columns; several drops per column at widely different speeds; heads that
  burn white with a glow; several depth layers (farther ones smaller, dimmer,
  denser and slower on screen).
- **`rainbow`** (v1): katakana rain with rainbow hues, recreating the canvas demo
  from
  [wasm-gaming/engine-specs](https://github.com/wasm-gaming/engine-specs/blob/main/demo/canvas-renderer.js).

## Footprint

Measured on an Intel Alder Lake-N (UHD Graphics) mini PC, 1920×1080 @ 60 Hz, 30 FPS.
The `rainbow` figures are from the kiosk service; the `film` ones from `--bench`
(not yet measured as the service).

| | `rainbow` | `film` (4 layers) |
|---|---|---|
| Binary size | ~520 KB | (same binary) |
| Process RSS | ~2.8 MB | ~3–4 MB expected (+ ~0.5 MB of glyph maps and a 490 KB band buffer) |
| Peak at start-up | 22 MB (font loaded) | 22 MB (font loaded) |
| Scan-out buffers | 2 × 8 MB (driver) | 2 × 8 MB (driver) |
| Time per frame, full clock | ~0.9 ms | ~4.5 ms |
| CPU, paced at 30 FPS | ~11–12.5 % of one core | ~30 % of one core |

For comparison, a browser kiosk (Firefox + cage) typically needs 300–500 MB.

The paced CPU figures are inflated by frequency scaling: the work comes in short
bursts (one frame, then sleep), so with the `powersave` governor the CPU stays at
a low clock. `--bench` reports both. Drawing is done on the CPU; the GPU only scans the buffers
out. Rendering on the GPU would cut CPU use but load the Mesa driver, adding tens
of MB of RAM.

## How it works

Common to both styles:

- Each frame is drawn into an off-screen dumb buffer and shown with a page flip
  synced to the display refresh (double buffering, no tearing). Both buffers are
  mapped once for the whole run.
- The font is read at start-up, the glyphs are rasterised once, and the ~20 MB
  font file is released.
- Video memory is never read back, and it is written in sequential rows (it is
  write-combined: fast for sequential writes, very slow for scattered ones).
- `--bench` renders frames into RAM and into real scan-out buffers (never shown)
  and reports the time per frame.

### `film`

- Each depth layer has its own glyph size (from `--max-size` for the nearest
  layer down to `--min-size` for the farthest, shrinking geometrically), brightness, saturation (down to 30 % for
  the farthest, fading to grey) and density. Columns are moved off their grid by
  up to ±40 % of a column width at random, and moved up by a random part of a row,
  so neither columns nor the gaps between rows line up.
  All layers fall at the same rows per second, so the far ones move slower on
  screen, as with real perspective.
- Glyphs are pre-rendered per layer as light maps, at whole-pixel positions so
  they stay sharp: plain, and with a soft glow (two box blurs) used only for heads.
- Drops fall at 5–24 rows per second each by default (`--speed-spread`), spread
  evenly on a log scale; trails fade linearly, as in v1.
- Light adds up: each glyph is added in its colour, per channel and saturating,
  so overlapping glows burn towards white.
- Colours: v1's hsl(hue, 80 %, 65 %) trails and (220, 255, 255) heads, the hue
  drifting with time as in v1. `full` gives every column (in every layer) its own
  random starting hue; `rainbow` takes it from the column's horizontal position
  (10° per `--size` pixels, as v1's columns), so all layers share vertical bands
  of colour; `green` uses the film's green with white heads; `monokai` gives each
  column one of Monokai's six accent colours (no drift), with Monokai's
  foreground (`#F8F8F2`) for heads.
- Layers overlap, so the whole frame is rebuilt every time. To stay in the CPU
  cache it is built in 64-row bands (a 490 KB buffer at 1920 px): the lit glyphs
  are listed first, then each band is filled with the background, gets its
  glyphs, and is copied into the scan-out buffer one sequential row at a time.

### `rainbow`

- At start-up the 96 katakana glyphs are rasterised once from Noto Sans CJK JP
  and kept as small coverage maps (<100 KB); the font file is then released.
- Glyphs never move: each one sits in a fixed cell of the grid, and only whether
  it is lit, its brightness, its hue and (now and then) its character change. Each
  buffer keeps a small key per cell (char, brightness level, hue step) and only
  cells whose key changed are redrawn — about a quarter of them per frame.
- Each glyph is stored as a coverage map exactly one cell in size. To redraw a
  cell, a 256-entry colour table (coverage → pixel, integer maths) is built for its
  colour and brightness, and the cell is written straight into the scan-out
  buffer, one sequential row at a time, with one table lookup per pixel.
- Glyphs are blended against the known background colour.

## Requirements

- Linux with a DRM/KMS graphics driver and a connected display.
- No other program holding the display (no desktop session on that output).
- `noto-fonts-cjk` (or any font with katakana, via `--font`).
- Rust to build it.

## Build

```sh
cargo build --release
```

## Usage

```
rain-kms [--style film|rainbow] [--layers 4] [--colors full|rainbow|green|monokai]
         [--min-size 15] [--density 3] [--speed-spread 2.2]
         [--res 1920x1080|native] [--size|--max-size 30] [--fps 30]
         [--card auto|/dev/dri/cardN] [--background 000000]
         [--font PATH] [--font-index 0] [--capture FILE.ppm] [--bench]
```

| Option | Default | Meaning |
|---|---|---|
| `--style` | `film` | `film` (green, with depth) or `rainbow` (v1). |
| `--layers` | `4` | Depth layers of the `film` style (1–8). Fewer layers use less CPU. |
| `--min-size` | half of `--size` | Glyph size of the farthest layer (`film`), in pixels. |
| `--speed-spread` | `2.2` | How different drop speeds are (`film`): they range from 11 ÷ spread to 11 × spread rows per second (5–24 by default). `1` = all the same; up to `10`. |
| `--density` | `3` | How much rain (`film`), 1 (sparse) to 5 (dense). Each level changes the waits between drops by 1.6×. |
| `--colors` | `full` | Colours of the `film` style: `full` (v1 colours, random per column), `rainbow` (v1 colours in vertical bands), `green` (as the film) or `monokai` (Monokai's accent colours, random per column; pair it with `--background 272822` for Monokai's background). |
| `--res` | `1920x1080` | Display mode; `native` uses the display's preferred mode. Lower resolutions use less RAM and CPU. |
| `--size`, `--max-size` | `30` | Glyph size in pixels (of the nearest layer, for `film`). |
| `--fps` | `30` | Frame-rate cap. |
| `--card` | `auto` | DRM device; `auto` picks the first card with a connected display. |
| `--background` | `000000` | Background colour as `RRGGBB` hex (a leading `#` is accepted). |
| `--font` | `/usr/share/fonts/noto-cjk/NotoSansCJK-Bold.ttc` | Font file. |
| `--font-index` | `0` | Face inside a font collection (`0` = Noto Sans CJK JP). |
| `--bench` | — | Render frames into RAM and into scan-out buffers, report the time per frame, and exit. |
| `--capture` | — | Render one frame (after 3 simulated seconds) to a PPM file and exit, without touching the display. |

Your user needs to be in the `video` group to run it by hand.

## Install as a kiosk

```sh
cargo build --release
sudo ./install.sh
```

This creates a `kiosk` system user, installs the binary to `/usr/local/bin`, and
enables `rain-kms.service`, which replaces the text console on `tty1`. The service
is sandboxed (no network, no home directories, 64 MB memory limit) and restarts
on failure. It runs at low priority (`Nice=10`, `CPUWeight=20`, idle I/O class):
the rain is decorative, so under load it gives way to other services and may drop
a few frames.

To remove it:

```sh
sudo systemctl disable --now rain-kms && sudo systemctl enable --now getty@tty1
```

## Repository conventions

- [`AGENTS.md`](AGENTS.md) — rules for agents (`CLAUDE.md` symlinks to it).
- [`ROADMAP/`](ROADMAP/) — project plan by date, with Mermaid Gantt diagrams.
- [`RECORD/`](RECORD/) — project memory (`.WIP` append-only, `.completed` read-only).

All code and documentation in this repository are written in English.
