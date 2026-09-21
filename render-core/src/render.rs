//! Compositing: lay out a string and blend each glyph's coverage into an RGB
//! canvas with the profile's tables. Greyscale and LCD paths mirror
//! `FreeTypeDrawBitmapGray` / `FreeTypeDrawBitmapPixelModeLCD` (ft.cpp).

use crate::config::Profile;
use crate::filter::Tables;
use crate::ft::{self, Ft, PIXEL_MODE_GRAY, PIXEL_MODE_LCD};

/// An RGB pixel buffer.
pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub rgb: Vec<u8>, // w*h*3
}

impl Canvas {
    pub fn new(w: usize, h: usize) -> Canvas {
        Canvas::filled(w, h, [255, 255, 255])
    }
    /// A canvas filled with a solid background colour.
    pub fn filled(w: usize, h: usize, bg: [u8; 3]) -> Canvas {
        let mut rgb = vec![0u8; w * h * 3];
        for px in rgb.chunks_exact_mut(3) {
            px.copy_from_slice(&bg);
        }
        Canvas { w, h, rgb }
    }
    #[inline]
    fn in_bounds(&self, x: i32, y: i32) -> bool {
        x >= 0 && (x as usize) < self.w && y >= 0 && (y as usize) < self.h
    }
    /// Build a canvas from a 32bpp top-down BGRA buffer (e.g. a DIB section).
    pub fn from_bgra_topdown(w: usize, h: usize, bgra: &[u8]) -> Canvas {
        let mut rgb = vec![0u8; w * h * 3];
        for i in 0..w * h {
            rgb[i * 3] = bgra[i * 4 + 2]; // R
            rgb[i * 3 + 1] = bgra[i * 4 + 1]; // G
            rgb[i * 3 + 2] = bgra[i * 4]; // B
        }
        Canvas { w, h, rgb }
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
#[derive(Clone, Copy)]
pub struct Ink {
    pub fg: [u8; 3],
}
impl Default for Ink {
    fn default() -> Ink { Ink { fg: [0, 0, 0] } }
}

/// Render `text` at `px` pixels onto a fresh `bg`-filled canvas using `profile`.
/// `pen` is the baseline origin (x, y).
pub fn render_text(ft: &Ft, tables: &Tables, profile: &Profile, ink: Ink, bg: [u8; 3],
                   text: &str, px: i32, pen: (i32, i32), size: (usize, usize)) -> Canvas {
    let mut canvas = Canvas::filled(size.0, size.1, bg);
    draw_text_onto(&mut canvas, ft, tables, profile, ink, text, px, pen);
    canvas
}

/// Like [`render_text`] but the input is a run of font glyph indices, as an
/// `ETO_GLYPH_INDEX` draw supplies.
pub fn render_glyphs(ft: &Ft, tables: &Tables, profile: &Profile, ink: Ink, bg: [u8; 3],
                     glyphs: &[u16], px: i32, pen: (i32, i32), size: (usize, usize)) -> Canvas {
    let mut canvas = Canvas::filled(size.0, size.1, bg);
    draw_glyphs_onto(&mut canvas, ft, tables, profile, ink, glyphs, px, pen);
    canvas
}

/// Composite `text` over the *existing* pixels of `canvas` (transparent draw).
/// `pen` is the baseline origin.
pub fn draw_text_onto(canvas: &mut Canvas, ft: &Ft, tables: &Tables, profile: &Profile,
                      ink: Ink, text: &str, px: i32, pen: (i32, i32)) {
    ft.prepare(profile);
    let (mut pen_x, base_y) = pen;
    let (lcd, bgr) = (profile.aa.is_lcd(), ft::is_bgr(profile.aa));
    for ch in text.chars() {
        if let Some(g) = ft.render(ch, px, profile) {
            blit_glyph(canvas, tables, ink, &g, pen_x, base_y, lcd, bgr);
            pen_x += g.advance_px;
        }
    }
}

/// Composite a run of glyph indices over the existing pixels of `canvas`.
pub fn draw_glyphs_onto(canvas: &mut Canvas, ft: &Ft, tables: &Tables, profile: &Profile,
                        ink: Ink, glyphs: &[u16], px: i32, pen: (i32, i32)) {
    ft.prepare(profile);
    let (mut pen_x, base_y) = pen;
    let (lcd, bgr) = (profile.aa.is_lcd(), ft::is_bgr(profile.aa));
    for &gi in glyphs {
        if let Some(g) = ft.render_glyph(gi, px, profile) {
            blit_glyph(canvas, tables, ink, &g, pen_x, base_y, lcd, bgr);
            pen_x += g.advance_px;
        }
    }
}

/// Blit one rendered glyph onto the canvas at the pen position.
fn blit_glyph(canvas: &mut Canvas, tables: &Tables, ink: Ink, g: &ft::Glyph,
              pen_x: i32, base_y: i32, lcd: bool, bgr: bool) {
    if g.rows == 0 || g.buffer.is_empty() {
        return;
    }
    if lcd && g.pixel_mode == PIXEL_MODE_LCD {
        blend_lcd(canvas, tables, ink, g, pen_x, base_y, bgr);
    } else if !lcd && g.pixel_mode == PIXEL_MODE_GRAY {
        blend_gray(canvas, tables, ink, g, pen_x, base_y);
    }
}

fn blend_gray(c: &mut Canvas, t: &Tables, ink: Ink, g: &ft::Glyph, pen_x: i32, base_y: i32) {
    for row in 0..g.rows {
        for col in 0..g.width {
            let cov = g.buffer[(row * g.pitch + col) as usize];
            if cov == 0 { continue; }
            let x = pen_x + g.left + col;
            let y = base_y - g.top + row;
            if !c.in_bounds(x, y) { continue; }
            let idx = (y as usize * c.w + x as usize) * 3;
            for k in 0..3 {
                c.rgb[idx + k] = t.blend(c.rgb[idx + k], ink.fg[k], cov);
            }
        }
    }
}

fn blend_lcd(c: &mut Canvas, t: &Tables, ink: Ink, g: &ft::Glyph, pen_x: i32, base_y: i32, bgr: bool) {
    let cells = g.width / 3;
    for row in 0..g.rows {
        let base = (row * g.pitch) as usize;
        for cell in 0..cells {
            let i = base + (cell * 3) as usize;
            // ft.cpp FreeTypeDrawBitmapPixelModeLCD: assign local alphaR/G/B by
            // subpixel order, then doAB(bg, alphaB, alphaG, alphaR) maps
            // arg1->R, arg2->G, arg3->B.
            let (a_r, a_g, a_b) = if bgr {
                (g.buffer[i + 2], g.buffer[i + 1], g.buffer[i])
            } else {
                (g.buffer[i], g.buffer[i + 1], g.buffer[i + 2])
            };
            if a_r == 0 && a_g == 0 && a_b == 0 { continue; }
            let x = pen_x + g.left + cell;
            let y = base_y - g.top + row;
            if !c.in_bounds(x, y) { continue; }
            let idx = (y as usize * c.w + x as usize) * 3;
            c.rgb[idx]     = t.blend(c.rgb[idx],     ink.fg[0], a_b); // R
            c.rgb[idx + 1] = t.blend(c.rgb[idx + 1], ink.fg[1], a_g); // G
            c.rgb[idx + 2] = t.blend(c.rgb[idx + 2], ink.fg[2], a_r); // B
        }
    }
}
