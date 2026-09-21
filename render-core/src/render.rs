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
use crate::ft::{self, Ft, PIXEL_MODE_GRAY, PIXEL_MODE_LCD};

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

/// Render `text` at `px` pixels onto a fresh `bg`-filled canvas using `profile`.
/// `pen` is the baseline origin (x, y).
pub fn render_text(ft: &Ft, tables: &Tables, profile: &Profile, ink: Ink, bg: [u8; 3],
                   text: &str, px: i32, pen: (i32, i32), size: (usize, usize)) -> Canvas {
    let mut canvas = Canvas::filled(size.0, size.1, bg);
    draw_text_onto(&mut canvas, ft, tables, profile, ink, text, px, pen, None);
    canvas
}

/// Like [`render_text`] but the input is a run of font glyph indices, as an
/// `ETO_GLYPH_INDEX` draw supplies.
pub fn render_glyphs(ft: &Ft, tables: &Tables, profile: &Profile, ink: Ink, bg: [u8; 3],
                     glyphs: &[u16], px: i32, pen: (i32, i32), size: (usize, usize)) -> Canvas {
    let mut canvas = Canvas::filled(size.0, size.1, bg);
    draw_glyphs_onto(&mut canvas, ft, tables, profile, ink, glyphs, px, pen, None);
    canvas
}

/// Composite `text` over the *existing* pixels of `canvas` (transparent draw).
/// `pen` is the baseline origin. `dx`, if given, overrides each glyph's advance
/// (as ExtTextOutW's lpDx does), one entry per character.
pub fn draw_text_onto(canvas: &mut Canvas, ft: &Ft, tables: &Tables, profile: &Profile,
                      ink: Ink, text: &str, px: i32, pen: (i32, i32), dx: Option<&[i32]>) {
    ft.prepare(profile);
    let (mut pen_x, base_y) = pen;
    let (lcd, bgr) = (profile.aa.is_lcd(), ft::is_bgr(profile.aa));
    for (i, ch) in text.chars().enumerate() {
        if let Some(g) = ft.render(ch, px, profile) {
            blit_glyph(canvas, &Blit { tables, ink, pen_x, base_y, lcd, bgr }, &g);
            pen_x += advance_of(dx, i, g.advance_px);
        }
    }
}

/// Composite a run of glyph indices over the existing pixels of `canvas`.
pub fn draw_glyphs_onto(canvas: &mut Canvas, ft: &Ft, tables: &Tables, profile: &Profile,
                        ink: Ink, glyphs: &[u16], px: i32, pen: (i32, i32), dx: Option<&[i32]>) {
    ft.prepare(profile);
    let (mut pen_x, base_y) = pen;
    let (lcd, bgr) = (profile.aa.is_lcd(), ft::is_bgr(profile.aa));
    for (i, &gi) in glyphs.iter().enumerate() {
        if let Some(g) = ft.render_glyph(gi, px, profile) {
            blit_glyph(canvas, &Blit { tables, ink, pen_x, base_y, lcd, bgr }, &g);
            pen_x += advance_of(dx, i, g.advance_px);
        }
    }
}

#[inline]
fn advance_of(dx: Option<&[i32]>, i: usize, default: i32) -> i32 {
    dx.and_then(|d| d.get(i)).copied().unwrap_or(default)
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
        draw_text_onto(&mut small, &ft, &tg, &grey, Ink::default(), "Wg", 20, (-4, 3), None);

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
                       Some(&[14, 14, 14, 14])); // dx path (advance_of Some)

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
        draw_text_onto(&mut c2, &ft, &tg, &grey, Ink::default(), " ", 20, (2, 18), None);
        draw_text_onto(&mut c2, &ft, &ts, &sharp, Ink::default(), " ", 20, (2, 18), None);

        // Tiny clip in the middle of a big glyph: pixels fall on every side of
        // the clip, so both the y>=t and y<b rejects fire.
        let mut vclip = Canvas::filled(80, 40, [255, 255, 255]);
        vclip.set_clip(Some((30, 18, 36, 22)));
        draw_text_onto(&mut vclip, &ft, &ts, &sharp, Ink::default(), "Ag", 30, (4, 34), None);

        // Zero-size render: FreeType produces no bitmap, so emit's error/empty
        // return fires (r != 0 or rows == 0) and blit_glyph's rows==0 early-out
        // and coverage_lcd's non-LCD/empty guard are all exercised.
        let _ = ft.render('A', 0, &grey);
        let mut z = Canvas::filled(20, 20, [255, 255, 255]);
        draw_text_onto(&mut z, &ft, &tg, &grey, Ink::default(), "A", 0, (2, 10), None);
        draw_glyphs_onto(&mut z, &ft, &ts, &sharp, Ink::default(), &[3, 4], 0, (2, 10), None);
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
