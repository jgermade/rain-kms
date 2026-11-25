//! Full-screen katakana rain with rainbow colours.
//!
//! Draws straight to the video output through DRM/KMS: no display server,
//! compositor or browser needed. It uses two image buffers (double buffering)
//! and syncs with the display refresh, so there is no tearing.
//!
//! Memory: two width×height×4-byte images (16 MB at 1920×1080) plus the 96
//! pre-rendered katakana glyphs (<100 KB). The font file is released as soon
//! as the glyphs are ready.
//!
//! Usage: rain-kms [--res 1920x1080|native] [--size 30] [--fps 30]
//!                 [--card auto|/dev/dri/cardN] [--font PATH.ttc] [--font-index 0]
//!                 [--capture FILE.ppm]

use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, BorrowedFd};
use std::time::{Duration, Instant};

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use drm::buffer::Buffer as _;
use drm::control::{connector, crtc, dumbbuffer::DumbBuffer, framebuffer, Device as ControlDevice, Event, Mode, PageFlipFlags};
use drm::Device;

const BACKGROUND: u32 = 0x0006_0910; // #060910
const FIRST_KANA: u32 = 0x30A0;
const KANA_COUNT: usize = 96;

// ---------------------------------------------------------------------------
// Graphics card

struct Card(File);
impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl Device for Card {}
impl ControlDevice for Card {}

// ---------------------------------------------------------------------------
// Options

struct Options {
    res: Option<(u16, u16)>,
    size: u32,
    fps: u32,
    card: String,
    font: String,
    font_index: u32,
    capture: Option<String>,
}

const USAGE: &str = "rain-kms [--res 1920x1080|native] [--size 30] [--fps 30] [--card auto|/dev/dri/cardN] \
[--font PATH] [--font-index 0] [--capture FILE.ppm]";

fn options() -> Options {
    let mut o = Options {
        res: Some((1920, 1080)),
        size: 30,
        fps: 30,
        card: "auto".into(),
        font: "/usr/share/fonts/noto-cjk/NotoSansCJK-Bold.ttc".into(),
        font_index: 0, // 0 = Noto Sans CJK JP inside the .ttc collection
        capture: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| fail(&format!("missing value for {a}")));
        match a.as_str() {
            "--res" => {
                let v = value();
                o.res = if v == "native" {
                    None
                } else {
                    let (w, h) = v.split_once('x').unwrap_or_else(|| fail("--res must be WIDTHxHEIGHT or native"));
                    Some((w.parse().unwrap_or_else(|_| fail("invalid width")), h.parse().unwrap_or_else(|_| fail("invalid height"))))
                }
            }
            "--size" => o.size = value().parse::<u32>().unwrap_or_else(|_| fail("invalid --size")).max(8),
            "--fps" => o.fps = value().parse::<u32>().unwrap_or_else(|_| fail("invalid --fps")).clamp(1, 240),
            "--card" => o.card = value(),
            "--font" => o.font = value(),
            "--font-index" => o.font_index = value().parse().unwrap_or_else(|_| fail("invalid --font-index")),
            // render one frame (after 3 simulated seconds) to a PPM file and exit, without touching the display
            "--capture" => o.capture = Some(value()),
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            _ => fail(&format!("unknown option: {a}\n{USAGE}")),
        }
    }
    o
}

fn fail(msg: &str) -> ! {
    eprintln!("rain-kms: {msg}");
    std::process::exit(1);
}

// ---------------------------------------------------------------------------
// Glyphs: the 96 katakana rendered once, as coverage maps (0-255)

struct Glyph {
    dx: i32, // offset inside the cell
    dy: i32,
    width: usize,
    height: usize,
    coverage: Vec<u8>,
}

fn render_glyphs(path: &str, index: u32, size: u32) -> Vec<Glyph> {
    let data = std::fs::read(path).unwrap_or_else(|e| fail(&format!("cannot read font {path}: {e}")));
    let font = FontRef::try_from_slice_and_index(&data, index).unwrap_or_else(|e| fail(&format!("invalid font: {e}")));
    let scale = PxScale::from(size as f32);
    let scaled = font.as_scaled(scale);
    let baseline = scaled.ascent();

    let glyphs = (0..KANA_COUNT as u32)
        .map(|i| {
            let c = char::from_u32(FIRST_KANA + i).unwrap_or(' ');
            let id = font.glyph_id(c);
            let advance = scaled.h_advance(id);
            let glyph = id.with_scale_and_position(scale, ab_glyph::point((size as f32 - advance) / 2.0, baseline));
            match font.outline_glyph(glyph) {
                Some(outline) => {
                    let b = outline.px_bounds();
                    let (width, height) = (b.width() as usize, b.height() as usize);
                    let mut coverage = vec![0u8; width * height];
                    outline.draw(|x, y, c| {
                        let i = y as usize * width + x as usize;
                        if i < coverage.len() {
                            coverage[i] = (c.clamp(0.0, 1.0) * 255.0) as u8;
                        }
                    });
                    Glyph { dx: b.min.x as i32, dy: b.min.y as i32, width, height, coverage }
                }
                None => Glyph { dx: 0, dy: 0, width: 0, height: 0, coverage: Vec::new() },
            }
        })
        .collect();
    drop(data); // frees the ~20 MB font file
    glyphs
}

// ---------------------------------------------------------------------------
// Random numbers (xorshift, no dependencies)

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    /// number in [0, 1)
    fn f(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
    fn range(&mut self, min: f32, max: f32) -> f32 {
        min + self.f() * (max - min)
    }
    fn kana(&mut self) -> u8 {
        (self.next() % KANA_COUNT as u64) as u8
    }
}

// ---------------------------------------------------------------------------
// The rain

struct RainDrop {
    y: f32,
    speed: f32,
    brightness: f32,
    length: i32,
    chars: Vec<u8>, // kana index per row
}

impl RainDrop {
    fn reset(&mut self, rng: &mut Rng, rows: i32, at_start: bool) {
        self.y = if at_start { rng.f() * rows as f32 } else { -rng.range(0.0, 10.0) };
        self.speed = rng.range(0.5, 2.0);
        self.brightness = rng.range(0.3, 1.0);
        self.length = rng.range(5.0, 25.0) as i32;
    }
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = (h / 60.0) % 6.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    ((r + m) * 255.0, (g + m) * 255.0, (b + m) * 255.0)
}

/// Paints a glyph over the background. It blends against the known background colour
/// instead of reading the buffer back: video memory is very slow to read.
#[allow(clippy::too_many_arguments)]
fn paint(px: &mut [u32], width: usize, height: usize, stride: usize, g: &Glyph, x0: i32, y0: i32, color: (f32, f32, f32), alpha: f32) {
    let (br, bg, bb) = (((BACKGROUND >> 16) & 0xff) as f32, ((BACKGROUND >> 8) & 0xff) as f32, (BACKGROUND & 0xff) as f32);
    let (dr, dg, db) = (color.0 - br, color.1 - bg, color.2 - bb);
    let alpha = alpha / 255.0;
    for gy in 0..g.height {
        let y = y0 + g.dy + gy as i32;
        if y < 0 || y as usize >= height {
            continue;
        }
        let row = &mut px[y as usize * stride..];
        for gx in 0..g.width {
            let c = g.coverage[gy * g.width + gx];
            if c == 0 {
                continue;
            }
            let x = x0 + g.dx + gx as i32;
            if x < 0 || x as usize >= width {
                continue;
            }
            let a = c as f32 * alpha;
            let r = (br + dr * a) as u32;
            let gr = (bg + dg * a) as u32;
            let b = (bb + db * a) as u32;
            row[x as usize] = (r << 16) | (gr << 8) | b;
        }
    }
}

struct Rain {
    drops: Vec<RainDrop>,
    rng: Rng,
    size: i32,
    rows: i32,
    width: usize,
    height: usize,
}

impl Rain {
    fn new(size: i32, width: usize, height: usize) -> Self {
        let cols = (width as i32 / size).max(1);
        let rows = height as i32 / size + 30;
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1) | 1;
        let mut rng = Rng(seed);
        let drops = (0..cols)
            .map(|_| {
                let mut d = RainDrop { y: 0.0, speed: 0.0, brightness: 0.0, length: 0, chars: (0..rows).map(|_| rng.kana()).collect() };
                d.reset(&mut rng, rows, true);
                d
            })
            .collect();
        Rain { drops, rng, size, rows, width, height }
    }

    /// Advances `dt` seconds and draws a full frame into `px` (stride = pixels per row).
    fn frame(&mut self, px: &mut [u32], stride: usize, glyphs: &[Glyph], dt: f32, ms: f32) {
        let (size, rows, width, height) = (self.size, self.rows, self.width, self.height);
        px.fill(BACKGROUND);
        for (x, d) in self.drops.iter_mut().enumerate() {
            if self.rng.f() > 0.4 {
                let i = (self.rng.next() % rows as u64) as usize;
                d.chars[i] = self.rng.kana();
            }
            d.y += d.speed * dt * 12.0;
            let head = d.y.floor() as i32;
            let hue = (ms / 15.0 + x as f32 * 10.0) % 360.0;
            let tail_color = hsl_to_rgb(hue, 0.8, 0.65);

            for i in 0..d.length {
                let row = head - i;
                if row < 0 || row >= rows {
                    continue;
                }
                let (color, alpha) = if i == 0 {
                    ((220.0, 255.0, 255.0), d.brightness)
                } else {
                    (tail_color, d.brightness * (1.0 - i as f32 / d.length as f32))
                };
                let glyph = &glyphs[d.chars[row as usize] as usize];
                // as in the original demo: the row marks the text baseline
                paint(px, width, height, stride, glyph, x as i32 * size, (row - 1) * size, color, alpha);
            }
            if ((d.y - d.length as f32) * size as f32) > height as f32 {
                d.reset(&mut self.rng, rows, false);
            }
        }
    }
}

/// Test mode: simulates 3 s and saves the frame as a PPM image.
fn capture(path: &str, res: Option<(u16, u16)>, size: i32, glyphs: &[Glyph]) {
    let (w, h) = res.unwrap_or((1920, 1080));
    let (width, height) = (w as usize, h as usize);
    let mut px = vec![0u32; width * height];
    let mut rain = Rain::new(size, width, height);
    for f in 0..90 {
        rain.frame(&mut px, width, glyphs, 1.0 / 30.0, f as f32 * 1000.0 / 30.0);
    }
    let mut out = format!("P6\n{width} {height}\n255\n").into_bytes();
    out.reserve(width * height * 3);
    for p in &px {
        out.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
    }
    std::fs::write(path, out).unwrap_or_else(|e| fail(&format!("cannot write {path}: {e}")));
}

// ---------------------------------------------------------------------------
// Display

fn pick_output(card: &Card, res: Option<(u16, u16)>) -> (connector::Handle, crtc::Handle, Mode) {
    let resources = card.resource_handles().unwrap_or_else(|e| fail(&format!("no access to the card: {e}")));
    for &con in resources.connectors() {
        let info = match card.get_connector(con, true) {
            Ok(i) => i,
            Err(_) => continue,
        };
        if info.state() != connector::State::Connected || info.modes().is_empty() {
            continue;
        }
        let modes = info.modes();
        let mode = match res {
            Some((w, h)) => modes
                .iter()
                .filter(|m| m.size() == (w, h))
                .min_by_key(|m| (m.vrefresh() as i32 - 60).abs())
                .copied()
                .unwrap_or_else(|| {
                    eprintln!("rain-kms: the display does not support {w}x{h}, using its native mode");
                    modes[0]
                }),
            None => modes[0],
        };
        // a controller (CRTC) able to drive this connector
        for enc in info.encoders() {
            if let Ok(e) = card.get_encoder(*enc) {
                if let Some(c) = e.crtc() {
                    return (con, c, mode);
                }
                if let Some(&c) = resources.filter_crtcs(e.possible_crtcs()).first() {
                    return (con, c, mode);
                }
            }
        }
    }
    fail("no connected display found");
}

fn open(path: &str) -> std::io::Result<Card> {
    OpenOptions::new().read(true).write(true).open(path).map(Card)
}

/// "auto": the first /dev/dri/cardN with a connected display.
fn open_card(card: &str) -> Card {
    if card != "auto" {
        return open(card).unwrap_or_else(|e| fail(&format!("cannot open {card}: {e}")));
    }
    for n in 0..8 {
        let path = format!("/dev/dri/card{n}");
        let Ok(c) = open(&path) else { continue };
        let Ok(res) = c.resource_handles() else { continue };
        let connected = res
            .connectors()
            .iter()
            .any(|&h| c.get_connector(h, false).map(|i| i.state() == connector::State::Connected).unwrap_or(false));
        if connected {
            return c;
        }
    }
    fail("no graphics card with a connected display found");
}

struct Buffer {
    db: DumbBuffer,
    fb: framebuffer::Handle,
}

fn main() {
    let o = options();
    let glyphs = render_glyphs(&o.font, o.font_index, o.size);
    if let Some(path) = &o.capture {
        capture(path, o.res, o.size as i32, &glyphs);
        return;
    }

    let card = open_card(&o.card);
    let (con, crtc, mode) = pick_output(&card, o.res);
    let (w, h) = mode.size();
    let (width, height) = (w as usize, h as usize);
    eprintln!("rain-kms: {}x{} at {} Hz, {} px glyphs, {} FPS", w, h, mode.vrefresh(), o.size, o.fps);

    let mut buffers: Vec<Buffer> = (0..2)
        .map(|_| {
            let db = card
                .create_dumb_buffer((w as u32, h as u32), drm_fourcc::DrmFourcc::Xrgb8888, 32)
                .unwrap_or_else(|e| fail(&format!("cannot create buffer: {e}")));
            let fb = card.add_framebuffer(&db, 24, 32).unwrap_or_else(|e| fail(&format!("cannot create framebuffer: {e}")));
            Buffer { db, fb }
        })
        .collect();

    let mut rain = Rain::new(o.size as i32, width, height);

    let start = Instant::now();
    let period = Duration::from_secs_f64(1.0 / o.fps as f64);
    let mut last = Instant::now();
    let mut current = 0usize;
    let mut first = true;

    loop {
        let now = Instant::now();
        let dt = (now - last).as_secs_f32().min(0.1);
        last = now;
        let ms = (now - start).as_secs_f32() * 1000.0;

        // draw into the buffer that is not on screen
        let b = &mut buffers[current];
        let stride = b.db.pitch() as usize / 4;
        {
            let mut map = card.map_dumb_buffer(&mut b.db).unwrap_or_else(|e| fail(&format!("cannot map buffer: {e}")));
            let bytes = map.as_mut();
            // SAFETY: the buffer is page-aligned and is stride*height*4 bytes long
            let px: &mut [u32] = unsafe { std::slice::from_raw_parts_mut(bytes.as_mut_ptr() as *mut u32, bytes.len() / 4) };
            rain.frame(px, stride, &glyphs, dt, ms);
        }

        // show it
        if first {
            card.set_crtc(crtc, Some(buffers[current].fb), (0, 0), &[con], Some(mode))
                .unwrap_or_else(|e| fail(&format!("cannot take over the display (is another program using it?): {e}")));
            first = false;
        } else {
            card.page_flip(crtc, buffers[current].fb, PageFlipFlags::EVENT, None)
                .unwrap_or_else(|e| fail(&format!("page flip failed: {e}")));
            // wait for the display refresh
            'wait: loop {
                match card.receive_events() {
                    Ok(events) => {
                        for ev in events {
                            if let Event::PageFlip(_) = ev {
                                break 'wait;
                            }
                        }
                    }
                    Err(e) => fail(&format!("waiting for refresh failed: {e}")),
                }
            }
        }
        current ^= 1;

        // cap the frame rate
        let spent = now.elapsed();
        if spent < period {
            std::thread::sleep(period - spent);
        }
    }
}
