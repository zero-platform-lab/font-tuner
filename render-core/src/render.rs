//! Compositing: lay out a string and blend each glyph's coverage into an RGB
//! canvas with the profile's tables. Greyscale and LCD paths mirror
//! `FreeTypeDrawBitmapGray` / `FreeTypeDrawBitmapPixelModeLCD` (ft.cpp).
//!
//! The `as` casts here are pixel coordinates and buffer indices, each
//! bounds-checked before use (`in_bounds`, or an explicit `0 <= v < limit`),
//! and image dimensions that fit u32; hence the scoped cast allows.
//!
//! `too_many_arguments` and `many_single_char_names` are allowed too: the
//! public render entry points take the font, tables, profile, ink, background,
//! text, size, pen and dx as distinct parameters, and the compositing loops
//! use the graphics-conventional `x/y/w/h/l/t/r/b` names.
#![allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::too_many_arguments,
    clippy::many_single_char_names
)]

use crate::config::Profile;
use crate::filter::Tables;
use std::sync::Arc;

use crate::ft::{self, Ft, GlyphStyle, PIXEL_MODE_GRAY, PIXEL_MODE_LCD};

/// An RGB pixel buffer.
pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub rgb: Vec<u8>, // w*h*3
    clip: Option<(i32, i32, i32, i32)>, // (left, top, right, bottom), exclusive r/b
}

impl Canvas {
    pub fn new(w: usize, h: usize) -> Canvas {
        Canvas::filled(w, h, [255, 255, 255])
    }
    /// A canvas filled with a solid background colour.
    pub fn filled(w: usize, h: usize, bg: [u8; 3]) -> Canvas {
        let mut rgb = vec![0u8; w * h * 3];
        let (chunks, _) = rgb.as_chunks_mut::<3>();
        for px in chunks {
            *px = bg;
        }
        Canvas { w, h, rgb, clip: None }
    }
    /// Restrict subsequent drawing to `rect` (left, top, right, bottom); `None`
    /// clears the clip.
    pub fn set_clip(&mut self, rect: Option<(i32, i32, i32, i32)>) {
        self.clip = rect;
    }
    /// Fill a rectangle with a solid colour (for ETO_OPAQUE background).
    pub fn fill_rect(&mut self, rect: (i32, i32, i32, i32), rgb: [u8; 3]) {
        let (l, t, r, b) = rect;
        for y in t.max(0)..b.min(self.h as i32) {
            for x in l.max(0)..r.min(self.w as i32) {
                let idx = (y as usize * self.w + x as usize) * 3;
                self.rgb[idx..idx + 3].copy_from_slice(&rgb);
            }
        }
    }
    #[inline]
    fn in_bounds(&self, x: i32, y: i32) -> bool {
        if x < 0 || (x as usize) >= self.w || y < 0 || (y as usize) >= self.h {
            return false;
        }
        match self.clip {
            Some((l, t, r, b)) => x >= l && x < r && y >= t && y < b,
            None => true,
        }
    }
    /// Build a canvas from a 32bpp top-down BGRA buffer (e.g. a DIB section).
    pub fn from_bgra_topdown(w: usize, h: usize, bgra: &[u8]) -> Canvas {
        let mut rgb = vec![0u8; w * h * 3];
        for i in 0..w * h {
            rgb[i * 3] = bgra[i * 4 + 2]; // R
            rgb[i * 3 + 1] = bgra[i * 4 + 1]; // G
            rgb[i * 3 + 2] = bgra[i * 4]; // B
        }
        Canvas { w, h, rgb, clip: None }
    }

    /// Write this canvas back into a 32bpp top-down BGRA buffer (alpha=255).
    pub fn blit_to_bgra_topdown(&self, bgra: &mut [u8]) {
        for i in 0..self.w * self.h {
            bgra[i * 4] = self.rgb[i * 3 + 2]; // B
            bgra[i * 4 + 1] = self.rgb[i * 3 + 1]; // G
            bgra[i * 4 + 2] = self.rgb[i * 3]; // R
            bgra[i * 4 + 3] = 255; // A
        }
    }

    /// Save as PNG.
    pub fn save(&self, path: &str) -> image::ImageResult<()> {
        let img = image::RgbImage::from_raw(self.w as u32, self.h as u32, self.rgb.clone())
            .expect("canvas size mismatch");
        img.save(path)
    }
}

/// Text colour, black by default. Only the greyscale/LCD-black case is fully
/// exercised by the verification; arbitrary colours use the same per-channel
/// blend.
#[derive(Clone, Copy, Default)]
pub struct Ink {
    pub fg: [u8; 3],
}

/// How the caller's device overrides the run's layout.
///
/// `dx` is ExtTextOutW's `lpDx` (one advance per character, replacing the
/// font's own). With `pdy` it holds `(dx, dy)` pairs instead - `ETO_PDY`,
/// which is how a vertical run is laid out - and a positive `dy` moves the pen
/// *up*, as GDI does (measured) and as upstream writes it (`ft.cpp`:
/// `FTInfo.y -= clpdx.gety(0)`).
///
/// `extra` is `SetTextCharacterExtra`, added to every horizontal advance on
/// top of `dx` (measured against GDI: with lpDx 20 and extra 10 the fifth
/// character starts at 4x30; upstream does `FTInfo.x += charExtra`).
#[derive(Clone, Copy, Default)]
pub struct Layout<'a> {
    pub dx: Option<&'a [i32]>,
    pub extra: i32,
    /// `dx` holds `(dx, dy)` pairs rather than plain advances.
    pub pdy: bool,
}

impl<'a> Layout<'a> {
    /// Just an `lpDx` array of plain advances.
    pub fn from_dx(dx: Option<&'a [i32]>) -> Layout<'a> {
        Layout { dx, extra: 0, pdy: false }
    }

    /// Entries per character in `dx`.
    #[inline]
    fn stride(&self) -> usize {
        if self.pdy { 2 } else { 1 }
    }

    /// The pen movement after the `i`-th glyph, whose own advance is
    /// `default`. `y` is positive downwards, so a positive `lpDx` `dy` (which
    /// moves up) comes back negated.
    #[inline]
    fn advance(&self, i: usize, default: i32) -> (i32, i32) {
        let at = i * self.stride();
        let x = self.dx.and_then(|d| d.get(at)).copied().unwrap_or(default) + self.extra;
        let y = if self.pdy { -self.dx.and_then(|d| d.get(at + 1)).copied().unwrap_or(0) } else { 0 };
        (x, y)
    }

    /// Width of a `count`-glyph run laid out with an explicit `dx`, or `None`
    /// when there is none (the caller then measures it another way).
    pub fn dx_width(&self, count: usize) -> Option<i32> {
        let dx = self.dx?;
        let (stride, mut sum, mut n) = (self.stride(), 0, 0);
        for i in 0..count {
            let Some(&step) = dx.get(i * stride) else { break };
            sum += step;
            n += 1;
        }
        Some(sum + self.extra * n)
    }

    /// How far the run travels vertically, for sizing the canvas. Positive
    /// means downwards. `None` without an `ETO_PDY` array.
    pub fn dy_travel(&self, count: usize) -> Option<(i32, i32)> {
        if !self.pdy {
            return None;
        }
        let dx = self.dx?;
        let (mut at, mut lo, mut hi) = (0, 0, 0);
        for i in 0..count {
            let Some(&step) = dx.get(i * 2 + 1) else { break };
            at -= step;
            lo = lo.min(at);
            hi = hi.max(at);
        }
        Some((lo, hi))
    }
}

/// Render `text` at `px` pixels onto a fresh `bg`-filled canvas using `profile`.
/// `pen` is the baseline origin (x, y).
pub fn render_text(ft: &Ft, tables: &Tables, profile: &Profile, ink: Ink, bg: [u8; 3],
                   text: &str, px: i32, pen: (i32, i32), size: (usize, usize)) -> Canvas {
    let mut canvas = Canvas::filled(size.0, size.1, bg);
    draw_text_onto(&mut canvas, ft, tables, profile, ink, text, px, pen, Layout::default());
    canvas
}

/// Like [`render_text`] but the input is a run of font glyph indices, as an
/// `ETO_GLYPH_INDEX` draw supplies.
pub fn render_glyphs(ft: &Ft, tables: &Tables, profile: &Profile, ink: Ink, bg: [u8; 3],
                     glyphs: &[u16], px: i32, pen: (i32, i32), size: (usize, usize)) -> Canvas {
    let mut canvas = Canvas::filled(size.0, size.1, bg);
    draw_glyphs_onto(&mut canvas, ft, tables, profile, ink, glyphs, px, pen, Layout::default());
    canvas
}

/// Composite `text` over the *existing* pixels of `canvas` (transparent draw).
/// `pen` is the baseline origin; `layout` carries the device's `lpDx` and
/// inter-character spacing.
pub fn draw_text_onto(canvas: &mut Canvas, ft: &Ft, tables: &Tables, profile: &Profile,
                      ink: Ink, text: &str, px: i32, pen: (i32, i32), layout: Layout<'_>) {
    ft.prepare(profile);
    let (mut pen_x, mut base_y) = pen;
    let (lcd, bgr) = (profile.aa.is_lcd(), ft::is_bgr(profile.aa));
    for (i, ch) in text.chars().enumerate() {
        if let Some(g) = ft.render(ch, px, profile) {
            blit_glyph(canvas, &Blit { tables, ink, pen_x, base_y, lcd, bgr }, &g);
            let (ax, ay) = layout.advance(i, g.advance_px);
            pen_x += ax;
            base_y += ay;
        }
    }
}

/// Composite a run of glyph indices over the existing pixels of `canvas`.
pub fn draw_glyphs_onto(canvas: &mut Canvas, ft: &Ft, tables: &Tables, profile: &Profile,
                        ink: Ink, glyphs: &[u16], px: i32, pen: (i32, i32), layout: Layout<'_>) {
    ft.prepare(profile);
    let (mut pen_x, mut base_y) = pen;
    let (lcd, bgr) = (profile.aa.is_lcd(), ft::is_bgr(profile.aa));
    for (i, &gi) in glyphs.iter().enumerate() {
        if let Some(g) = ft.render_glyph(gi, px, profile) {
            blit_glyph(canvas, &Blit { tables, ink, pen_x, base_y, lcd, bgr }, &g);
            let (ax, ay) = layout.advance(i, g.advance_px);
            pen_x += ax;
            base_y += ay;
        }
    }
}

/// Raw LCD subpixel coverage (3 bytes/px, **no blend**) for a glyph-index run,
/// for the DirectWrite CLEARTYPE_3x1 alpha-texture path (IDWriteGlyphRunAnalysis::
/// CreateAlphaTexture): the caller composites it itself, so we supply only the
/// FreeType-hinted coverage. `pen` is the baseline in buffer coords. Returns a
/// `w*h*3` buffer (0 = no coverage). `profile.aa` should be an LCD mode.
pub fn glyph_run_coverage_lcd(ft: &Ft, profile: &Profile, glyphs: &[u16], px: i32,
                              pen: (i32, i32), w: usize, h: usize) -> Vec<u8> {
    ft.prepare(profile);
    let mut cov = vec![0u8; w * h * 3];
    let (mut pen_x, base_y) = pen;
    for &gi in glyphs {
        if let Some(g) = ft.render_glyph(gi, px, profile) {
            if g.pixel_mode == PIXEL_MODE_LCD && g.rows > 0 && !g.buffer.is_empty() {
                let cells = g.width / 3;
                for row in 0..g.rows {
                    let bi = (row * g.pitch) as usize;
                    for cell in 0..cells {
                        let i = bi + (cell * 3) as usize;
                        let x = pen_x + g.left + cell;
                        let y = base_y - g.top + row;
                        if x >= 0 && (x as usize) < w && y >= 0 && (y as usize) < h {
                            let o = (y as usize * w + x as usize) * 3;
                            for c in 0..3 {
                                cov[o + c] = cov[o + c].max(g.buffer[i + c]);
                            }
                        }
                    }
                }
            }
            pen_x += g.advance_px;
        }
    }
    cov
}

/// A glyph index at a device-pixel origin (where its baseline starts).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placed {
    pub gi: u16,
    pub x: i32,
    pub y: i32,
}

/// A pixel rectangle `(left, top, right, bottom)`, right and bottom exclusive.
pub type Rect = (i32, i32, i32, i32);

fn union(a: Option<Rect>, b: Rect) -> Rect {
    match a {
        None => b,
        Some(a) => (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)),
    }
}

/// Where glyph `g` placed at `at` covers, in the same pixel space as `at`.
/// LCD bitmaps are three bytes per pixel, so their pixel width is a third.
fn glyph_rect(g: &ft::Glyph, at: &Placed) -> Option<Rect> {
    if g.rows == 0 || g.buffer.is_empty() {
        return None;
    }
    let w = if g.pixel_mode == PIXEL_MODE_LCD { g.width / 3 } else { g.width };
    let (l, t) = (at.x + g.left, at.y - g.top);
    Some((l, t, l + w, t + g.rows))
}

/// One rasterised glyph of a run and where it goes.
struct PlacedGlyph {
    at: Placed,
    g: Arc<ft::Glyph>,
}

/// A run rasterised once: its glyph bitmaps (shared with `Ft`'s cache) and
/// the union of where they land.
pub struct RenderedRun {
    glyphs: Vec<PlacedGlyph>,
    /// Ink bounds in the glyphs' pixel space; `None` when nothing has ink.
    pub bounds: Option<Rect>,
}

/// Rasterise `glyphs` with `profile` in `style`.
pub fn render_placed(ft: &Ft, profile: &Profile, glyphs: &[Placed], style: &GlyphStyle) -> RenderedRun {
    ft.prepare(profile);
    let mut out = RenderedRun { glyphs: Vec::with_capacity(glyphs.len()), bounds: None };
    for at in glyphs {
        let Some(g) = ft.render_glyph_styled(at.gi, style, profile) else { continue };
        let Some(r) = glyph_rect(&g, at) else { continue };
        out.bounds = Some(union(out.bounds, r));
        out.glyphs.push(PlacedGlyph { at: *at, g });
    }
    out
}

impl RenderedRun {
    /// Composite the run over `canvas`, whose top-left pixel sits at `origin`
    /// in the glyphs' pixel space. `profile` must be the one it was rendered
    /// with (it picks the greyscale or LCD blend and the subpixel order).
    pub fn draw_onto(&self, canvas: &mut Canvas, origin: (i32, i32), tables: &Tables, profile: &Profile, ink: Ink) {
        let (lcd, bgr) = (profile.aa.is_lcd(), ft::is_bgr(profile.aa));
        for PlacedGlyph { at, g } in &self.glyphs {
            let blit = Blit { tables, ink, pen_x: at.x - origin.0, base_y: at.y - origin.1, lcd, bgr };
            blit_glyph(canvas, &blit, g);
        }
    }

    /// Raw coverage inside `rect` (the glyphs' pixel space), **no blend**,
    /// for a caller that composites it itself (DirectWrite's
    /// `CreateAlphaTexture`). `channels` is 3 for a ClearType 3x1 texture
    /// (R, G, B per pixel in panel order; a greyscale glyph fills all three)
    /// or 1 for an aliased 1x1 texture (LCD glyphs are averaged). `bgr` is
    /// the profile's subpixel order. Overlapping glyphs keep the max.
    pub fn coverage(&self, rect: Rect, channels: usize, bgr: bool) -> Vec<u8> {
        let (w, h) = ((rect.2 - rect.0).max(0) as usize, (rect.3 - rect.1).max(0) as usize);
        let mut cov = vec![0u8; w * h * channels];
        for PlacedGlyph { at, g } in &self.glyphs {
            let lcd = g.pixel_mode == PIXEL_MODE_LCD;
            let gw = if lcd { g.width / 3 } else { g.width };
            let (gl, gt) = (at.x + g.left, at.y - g.top);
            for row in 0..g.rows {
                let y = gt + row - rect.1;
                if y < 0 || y as usize >= h {
                    continue;
                }
                for col in 0..gw {
                    let x = gl + col - rect.0;
                    if x < 0 || x as usize >= w {
                        continue;
                    }
                    let src = (row * g.pitch) as usize + if lcd { (col * 3) as usize } else { col as usize };
                    let px = if lcd {
                        let (a, b, c) = (g.buffer[src], g.buffer[src + 1], g.buffer[src + 2]);
                        if bgr { [c, b, a] } else { [a, b, c] }
                    } else {
                        [g.buffer[src]; 3]
                    };
                    let o = (y as usize * w + x as usize) * channels;
                    if channels == 3 {
                        for k in 0..3 {
                            cov[o + k] = cov[o + k].max(px[k]);
                        }
                    } else {
                        let v = if lcd { ((u16::from(px[0]) + u16::from(px[1]) + u16::from(px[2])) / 3) as u8 } else { px[0] };
                        cov[o] = cov[o].max(v);
                    }
                }
            }
        }
        cov
    }
}

/// Where and how to composite a glyph: the profile's tables, the ink, the pen
/// baseline, and the LCD subpixel choice. Bundled so the blit helpers take one
/// context instead of eight positional arguments.
struct Blit<'a> {
    tables: &'a Tables,
    ink: Ink,
    pen_x: i32,
    base_y: i32,
    lcd: bool,
    bgr: bool,
}

/// Blit one rendered glyph onto the canvas at the pen position.
fn blit_glyph(canvas: &mut Canvas, blit: &Blit, glyph: &ft::Glyph) {
    if glyph.rows == 0 || glyph.buffer.is_empty() {
        return;
    }
    if blit.lcd && glyph.pixel_mode == PIXEL_MODE_LCD {
        blend_lcd(canvas, blit, glyph);
    } else if !blit.lcd && glyph.pixel_mode == PIXEL_MODE_GRAY {
        blend_gray(canvas, blit, glyph);
    }
}

fn blend_gray(canvas: &mut Canvas, blit: &Blit, glyph: &ft::Glyph) {
    for row in 0..glyph.rows {
        for col in 0..glyph.width {
            let cov = glyph.buffer[(row * glyph.pitch + col) as usize];
            if cov == 0 {
                continue;
            }
            let x = blit.pen_x + glyph.left + col;
            let y = blit.base_y - glyph.top + row;
            if !canvas.in_bounds(x, y) {
                continue;
            }
            let idx = (y as usize * canvas.w + x as usize) * 3;
            for k in 0..3 {
                canvas.rgb[idx + k] = blit.tables.blend(canvas.rgb[idx + k], blit.ink.fg[k], cov);
            }
        }
    }
}

fn blend_lcd(canvas: &mut Canvas, blit: &Blit, glyph: &ft::Glyph) {
    let cells = glyph.width / 3;
    for row in 0..glyph.rows {
        let base = (row * glyph.pitch) as usize;
        for cell in 0..cells {
            let i = base + (cell * 3) as usize;
            // ft.cpp FreeTypeDrawBitmapPixelModeLCD: assign local alphaR/G/B by
            // subpixel order, then doAB(bg, alphaB, alphaG, alphaR) maps
            // arg1->R, arg2->G, arg3->B.
            let (a_r, a_g, a_b) = if blit.bgr {
                (glyph.buffer[i + 2], glyph.buffer[i + 1], glyph.buffer[i])
            } else {
                (glyph.buffer[i], glyph.buffer[i + 1], glyph.buffer[i + 2])
            };
            if a_r == 0 && a_g == 0 && a_b == 0 {
                continue;
            }
            let x = blit.pen_x + glyph.left + cell;
            let y = blit.base_y - glyph.top + row;
            if !canvas.in_bounds(x, y) {
                continue;
            }
            let idx = (y as usize * canvas.w + x as usize) * 3;
            canvas.rgb[idx] = blit.tables.blend(canvas.rgb[idx], blit.ink.fg[0], a_b); // R
            canvas.rgb[idx + 1] = blit.tables.blend(canvas.rgb[idx + 1], blit.ink.fg[1], a_g); // G
            canvas.rgb[idx + 2] = blit.tables.blend(canvas.rgb[idx + 2], blit.ink.fg[2], a_r); // B
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Aa;
    use crate::filter::Tables;

    /// `SetTextCharacterExtra` adds to every advance, on top of an explicit
    /// `lpDx`. Measured against GDI: lpDx 20 with extra 10 puts the fifth
    /// character at 4x30 from the origin.
    #[test]
    fn layout_adds_character_extra_on_top_of_dx() {
        let dx = [20, 20, 20, 20, 20];
        let plain = Layout::from_dx(Some(&dx));
        assert_eq!(plain.advance(0, 99), (20, 0), "lpDx overrides the font advance");
        assert_eq!(plain.dx_width(5), Some(100));

        let spaced = Layout { dx: Some(&dx), extra: 10, pdy: false };
        assert_eq!(spaced.advance(0, 99), (30, 0));
        assert_eq!(spaced.dx_width(5), Some(150));

        // No lpDx: the font's own advance, still widened by the spacing.
        let no_dx = Layout { dx: None, extra: 6, pdy: false };
        assert_eq!(no_dx.advance(3, 12), (18, 0));
        assert_eq!(no_dx.dx_width(5), None, "the caller measures it instead");

        // A short lpDx falls back to the glyph's advance past its end.
        let short = Layout { dx: Some(&dx[..2]), extra: 0, pdy: false };
        assert_eq!(short.advance(5, 7), (7, 0));
        assert_eq!(short.dx_width(5), Some(40), "only the entries that exist");
    }

    /// `ETO_PDY`: the array is (dx, dy) pairs, and a positive dy moves the
    /// pen up, so it comes back negated (measured against GDI; upstream
    /// writes `FTInfo.y -= gety()`). Reading such an array as plain advances
    /// takes every other dy as an x advance - which drew a vertical run
    /// horizontally with the characters overlapping in pairs.
    #[test]
    fn pdy_pairs_move_the_pen_up_the_page() {
        let pairs = [0, 24, 0, 24, 0, 24];
        let vertical = Layout { dx: Some(&pairs), extra: 0, pdy: true };
        assert_eq!(vertical.advance(0, 99), (0, -24));
        assert_eq!(vertical.advance(2, 99), (0, -24));
        assert_eq!(vertical.dx_width(3), Some(0), "no horizontal travel");
        assert_eq!(vertical.dy_travel(3), Some((-72, 0)), "three steps upwards");

        // The same array without the flag is what the port used to see.
        let flat = Layout::from_dx(Some(&pairs));
        assert_eq!(flat.advance(1, 99), (24, 0));
        assert_eq!(flat.dy_travel(3), None);
    }

    #[test]
    fn canvas_buffer_ops_are_pure() {
        // new / filled / fill_rect (in- and out-of-bounds clamp) / clip toggles.
        let mut c = Canvas::new(4, 3);
        assert_eq!(c.rgb, vec![255u8; 4 * 3 * 3]);
        let mut c2 = Canvas::filled(4, 3, [1, 2, 3]);
        assert_eq!(&c2.rgb[0..3], &[1, 2, 3]);
        c2.fill_rect((-2, -2, 100, 100), [9, 9, 9]); // clamps to the canvas
        assert!(c2.rgb.iter().all(|&v| v == 9));
        c.set_clip(Some((1, 1, 3, 3)));
        assert!(c.in_bounds(1, 1));
        assert!(!c.in_bounds(0, 0)); // outside clip
        assert!(!c.in_bounds(3, 3)); // clip is exclusive on r/b
        c.set_clip(None);
        assert!(c.in_bounds(0, 0));
        assert!(!c.in_bounds(-1, 0)); // x < 0
        assert!(!c.in_bounds(4, 0)); // x >= w
        assert!(!c.in_bounds(0, -1)); // y < 0
        assert!(!c.in_bounds(0, 3)); // y >= h

        // BGRA round-trip: from_bgra_topdown then blit_to_bgra_topdown is identity.
        let bgra: Vec<u8> = (0..4 * 3 * 4).map(|i| i as u8).collect();
        let cv = Canvas::from_bgra_topdown(4, 3, &bgra);
        let mut out = vec![0u8; 4 * 3 * 4];
        cv.blit_to_bgra_topdown(&mut out);
        for i in 0..4 * 3 {
            assert_eq!(out[i * 4], bgra[i * 4]); // B
            assert_eq!(out[i * 4 + 1], bgra[i * 4 + 1]); // G
            assert_eq!(out[i * 4 + 2], bgra[i * 4 + 2]); // R
            assert_eq!(out[i * 4 + 3], 255); // A forced opaque
        }
    }

    #[test]
    fn is_bgr_matches_lcd_order() {
        assert!(ft::is_bgr(Aa::LcdBgr));
        assert!(ft::is_bgr(Aa::LightLcdBgr));
        assert!(!ft::is_bgr(Aa::LcdRgb));
        assert!(!ft::is_bgr(Aa::Grey));
    }

    // A single FreeType-backed test: one process-global `Ft`, run sequentially,
    // exercising every hinting/AA/LCD-order/clip/empty-glyph/dx branch in ft.rs
    // and render.rs. Skips cleanly if the system font is unavailable.
    #[test]
    fn freetype_render_paths() {
        const FONT: &str = r"C:\Windows\Fonts\meiryo.ttc";
        let Ok(bytes) = std::fs::read(FONT) else { eprintln!("skip: no {FONT}"); return; };
        let Ok(ft) = Ft::open(FONT, 0) else { eprintln!("skip: FT open failed"); return; };

        // reface: memory-by-index Ok, memory-by-family Ok, and the Err arm.
        assert!(ft.reface_memory_index(&bytes, 0).is_ok());
        let _ = ft.reface_memory(&bytes, "Meiryo"); // family match may vary; just drive it
        assert!(ft.reface_memory(&[0u8, 1, 2, 3], "Nope").is_err()); // reface_memory Err arm
        assert!(ft.reface_memory_index(&[0u8, 1, 2, 3], 9).is_err());
        assert!(ft.reface_memory_index(&bytes, 0).is_ok()); // restore a valid face

        // Greyscale path: hinting 0 (native), draw_text_onto + blend_gray, and
        // an out-of-bounds pen so in_bounds' reject branch fires.
        let grey = Profile::clean_greyscale(); // hinting 0, Grey
        let tg = Tables::build(grey.gamma, grey.weight, grey.contrast, grey.gamma_mode);
        let cv = render_text(&ft, &tg, &grey, Ink::default(), [255, 255, 255],
                             "Ag.", 20, (2, 18), (60, 24));
        assert!(cv.rgb.iter().any(|&v| v < 255), "greyscale drew ink");
        // draw partly off-canvas, with a negative pen, so in_bounds rejects
        // pixels on every side (x<0, x>=w, y<0, y>=h).
        let mut small = Canvas::filled(6, 6, [255, 255, 255]);
        draw_text_onto(&mut small, &ft, &tg, &grey, Ink::default(), "Wg", 20, (-4, 3), Layout::default());

        // hinting 1 (no hinting) via clean_sharp, and hinting 2 (autohint) via
        // accurate — both LCD; RGB order then forced BGR to hit blend_lcd's
        // bgr branch. Also drives prepare()'s LCD-filter branch.
        for (p, label) in [(Profile::clean_sharp(), "sharp/h1"),
                           (Profile::accurate(), "accurate/h2")] {
            let t = Tables::build(p.gamma, p.weight, p.contrast, p.gamma_mode);
            let cv = render_text(&ft, &t, &p, Ink { fg: [10, 20, 30] }, [250, 250, 250],
                                 "Ag水0", p.gamma as i32 + 18, (3, 20), (120, 28));
            assert!(cv.rgb.iter().any(|&v| v != 250), "{label} drew ink");
            // same run, BGR subpixel order
            let bgr = Profile { aa: Aa::LcdBgr, ..p };
            let _ = render_text(&ft, &t, &bgr, Ink::default(), [255, 255, 255],
                                "Ag", 22, (2, 20), (80, 28));
        }

        // Clip-restricted transparent draw (in_bounds Some(clip) branch).
        let sharp = Profile::clean_sharp();
        let ts = Tables::build(sharp.gamma, sharp.weight, sharp.contrast, sharp.gamma_mode);
        let mut clipped = Canvas::filled(120, 28, [255, 255, 255]);
        clipped.set_clip(Some((10, 4, 60, 24)));
        draw_text_onto(&mut clipped, &ft, &ts, &sharp, Ink::default(), "Clip", 20, (2, 20),
                       Layout::from_dx(Some(&[14, 14, 14, 14]))); // lpDx path

        // Glyph-index paths: render_glyphs, draw_glyphs_onto, and the raw LCD
        // coverage used by the CreateAlphaTexture hook. Indices include .notdef
        // (0) and a spread that yields both drawn and empty glyphs.
        let glyphs: Vec<u16> = vec![0, 1, 2, 3, 4, 5, 36, 68];
        let cvg = render_glyphs(&ft, &ts, &sharp, Ink::default(), [255, 255, 255],
                                &glyphs, 22, (4, 20), (160, 28));
        assert_eq!(cvg.w, 160);
        // coverage with a negative pen so the bounds test rejects x<0 / y<0 too.
        let cov = glyph_run_coverage_lcd(&ft, &sharp, &glyphs, 22, (-6, 4), 40, 28);
        assert_eq!(cov.len(), 40 * 28 * 3);

        // empty glyph (space) exercises blit_glyph's rows==0 early return, in both
        // the greyscale and LCD dispatch arms.
        let mut c2 = Canvas::filled(40, 24, [255, 255, 255]);
        draw_text_onto(&mut c2, &ft, &tg, &grey, Ink::default(), " ", 20, (2, 18), Layout::default());
        draw_text_onto(&mut c2, &ft, &ts, &sharp, Ink::default(), " ", 20, (2, 18), Layout::default());

        // Tiny clip in the middle of a big glyph: pixels fall on every side of
        // the clip, so both the y>=t and y<b rejects fire.
        let mut vclip = Canvas::filled(80, 40, [255, 255, 255]);
        vclip.set_clip(Some((30, 18, 36, 22)));
        draw_text_onto(&mut vclip, &ft, &ts, &sharp, Ink::default(), "Ag", 30, (4, 34), Layout::default());

        // Zero-size render: FreeType produces no bitmap, so emit's error/empty
        // return fires (r != 0 or rows == 0) and blit_glyph's rows==0 early-out
        // and coverage_lcd's non-LCD/empty guard are all exercised.
        let _ = ft.render('A', 0, &grey);
        let mut z = Canvas::filled(20, 20, [255, 255, 255]);
        draw_text_onto(&mut z, &ft, &tg, &grey, Ink::default(), "A", 0, (2, 10), Layout::default());
        draw_glyphs_onto(&mut z, &ft, &ts, &sharp, Ink::default(), &[3, 4], 0, (2, 10), Layout::default());
        let empty_cov = glyph_run_coverage_lcd(&ft, &sharp, &[3, 4], 0, (2, 10), 20, 20);
        assert_eq!(empty_cov.len(), 20 * 20 * 3);

        // Coverage with a pen that pushes glyphs past the bottom edge (y >= h).
        let _ = glyph_run_coverage_lcd(&ft, &sharp, &glyphs, 22, (2, 40), 60, 20);

        // Canvas::save round-trips to disk then is removed.
        let png = std::env::temp_dir().join(format!("rc-cov-{}.png", std::process::id()));
        cv_save_ok(&cvg, &png);
        std::fs::remove_file(&png).ok();

        // Ft::open error arm: shim_open on a missing path returns non-zero.
        assert!(Ft::open(r"C:\__no_such_font__.ttf", 0).is_err());
    }

    fn cv_save_ok(c: &Canvas, path: &std::path::Path) {
        c.save(path.to_str().unwrap()).expect("save png");
    }
}
