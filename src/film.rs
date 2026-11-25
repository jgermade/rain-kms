//! "film" style (v2): digital rain with depth, closer to the monitors in the
//! film, in the rainbow colours of v1 (or the film's green).
//!
//! - Glyphs: mirrored half-width katakana, digits and a few symbols, in narrow
//!   columns (taller than wide).
//! - Depth: several layers of columns. Farther layers have smaller glyphs, are
//!   dimmer and denser. Every layer falls at the same rows per second, so on
//!   screen the far ones move slower (parallax).
//! - Colour: v1's hsl(hue, 80 %, 65 %) trails and (220, 255, 255) heads, the hue
//!   drifting with time as in v1 (a full turn every 5.4 s). With `Colors::Full`
//!   every column starts at its own random hue; with `Colors::Rainbow` the hue
//!   follows the horizontal position (10° per `size` pixels, as v1's columns), so
//!   the colours form vertical bands shared by all layers. `Colors::Green` is the
//!   film's green.
//! - Light: heads carry a pre-computed soft glow, and light adds up: glyphs are
//!   added per channel, saturating, so overlapping glows burn towards white, like
//!   an overexposed CRT.
//!
//! Layers overlap, so unlike the rainbow style the whole frame is rebuilt every
//! time. To stay in the CPU cache it is built in horizontal bands: the lit glyphs
//! are listed first, then each band is filled with the background, gets its glyphs
//! added, and is copied to the scan-out buffer one sequential row at a time (the
//! fast way to write write-combined memory).

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};

use crate::rainbow::hsl_to_rgb;
use crate::{background, Rng};

/// Settings of the film style.
pub struct Params {
    /// glyph size of the nearest layer, in pixels
    pub max_size: u32,
    /// glyph size of the farthest layer, in pixels
    pub min_size: u32,
    /// depth layers
    pub layers: u32,
    /// 1 (sparse) to 5 (dense); 3 is the default
    pub density: u32,
    /// drop speeds range from `SPEED / speed_spread` to `SPEED × speed_spread`
    pub speed_spread: f32,
    pub colors: Colors,
}

/// Middle drop speed, in rows per second (geometric mean of the range).
const SPEED: f32 = 11.0;

/// Each density level makes the waits between drops this many times shorter.
const DENSITY_STEP: f32 = 1.6;

/// Colours of the film style.
#[derive(Clone, Copy, PartialEq)]
pub enum Colors {
    /// v1 colours, a random hue per column, drifting with time; cyan-white heads
    Full,
    /// v1 colours, the hue by horizontal position (vertical bands), drifting with time
    Rainbow,
    /// the film's green, with white heads
    Green,
    /// a random Monokai accent colour per column, with Monokai's foreground for heads
    Monokai,
}

/// Monokai's accent colours (pink, orange, yellow, green, cyan, purple) and its
/// foreground colour, used for heads.
const MONOKAI: [[f32; 3]; 6] =
    [[249.0, 38.0, 114.0], [253.0, 151.0, 31.0], [230.0, 219.0, 116.0], [166.0, 226.0, 46.0], [102.0, 217.0, 239.0], [174.0, 129.0, 255.0]];
const MONOKAI_FOREGROUND: [f32; 3] = [248.0, 248.0, 242.0];

/// Glyphs whose brightest channel would be below this (of 255) are not drawn.
const MIN_LIGHT: f32 = 3.0;
/// Column width relative to the glyph size.
const COLUMN_WIDTH: f32 = 0.62;
/// Each column is moved off its grid position by up to this share of the column
/// width, either way, so columns (and the gaps between them) do not line up.
const COLUMN_JITTER: f32 = 0.4;
/// Saturation of the farthest layer relative to the nearest (colours fade to grey
/// with depth).
const FAR_SATURATION: f32 = 0.3;
/// Light of a drop's head relative to full brightness, so that far (dimmed) heads
/// still stand out; the colour is capped at the head colour.
const HEAD_LIGHT: f32 = 2.2;
/// Strength of the glow around heads (only heads glow: the rest stays sharp, and
/// it is cheaper).
const GLOW: f32 = 0.9;
/// Independent drops ("cursors") sharing each column.
const DROPS_PER_COLUMN: usize = 2;
/// Height of the bands the frame is built in: small enough for the band's buffer
/// (width × BAND × 4 bytes) to stay in the CPU cache.
const BAND: usize = 64;

/// Share of glyphs that are katakana; the rest are digits and symbols.
const KANA_SHARE: f32 = 0.85;

/// The characters used, and whether each is drawn mirrored (the kana are), plus
/// how many of them (the first ones) are katakana.
fn charset(font: &FontRef) -> (Vec<(char, bool)>, usize) {
    let present = |&(c, _): &(char, bool)| font.glyph_id(c).0 != 0;
    let kana: Vec<_> = (0xFF66..=0xFF9D).filter_map(char::from_u32).map(|c| (c, true)).filter(present).collect();
    let other = "0123456789:.=*+-<>¦|\"".chars().map(|c| (c, false)).filter(present);
    let n = kana.len();
    (kana.into_iter().chain(other).collect(), n)
}

/// A random character index: katakana most of the time.
fn pick(rng: &mut Rng, kana: usize, total: usize) -> u8 {
    if kana == 0 || kana == total {
        return rng.below(total) as u8;
    }
    if rng.f() < KANA_SHARE {
        rng.below(kana) as u8
    } else {
        (kana + rng.below(total - kana)) as u8
    }
}

/// Separable box blur of radius `r` over a w×h image (outside counts as zero).
fn box_blur(src: &[f32], w: usize, h: usize, r: usize) -> Vec<f32> {
    let n = (2 * r + 1) as f32;
    let mut tmp = vec![0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            let (a, b) = (x.saturating_sub(r), (x + r + 1).min(w));
            tmp[y * w + x] = src[y * w + a..y * w + b].iter().sum::<f32>() / n;
        }
    }
    let mut out = vec![0f32; w * h];
    for y in 0..h {
        let (a, b) = (y.saturating_sub(r), (y + r + 1).min(h));
        for x in 0..w {
            out[y * w + x] = (a..b).map(|yy| tmp[yy * w + x]).sum::<f32>() / n;
        }
    }
    out
}

/// A light map (0-255) of w×h pixels, plus the span of each row that is not zero,
/// so drawing it skips the empty parts.
#[derive(Clone)]
struct LightMap {
    w: usize,
    h: usize,
    data: Vec<u8>,
    spans: Vec<(usize, usize)>,
}

impl LightMap {
    fn new(w: usize, h: usize, light: impl Iterator<Item = f32>) -> Self {
        // levels this low add nothing visible, even for the brightest glyphs
        let data: Vec<u8> = light.map(|v| (v.min(1.0) * 255.0).round() as u8).map(|v| if v < 4 { 0 } else { v }).collect();
        let spans = data
            .chunks_exact(w)
            .map(|row| match (row.iter().position(|&v| v != 0), row.iter().rposition(|&v| v != 0)) {
                (Some(a), Some(b)) => (a, b + 1),
                _ => (0, 0),
            })
            .collect();
        LightMap { w, h, data, spans }
    }
}

/// Light maps for one layer, two per character: the plain glyph (cw×ch),
/// and the glyph with its glow (mw×mh, the cell at (pad, pad) and the glow spilling
/// into the padding).
#[allow(clippy::too_many_arguments)]
fn glyph_maps(
    font: &FontRef,
    chars: &[(char, bool)],
    size: f32,
    cw: usize,
    ch: usize,
    pad: usize,
    glow_r: usize,
) -> (Vec<LightMap>, Vec<LightMap>) {
    let (mw, mh) = (cw + 2 * pad, ch + 2 * pad);
    let scale = PxScale::from(size * 1.25);
    let scaled = font.as_scaled(scale);
    chars
        .iter()
        .map(|&(c, mirror)| {
            let id = font.glyph_id(c);
            let advance = scaled.h_advance(id);
            // whole-pixel position, so stems stay sharp
            let at = ab_glyph::point(((cw as f32 - advance) / 2.0).round(), (ch as f32 * 0.84).round());
            let glyph = id.with_scale_and_position(scale, at);
            let mut core = vec![0f32; mw * mh];
            if let Some(outline) = font.outline_glyph(glyph) {
                let b = outline.px_bounds();
                outline.draw(|x, y, v| {
                    let cx = b.min.x as i32 + x as i32;
                    let cx = if mirror { cw as i32 - 1 - cx } else { cx } + pad as i32;
                    let cy = b.min.y as i32 + y as i32 + pad as i32;
                    if (0..mw as i32).contains(&cx) && (0..mh as i32).contains(&cy) {
                        core[cy as usize * mw + cx as usize] = v.clamp(0.0, 1.0);
                    }
                });
            }
            let plain =
                LightMap::new(cw, ch, (0..ch).flat_map(|y| (0..cw).map(move |x| (y + pad) * mw + x + pad)).map(|i| core[i]));
            let glow = box_blur(&box_blur(&core, mw, mh, glow_r), mw, mh, glow_r);
            let glowing = LightMap::new(mw, mh, core.iter().zip(&glow).map(|(&c, &g)| c + g * GLOW));
            (plain, glowing)
        })
        .unzip()
}

#[derive(Clone, Copy, Default)]
struct Drop {
    /// head position, in rows
    y: f32,
    /// rows per second
    speed: f32,
    /// trail length, in rows
    length: f32,
    brightness: f32,
    /// row the head was last in (a head entering a row gives it a new glyph)
    head: i32,
}

impl Drop {
    fn reset(&mut self, rng: &mut Rng, rows: usize, gap: f32, spread: f32, at_start: bool) {
        let rows = rows as f32;
        // in rows, the same at every depth (like speed), so far trails are shorter on screen
        self.length = rng.range(6.0, 30.0);
        // widely different speeds (5-24 rows/s with the default spread of 2.2, about
        // v1's 6-24); slow ones linger, fast ones streak. Spread evenly on a log scale,
        // so there are as many slow drops as fast ones.
        self.speed = SPEED * spread.powf(rng.range(-1.0, 1.0));
        self.brightness = rng.range(0.55, 1.0);
        // waiting above the screen leaves gaps; larger `gap` = sparser rain
        self.y = if at_start { rng.range(-rows * gap, rows + self.length) } else { -rng.range(0.0, rows * gap) };
        self.head = i32::MIN;
    }
}

#[derive(Clone)]
struct Column {
    glyphs: Vec<u8>, // character index per row
    /// left edge, in pixels
    x: i32,
    /// how far the column is moved up, in pixels (0..cell height), so the gaps
    /// between rows do not line up with the neighbouring columns
    y_offset: i32,
    /// hue at time 0, in degrees (full and rainbow colours), or the index of its
    /// colour in `MONOKAI`
    hue: f32,
    drops: [Drop; DROPS_PER_COLUMN],
}

#[derive(Clone)]
struct Layer {
    /// cell height
    ch: usize,
    /// light maps per character: `plain[c]` is cw×ch; `glowing[c]` is larger, with
    /// the cell at (pad, pad)
    pad: usize,
    plain: Vec<LightMap>,
    glowing: Vec<LightMap>,
    rows: usize,
    brightness: f32,
    /// 1.0 = full colour, 0.0 = grey
    saturation: f32,
    gap: f32,
    columns: Vec<Column>,
}

/// A lit glyph to draw this frame.
#[derive(Clone, Copy)]
struct Lit {
    /// top-left corner of its light map
    x: i32,
    y: i32,
    layer: u8,
    glyph: u8,
    glow: bool,
    /// colour, blue-green-red (the byte order of the scan-out buffer)
    bgr: [u8; 3],
}

#[derive(Clone)]
pub struct Film {
    width: usize,
    height: usize,
    layers: Vec<Layer>,
    /// the first `kana` characters are katakana
    kana: usize,
    rng: Rng,
    /// lit glyphs of this frame, per band they touch
    bands: Vec<Vec<Lit>>,
    /// one band of the frame, 4 bytes per pixel in the scan-out buffer's order (B, G, R, X)
    light: Vec<u8>,
    colors: Colors,
    /// see `Params::speed_spread`
    speed_spread: f32,
    /// seconds since start (the rainbow drifts with it)
    time: f32,
    /// light per row of the column being drawn
    scratch: Vec<f32>,
}

/// Adds `map` at (x, y) into `light` (w×h pixels of B, G, R, X bytes) in colour
/// `bgr`, saturating, clipped.
fn add_glyph(light: &mut [u8], w: usize, h: usize, map: &LightMap, x: i32, y: i32, bgr: [u8; 3]) {
    let [kb, kg, kr] = bgr.map(|c| c as u16 + 1); // 256 = 1.0
    let (y0, y1) = (y.max(0), (y + map.h as i32).min(h as i32));
    for yy in y0..y1 {
        let my = (yy - y) as usize;
        let (a, b) = map.spans[my];
        // the row's non-zero span, clipped to the screen
        let (x0, x1) = ((x + a as i32).max(0), (x + b as i32).min(w as i32));
        if x0 >= x1 {
            continue;
        }
        let src = &map.data[my * map.w + (x0 - x) as usize..my * map.w + (x1 - x) as usize];
        let row = yy as usize * w;
        let dst = &mut light[(row + x0 as usize) * 4..(row + x1 as usize) * 4];
        for (p, &c) in dst.as_chunks_mut::<4>().0.iter_mut().zip(src) {
            let c = c as u16;
            p[0] = p[0].saturating_add(((c * kb) >> 8) as u8);
            p[1] = p[1].saturating_add(((c * kg) >> 8) as u8);
            p[2] = p[2].saturating_add(((c * kr) >> 8) as u8);
        }
    }
}

impl Film {
    /// `size` is the glyph size of the nearest layer; `layers` the number of depth layers.
    pub fn new(font: &FontRef, p: &Params, width: usize, height: usize) -> Self {
        let colors = p.colors;
        let (max, min) = (p.max_size as f32, p.min_size.min(p.max_size) as f32);
        // waits between drops: shorter for higher densities
        let wait = DENSITY_STEP.powf(3.0 - p.density as f32);
        let (chars, kana) = charset(font);
        let mut rng = Rng::seeded();
        let n = p.layers.max(1);
        let layers = (0..n)
            .map(|k| {
                let depth = if n > 1 { k as f32 / (n - 1) as f32 } else { 0.0 }; // 0 near, 1 far
                // sizes shrink geometrically, as with perspective
                let s = max * (min / max).powf(depth);
                let ch = s.round().max(6.0) as usize;
                let cw = (s * COLUMN_WIDTH).round().max(4.0) as usize;
                let glow_r = (s / 14.0).round().max(1.0) as usize;
                let pad = 2 * glow_r + 1;
                let (plain, glowing) = glyph_maps(font, &chars, s, cw, ch, pad, glow_r);
                let rows = height / ch + 3; // one more, as columns are moved up by up to a row
                let gap = (2.2 - 0.6 * depth) * wait; // the near layer is the sparsest
                let x0 = -(rng.f() * cw as f32) as i32;
                let columns = (0..width / cw + 2)
                    .map(|i| {
                        let x = x0 + ((i as f32 + rng.range(-COLUMN_JITTER, COLUMN_JITTER)) * cw as f32).round() as i32;
                        let hue = match colors {
                            // as v1: 10° per v1 column of `size` pixels, by the column's centre
                            Colors::Rainbow => (x as f32 + cw as f32 / 2.0) / max * 10.0,
                            Colors::Monokai => rng.below(MONOKAI.len()) as f32,
                            _ => rng.range(0.0, 360.0),
                        };
                        let mut c = Column {
                            glyphs: (0..rows).map(|_| pick(&mut rng, kana, chars.len())).collect(),
                            x,
                            y_offset: rng.below(ch) as i32,
                            hue,
                            drops: Default::default(),
                        };
                        for d in &mut c.drops {
                            d.reset(&mut rng, rows, gap, p.speed_spread, true);
                        }
                        c
                    })
                    .collect();
                Layer {
                    ch,
                    pad,
                    plain,
                    glowing,
                    rows,
                    brightness: 1.0 - 0.75 * depth,
                    saturation: 1.0 - (1.0 - FAR_SATURATION) * depth,
                    gap,
                    columns,
                }
            })
            .collect::<Vec<Layer>>();
        let max_rows = layers.iter().map(|l| l.rows).max().unwrap_or(0);
        Film {
            width,
            height,
            layers,
            kana,
            rng,
            bands: vec![Vec::new(); height.div_ceil(BAND)],
            light: vec![0; width * BAND * 4],
            colors,
            speed_spread: p.speed_spread.max(1.0),
            time: 0.0,
            scratch: vec![0.0; max_rows],
        }
    }

    /// Advances `dt` seconds and draws the whole frame into `px` (stride = pixels per
    /// row). Returns the number of glyphs drawn.
    pub fn frame(&mut self, px: &mut [u32], stride: usize, dt: f32) -> usize {
        let Film { width: w, height: h, layers, kana, rng, bands, light, colors, speed_spread, time, scratch } = self;
        *time += dt;
        let (w, h) = (*w, *h);
        bands.iter_mut().for_each(Vec::clear);
        let mut drawn = 0;
        for (li, layer) in layers.iter_mut().enumerate() {
            let rows = layer.rows;
            let nchars = layer.plain.len();
            let kana = *kana;
            for col in layer.columns.iter_mut() {
                // glyphs change now and then
                if rng.f() < 0.3 {
                    col.glyphs[rng.below(rows)] = pick(rng, kana, nchars);
                }
                let light_of = &mut scratch[..rows];
                light_of.fill(0.0);
                for d in &mut col.drops {
                    d.y += d.speed * dt;
                    let head = d.y.floor() as i32;
                    if head != d.head {
                        if (0..rows as i32).contains(&head) {
                            col.glyphs[head as usize] = pick(rng, kana, nchars);
                        }
                        d.head = head;
                    }
                    let len = d.length.max(1.0);
                    for i in 0..len as i32 {
                        let r = head - i;
                        if r < 0 {
                            break;
                        }
                        if r as usize >= rows {
                            continue;
                        }
                        // heads are always bright; the trail fades linearly, as in v1, dimmed by the drop's brightness
                        let l = if i == 0 { HEAD_LIGHT } else { (1.0 - (i - 1) as f32 / len) * d.brightness };
                        let v = &mut light_of[r as usize];
                        *v = v.max(l);
                    }
                    if head as f32 - len >= rows as f32 {
                        d.reset(rng, rows, layer.gap, *speed_spread, false);
                    }
                }
                // trail and head colours of this column, 0-255, faded towards grey with depth
                let fade = |c: [f32; 3]| {
                    let grey = 0.3 * c[0] + 0.59 * c[1] + 0.11 * c[2];
                    c.map(|v| grey + (v - grey) * layer.saturation)
                };
                let (color, white) = match colors {
                    Colors::Green => ([0.12 * 255.0, 255.0, 0.38 * 255.0], [216.0, 255.0, 224.0]),
                    Colors::Monokai => (MONOKAI[col.hue as usize], MONOKAI_FOREGROUND),
                    Colors::Full | Colors::Rainbow => {
                        // as in v1: the hue drifts by ms / 15 degrees
                        let (r, g, b) = hsl_to_rgb((col.hue + *time * 1000.0 / 15.0).rem_euclid(360.0), 0.8, 0.65);
                        ([r, g, b], [220.0, 255.0, 255.0])
                    }
                };
                let (color, white) = (fade(color), fade(white));
                let x = col.x;
                for (r, &l) in light_of.iter().enumerate() {
                    let glow = l > 1.0; // only heads are brighter than 1
                    let (base, l) = if glow { (white, (l * layer.brightness).min(1.0)) } else { (color, l * layer.brightness) };
                    if l * 255.0 < MIN_LIGHT {
                        continue;
                    }
                    let bgr = [2, 1, 0].map(|i| (base[i] * l).min(255.0) as u8);
                    let glyph = col.glyphs[r];
                    let (pad, map_h) = if glow { (layer.pad as i32, layer.glowing[0].h) } else { (0, layer.ch) };
                    let (x, y) = (x - pad, (r * layer.ch) as i32 - col.y_offset - pad);
                    let item = Lit { x, y, layer: li as u8, glyph, glow, bgr };
                    // the bands it touches
                    let first = y.max(0) as usize / BAND;
                    let last = ((y + map_h as i32 - 1).max(0) as usize / BAND).min(bands.len() - 1);
                    for band in bands.iter_mut().take(last + 1).skip(first) {
                        band.push(item);
                    }
                    drawn += 1;
                }
            }
        }
        let bg = background().to_le_bytes(); // B, G, R, X
        for (b, items) in bands.iter().enumerate() {
            let top = b * BAND;
            let bh = BAND.min(h - top);
            let light = &mut light[..w * bh * 4];
            light.as_chunks_mut::<4>().0.fill(bg);
            for it in items {
                let layer = &layers[it.layer as usize];
                let map = if it.glow { &layer.glowing[it.glyph as usize] } else { &layer.plain[it.glyph as usize] };
                add_glyph(light, w, bh, map, it.x, it.y - top as i32, it.bgr);
            }
            // copy to the scan-out buffer, one sequential row at a time
            for y in 0..bh {
                let src = &light[y * w * 4..(y + 1) * w * 4];
                let dst = &mut px[(top + y) * stride..(top + y) * stride + w];
                for (p, s) in dst.iter_mut().zip(src.as_chunks::<4>().0) {
                    *p = u32::from_le_bytes(*s);
                }
            }
        }
        drawn
    }
}
