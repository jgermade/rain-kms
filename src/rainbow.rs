//! "rainbow" style (v1): full-width katakana rain with drifting rainbow hues.
//!
//! Glyphs never move: each one sits in a fixed cell of the grid, and only whether
//! it is lit, its brightness, its hue and (now and then) its character change.
//! Each buffer keeps a key per cell and only cells whose key changed are redrawn,
//! straight into the scan-out buffer.

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};

use crate::{background, Rng};

const FIRST_KANA: u32 = 0x30A0;
const KANA_COUNT: usize = 96;

/// A glyph as a coverage map (0-255) exactly one cell (size × size) in size,
/// already positioned inside the cell.
#[derive(Clone)]
struct Glyph {
    coverage: Vec<u8>,
}

fn render_glyphs(font: &FontRef, size: u32) -> Vec<Glyph> {
    let scale = PxScale::from(size as f32);
    let scaled = font.as_scaled(scale);
    let baseline = scaled.ascent();

    (0..KANA_COUNT as u32)
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
        .collect()
}

fn kana(rng: &mut Rng) -> u8 {
    rng.below(KANA_COUNT) as u8
}

#[derive(Clone)]
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

pub fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
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

#[derive(Clone)]
pub struct Rain {
    glyphs: Vec<Glyph>,
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
    pub fn new(font: &FontRef, size: u32, width: usize, height: usize) -> Self {
        let glyphs = render_glyphs(font, size);
        let size = size as i32;
        let cols = (width as i32 / size).max(1);
        let rows = height as i32 / size + 30;
        let mut rng = Rng::seeded();
        let drops = (0..cols)
            .map(|_| {
                let mut d = RainDrop { y: 0.0, speed: 0.0, brightness: 0.0, length: 0, chars: (0..rows).map(|_| kana(&mut rng)).collect() };
                d.reset(&mut rng, rows, true);
                d
            })
            .collect();
        let visible_rows = height.div_ceil(size as usize) + 1;
        Rain { glyphs, drops, rng, size, rows, height, visible_rows }
    }

    /// Number of cells; each buffer keeps one key per cell (see `frame`).
    pub fn cells(&self) -> usize {
        self.drops.len() * self.visible_rows
    }

    /// Advances `dt` seconds and updates the frame in `px` (stride = pixels per row).
    /// Returns the number of cells redrawn.
    ///
    /// `keys` holds what each cell of this buffer currently shows; only cells whose
    /// key differs are redrawn. `px` must start out filled with the background colour
    /// and `keys` with zeros.
    ///
    /// `px` is usually a scan-out buffer in write-combined memory: fast for sequential
    /// writes, very slow for scattered single-pixel ones. So each changed cell is
    /// composed in a small tile in ordinary RAM and copied to `px` row by row.
    pub fn frame(&mut self, px: &mut [u32], stride: usize, dt: f32, ms: f32, keys: &mut [u32]) -> usize {
        let (size, rows, height, vis) = (self.size, self.rows, self.height, self.visible_rows);
        let bg = background();
        let cell = size as usize;
        let mut redrawn = 0;
        for (x, d) in self.drops.iter_mut().enumerate() {
            if self.rng.f() > 0.4 {
                let i = self.rng.below(rows as usize);
                d.chars[i] = kana(&mut self.rng);
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
                    Some((blend_table(bg, color, alpha), &self.glyphs[((key >> 16) & 0x7f) as usize].coverage))
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
