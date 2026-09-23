//! Draws text through an `ID2D1DCRenderTarget` bound to a memory bitmap —
//! the setup Scintilla's DirectWrite-DC mode uses — and reports where the ink
//! landed, its darkest colour and how dark it is on average.
//!
//! Run it twice — once as-is for plain Direct2D, once with the core's path as
//! the first argument so the process loads it and the detours are in play:
//!
//! ```text
//! cargo run --release
//! cargo run --release -- "C:\Program Files\Font-tuner\RenderCore64.dll"
//! ```
//!
//! Keep the tray stopped (AGENTS.md: verify with the tray off). A second
//! argument writes each case's bitmap as `<dir>/<case>.bmp`.

use core::ffi::c_void;

use windows::core::{w, Interface, PCWSTR};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_GRADIENT_STOP, D2D1_PIXEL_FORMAT, D2D_RECT_F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory, ID2D1RenderTarget, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
    D2D1_DRAW_TEXT_OPTIONS, D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
    D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_EXTEND_MODE_CLAMP, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_GAMMA_2_2,
    D2D1_LAYER_OPTIONS_NONE, D2D1_LAYER_PARAMETERS, D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES,
    D2D1_RENDER_TARGET_PROPERTIES,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteFontFace, IDWriteTextFormat, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_GLYPH_OFFSET,
    DWRITE_GLYPH_RUN, DWRITE_MEASURING_MODE, DWRITE_MEASURING_MODE_GDI_CLASSIC, DWRITE_MEASURING_MODE_NATURAL,
    DWRITE_READING_DIRECTION_RIGHT_TO_LEFT, DWRITE_TEXT_RANGE,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, SelectObject, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, HDC,
};
use windows::Win32::System::LibraryLoader::LoadLibraryW;
use windows_numerics::{Matrix3x2, Vector2};

const W: i32 = 480;
const H: i32 = 200;
const EM: f32 = 24.0;
const BASELINE: Vector2 = Vector2 { X: 40.0, Y: 60.0 };

struct Ctx {
    rt: ID2D1RenderTarget,
    dw: IDWriteFactory,
    face: IDWriteFontFace,
    format: IDWriteTextFormat,
}

type Draw = fn(&Ctx) -> windows::core::Result<()>;

fn main() -> windows::core::Result<()> {
    let mut args = std::env::args().skip(1);
    if let Some(dll) = args.next().filter(|a| !a.is_empty()) {
        let wide: Vec<u16> = dll.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: a NUL-terminated path; loading is the point of the probe.
        let h = unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) };
        println!("loaded {dll} -> {h:?}");
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }
    let dump = args.next();

    // SAFETY: documented Direct2D / DirectWrite / GDI calls with arguments
    // that live for each call; the probe is single-threaded.
    unsafe {
        let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let face = system_face(&dw, w!("Yu Gothic UI"))?;
        let format = dw.CreateTextFormat(
            w!("Yu Gothic UI"),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            EM,
            w!("ja-jp"),
        )?;

        let hdc = CreateCompatibleDC(None);
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: W,
                biHeight: -H,
                biPlanes: 1,
                biBitCount: 32,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(Some(hdc), &raw const bmi, DIB_RGB_COLORS, &raw mut bits, None, 0)?;
        SelectObject(hdc, dib.into());
        let pixels = std::slice::from_raw_parts_mut(bits.cast::<u8>(), (W * H * 4) as usize);

        let props = D2D1_RENDER_TARGET_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_IGNORE },
            ..Default::default()
        };
        let dcrt: ID2D1DCRenderTarget = factory.CreateDCRenderTarget(&raw const props)?;
        dcrt.BindDC(hdc, &RECT { left: 0, top: 0, right: W, bottom: H })?;
        let rt: ID2D1RenderTarget = dcrt.cast()?;
        let ctx = Ctx { rt, dw, face, format };

        for (name, draw) in cases() {
            ctx.rt.BeginDraw();
            ctx.rt.SetDpi(96.0, 96.0);
            ctx.rt.SetTransform(&Matrix3x2::identity());
            ctx.rt.Clear(Some(&D2D1_COLOR_F { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }));
            let t0 = std::time::Instant::now();
            let r = draw(&ctx);
            let e = ctx.rt.EndDraw(None, None);
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            println!("{:<22} {} {} {ms:.1}ms", name, if r.is_ok() && e.is_ok() { "ok " } else { "ERR" }, ink(pixels));
            if let Some(dir) = &dump {
                save_bmp(pixels, &format!("{dir}\\{}.bmp", name.replace(' ', "_")));
            }
        }
        let _ = hdc;
    }
    Ok(())
}

fn cases() -> Vec<(&'static str, Draw)> {
    vec![
        ("glyphs adv null", |c| glyphs(c, "AVATo", None, None, 0, false, DWRITE_MEASURING_MODE_NATURAL)),
        ("glyphs adv 12", |c| glyphs(c, "AVATo", Some(12.0), None, 0, false, DWRITE_MEASURING_MODE_NATURAL)),
        ("glyphs gdi-classic", |c| glyphs(c, "AVATo", None, None, 0, false, DWRITE_MEASURING_MODE_GDI_CLASSIC)),
        ("glyphs offset", |c| glyphs(c, "AVATo", Some(12.0), Some((5.0, 8.0)), 0, false, DWRITE_MEASURING_MODE_NATURAL)),
        ("glyphs RTL", |c| glyphs(c, "AVATo", Some(12.0), None, 1, false, DWRITE_MEASURING_MODE_NATURAL)),
        ("glyphs sideways", |c| glyphs(c, "AVATo", Some(12.0), None, 0, true, DWRITE_MEASURING_MODE_NATURAL)),
        ("glyphs japanese", |c| glyphs(c, "日本語の文字", None, None, 0, false, DWRITE_MEASURING_MODE_NATURAL)),
        ("dpi 144", |c| {
            // SAFETY: a setter on a live target.
            unsafe { c.rt.SetDpi(144.0, 144.0) };
            glyphs(c, "AVATo", None, None, 0, false, DWRITE_MEASURING_MODE_NATURAL)
        }),
        ("translate 10,5", |c| {
            transform(c, Matrix3x2::translation(10.0, 5.0));
            glyphs(c, "AVATo", None, None, 0, false, DWRITE_MEASURING_MODE_NATURAL)
        }),
        ("scale 2", |c| {
            transform(c, Matrix3x2 { M11: 2.0, M22: 2.0, ..Matrix3x2::identity() });
            glyphs(c, "AVATo", None, None, 0, false, DWRITE_MEASURING_MODE_NATURAL)
        }),
        ("rotate 30", |c| {
            transform(c, Matrix3x2::rotation(30.0));
            glyphs(c, "AVATo", None, None, 0, false, DWRITE_MEASURING_MODE_NATURAL)
        }),
        ("red brush", |c| glyphs_with(c, &solid(c, 1.0, 0.0, 0.0, 1.0)?)),
        ("half alpha brush", |c| glyphs_with(c, &solid(c, 0.0, 0.0, 0.0, 0.5)?)),
        ("gradient brush", |c| {
            // SAFETY: brush creation on a live target.
            let brush: windows::Win32::Graphics::Direct2D::ID2D1Brush = unsafe {
                let stops = [
                    D2D1_GRADIENT_STOP { position: 0.0, color: D2D1_COLOR_F { r: 1.0, g: 0.0, b: 0.0, a: 1.0 } },
                    D2D1_GRADIENT_STOP { position: 1.0, color: D2D1_COLOR_F { r: 0.0, g: 0.0, b: 1.0, a: 1.0 } },
                ];
                let coll = c.rt.CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP)?;
                let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                    startPoint: Vector2 { X: 40.0, Y: 0.0 },
                    endPoint: Vector2 { X: 110.0, Y: 0.0 },
                };
                c.rt.CreateLinearGradientBrush(&raw const props, None, &coll)?.cast()?
            };
            glyphs_with(c, &brush)
        }),
        ("clip half", |c| {
            // SAFETY: a push/pop pair on a live target.
            unsafe {
                c.rt.PushAxisAlignedClip(&D2D_RECT_F { left: 0.0, top: 0.0, right: 70.0, bottom: H as f32 }, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            }
            let r = glyphs(c, "AVATo", None, None, 0, false, DWRITE_MEASURING_MODE_NATURAL);
            // SAFETY: as above.
            unsafe { c.rt.PopAxisAlignedClip() };
            r
        }),
        ("layer 50%", |c| {
            // SAFETY: a push/pop pair on a live target.
            unsafe {
                let layer = c.rt.CreateLayer(None)?;
                let params = D2D1_LAYER_PARAMETERS {
                    contentBounds: D2D_RECT_F { left: -1e9, top: -1e9, right: 1e9, bottom: 1e9 },
                    maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                    maskTransform: Matrix3x2::identity(),
                    opacity: 0.5,
                    layerOptions: D2D1_LAYER_OPTIONS_NONE,
                    ..Default::default()
                };
                c.rt.PushLayer(&raw const params, &layer);
            }
            let r = glyphs(c, "AVATo", None, None, 0, false, DWRITE_MEASURING_MODE_NATURAL);
            // SAFETY: as above.
            unsafe { c.rt.PopLayer() };
            r
        }),
        ("layout plain", |c| layout(c, "The quick brown fox 日本語", 400.0, D2D1_DRAW_TEXT_OPTIONS_NONE, |_| Ok(()))),
        ("layout underline", |c| {
            layout(c, "Underlined strike", 400.0, D2D1_DRAW_TEXT_OPTIONS_NONE, |l| {
                // SAFETY: setters on a live layout.
                unsafe {
                    l.SetUnderline(true, DWRITE_TEXT_RANGE { startPosition: 0, length: 10 })?;
                    l.SetStrikethrough(true, DWRITE_TEXT_RANGE { startPosition: 11, length: 6 })
                }
            })
        }),
        ("layout effect red", |c| {
            let red = solid(c, 1.0, 0.0, 0.0, 1.0)?;
            layout(c, "black RED black", 400.0, D2D1_DRAW_TEXT_OPTIONS_NONE, move |l| {
                // SAFETY: a setter on a live layout.
                unsafe { l.SetDrawingEffect(&red, DWRITE_TEXT_RANGE { startPosition: 6, length: 3 }) }
            })
        }),
        ("layout wrap", |c| layout(c, "wrapping words across lines here", 150.0, D2D1_DRAW_TEXT_OPTIONS_NONE, |_| Ok(()))),
        ("layout clip", |c| layout(c, "clipped_long_word_overflowing", 100.0, D2D1_DRAW_TEXT_OPTIONS_CLIP, |_| Ok(()))),
        ("layout rtl", |c| {
            // SAFETY: a setter on a live format (restored below).
            unsafe { c.format.SetReadingDirection(DWRITE_READING_DIRECTION_RIGHT_TO_LEFT)? };
            let r = layout(c, "שלום עולם", 400.0, D2D1_DRAW_TEXT_OPTIONS_NONE, |_| Ok(()));
            // SAFETY: as above.
            unsafe { c.format.SetReadingDirection(Default::default())? };
            r
        }),
        ("layout emoji", |c| layout(c, "A😀B", 400.0, D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT, |_| Ok(()))),
        ("drawtext", |c| drawtext(c, "DrawText 日本", DWRITE_MEASURING_MODE_NATURAL)),
        ("perf 500 runs", |c| {
            for _ in 0..500 {
                glyphs(c, "The quick brown fox", None, None, 0, false, DWRITE_MEASURING_MODE_NATURAL)?;
            }
            Ok(())
        }),
        ("perf 200 layouts", |c| {
            for _ in 0..200 {
                layout(c, "The quick brown fox 日本語", 400.0, D2D1_DRAW_TEXT_OPTIONS_NONE, |_| Ok(()))?;
            }
            Ok(())
        }),
        ("drawtext gdi-classic", |c| drawtext(c, "DrawText 日本", DWRITE_MEASURING_MODE_GDI_CLASSIC)),
    ]
}

fn transform(c: &Ctx, m: Matrix3x2) {
    // SAFETY: a setter on a live target.
    unsafe { c.rt.SetTransform(&raw const m) };
}

fn solid(c: &Ctx, r: f32, g: f32, b: f32, a: f32) -> windows::core::Result<windows::Win32::Graphics::Direct2D::ID2D1Brush> {
    // SAFETY: brush creation on a live target.
    unsafe { c.rt.CreateSolidColorBrush(&D2D1_COLOR_F { r, g, b, a }, None)?.cast() }
}

fn glyphs_with(c: &Ctx, brush: &windows::Win32::Graphics::Direct2D::ID2D1Brush) -> windows::core::Result<()> {
    let ids = glyph_indices(&c.face, "AVATo")?;
    let run = run_of(c, &ids, &[], &[], 0, false);
    // SAFETY: a draw call on a live target with a run that lives for it.
    unsafe { c.rt.DrawGlyphRun(BASELINE, &raw const run, brush, DWRITE_MEASURING_MODE_NATURAL) };
    Ok(())
}

fn glyphs(c: &Ctx, text: &str, adv: Option<f32>, off: Option<(f32, f32)>, bidi: u32, sideways: bool, mode: DWRITE_MEASURING_MODE) -> windows::core::Result<()> {
    let ids = glyph_indices(&c.face, text)?;
    let advances: Vec<f32> = adv.map_or_else(Vec::new, |a| vec![a; ids.len()]);
    let offsets: Vec<DWRITE_GLYPH_OFFSET> =
        off.map(|(a, u)| vec![DWRITE_GLYPH_OFFSET { advanceOffset: a, ascenderOffset: u }; ids.len()]).unwrap_or_default();
    let run = run_of(c, &ids, &advances, &offsets, bidi, sideways);
    let black = solid(c, 0.0, 0.0, 0.0, 1.0)?;
    // SAFETY: as in `glyphs_with`.
    unsafe { c.rt.DrawGlyphRun(BASELINE, &raw const run, &black, mode) };
    Ok(())
}

fn run_of(c: &Ctx, ids: &[u16], adv: &[f32], off: &[DWRITE_GLYPH_OFFSET], bidi: u32, sideways: bool) -> DWRITE_GLYPH_RUN {
    DWRITE_GLYPH_RUN {
        fontFace: std::mem::ManuallyDrop::new(Some(c.face.clone())),
        fontEmSize: EM,
        glyphCount: ids.len() as u32,
        glyphIndices: ids.as_ptr(),
        glyphAdvances: if adv.is_empty() { std::ptr::null() } else { adv.as_ptr() },
        glyphOffsets: if off.is_empty() { std::ptr::null() } else { off.as_ptr() },
        isSideways: sideways.into(),
        bidiLevel: bidi,
    }
}

fn layout(
    c: &Ctx, text: &str, max_w: f32, options: D2D1_DRAW_TEXT_OPTIONS,
    setup: impl FnOnce(&windows::Win32::Graphics::DirectWrite::IDWriteTextLayout) -> windows::core::Result<()>,
) -> windows::core::Result<()> {
    let wide: Vec<u16> = text.encode_utf16().collect();
    // SAFETY: layout creation and drawing on live objects.
    unsafe {
        let l = c.dw.CreateTextLayout(&wide, &c.format, max_w, 150.0)?;
        setup(&l)?;
        let black = solid(c, 0.0, 0.0, 0.0, 1.0)?;
        c.rt.DrawTextLayout(Vector2 { X: 40.0, Y: 30.0 }, &l, &black, options);
    }
    Ok(())
}

fn drawtext(c: &Ctx, text: &str, mode: DWRITE_MEASURING_MODE) -> windows::core::Result<()> {
    let wide: Vec<u16> = text.encode_utf16().collect();
    let black = solid(c, 0.0, 0.0, 0.0, 1.0)?;
    // SAFETY: a draw call on a live target.
    unsafe {
        c.rt.DrawText(&wide, &c.format, &D2D_RECT_F { left: 40.0, top: 30.0, right: 400.0, bottom: 120.0 }, &black, D2D1_DRAW_TEXT_OPTIONS_NONE, mode);
    }
    Ok(())
}

/// The first font face of `family` in the system collection.
unsafe fn system_face(factory: &IDWriteFactory, family: PCWSTR) -> windows::core::Result<IDWriteFontFace> {
    unsafe {
        let mut c = None;
        factory.GetSystemFontCollection(&mut c, false)?;
        let collection = c.expect("system font collection");
        let mut index = 0u32;
        let mut exists = windows::core::BOOL(0);
        collection.FindFamilyName(family, &mut index, &mut exists)?;
        assert!(exists.as_bool(), "font family not installed");
        let fam = collection.GetFontFamily(index)?;
        let font = fam.GetFirstMatchingFont(DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL)?;
        font.CreateFontFace()
    }
}

fn glyph_indices(face: &IDWriteFontFace, text: &str) -> windows::core::Result<Vec<u16>> {
    let codes: Vec<u32> = text.chars().map(|c| c as u32).collect();
    let mut out = vec![0u16; codes.len()];
    // SAFETY: both slices are ours and of the same length.
    unsafe { face.GetGlyphIndices(codes.as_ptr(), codes.len() as u32, out.as_mut_ptr())? };
    Ok(out)
}

/// Ink bounds (anything darker than white), the darkest pixel and the mean
/// darkness of the ink, from the BGRA bitmap.
fn ink(px: &[u8]) -> String {
    let (mut x0, mut x1, mut y0, mut y1) = (i32::MAX, i32::MIN, i32::MAX, i32::MIN);
    let (mut darkest, mut dsum, mut n, mut sum) = ([255u8; 3], 765u32, 0u32, 0u64);
    for y in 0..H {
        for x in 0..W {
            let o = ((y * W + x) * 4) as usize;
            let (b, g, r) = (px[o], px[o + 1], px[o + 2]);
            let s = u32::from(r) + u32::from(g) + u32::from(b);
            if s < 600 {
                x0 = x0.min(x);
                x1 = x1.max(x);
                y0 = y0.min(y);
                y1 = y1.max(y);
                n += 1;
                sum += u64::from(765 - s);
                if s < dsum {
                    dsum = s;
                    darkest = [r, g, b];
                }
            }
        }
    }
    if n == 0 {
        return "ink=(none)".into();
    }
    format!(
        "ink=x={x0}..{x1} y={y0}..{y1} darkest=#{:02x}{:02x}{:02x} mean={} px={n}",
        darkest[0],
        darkest[1],
        darkest[2],
        sum / u64::from(n),
    )
}

fn save_bmp(px: &[u8], path: &str) {
    let row = (W * 3 + 3) & !3;
    let mut data = vec![0u8; (row * H) as usize];
    for y in 0..H {
        for x in 0..W {
            let s = ((y * W + x) * 4) as usize;
            let d = ((H - 1 - y) * row + x * 3) as usize;
            data[d..d + 3].copy_from_slice(&px[s..s + 3]);
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

/// `HDC` is used only through the render target once bound.
#[allow(dead_code)]
fn _hdc(_: HDC) {}
