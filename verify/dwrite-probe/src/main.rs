//! Draws glyph runs through `IDWriteBitmapRenderTarget::DrawGlyphRun` and
//! reports what came back: the `blackBoxRect` DirectWrite filled in, and where
//! the ink actually landed in the target bitmap.
//!
//! Run it twice — once as-is for plain DirectWrite, once with the core's path
//! as the first argument so the process loads it and the detour is in play:
//!
//! ```text
//! cargo run --release
//! cargo run --release -- "C:\Program Files\Font-tuner\RenderCore64.dll"
//! ```
//!
//! The numbers are the comparison; keep the tray stopped so only the DLL named
//! here is in the process (AGENTS.md: verify with the tray off). A second
//! argument writes each case's bitmap as `<dir>/<case>.bmp` for a visual check.

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteBitmapRenderTarget, IDWriteFactory, IDWriteFontFace, IDWriteFontFile,
    IDWriteGdiInterop, IDWriteRenderingParams, DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_SIMULATIONS,
    DWRITE_FONT_SIMULATIONS_BOLD, DWRITE_FONT_SIMULATIONS_NONE, DWRITE_FONT_SIMULATIONS_OBLIQUE,
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_GLYPH_OFFSET,
    DWRITE_GLYPH_RUN, DWRITE_MATRIX, DWRITE_MEASURING_MODE, DWRITE_MEASURING_MODE_GDI_CLASSIC,
    DWRITE_MEASURING_MODE_NATURAL, IDWriteFactory3, IDWriteGlyphRunAnalysis, DWRITE_GRID_FIT_MODE_DEFAULT,
    DWRITE_RENDERING_MODE1_NATURAL_SYMMETRIC, DWRITE_RENDERING_MODE_NATURAL_SYMMETRIC, DWRITE_TEXTURE_ALIASED_1x1,
    DWRITE_TEXTURE_CLEARTYPE_3x1, DWRITE_TEXTURE_TYPE, DWRITE_TEXT_ANTIALIAS_MODE_CLEARTYPE,
    DWRITE_TEXT_ANTIALIAS_MODE_GRAYSCALE,
};
use windows::core::Interface;
use windows::Win32::Graphics::Gdi::{GetPixel, HDC};
use windows::Win32::System::LibraryLoader::LoadLibraryW;

const W: i32 = 480;
const H: i32 = 200;
const EM: f32 = 24.0;
/// Baseline origin in DIPs, away from the edges so nothing clips.
const BASELINE: (f32, f32) = (40.0, 60.0);
const IDENTITY: DWRITE_MATRIX = DWRITE_MATRIX { m11: 1.0, m12: 0.0, m21: 0.0, m22: 1.0, dx: 0.0, dy: 0.0 };

fn main() -> windows::core::Result<()> {
    let mut args = std::env::args().skip(1);
    if let Some(dll) = args.next().filter(|a| !a.is_empty()) {
        let wide: Vec<u16> = dll.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: a NUL-terminated path; loading is the point of the probe.
        let h = unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) };
        println!("loaded {dll} -> {h:?}");
        // The core hooks on a background thread off the loader lock; give it a
        // moment before drawing anything.
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }
    let dump = args.next();

    // SAFETY: every call below is a documented DirectWrite/GDI entry point with
    // arguments that live for the call; the probe is single-threaded.
    unsafe {
        let factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let face = system_face(&factory, w!("Yu Gothic UI"))?;
        let interop: IDWriteGdiInterop = factory.GetGdiInterop()?;
        let target: IDWriteBitmapRenderTarget = interop.CreateBitmapRenderTarget(None, W as u32, H as u32)?;
        let hdc = target.GetMemoryDC();
        // DirectWrite rejects the call with E_INVALIDARG when this is null.
        let params: IDWriteRenderingParams = factory.CreateRenderingParams()?;

        for case in cases() {
            clear(hdc);
            let face = if case.sim == DWRITE_FONT_SIMULATIONS_NONE { face.clone() } else { simulated(&factory, &face, case.sim)? };
            let glyphs = glyph_indices(&face, case.text)?;
            let advances: Vec<f32> = case.advances.map_or_else(Vec::new, |a| vec![a; glyphs.len()]);
            let offsets: Vec<DWRITE_GLYPH_OFFSET> = case
                .offset
                .map(|(ax, asc)| vec![DWRITE_GLYPH_OFFSET { advanceOffset: ax, ascenderOffset: asc }; glyphs.len()])
                .unwrap_or_default();
            let run = DWRITE_GLYPH_RUN {
                fontFace: std::mem::ManuallyDrop::new(Some(face.clone())),
                fontEmSize: case.em,
                glyphCount: glyphs.len() as u32,
                glyphIndices: glyphs.as_ptr(),
                glyphAdvances: if advances.is_empty() { std::ptr::null() } else { advances.as_ptr() },
                glyphOffsets: if offsets.is_empty() { std::ptr::null() } else { offsets.as_ptr() },
                isSideways: case.sideways.into(),
                bidiLevel: case.bidi,
            };
            target.SetPixelsPerDip(case.ppd)?;
            target.SetCurrentTransform(Some(&raw const case.transform))?;

            // Pre-fill with a value DirectWrite must overwrite, so "untouched"
            // is visible rather than looking like an empty rect.
            let mut bbox = RECT { left: -1, top: -1, right: -1, bottom: -1 };
            let hr = target.DrawGlyphRun(
                BASELINE.0,
                BASELINE.1,
                case.mode,
                &run,
                &params,
                COLORREF(0),
                Some(&raw mut bbox),
            );
            let ink = ink_bounds(hdc);
            println!(
                "{:<20} {} blackBox=({},{})-({},{})  ink={}",
                case.name,
                if hr.is_ok() { "ok " } else { "ERR" },
                bbox.left,
                bbox.top,
                bbox.right,
                bbox.bottom,
                ink
            );
            if let Some(dir) = &dump {
                save_bmp(hdc, &format!("{dir}\\{}.bmp", case.name.replace(' ', "_")));
            }
        }

        // The glyph-run analysis path: bounds per texture type, and where the
        // coverage in the texture actually is.
        println!("--- analysis (f1: pixelsPerDip argument; f3: transform only, ClearType / greyscale)");
        let f3: Option<IDWriteFactory3> = factory.cast().ok();
        for case in cases() {
            let face = if case.sim == DWRITE_FONT_SIMULATIONS_NONE { face.clone() } else { simulated(&factory, &face, case.sim)? };
            let glyphs = glyph_indices(&face, case.text)?;
            let advances: Vec<f32> = case.advances.map_or_else(Vec::new, |a| vec![a; glyphs.len()]);
            let offsets: Vec<DWRITE_GLYPH_OFFSET> = case
                .offset
                .map(|(ax, asc)| vec![DWRITE_GLYPH_OFFSET { advanceOffset: ax, ascenderOffset: asc }; glyphs.len()])
                .unwrap_or_default();
            let run = DWRITE_GLYPH_RUN {
                fontFace: std::mem::ManuallyDrop::new(Some(face.clone())),
                fontEmSize: case.em,
                glyphCount: glyphs.len() as u32,
                glyphIndices: glyphs.as_ptr(),
                glyphAdvances: if advances.is_empty() { std::ptr::null() } else { advances.as_ptr() },
                glyphOffsets: if offsets.is_empty() { std::ptr::null() } else { offsets.as_ptr() },
                isSideways: case.sideways.into(),
                bidiLevel: case.bidi,
            };
            let a1 = factory.CreateGlyphRunAnalysis(
                &run,
                case.ppd,
                Some(&raw const case.transform),
                DWRITE_RENDERING_MODE_NATURAL_SYMMETRIC,
                case.mode,
                BASELINE.0,
                BASELINE.1,
            );
            print_analysis(&format!("{} f1", case.name), a1);
            if let Some(f3) = &f3 {
                // Factory 3 takes no pixelsPerDip: fold it into the transform.
                let t = &case.transform;
                let p = case.ppd;
                let m = DWRITE_MATRIX { m11: t.m11 * p, m12: t.m12 * p, m21: t.m21 * p, m22: t.m22 * p, dx: t.dx * p, dy: t.dy * p };
                for (tag, aa) in [("f3 ct", DWRITE_TEXT_ANTIALIAS_MODE_CLEARTYPE), ("f3 grey", DWRITE_TEXT_ANTIALIAS_MODE_GRAYSCALE)] {
                    let a3 = f3.CreateGlyphRunAnalysis(
                        &run,
                        Some(&raw const m),
                        DWRITE_RENDERING_MODE1_NATURAL_SYMMETRIC,
                        case.mode,
                        DWRITE_GRID_FIT_MODE_DEFAULT,
                        aa,
                        BASELINE.0,
                        BASELINE.1,
                    );
                    print_analysis(&format!("{} {tag}", case.name), a3);
                }
            }
        }
    }
    Ok(())
}

/// Bounds of each texture type, and the extent of coverage above 60 in it.
unsafe fn print_analysis(name: &str, a: windows::core::Result<IDWriteGlyphRunAnalysis>) {
    let a = match a {
        Ok(a) => a,
        Err(e) => {
            println!("{name:<28} create failed {e:?}");
            return;
        }
    };
    let mut line = format!("{name:<28}");
    for (tag, ty, bpp) in [("1x1", DWRITE_TEXTURE_ALIASED_1x1, 1usize), ("3x1", DWRITE_TEXTURE_CLEARTYPE_3x1, 3)] {
        let ty: DWRITE_TEXTURE_TYPE = ty;
        // SAFETY: a live analysis; the buffer is sized from the bounds.
        let r = unsafe { a.GetAlphaTextureBounds(ty) }.unwrap_or_default();
        let (w, h) = ((r.right - r.left).max(0) as usize, (r.bottom - r.top).max(0) as usize);
        if w == 0 || h == 0 {
            line += &format!(" {tag}=empty");
            continue;
        }
        let mut buf = vec![0u8; w * h * bpp];
        // SAFETY: as above.
        let ok = unsafe { a.CreateAlphaTexture(ty, &raw const r, &mut buf) }.is_ok();
        let spread = if bpp == 3 {
            buf.chunks(3).map(|c| c.iter().max().unwrap() - c.iter().min().unwrap()).max().unwrap_or(0)
        } else {
            0
        };
        let (mut x0, mut x1, mut y0, mut y1) = (i32::MAX, i32::MIN, i32::MAX, i32::MIN);
        for y in 0..h {
            for x in 0..w {
                if buf[(y * w + x) * bpp..(y * w + x + 1) * bpp].iter().any(|&v| v > 60) {
                    x0 = x0.min(x as i32 + r.left);
                    x1 = x1.max(x as i32 + r.left);
                    y0 = y0.min(y as i32 + r.top);
                    y1 = y1.max(y as i32 + r.top);
                }
            }
        }
        let err = if ok { "" } else { " ERR" };
        line += &format!(" {tag}=({},{})-({},{}){err} cov=x{x0}..{x1} y{y0}..{y1} spread={spread}", r.left, r.top, r.right, r.bottom);
    }
    println!("{line}");
}

struct Case {
    name: &'static str,
    text: &'static str,
    em: f32,
    /// Uniform advance per glyph, or `None` to pass a null `glyphAdvances`.
    advances: Option<f32>,
    /// Uniform `(advanceOffset, ascenderOffset)`, or `None` for none.
    offset: Option<(f32, f32)>,
    bidi: u32,
    sideways: bool,
    ppd: f32,
    transform: DWRITE_MATRIX,
    mode: DWRITE_MEASURING_MODE,
    sim: DWRITE_FONT_SIMULATIONS,
}

fn cases() -> Vec<Case> {
    let base = Case {
        name: "",
        text: "AVATo",
        em: EM,
        advances: Some(EM * 0.5),
        offset: None,
        bidi: 0,
        sideways: false,
        ppd: 1.0,
        transform: IDENTITY,
        mode: DWRITE_MEASURING_MODE_NATURAL,
        sim: DWRITE_FONT_SIMULATIONS_NONE,
    };
    let rot = |deg: f32| {
        let (s, c) = deg.to_radians().sin_cos();
        DWRITE_MATRIX { m11: c, m12: s, m21: -s, m22: c, dx: 0.0, dy: 0.0 }
    };
    vec![
        Case { name: "advances 12dip", ..base },
        Case { name: "advances 20dip", advances: Some(EM * 0.85), ..base },
        Case { name: "advances null", advances: None, ..base },
        Case { name: "gdi-classic null", advances: None, mode: DWRITE_MEASURING_MODE_GDI_CLASSIC, ..base },
        Case { name: "offset +8 up", offset: Some((0.0, 8.0)), ..base },
        Case { name: "offset +5 along", offset: Some((5.0, 0.0)), ..base },
        Case { name: "bidi 1 (RTL)", bidi: 1, ..base },
        Case { name: "RTL + offset along", bidi: 1, offset: Some((5.0, 0.0)), ..base },
        Case { name: "sideways", sideways: true, ..base },
        Case { name: "sideways null adv", sideways: true, advances: None, ..base },
        Case { name: "ppd 1.5", ppd: 1.5, ..base },
        Case { name: "scale 2", transform: DWRITE_MATRIX { m11: 2.0, m22: 2.0, ..IDENTITY }, ..base },
        Case { name: "translate 10,5", transform: DWRITE_MATRIX { dx: 10.0, dy: 5.0, ..IDENTITY }, ..base },
        Case { name: "ppd 1.5 + translate", ppd: 1.5, transform: DWRITE_MATRIX { dx: 10.0, dy: 5.0, ..IDENTITY }, ..base },
        Case { name: "rotate 30", transform: rot(30.0), ..base },
        Case { name: "em 13.5", em: 13.5, advances: None, ..base },
        Case { name: "japanese", text: "日本語の文字", advances: None, ..base },
        Case { name: "spaces only", text: "   ", ..base },
        Case { name: "sim bold", sim: DWRITE_FONT_SIMULATIONS_BOLD, advances: None, ..base },
        Case { name: "sim oblique", sim: DWRITE_FONT_SIMULATIONS_OBLIQUE, advances: None, ..base },
        Case { name: "l60 none", text: "l", em: 60.0, advances: None, ..base },
        Case { name: "l60 bold", text: "l", em: 60.0, advances: None, sim: DWRITE_FONT_SIMULATIONS_BOLD, ..base },
        Case { name: "l60 oblique", text: "l", em: 60.0, advances: None, sim: DWRITE_FONT_SIMULATIONS_OBLIQUE, ..base },
        Case { name: "l120 none", text: "l", em: 120.0, advances: None, transform: DWRITE_MATRIX { dy: 80.0, ..IDENTITY }, ..base },
        Case { name: "l120 bold", text: "l", em: 120.0, advances: None, transform: DWRITE_MATRIX { dy: 80.0, ..IDENTITY }, sim: DWRITE_FONT_SIMULATIONS_BOLD, ..base },
        Case { name: "l120 oblique", text: "l", em: 120.0, advances: None, transform: DWRITE_MATRIX { dy: 80.0, ..IDENTITY }, sim: DWRITE_FONT_SIMULATIONS_OBLIQUE, ..base },
    ]
}

/// The first font face of `family` in the system collection.
unsafe fn system_face(factory: &IDWriteFactory, family: PCWSTR) -> windows::core::Result<IDWriteFontFace> {
    unsafe {
        let collection = {
            let mut c = None;
            factory.GetSystemFontCollection(&mut c, false)?;
            c.expect("system font collection")
        };
        let mut index = 0u32;
        let mut exists = windows::core::BOOL(0);
        collection.FindFamilyName(family, &mut index, &mut exists)?;
        assert!(exists.as_bool(), "font family not installed");
        let fam = collection.GetFontFamily(index)?;
        let font = fam.GetFirstMatchingFont(DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL)?;
        font.CreateFontFace()
    }
}

/// `face` again, with DirectWrite's synthetic bold / oblique applied.
unsafe fn simulated(factory: &IDWriteFactory, face: &IDWriteFontFace, sim: DWRITE_FONT_SIMULATIONS) -> windows::core::Result<IDWriteFontFace> {
    unsafe {
        let mut n = 0u32;
        face.GetFiles(&mut n, None)?;
        let mut files: Vec<Option<IDWriteFontFile>> = vec![None; n as usize];
        face.GetFiles(&mut n, Some(files.as_mut_ptr()))?;
        factory.CreateFontFace(face.GetType(), &files, face.GetIndex(), sim)
    }
}

unsafe fn glyph_indices(face: &IDWriteFontFace, text: &str) -> windows::core::Result<Vec<u16>> {
    let codes: Vec<u32> = text.chars().map(|c| c as u32).collect();
    let mut out = vec![0u16; codes.len()];
    // SAFETY: both slices are ours and of the same length.
    unsafe { face.GetGlyphIndices(codes.as_ptr(), codes.len() as u32, out.as_mut_ptr())? };
    Ok(out)
}

/// Paint the target white so ink is whatever is darker.
unsafe fn clear(hdc: HDC) {
    use windows::Win32::Graphics::Gdi::PatBlt;
    const WHITENESS: windows::Win32::Graphics::Gdi::ROP_CODE = windows::Win32::Graphics::Gdi::ROP_CODE(0x00FF_0062);
    // SAFETY: `hdc` is the target's own memory DC.
    let _ = unsafe { PatBlt(hdc, 0, 0, W, H, WHITENESS) };
}

/// Bounding box of everything darker than white, as `x=a..b y=c..d`.
unsafe fn ink_bounds(hdc: HDC) -> String {
    let (mut x0, mut x1, mut y0, mut y1) = (i32::MAX, i32::MIN, i32::MAX, i32::MIN);
    for y in 0..H {
        for x in 0..W {
            // SAFETY: reading inside the bitmap selected into `hdc`.
            let c = unsafe { GetPixel(hdc, x, y) }.0;
            let sum = (c & 0xFF) + ((c >> 8) & 0xFF) + ((c >> 16) & 0xFF);
            if sum < 600 {
                x0 = x0.min(x);
                x1 = x1.max(x);
                y0 = y0.min(y);
                y1 = y1.max(y);
            }
        }
    }
    if x1 < x0 {
        return "(none)".into();
    }
    format!("x={x0}..{x1} (w={}) y={y0}..{y1} (h={})", x1 - x0 + 1, y1 - y0 + 1)
}

/// Write the target's pixels as a 24-bit bottom-up BMP.
unsafe fn save_bmp(hdc: HDC, path: &str) {
    let row = ((W * 3 + 3) & !3) as usize;
    let mut data = vec![0u8; row * H as usize];
    for y in 0..H {
        for x in 0..W {
            // SAFETY: reading inside the bitmap selected into `hdc`.
            let c = unsafe { GetPixel(hdc, x, y) }.0;
            let o = (H - 1 - y) as usize * row + x as usize * 3;
            data[o] = ((c >> 16) & 0xFF) as u8;
            data[o + 1] = ((c >> 8) & 0xFF) as u8;
            data[o + 2] = (c & 0xFF) as u8;
        }
    }
    let mut f = Vec::with_capacity(54 + data.len());
    f.extend_from_slice(b"BM");
    f.extend_from_slice(&((54 + data.len()) as u32).to_le_bytes());
    f.extend_from_slice(&[0; 4]);
    f.extend_from_slice(&54u32.to_le_bytes());
    f.extend_from_slice(&40u32.to_le_bytes());
    f.extend_from_slice(&W.to_le_bytes());
    f.extend_from_slice(&H.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&24u16.to_le_bytes());
    f.extend_from_slice(&[0; 24]);
    f.extend_from_slice(&data);
    let _ = std::fs::write(path, f);
}
