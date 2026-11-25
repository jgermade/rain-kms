//! Full-screen digital rain drawn straight to the display.
//!
//! Draws straight to the video output through DRM/KMS: no display server,
//! compositor or browser needed. It uses two image buffers (double buffering)
//! and syncs with the display refresh, so there is no tearing.
//!
//! Two styles:
//! - `film` (default): green rain in several depth layers with glow, like the
//!   monitors in the film (see `film.rs`).
//! - `rainbow`: the original single-layer katakana rain with rainbow hues
//!   (see `rainbow.rs`).
//!
//! The glyphs are pre-rendered at start-up and the font file is released as soon
//! as they are ready.

mod film;
mod rainbow;

use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use ab_glyph::FontRef;
use drm::buffer::Buffer as _;
use drm::control::{connector, crtc, dumbbuffer::DumbBuffer, framebuffer, Device as ControlDevice, Event, Mode, PageFlipFlags};
use drm::Device;

/// Background colour (0xRRGGBB), set once at start-up from `--background`.
static BACKGROUND: AtomicU32 = AtomicU32::new(0x00_0000);

fn background() -> u32 {
    BACKGROUND.load(Ordering::Relaxed)
}

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

#[derive(Clone, Copy, PartialEq)]
enum Style {
    Film,
    Rainbow,
}

struct Options {
    style: Style,
    layers: u32,
    colors: film::Colors,
    min_size: Option<u32>,
    density: u32,
    speed_spread: f32,
    res: Option<(u16, u16)>,
    size: u32,
    fps: u32,
    card: String,
    font: String,
    font_index: u32,
    capture: Option<String>,
    bench: bool,
}

const USAGE: &str = "rain-kms [--style film|rainbow] [--layers 4] [--colors full|rainbow|green|monokai] [--min-size 15] [--density 3] [--speed-spread 2.2] \
[--res 1920x1080|native] [--size|--max-size 30] [--fps 30] \
[--card auto|/dev/dri/cardN] [--background 000000] [--font PATH] [--font-index 0] [--capture FILE.ppm] [--bench]";

fn options() -> Options {
    let mut o = Options {
        style: Style::Film,
        layers: 4,
        colors: film::Colors::Full,
        min_size: None, // half of --size
        density: 3,
        speed_spread: 2.2,
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
            "--style" => {
                o.style = match value().as_str() {
                    "film" => Style::Film,
                    "rainbow" => Style::Rainbow,
                    _ => fail("--style must be film or rainbow"),
                }
            }
            "--colors" => {
                o.colors = match value().as_str() {
                    "full" => film::Colors::Full,
                    "monokai" => film::Colors::Monokai,
                    "rainbow" => film::Colors::Rainbow,
                    "green" => film::Colors::Green,
                    _ => fail("--colors must be full, rainbow, green or monokai"),
                }
            }
            "--layers" => o.layers = value().parse::<u32>().unwrap_or_else(|_| fail("invalid --layers")).clamp(1, 8),
            "--res" => {
                let v = value();
                o.res = if v == "native" {
                    None
                } else {
                    let (w, h) = v.split_once('x').unwrap_or_else(|| fail("--res must be WIDTHxHEIGHT or native"));
                    Some((w.parse().unwrap_or_else(|_| fail("invalid width")), h.parse().unwrap_or_else(|_| fail("invalid height"))))
                }
            }
            "--size" | "--max-size" => o.size = value().parse::<u32>().unwrap_or_else(|_| fail(&format!("invalid {a}"))).max(8),
            "--min-size" => o.min_size = Some(value().parse::<u32>().unwrap_or_else(|_| fail("invalid --min-size")).max(6)),
            "--speed-spread" => o.speed_spread = value().parse::<f32>().unwrap_or_else(|_| fail("invalid --speed-spread")).clamp(1.0, 10.0),
            "--density" => o.density = value().parse::<u32>().unwrap_or_else(|_| fail("invalid --density")).clamp(1, 5),
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
// Random numbers (xorshift, no dependencies)

#[derive(Clone)]
struct Rng(u64);
impl Rng {
    /// seeded from the clock
    fn seeded() -> Self {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
        Rng(t.map(|d| d.as_nanos() as u64).unwrap_or(1) | 1)
    }
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
    /// integer in [0, n)
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

// ---------------------------------------------------------------------------
// Scenes

#[derive(Clone)]
enum Scene {
    Film(film::Film),
    Rainbow(rainbow::Rain),
}

impl Scene {
    /// Rasterises the glyphs for `o.style` and sets up the animation. Reads the font
    /// and releases it before returning.
    fn new(o: &Options, width: usize, height: usize) -> Self {
        let data = std::fs::read(&o.font).unwrap_or_else(|e| fail(&format!("cannot read font {}: {e}", o.font)));
        let font = FontRef::try_from_slice_and_index(&data, o.font_index).unwrap_or_else(|e| fail(&format!("invalid font: {e}")));
        match o.style {
            Style::Film => {
                let p = film::Params {
                    max_size: o.size,
                    min_size: o.min_size.unwrap_or(o.size / 2),
                    layers: o.layers,
                    density: o.density,
                    speed_spread: o.speed_spread,
                    colors: o.colors,
                };
                Scene::Film(film::Film::new(&font, &p, width, height))
            }
            Style::Rainbow => Scene::Rainbow(rainbow::Rain::new(&font, o.size, width, height)),
        }
        // `data` (the ~20 MB font file) is freed here
    }

    /// Size of the per-buffer state `frame` needs (cell keys for the rainbow style).
    fn cells(&self) -> usize {
        match self {
            Scene::Film(_) => 0,
            Scene::Rainbow(r) => r.cells(),
        }
    }

    /// Advances `dt` seconds and updates the frame in `px`. `px` must start out
    /// filled with the background colour and `keys` with zeros. Returns the number of
    /// glyphs drawn (film) or cells redrawn (rainbow).
    fn frame(&mut self, px: &mut [u32], stride: usize, dt: f32, ms: f32, keys: &mut [u32]) -> usize {
        match self {
            Scene::Film(f) => f.frame(px, stride, dt),
            Scene::Rainbow(r) => r.frame(px, stride, dt, ms, keys),
        }
    }
}

/// Benchmark mode: renders frames and reports the cost per frame, first into ordinary
/// RAM and then into real scan-out buffers of the graphics card (created and mapped
/// but never shown, so it works while another program owns the display).
fn bench(o: &Options) {
    let (w, h) = o.res.unwrap_or((1920, 1080));
    let (width, height) = (w as usize, h as usize);
    let scene = Scene::new(o, width, height);
    let work = match o.style {
        Style::Film => "glyphs drawn per frame".to_string(),
        Style::Rainbow => format!("of {} cells redrawn per frame", scene.cells()),
    };

    let run = |label: &str, a: &mut [u32], b: &mut [u32], stride: usize| {
        let mut rain = scene.clone();
        let mut keys = [vec![0u32; rain.cells()], vec![0u32; rain.cells()]];
        let frames = 300;
        let mut redrawn = 0;
        let t = Instant::now();
        for f in 0..frames {
            let px: &mut [u32] = if f % 2 == 0 { &mut *a } else { &mut *b };
            redrawn += rain.frame(px, stride, 1.0 / 30.0, f as f32 * 1000.0 / 30.0, &mut keys[f % 2]);
        }
        let per_frame = t.elapsed().as_secs_f64() * 1000.0 / frames as f64;
        println!(
            "{label}: {per_frame:.2} ms per frame -> {:.1} % of one core at 30 FPS; {} {work}",
            per_frame * 30.0 / 10.0,
            redrawn / frames,
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

    println!("{w}x{h}, {} px glyphs", o.size);
    let mut ram = [vec![background(); width * height], vec![background(); width * height]];
    let [ra, rb] = &mut ram;
    fill_speed("RAM", ra);
    run("RAM", ra, rb, width);

    // the same into the card's scan-out buffers
    let c = open_card(&o.card);
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
    let mut rain = scene.clone();
    let mut keys = [vec![0u32; rain.cells()], vec![0u32; rain.cells()]];
    let period = Duration::from_secs_f64(1.0 / 30.0);
    let frames = 150;
    let (cpu0, t0) = (cpu_time(), Instant::now());
    let mut busy = Duration::ZERO;
    for f in 0..frames {
        let start = Instant::now();
        let px: &mut [u32] = if f % 2 == 0 { &mut *sa[0] } else { &mut *sb[0] };
        rain.frame(px, stride, 1.0 / 30.0, f as f32 * 1000.0 / 30.0, &mut keys[f % 2]);
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
fn capture(path: &str, o: &Options) {
    let (w, h) = o.res.unwrap_or((1920, 1080));
    let (width, height) = (w as usize, h as usize);
    let mut rain = Scene::new(o, width, height);
    // two buffers, alternated, as on the real display; the last one drawn is saved
    let mut bufs = [vec![background(); width * height], vec![background(); width * height]];
    let mut keys = [vec![0u32; rain.cells()], vec![0u32; rain.cells()]];
    let frames = 90;
    for f in 0..frames {
        let b = f % 2;
        rain.frame(&mut bufs[b], width, 1.0 / 30.0, f as f32 * 1000.0 / 30.0, &mut keys[b]);
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
/// thousands of page faults), plus what each of its cells shows (rainbow style).
struct Buffer<'a> {
    fb: framebuffer::Handle,
    px: &'a mut [u32],
    stride: usize,
    keys: Vec<u32>,
}

fn main() {
    let o = options();
    if let Some(path) = &o.capture {
        capture(path, &o);
        return;
    }
    if o.bench {
        bench(&o);
        return;
    }

    let card = open_card(&o.card);
    let (con, crtc, mode) = pick_output(&card, o.res);
    let (w, h) = mode.size();
    let (width, height) = (w as usize, h as usize);
    let style = match o.style {
        Style::Film => format!(
            "film style, {} layers of {}-{} px, density {}, speed spread {}",
            o.layers,
            o.min_size.unwrap_or(o.size / 2).min(o.size),
            o.size,
            o.density,
            o.speed_spread
        ),
        Style::Rainbow => "rainbow style".into(),
    };
    eprintln!("rain-kms: {}x{} at {} Hz, {style}, {} px glyphs, {} FPS", w, h, mode.vrefresh(), o.size, o.fps);

    let mut rain = Scene::new(&o, width, height);

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
            px.fill(background()); // full clear once; the rainbow style then redraws only changed cells
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
        rain.frame(b.px, b.stride, dt, ms, &mut b.keys);

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
