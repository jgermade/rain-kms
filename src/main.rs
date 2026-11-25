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
//!                 [--card auto|/dev/dri/cardN] [--background 000000]
//!                 [--font PATH.ttc] [--font-index 0] [--capture FILE.ppm]

use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use drm::buffer::Buffer as _;
use drm::control::{connector, crtc, dumbbuffer::DumbBuffer, framebuffer, Device as ControlDevice, Event, Mode, PageFlipFlags};
use drm::Device;

/// Background colour (0xRRGGBB), set once at start-up from `--background`.
static BACKGROUND: AtomicU32 = AtomicU32::new(0x00_0000);

fn background() -> u32 {
    BACKGROUND.load(Ordering::Relaxed)
}
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
    bench: bool,
}

const USAGE: &str = "rain-kms [--res 1920x1080|native] [--size 30] [--fps 30] [--card auto|/dev/dri/cardN] \
[--background 000000] [--font PATH] [--font-index 0] [--capture FILE.ppm] [--bench]";

fn options() -> Options {
    let mut o = Options {
        res: Some((1920, 1080)),
        size: 30,
        fps: 30,
        card: "auto".into(),
        font: "/usr/share/fonts/noto-cjk/NotoSansCJK-Bold.ttc".into(),
        font_index: 0, // 0 = Noto Sans CJK JP inside the .ttc collection
        capture: None,
        bench: false,
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
            // render frames into RAM and report the time per frame, without touching the display
            "--bench" => o.bench = true,
            "--background" => {
                let v = value();
                let hex = v.trim_start_matches('#');
                let rgb = u32::from_str_radix(hex, 16)
                    .ok()
                    .filter(|_| hex.len() == 6)
                    .unwrap_or_else(|| fail("--background must be a RRGGBB hex colour, e.g. 000000"));
                BACKGROUND.store(rgb, Ordering::Relaxed);
            }
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

/// A glyph as a coverage map (0-255) exactly one cell (size × size) in size,
/// already positioned inside the cell.
struct Glyph {
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
            let cell = size as usize;
            let mut coverage = vec![0u8; cell * cell];
            if let Some(outline) = font.outline_glyph(glyph) {
                let b = outline.px_bounds();
                let (dx, dy) = (b.min.x as i32, b.min.y as i32);
                // anything spilling out of the cell is clipped
                outline.draw(|x, y, c| {
                    let (cx, cy) = (dx + x as i32, dy + y as i32);
                    if (0..cell as i32).contains(&cx) && (0..cell as i32).contains(&cy) {
                        coverage[cy as usize * cell + cx as usize] = (c.clamp(0.0, 1.0) * 255.0) as u8;
                    }
                });
            }
            Glyph { coverage }
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

/// Colour for every coverage level (0-255) of a glyph drawn in `color` at `alpha`
/// over the background, in integer maths. Drawing a cell is then one table lookup
/// per pixel. Blending against the known background colour means video memory is
/// never read back.
fn blend_table(bg: u32, color: (u32, u32, u32), alpha: f32) -> [u32; 256] {
    let (br, bgg, bb) = ((bg >> 16) & 0xff, (bg >> 8) & 0xff, bg & 0xff);
    let alpha = (alpha.clamp(0.0, 1.0) * 256.0) as u32; // 0..=256
    let mut table = [0u32; 256];
    for (c, out) in table.iter_mut().enumerate() {
        let a = (c as u32 * alpha + 128) >> 8; // 0..=256
        let mix = |b: u32, f: u32| (b * (256 - a) + f * a) >> 8;
        *out = (mix(br, color.0) << 16) | (mix(bgg, color.1) << 8) | mix(bb, color.2);
    }
    table
}

/// Hue is rounded to steps of this many degrees, so a lit cell only needs redrawing
/// every few frames instead of on every frame as the rainbow drifts.
const HUE_STEP: f32 = 6.0;
/// Brightness levels a cell can take.
const ALPHA_LEVELS: f32 = 63.0;

struct Rain {
    drops: Vec<RainDrop>,
    rng: Rng,
    size: i32,
    rows: i32,
    height: usize,
    /// rows of cells that can be visible: cell `r` covers pixel rows [(r-1)*size, r*size)
    visible_rows: usize,
}

/// What a cell shows, packed in a u32 so frames can be compared cheaply.
/// 0 = empty; otherwise lit | head | char | alpha level | hue step.
fn cell_key(head: bool, kana: u8, alpha: f32, hue: f32) -> u32 {
    let a = (alpha.clamp(0.0, 1.0) * ALPHA_LEVELS).round() as u32;
    if a == 0 {
        return 0;
    }
    let h = if head { 0 } else { ((hue / HUE_STEP) as u32) % (360.0 / HUE_STEP) as u32 };
    (1 << 31) | ((head as u32) << 30) | ((kana as u32) << 16) | (a << 8) | h
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
        let visible_rows = height.div_ceil(size as usize) + 1;
        Rain { drops, rng, size, rows, height, visible_rows }
    }

    /// Number of cells; each buffer keeps one key per cell (see `frame`).
    fn cells(&self) -> usize {
        self.drops.len() * self.visible_rows
    }

    /// Advances `dt` seconds and updates the frame in `px` (stride = pixels per row).
    ///
    /// Glyphs never move: each one sits in a fixed cell of the grid, and only whether
    /// it is lit, its brightness, its hue and (now and then) its character change.
    /// `keys` holds what each cell of this buffer currently shows; only cells whose
    /// key differs are redrawn. `px` must start out filled with the background colour
    /// and `keys` with zeros.
    ///
    /// `px` is usually a scan-out buffer in write-combined memory: fast for sequential
    /// writes, very slow for scattered single-pixel ones. So each changed cell is
    /// composed in a small tile in ordinary RAM and copied to `px` row by row.
    #[allow(clippy::too_many_arguments)]
    fn frame(&mut self, px: &mut [u32], stride: usize, glyphs: &[Glyph], dt: f32, ms: f32, keys: &mut [u32]) -> usize {
        let (size, rows, height, vis) = (self.size, self.rows, self.height, self.visible_rows);
        let bg = background();
        let cell = size as usize;
        let mut redrawn = 0;
        for (x, d) in self.drops.iter_mut().enumerate() {
            if self.rng.f() > 0.4 {
                let i = (self.rng.next() % rows as u64) as usize;
                d.chars[i] = self.rng.kana();
            }
            d.y += d.speed * dt * 12.0;
            let head = d.y.floor() as i32;
            let hue = (ms / 15.0 + x as f32 * 10.0).rem_euclid(360.0);

            for r in 1..vis {
                let i = head - r as i32; // distance from the head; 0 = head
                let key = if (0..d.length).contains(&i) && (r as i32) < rows {
                    let alpha = if i == 0 { d.brightness } else { d.brightness * (1.0 - i as f32 / d.length as f32) };
                    cell_key(i == 0, d.chars[r], alpha, hue)
                } else {
                    0
                };
                let slot = &mut keys[x * vis + r];
                if *slot == key {
                    continue;
                }
                *slot = key;
                redrawn += 1;

                // draw the cell from its key alone, so equal keys give equal pixels; its top
                // is (r-1)*size (the row marks the baseline, as in the original demo)
                let top = (r as i32 - 1) * size;
                let table = if key != 0 {
                    let alpha = ((key >> 8) & 0xff) as f32 / ALPHA_LEVELS;
                    let color = if key & (1 << 30) != 0 {
                        (220, 255, 255)
                    } else {
                        let (r, g, b) = hsl_to_rgb((key & 0xff) as f32 * HUE_STEP, 0.8, 0.65);
                        (r as u32, g as u32, b as u32)
                    };
                    Some((blend_table(bg, color, alpha), &glyphs[((key >> 16) & 0x7f) as usize].coverage))
                } else {
                    None
                };
                for ty in 0..cell {
                    let y = top + ty as i32;
                    if y < 0 || y as usize >= height {
                        continue;
                    }
                    let start = y as usize * stride + x * cell;
                    let dst = &mut px[start..start + cell];
                    match &table {
                        // written straight into the buffer, one sequential row at a time
                        Some((t, cov)) => {
                            for (p, &c) in dst.iter_mut().zip(&cov[ty * cell..(ty + 1) * cell]) {
                                *p = t[c as usize];
                            }
                        }
                        None => dst.fill(bg),
                    }
                }
            }
            if ((d.y - d.length as f32) * size as f32) > height as f32 {
                d.reset(&mut self.rng, rows, false);
            }
        }
        redrawn
    }
}

/// Benchmark mode: renders frames and reports the cost per frame, first into ordinary
/// RAM and then into real scan-out buffers of the graphics card (created and mapped
/// but never shown, so it works while another program owns the display).
fn bench(res: Option<(u16, u16)>, size: i32, glyphs: &[Glyph], card: &str) {
    let (w, h) = res.unwrap_or((1920, 1080));
    let (width, height) = (w as usize, h as usize);

    let run = |label: &str, a: &mut [u32], b: &mut [u32], stride: usize| {
        let mut rain = Rain::new(size, width, height);
        let mut keys = [vec![0u32; rain.cells()], vec![0u32; rain.cells()]];
        let frames = 300;
        let mut redrawn = 0;
        let t = Instant::now();
        for f in 0..frames {
            let px: &mut [u32] = if f % 2 == 0 { &mut *a } else { &mut *b };
            redrawn += rain.frame(px, stride, glyphs, 1.0 / 30.0, f as f32 * 1000.0 / 30.0, &mut keys[f % 2]);
        }
        let per_frame = t.elapsed().as_secs_f64() * 1000.0 / frames as f64;
        println!(
            "{label}: {per_frame:.2} ms per frame -> {:.1} % of one core at 30 FPS; {} of {} cells redrawn per frame",
            per_frame * 30.0 / 10.0,
            redrawn / frames,
            rain.cells()
        );
    };
    let fill_speed = |label: &str, px: &mut [u32]| {
        let t = Instant::now();
        for i in 0..10 {
            px.fill(i);
        }
        let secs = t.elapsed().as_secs_f64();
        println!("{label}: sequential fill {:.0} MB/s", (px.len() * 4 * 10) as f64 / secs / 1e6);
        px.fill(background());
    };

    println!("{w}x{h}, {size} px glyphs");
    let mut ram = [vec![background(); width * height], vec![background(); width * height]];
    let [ra, rb] = &mut ram;
    fill_speed("RAM", ra);
    run("RAM", ra, rb, width);

    // the same into the card's scan-out buffers
    let c = open_card(card);
    let mut dumbs: Vec<DumbBuffer> = (0..2)
        .map(|_| {
            c.create_dumb_buffer((w as u32, h as u32), drm_fourcc::DrmFourcc::Xrgb8888, 32)
                .unwrap_or_else(|e| fail(&format!("cannot create buffer: {e}")))
        })
        .collect();
    let stride = dumbs[0].pitch() as usize / 4;
    let mut maps: Vec<_> = dumbs
        .iter_mut()
        .map(|db| c.map_dumb_buffer(db).unwrap_or_else(|e| fail(&format!("cannot map buffer: {e}"))))
        .collect();
    let mut slices: Vec<&mut [u32]> = maps
        .iter_mut()
        .map(|m| {
            let bytes = m.as_mut();
            // SAFETY: page-aligned mapping of stride*height*4 bytes, alive for this function
            unsafe { std::slice::from_raw_parts_mut(bytes.as_mut_ptr() as *mut u32, bytes.len() / 4) }
        })
        .collect();
    let (sa, sb) = slices.split_at_mut(1);
    fill_speed("scan-out buffer", sa[0]);
    sb[0].fill(background());
    run("scan-out buffer", sa[0], sb[0], stride);

    // Paced like the real loop (one frame, then sleep until the next one) and measured
    // as CPU time, which is what tools like top report. Short bursts of work let the
    // CPU stay at a low clock, so this can differ a lot from the figures above.
    let mut rain = Rain::new(size, width, height);
    let mut keys = [vec![0u32; rain.cells()], vec![0u32; rain.cells()]];
    let period = Duration::from_secs_f64(1.0 / 30.0);
    let frames = 150;
    let (cpu0, t0) = (cpu_time(), Instant::now());
    let mut busy = Duration::ZERO;
    for f in 0..frames {
        let start = Instant::now();
        let px: &mut [u32] = if f % 2 == 0 { &mut *sa[0] } else { &mut *sb[0] };
        rain.frame(px, stride, glyphs, 1.0 / 30.0, f as f32 * 1000.0 / 30.0, &mut keys[f % 2]);
        let spent = start.elapsed();
        busy += spent;
        if spent < period {
            std::thread::sleep(period - spent);
        }
    }
    let wall = t0.elapsed().as_secs_f64();
    println!(
        "paced at 30 FPS: {:.2} ms per frame, {:.1} % CPU time",
        busy.as_secs_f64() * 1000.0 / frames as f64,
        (cpu_time() - cpu0) * 100.0 / wall
    );
}

/// CPU time (user + system) used by this process so far, in seconds.
fn cpu_time() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    // fields after the command name, which is in parentheses and may contain spaces
    let rest = stat.rsplit(')').next().unwrap_or("");
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ticks = |i: usize| f.get(i).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    (ticks(11) + ticks(12)) / 100.0 // utime, stime; USER_HZ is 100 on Linux
}

/// Test mode: simulates 3 s and saves the frame as a PPM image.
fn capture(path: &str, res: Option<(u16, u16)>, size: i32, glyphs: &[Glyph]) {
    let (w, h) = res.unwrap_or((1920, 1080));
    let (width, height) = (w as usize, h as usize);
    let mut rain = Rain::new(size, width, height);
    // two buffers, alternated, as on the real display; the last one drawn is saved
    let mut bufs = [vec![background(); width * height], vec![background(); width * height]];
    let mut keys = [vec![0u32; rain.cells()], vec![0u32; rain.cells()]];
    let frames = 90;
    for f in 0..frames {
        let b = f % 2;
        rain.frame(&mut bufs[b], width, glyphs, 1.0 / 30.0, f as f32 * 1000.0 / 30.0, &mut keys[b]);
    }
    let px = &bufs[(frames - 1) % 2];
    let mut out = format!("P6\n{width} {height}\n255\n").into_bytes();
    out.reserve(width * height * 3);
    for p in px.iter() {
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

/// One scan-out buffer, mapped once for the whole run (re-mapping every frame costs
/// thousands of page faults), plus the rectangles last painted into it.
struct Buffer<'a> {
    fb: framebuffer::Handle,
    px: &'a mut [u32],
    stride: usize,
    keys: Vec<u32>,
}

fn main() {
    let o = options();
    let glyphs = render_glyphs(&o.font, o.font_index, o.size);
    if let Some(path) = &o.capture {
        capture(path, o.res, o.size as i32, &glyphs);
        return;
    }
    if o.bench {
        bench(o.res, o.size as i32, &glyphs, &o.card);
        return;
    }

    let card = open_card(&o.card);
    let (con, crtc, mode) = pick_output(&card, o.res);
    let (w, h) = mode.size();
    let (width, height) = (w as usize, h as usize);
    eprintln!("rain-kms: {}x{} at {} Hz, {} px glyphs, {} FPS", w, h, mode.vrefresh(), o.size, o.fps);

    let mut rain = Rain::new(o.size as i32, width, height);

    let mut dumbs: Vec<DumbBuffer> = (0..2)
        .map(|_| {
            card.create_dumb_buffer((w as u32, h as u32), drm_fourcc::DrmFourcc::Xrgb8888, 32)
                .unwrap_or_else(|e| fail(&format!("cannot create buffer: {e}")))
        })
        .collect();
    let fbs: Vec<framebuffer::Handle> = dumbs
        .iter()
        .map(|db| card.add_framebuffer(db, 24, 32).unwrap_or_else(|e| fail(&format!("cannot create framebuffer: {e}"))))
        .collect();
    let strides: Vec<usize> = dumbs.iter().map(|db| db.pitch() as usize / 4).collect();
    let mut maps: Vec<_> = dumbs
        .iter_mut()
        .map(|db| card.map_dumb_buffer(db).unwrap_or_else(|e| fail(&format!("cannot map buffer: {e}"))))
        .collect();
    let mut buffers: Vec<Buffer> = maps
        .iter_mut()
        .zip(fbs.iter().zip(&strides))
        .map(|(map, (&fb, &stride))| {
            let bytes = map.as_mut();
            // SAFETY: the mapping is page-aligned, stride*height*4 bytes long, and lives
            // (in `maps`) for as long as this slice is used.
            let px: &mut [u32] = unsafe { std::slice::from_raw_parts_mut(bytes.as_mut_ptr() as *mut u32, bytes.len() / 4) };
            px.fill(background()); // full clear once; afterwards only changed cells
            Buffer { fb, px, stride, keys: vec![0; rain.cells()] }
        })
        .collect();

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
        rain.frame(b.px, b.stride, &glyphs, dt, ms, &mut b.keys);

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
