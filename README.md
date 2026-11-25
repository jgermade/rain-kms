# rain-kms

Full-screen katakana rain with rainbow colours, drawn **straight to the display
through DRM/KMS** — no display server, no compositor, no browser. Built to run a
kiosk screen with as little RAM as possible.

It recreates the canvas demo from
[wasm-gaming/engine-specs](https://github.com/wasm-gaming/engine-specs/blob/main/demo/canvas-renderer.js)
as a small native Rust program.

## Footprint

Measured on an Intel Alder Lake-N (UHD Graphics) mini PC, 1920×1080 @ 60 Hz, 30 FPS:

| | |
|---|---|
| Binary size | ~450 KB |
| Process RSS | ~2.6 MB (22 MB peak at start-up while the font is loaded) |
| Scan-out buffers | 2 × 8 MB (allocated by the graphics driver) |
| CPU | ~17 % of one core |

For comparison, a browser kiosk (Firefox + cage) typically needs 300–500 MB.

## How it works

- At start-up the 96 katakana glyphs are rasterised once from Noto Sans CJK JP
  and kept as small coverage maps (<100 KB); the font file is then released.
- Each frame is drawn into an off-screen dumb buffer and shown with a page flip
  synced to the display refresh (double buffering, no tearing).
- Glyphs are blended against the known background colour, so video memory is
  never read back.

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
rain-kms [--res 1920x1080|native] [--size 30] [--fps 30]
         [--card auto|/dev/dri/cardN] [--font PATH] [--font-index 0]
         [--capture FILE.ppm]
```

| Option | Default | Meaning |
|---|---|---|
| `--res` | `1920x1080` | Display mode; `native` uses the display's preferred mode. Lower resolutions use less RAM and CPU. |
| `--size` | `30` | Glyph size in pixels. |
| `--fps` | `30` | Frame-rate cap. |
| `--card` | `auto` | DRM device; `auto` picks the first card with a connected display. |
| `--font` | `/usr/share/fonts/noto-cjk/NotoSansCJK-Bold.ttc` | Font file. |
| `--font-index` | `0` | Face inside a font collection (`0` = Noto Sans CJK JP). |
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
on failure.

To remove it:

```sh
sudo systemctl disable --now rain-kms && sudo systemctl enable --now getty@tty1
```

## Repository conventions

- [`AGENTS.md`](AGENTS.md) — rules for agents (`CLAUDE.md` symlinks to it).
- [`ROADMAP/`](ROADMAP/) — project plan by date, with Mermaid Gantt diagrams.
- [`RECORD/`](RECORD/) — project memory (`.WIP` append-only, `.completed` read-only).

All code and documentation in this repository are written in English.
