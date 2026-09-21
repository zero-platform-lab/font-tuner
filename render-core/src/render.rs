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

/// Render `text` at `px` pixels onto a fresh canvas using `profile`.
/// `pen` is the baseline origin (x, y); `bg` is the background colour.
pub fn render_text(ft: &Ft, tables: &Tables, profile: &Profile, ink: Ink, bg: [u8; 3],
                   text: &str, px: i32, pen: (i32, i32), size: (usize, usize)) -> Canvas {
    ft.prepare(profile);
    let mut canvas = Canvas::filled(size.0, size.1, bg);
    let (mut pen_x, base_y) = pen;
    let lcd = profile.aa.is_lcd();
    let bgr = ft::is_bgr(profile.aa);

    for ch in text.chars() {
        let g = match ft.render(ch, px, profile) {
            Some(g) => g,
            None => continue,
        };
        if g.rows > 0 && !g.buffer.is_empty() {
            if lcd && g.pixel_mode == PIXEL_MODE_LCD {
                blend_lcd(&mut canvas, tables, ink, &g, pen_x, base_y, bgr);
            } else if !lcd && g.pixel_mode == PIXEL_MODE_GRAY {
                blend_gray(&mut canvas, tables, ink, &g, pen_x, base_y);
            }
        }
        pen_x += g.advance_px;
    }
    canvas
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
