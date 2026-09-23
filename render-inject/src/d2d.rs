//! Direct2D text: `DrawGlyphRun`, `DrawTextLayout` and `DrawText` on every
//! kind of render target.
//!
//! Render targets are created at runtime, so reaching them is a chain of
//! creation hooks, the same chain upstream `directwrite.cpp` walks:
//!
//! ```text
//! d2d1!D2D1CreateFactory ─▶ ID2D1Factory ─┬─▶ CreateHwndRenderTarget / CreateDCRenderTarget /
//!                                         │   CreateWicBitmapRenderTarget       ─▶ render target
//!                                         └─▶ ID2D1Factory1..7::CreateDevice    ─▶ ID2D1Device
//! d2d1!D2D1CreateDevice ──────────────────────────────────────────────────────▶ ID2D1Device
//!     ID2D1Device..6::CreateDeviceContext ────────────────────────────────────▶ ID2D1DeviceContext
//! d2d1!D2D1CreateDeviceContext ───────────────────────────────────────────────▶ ID2D1DeviceContext
//!     any target's CreateCompatibleRenderTarget ──────────────────────────────▶ bitmap render target
//! ```
//!
//! On each render target / device context we patch `DrawText` (slot 27),
//! `DrawTextLayout` (28), `DrawGlyphRun` (29), `ID2D1DeviceContext::DrawGlyphRun`
//! (82, the overload with a run description), `SetTextAntialiasMode` (34),
//! `SetTextRenderingParams` (36) and `CreateCompatibleRenderTarget` (12) - on
//! the target's vtable and, when it differs, its `ID2D1DeviceContext` one.
//! Every vtable is patched once, tracked in `SLOT_ORIG` by (vtable, slot).
//! Slot numbers were checked against the `windows` crate's `*_Vtbl`
//! definitions. When Direct2D was loaded before the core, probe objects patch
//! the shared vtables of objects the app already holds (`probe_vtables`).
//!
//! Text is drawn by render-core, through Direct2D itself: the run's coverage
//! becomes an alpha bitmap that the target fills with the app's brush
//! (`FillOpacityMask`), so clips, layers, transforms and any brush apply as
//! they would to Direct2D's own text. `DrawTextLayout` / `DrawText` are
//! walked into glyph runs with our own `IDWriteTextRenderer`. What cannot go
//! through a greyscale mask - a ClearType profile, aliased text, colour
//! glyphs, a rotated or skewed transform - is left to Direct2D as upstream
//! leaves all of it: with the profile's `[DirectWrite]` rendering params and
//! antialias mode, and the transform nudged by 1/65535 when grid fitting is
//! off.
//!
//! Every detour is a thin `unsafe extern "system"` shell: it borrows what it
//! was handed (a COM object, a glyph run) once and calls safe code.

use core::ffi::c_void;
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use render_core::render::render_placed;
use render_core::Rect;
use windows::core::{implement, s, w, AsImpl, Interface, Ref, BOOL, GUID, HRESULT, PCSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Brush, ID2D1Device, ID2D1Device1, ID2D1Device2, ID2D1Device3, ID2D1Device4, ID2D1Device5,
    ID2D1Device6, ID2D1DeviceContext, ID2D1Factory, ID2D1Factory1, ID2D1Factory2, ID2D1Factory3, ID2D1Factory4,
    ID2D1Factory5, ID2D1Factory6, ID2D1Factory7, ID2D1Multithread, ID2D1RenderTarget, ID2D1SolidColorBrush, D2D1_ANTIALIAS_MODE_ALIASED,
    D2D1_BITMAP_PROPERTIES, D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_HWND_RENDER_TARGET_PROPERTIES, D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
    D2D1_DRAW_TEXT_OPTIONS_NO_SNAP, D2D1_FACTORY_TYPE_MULTI_THREADED, D2D1_FACTORY_TYPE_SINGLE_THREADED,
    D2D1_OPACITY_MASK_CONTENT_TEXT_NATURAL, D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_TEXT_ANTIALIAS_MODE,
    D2D1_TEXT_ANTIALIAS_MODE_ALIASED, D2D1_UNIT_MODE_PIXELS,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteFactory2, IDWriteInlineObject, IDWritePixelSnapping_Impl,
    IDWriteTextFormat, IDWriteTextLayout, IDWriteTextRenderer, IDWriteTextRenderer_Impl, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_GLYPH_RUN, DWRITE_GLYPH_RUN_DESCRIPTION, DWRITE_MATRIX, DWRITE_MEASURING_MODE,
    DWRITE_MEASURING_MODE_GDI_NATURAL, DWRITE_MEASURING_MODE_NATURAL, DWRITE_READING_DIRECTION,
    DWRITE_READING_DIRECTION_RIGHT_TO_LEFT, DWRITE_STRIKETHROUGH, DWRITE_UNDERLINE,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE};
use windows::core::IUnknown;
use windows_numerics::{Matrix3x2, Vector2};

use crate::dwrite::dw_rendering;
use crate::fonts::reface;
use crate::hook::{install_hook, patch_slot};
use crate::layout::{self, Mapping};
use crate::log;
use crate::state::{orig, RENDER};

// ---- signatures ----

type FnD2DCreateFactory = unsafe extern "system" fn(i32, *const GUID, *const c_void, *mut *mut c_void) -> HRESULT;
/// `D2D1CreateDevice(IDXGIDevice*, const D2D1_CREATION_PROPERTIES*, ID2D1Device**)`
/// and `D2D1CreateDeviceContext(IDXGISurface*, ..., ID2D1DeviceContext**)`.
type FnD2DCreateDevice = unsafe extern "system" fn(*mut c_void, *const c_void, *mut *mut c_void) -> HRESULT;
/// `ID2D1Factory::CreateDCRenderTarget(props, out)`, and the same shape as
/// every `ID2D1FactoryN::CreateDevice(IDXGIDevice*, out)`.
type FnCreate2 = unsafe extern "system" fn(*mut c_void, *const c_void, *mut *mut c_void) -> HRESULT;
/// `CreateHwndRenderTarget(props, hwndProps, out)`; also `CreateWicBitmapRenderTarget(bitmap, props, out)`.
type FnCreate3 = unsafe extern "system" fn(*mut c_void, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
/// `ID2D1DeviceN::CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS, out)`.
type FnCreateDeviceContext = unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT;
/// `CreateCompatibleRenderTarget(size*, pixelSize*, format*, options, out)`.
type FnCreateCompatibleRT =
    unsafe extern "system" fn(*mut c_void, *const c_void, *const c_void, *const c_void, u32, *mut *mut c_void) -> HRESULT;
type FnDrawGlyphRun = unsafe extern "system" fn(*mut c_void, Vector2, *const DWRITE_GLYPH_RUN, *mut c_void, i32);
/// `ID2D1DeviceContext::DrawGlyphRun(baseline, run, description, brush, measuring)`.
type FnDrawGlyphRun1 = unsafe extern "system" fn(*mut c_void, Vector2, *const DWRITE_GLYPH_RUN, *const c_void, *mut c_void, i32);
/// `DrawText(string, length, format, layoutRect, brush, options, measuringMode)`.
type FnDrawText = unsafe extern "system" fn(*mut c_void, *const u16, u32, *mut c_void, *const D2D_RECT_F, *mut c_void, i32, i32);
/// `DrawTextLayout(origin, layout, brush, options)`.
type FnDrawTextLayout = unsafe extern "system" fn(*mut c_void, Vector2, *mut c_void, *mut c_void, i32);
type FnSetTextAaMode = unsafe extern "system" fn(*mut c_void, i32);
type FnSetTextRenderingParams = unsafe extern "system" fn(*mut c_void, *mut c_void);

static ORIG_D2DCF: OnceLock<FnD2DCreateFactory> = OnceLock::new();
static ORIG_D2DCD: OnceLock<FnD2DCreateDevice> = OnceLock::new();
static ORIG_D2DCDC: OnceLock<FnD2DCreateDevice> = OnceLock::new();
/// (vtable address, slot) → original function, for every COM slot patched
/// here. One mutex both serialises the one-time patches and guards the map,
/// so two threads creating render targets at once cannot both capture a slot
/// that already holds our detour (the detour would then call itself).
static SLOT_ORIG: Mutex<Option<HashMap<(usize, usize), usize>>> = Mutex::new(None);
static D2D_CAPTURED: AtomicBool = AtomicBool::new(false); // log the first D2D text drawn by render-core once

// ID2D1Factory
const SLOT_CREATE_WIC_RT: usize = 13;
const SLOT_CREATE_HWND_RT: usize = 14;
const SLOT_CREATE_DC_RT: usize = 16;
/// `CreateDevice` in ID2D1Factory1 … ID2D1Factory7, in that order.
const SLOTS_FACTORY_CREATE_DEVICE: [usize; 7] = [17, 27, 28, 29, 30, 31, 32];
/// `CreateDeviceContext` in ID2D1Device … ID2D1Device6, in that order.
const SLOTS_DEVICE_CREATE_CONTEXT: [usize; 7] = [4, 11, 12, 15, 16, 19, 20];
// ID2D1RenderTarget / ID2D1DeviceContext
const SLOT_CREATE_COMPATIBLE_RT: usize = 12;
const SLOT_DRAW_TEXT: usize = 27;
const SLOT_DRAW_TEXT_LAYOUT: usize = 28;
const SLOT_DRAW_GLYPH_RUN: usize = 29;
const SLOT_SET_TEXT_AA_MODE: usize = 34;
const SLOT_SET_TEXT_RENDERING_PARAMS: usize = 36;
const SLOT_DRAW_GLYPH_RUN1: usize = 82;

// ---- vtable bookkeeping ----

/// A live COM object's vtable, as the pointer we patch through.
///
/// # Safety
/// `obj` must be a live COM interface pointer.
unsafe fn vtable_of(obj: *mut c_void) -> *mut *const () {
    // SAFETY: the first word of any COM object is its vtable pointer.
    unsafe { *obj.cast::<*mut *const ()>() }
}

/// Patch `slot` of `obj`'s vtable to `detour` unless that (vtable, slot) is
/// already ours. Returns whether a patch was made.
///
/// # Safety
/// `obj` must be a live COM object whose interface has at least `slot + 1`
/// methods, and `detour` must match that method's ABI and signature.
unsafe fn patch_once(obj: *mut c_void, slot: usize, detour: *const ()) -> bool {
    // SAFETY: per the contract above.
    let vtbl = unsafe { vtable_of(obj) };
    let key = (vtbl.addr(), slot);
    let Ok(mut m) = SLOT_ORIG.lock() else { return false };
    let map = m.get_or_insert_with(HashMap::new);
    if map.contains_key(&key) {
        return false;
    }
    // SAFETY: `slot` is within the interface per the contract above.
    unsafe {
        patch_slot(vtbl.add(slot), detour, |old| {
            map.insert(key, old.expose_provenance());
        })
    }
}

/// The original function behind `slot` for `this`'s vtable, if we patched
/// it, typed as `F`.
///
/// # Safety
/// `this` must be a live COM object and `F` the exact fn-pointer type of
/// that slot's method.
unsafe fn slot_orig<F: Copy>(this: *mut c_void, slot: usize) -> Option<F> {
    const {
        assert!(core::mem::size_of::<F>() == core::mem::size_of::<*const ()>(), "F must be a fn pointer");
    }
    // SAFETY: `this` is live per the contract above.
    let vtbl = unsafe { vtable_of(this) }.addr();
    let addr = SLOT_ORIG.lock().ok()?.as_ref()?.get(&(vtbl, slot)).copied()?;
    let p = core::ptr::with_exposed_provenance::<()>(addr);
    // SAFETY: `F` is a fn pointer type of the same size, and the address is
    // the one `patch_slot` read out of the slot.
    Some(unsafe { core::mem::transmute_copy::<*const (), F>(&p) })
}

/// The original of any of `slots` on `this`'s vtable — for method families
/// (`CreateDevice`, `CreateDeviceContext`) whose overloads share one
/// signature and one implementation per object.
///
/// # Safety
/// As `slot_orig`.
unsafe fn any_slot_orig<F: Copy>(this: *mut c_void, slots: &[usize]) -> Option<F> {
    // SAFETY: forwarded.
    slots.iter().find_map(|&s| unsafe { slot_orig::<F>(this, s) })
}

/// The object a creation call wrote to `out`, if it succeeded.
///
/// # Safety
/// `out` is the call's out-parameter, valid for the call's duration.
unsafe fn created(hr: HRESULT, out: *mut *mut c_void) -> Option<*mut c_void> {
    if !hr.is_ok() {
        return None;
    }
    // SAFETY: per the contract above.
    unsafe { out.as_ref() }.copied().filter(|p| !p.is_null())
}

// ---- creation chain ----

unsafe extern "system" fn d2dcf_detour(ftype: i32, riid: *const GUID, opts: *const c_void, out: *mut *mut c_void) -> HRESULT {
    // SAFETY: arguments forwarded untouched; `out` inspected under the call's contract.
    unsafe {
        let hr = (orig(&ORIG_D2DCF))(ftype, riid, opts, out);
        if let Some(f) = created(hr, out) {
            hook_factory(f);
        }
        hr
    }
}

unsafe extern "system" fn d2dcd_detour(dxgi: *mut c_void, props: *const c_void, out: *mut *mut c_void) -> HRESULT {
    // SAFETY: as in `d2dcf_detour`.
    unsafe {
        let hr = (orig(&ORIG_D2DCD))(dxgi, props, out);
        if let Some(d) = created(hr, out) {
            hook_device(d);
        }
        hr
    }
}

unsafe extern "system" fn d2dcdc_detour(surface: *mut c_void, props: *const c_void, out: *mut *mut c_void) -> HRESULT {
    // SAFETY: as in `d2dcf_detour`.
    unsafe {
        let hr = (orig(&ORIG_D2DCDC))(surface, props, out);
        if let Some(rt) = created(hr, out) {
            hook_render_target(rt);
        }
        hr
    }
}

/// Patch the factory's render-target and device creation slots. Which
/// `CreateDevice` overloads exist depends on the OS's Direct2D version, so
/// each is patched only when the factory answers the matching QueryInterface.
///
/// # Safety
/// `factory` must be a live `ID2D1Factory`.
unsafe fn hook_factory(factory: *mut c_void) {
    // SAFETY: per the contract above; `from_raw_borrowed` does not AddRef.
    let Some(f) = (unsafe { ID2D1Factory::from_raw_borrowed(&factory) }) else { return };
    let supported = [
        f.cast::<ID2D1Factory1>().is_ok(),
        f.cast::<ID2D1Factory2>().is_ok(),
        f.cast::<ID2D1Factory3>().is_ok(),
        f.cast::<ID2D1Factory4>().is_ok(),
        f.cast::<ID2D1Factory5>().is_ok(),
        f.cast::<ID2D1Factory6>().is_ok(),
        f.cast::<ID2D1Factory7>().is_ok(),
    ];
    // SAFETY: each slot exists on the interface the QueryInterface confirmed,
    // and each detour matches that slot's signature.
    let n = unsafe {
        u32::from(patch_once(factory, SLOT_CREATE_WIC_RT, create_wic_detour as *const ()))
            + u32::from(patch_once(factory, SLOT_CREATE_HWND_RT, create_hwnd_detour as *const ()))
            + u32::from(patch_once(factory, SLOT_CREATE_DC_RT, create_dc_detour as *const ()))
            + SLOTS_FACTORY_CREATE_DEVICE
                .iter()
                .zip(supported)
                .filter(|(_, ok)| *ok)
                .map(|(&slot, _)| u32::from(patch_once(factory, slot, create_device_detour as *const ())))
                .sum::<u32>()
    };
    if n > 0 {
        log(&format!("hook installed on D2D1Factory creation ({n} slots)"));
    }
}

/// Patch every `CreateDeviceContext` overload the device supports.
///
/// # Safety
/// `dev` must be a live `ID2D1Device`.
unsafe fn hook_device(dev: *mut c_void) {
    // SAFETY: per the contract above.
    let Some(d) = (unsafe { ID2D1Device::from_raw_borrowed(&dev) }) else { return };
    let supported = [
        true,
        d.cast::<ID2D1Device1>().is_ok(),
        d.cast::<ID2D1Device2>().is_ok(),
        d.cast::<ID2D1Device3>().is_ok(),
        d.cast::<ID2D1Device4>().is_ok(),
        d.cast::<ID2D1Device5>().is_ok(),
        d.cast::<ID2D1Device6>().is_ok(),
    ];
    // SAFETY: as in `hook_factory`.
    let n = unsafe {
        SLOTS_DEVICE_CREATE_CONTEXT
            .iter()
            .zip(supported)
            .filter(|(_, ok)| *ok)
            .map(|(&slot, _)| u32::from(patch_once(dev, slot, create_context_detour as *const ())))
            .sum::<u32>()
    };
    if n > 0 {
        log(&format!("hook installed on ID2D1Device::CreateDeviceContext ({n} slots)"));
    }
}

/// Patch the ID2D1RenderTarget slots we hook on `rt`'s vtable. Returns how
/// many were patched now.
///
/// # Safety
/// `rt` must be a live `ID2D1RenderTarget` (or an interface deriving from it).
unsafe fn patch_target_slots(rt: *mut c_void) -> u32 {
    // SAFETY: every slot is on ID2D1RenderTarget, and each detour matches its
    // signature.
    unsafe {
        u32::from(patch_once(rt, SLOT_CREATE_COMPATIBLE_RT, create_compatible_detour as *const ()))
            + u32::from(patch_once(rt, SLOT_DRAW_TEXT, draw_text_detour as *const ()))
            + u32::from(patch_once(rt, SLOT_DRAW_TEXT_LAYOUT, draw_text_layout_detour as *const ()))
            + u32::from(patch_once(rt, SLOT_DRAW_GLYPH_RUN, dgr_detour as *const ()))
            + u32::from(patch_once(rt, SLOT_SET_TEXT_AA_MODE, set_text_aa_detour as *const ()))
            + u32::from(patch_once(rt, SLOT_SET_TEXT_RENDERING_PARAMS, set_text_rp_detour as *const ()))
    }
}

/// Patch the text slots of a render target or device context, then apply the
/// profile's rendering params to it (as upstream does at creation).
///
/// # Safety
/// `rt` must be a live `ID2D1RenderTarget` (or any interface deriving from it).
unsafe fn hook_render_target(rt: *mut c_void) {
    // SAFETY: per the contract above; every slot is on ID2D1RenderTarget
    // itself, and each detour matches its signature.
    let mut n = unsafe { patch_target_slots(rt) };
    // SAFETY: per the contract above.
    if let Some(r) = unsafe { ID2D1RenderTarget::from_raw_borrowed(&rt) } {
        // Every modern render target is a device context underneath; its
        // ID2D1DeviceContext vtable may be a separate (thunk) table, so patch
        // the description overload through that interface pointer.
        if let Ok(dc) = r.cast::<ID2D1DeviceContext>() {
            // SAFETY: ID2D1DeviceContext derives from ID2D1RenderTarget, and
            // slot 82 is its own, which `cast` confirmed. When the interface
            // shares the target's vtable, `patch_once` skips the repeats.
            n += unsafe {
                patch_target_slots(dc.as_raw()) + u32::from(patch_once(dc.as_raw(), SLOT_DRAW_GLYPH_RUN1, dgr1_detour as *const ()))
            };
        }
        if let Some(dw) = dw_rendering() {
            // SAFETY: setters on a live render target.
            unsafe {
                r.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE(dw.aa_mode));
                r.SetTextRenderingParams(&dw.params);
            }
        }
    }
    if n > 0 {
        log(&format!("hook installed on D2D render target text ({n} slots)"));
    }
}

unsafe extern "system" fn create_dc_detour(this: *mut c_void, props: *const c_void, out: *mut *mut c_void) -> HRESULT {
    // SAFETY: `this` is the factory the app called; arguments forwarded.
    unsafe {
        let Some(f) = slot_orig::<FnCreate2>(this, SLOT_CREATE_DC_RT) else { return HRESULT(-1) };
        let hr = f(this, props, out);
        if let Some(rt) = created(hr, out) {
            hook_render_target(rt);
        }
        hr
    }
}

unsafe extern "system" fn create_hwnd_detour(this: *mut c_void, p1: *const c_void, p2: *const c_void, out: *mut *mut c_void) -> HRESULT {
    // SAFETY: as in `create_dc_detour`.
    unsafe {
        let Some(f) = slot_orig::<FnCreate3>(this, SLOT_CREATE_HWND_RT) else { return HRESULT(-1) };
        let hr = f(this, p1, p2, out);
        if let Some(rt) = created(hr, out) {
            hook_render_target(rt);
        }
        hr
    }
}

unsafe extern "system" fn create_wic_detour(this: *mut c_void, bitmap: *const c_void, props: *const c_void, out: *mut *mut c_void) -> HRESULT {
    // SAFETY: as in `create_dc_detour`.
    unsafe {
        let Some(f) = slot_orig::<FnCreate3>(this, SLOT_CREATE_WIC_RT) else { return HRESULT(-1) };
        let hr = f(this, bitmap, props, out);
        if let Some(rt) = created(hr, out) {
            hook_render_target(rt);
        }
        hr
    }
}

unsafe extern "system" fn create_compatible_detour(
    this: *mut c_void, size: *const c_void, pixel_size: *const c_void, format: *const c_void, options: u32, out: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: as in `create_dc_detour`.
    unsafe {
        let Some(f) = slot_orig::<FnCreateCompatibleRT>(this, SLOT_CREATE_COMPATIBLE_RT) else { return HRESULT(-1) };
        let hr = f(this, size, pixel_size, format, options, out);
        if let Some(rt) = created(hr, out) {
            hook_render_target(rt);
        }
        hr
    }
}

/// One detour serves every `ID2D1FactoryN::CreateDevice`: the signatures are
/// identical. We cannot tell which slot the app called, so the original of
/// any patched slot on this vtable is used — they all create the same device
/// (each overload only differs in the interface it returns).
unsafe extern "system" fn create_device_detour(this: *mut c_void, dxgi: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    // SAFETY: as in `create_dc_detour`.
    unsafe {
        let Some(f) = any_slot_orig::<FnCreate2>(this, &SLOTS_FACTORY_CREATE_DEVICE) else { return HRESULT(-1) };
        let hr = f(this, dxgi, out);
        if let Some(d) = created(hr, out) {
            hook_device(d);
        }
        hr
    }
}

/// Same for `ID2D1DeviceN::CreateDeviceContext`.
unsafe extern "system" fn create_context_detour(this: *mut c_void, options: u32, out: *mut *mut c_void) -> HRESULT {
    // SAFETY: as in `create_dc_detour`.
    unsafe {
        let Some(f) = any_slot_orig::<FnCreateDeviceContext>(this, &SLOTS_DEVICE_CREATE_CONTEXT) else { return HRESULT(-1) };
        let hr = f(this, options, out);
        if let Some(rt) = created(hr, out) {
            hook_render_target(rt);
        }
        hr
    }
}

// ---- text slots ----

/// The app sets its own antialias mode / rendering params: keep ours instead
/// (upstream does the same), unless we have none.
unsafe extern "system" fn set_text_aa_detour(this: *mut c_void, mode: i32) {
    // SAFETY: `this` is the render target the app called.
    unsafe {
        let Some(f) = slot_orig::<FnSetTextAaMode>(this, SLOT_SET_TEXT_AA_MODE) else { return };
        f(this, dw_rendering().map_or(mode, |d| d.aa_mode));
    }
}

unsafe extern "system" fn set_text_rp_detour(this: *mut c_void, params: *mut c_void) {
    // Keep our clone alive across the call: a profile reload on another
    // thread may replace DW_RENDERING meanwhile, and this clone is then the
    // only reference behind the raw pointer we pass.
    let dw = dw_rendering();
    // SAFETY: `this` is the render target the app called.
    unsafe {
        let Some(f) = slot_orig::<FnSetTextRenderingParams>(this, SLOT_SET_TEXT_RENDERING_PARAMS) else { return };
        f(this, dw.as_ref().map_or(params, |d| d.params.as_raw()));
    }
}

/// Run the original draw with the grid-fit nudge upstream applies: with
/// grid fitting off, a 1/65535 skew keeps DirectWrite from snapping glyphs
/// to whole pixels.
fn with_grid_fit_nudge(rt: &ID2D1RenderTarget, draw: impl FnOnce()) {
    if !dw_rendering().is_some_and(|d| d.grid_fit_disabled) {
        draw();
        return;
    }
    let mut prev = Matrix3x2::default();
    // SAFETY: transform get/set on a live render target, restored after.
    unsafe {
        rt.GetTransform(&raw mut prev);
        let mut skew = prev;
        skew.M12 += 1.0 / 65535.0;
        skew.M21 += 1.0 / 65535.0;
        rt.SetTransform(&raw const skew);
        draw();
        rt.SetTransform(&raw const prev);
    }
}

unsafe extern "system" fn dgr_detour(this: *mut c_void, baseline: Vector2, run: *const DWRITE_GLYPH_RUN, brush: *mut c_void, measuring: i32) {
    // SAFETY: `this` is the render target the app called, `run` and `brush`
    // its arguments, all live for the call.
    unsafe {
        let Some(rt) = ID2D1RenderTarget::from_raw_borrowed(&this) else { return };
        let Some(b) = ID2D1Brush::from_raw_borrowed(&brush) else { return };
        if run.as_ref().is_some_and(|r| draw_run(rt, baseline, r, b, measuring).is_some()) {
            return;
        }
        if let Some(f) = slot_orig::<FnDrawGlyphRun>(this, SLOT_DRAW_GLYPH_RUN) {
            with_grid_fit_nudge(rt, || f(this, baseline, run, brush, measuring));
        }
    }
}

unsafe extern "system" fn dgr1_detour(
    this: *mut c_void, baseline: Vector2, run: *const DWRITE_GLYPH_RUN, desc: *const c_void, brush: *mut c_void, measuring: i32,
) {
    // SAFETY: as in `dgr_detour`.
    unsafe {
        let Some(rt) = ID2D1RenderTarget::from_raw_borrowed(&this) else { return };
        let Some(b) = ID2D1Brush::from_raw_borrowed(&brush) else { return };
        if run.as_ref().is_some_and(|r| draw_run(rt, baseline, r, b, measuring).is_some()) {
            return;
        }
        if let Some(f) = slot_orig::<FnDrawGlyphRun1>(this, SLOT_DRAW_GLYPH_RUN1) {
            with_grid_fit_nudge(rt, || f(this, baseline, run, desc, brush, measuring));
        }
    }
}

unsafe extern "system" fn draw_text_layout_detour(this: *mut c_void, origin: Vector2, layout: *mut c_void, brush: *mut c_void, options: i32) {
    // SAFETY: as in `dgr_detour`; `layout` is the app's text layout.
    unsafe {
        let Some(rt) = ID2D1RenderTarget::from_raw_borrowed(&this) else { return };
        let drawn = IDWriteTextLayout::from_raw_borrowed(&layout)
            .zip(ID2D1Brush::from_raw_borrowed(&brush))
            .is_some_and(|(l, b)| draw_layout(rt, origin, l, b, options).is_some());
        if drawn {
            return;
        }
        if let Some(f) = slot_orig::<FnDrawTextLayout>(this, SLOT_DRAW_TEXT_LAYOUT) {
            with_grid_fit_nudge(rt, || f(this, origin, layout, brush, options));
        }
    }
}

unsafe extern "system" fn draw_text_detour(
    this: *mut c_void, text: *const u16, len: u32, format: *mut c_void, rect: *const D2D_RECT_F, brush: *mut c_void, options: i32,
    measuring: i32,
) {
    // SAFETY: as in `dgr_detour`; `text` holds `len` UTF-16 units and
    // `format` / `rect` are the app's, all live for the call.
    unsafe {
        let Some(rt) = ID2D1RenderTarget::from_raw_borrowed(&this) else { return };
        let drawn = (!text.is_null() || len == 0)
            .then(|| if len == 0 { &[][..] } else { core::slice::from_raw_parts(text, len as usize) })
            .zip(IDWriteTextFormat::from_raw_borrowed(&format))
            .zip(rect.as_ref())
            .zip(ID2D1Brush::from_raw_borrowed(&brush))
            .is_some_and(|(((t, f), r), b)| draw_text(rt, t, f, r, b, options, measuring).is_some());
        if drawn {
            return;
        }
        if let Some(fp) = slot_orig::<FnDrawText>(this, SLOT_DRAW_TEXT) {
            with_grid_fit_nudge(rt, || fp(this, text, len, format, rect, brush, options, measuring));
        }
    }
}

// ---- drawing with render-core ----
//
// Direct2D composites in its own space - with clips, layers, transforms and
// any brush - so the text is not drawn around it through a GDI DC. Instead
// the run's coverage becomes an alpha bitmap that Direct2D fills with the
// app's brush (`FillOpacityMask`). Everything the target applies to a fill
// then applies to the text too.
//
// Only greyscale can go through an opacity mask, so a ClearType (LCD)
// profile leaves Direct2D text to Direct2D (with the profile's rendering
// params, as upstream). So do aliased text, a rotated / skewed / mirrored
// transform, and a run whose font file cannot be read.

/// DIP → device pixel for `rt`: its transform, and its DPI unless the target
/// works in pixels (`D2D1_UNIT_MODE_PIXELS`). `None` for a transform the
/// layout cannot place (see `layout::Mapping`).
fn target_mapping(rt: &ID2D1RenderTarget) -> Option<(Mapping, f32)> {
    let mut t = Matrix3x2::default();
    let (mut dx, mut dy) = (0.0f32, 0.0f32);
    // SAFETY: getters on a live render target.
    unsafe {
        rt.GetTransform(&raw mut t);
        rt.GetDpi(&raw mut dx, &raw mut dy);
    }
    let pixels = rt.cast::<ID2D1DeviceContext>().is_ok_and(|dc| {
        // SAFETY: a getter on a live device context.
        (unsafe { dc.GetUnitMode() }) == D2D1_UNIT_MODE_PIXELS
    });
    let ppd = if pixels { 1.0 } else { dx / 96.0 };
    if !pixels && (dx - dy).abs() > f32::EPSILON {
        return None;
    }
    let m = DWRITE_MATRIX { m11: t.M11, m12: t.M12, m21: t.M21, m22: t.M22, dx: t.M31, dy: t.M32 };
    Some((Mapping::new(&m, ppd)?, ppd))
}

/// Whether this target's text is ours to draw at all.
fn self_drawn(rt: &ID2D1RenderTarget) -> bool {
    // SAFETY: a getter on a live render target.
    let aliased = unsafe { rt.GetTextAntialiasMode() } == D2D1_TEXT_ANTIALIAS_MODE_ALIASED;
    !aliased && RENDER.lock().is_ok_and(|g| g.as_ref().is_some_and(|st| !st.profile.aa.is_lcd()))
}

/// Draw one glyph run with render-core. `None` leaves it to Direct2D.
fn draw_run(rt: &ID2D1RenderTarget, baseline: Vector2, run: &DWRITE_GLYPH_RUN, brush: &ID2D1Brush, measuring: i32) -> Option<()> {
    if !self_drawn(rt) {
        return None;
    }
    let (map, ppd) = target_mapping(rt)?;
    // SAFETY: `run` is the app's argument, live for the call.
    let geo = unsafe { layout::lay_out(run, (baseline.X, baseline.Y), &map, measuring) }?;
    let face = run.fontFace.as_ref()?;
    let light = brush_is_light(brush);
    let (rect, mask) = {
        let mut guard = RENDER.lock().ok()?;
        let st = guard.as_mut()?;
        reface(st, face)?;
        let rendered = render_placed(&st.ft, &st.profile, &geo.glyphs, &geo.style);
        let Some(rect) = rendered.bounds else { return Some(()) }; // no ink: nothing to fill
        let mut mask = rendered.coverage(rect, 1, false);
        for v in &mut mask {
            *v = st.tables.mask_alpha(*v, light);
        }
        drop(guard);
        (rect, mask)
    };
    fill_mask(rt, brush, rect, &mask, ppd)?;
    if !D2D_CAPTURED.swap(true, Ordering::SeqCst) {
        log(&format!("substituted D2D text via render-core ({} glyphs)", geo.glyphs.len()));
    }
    Some(())
}

/// Is `brush` a solid colour lighter than mid-grey? Light ink gets the
/// light-on-dark opacity curve (see `Tables::mask_alpha`).
fn brush_is_light(brush: &ID2D1Brush) -> bool {
    brush.cast::<ID2D1SolidColorBrush>().is_ok_and(|b| {
        // SAFETY: a getter on a live brush.
        let c = unsafe { b.GetColor() };
        0.299 * c.r + 0.587 * c.g + 0.114 * c.b > 0.5
    })
}

/// Fill `rect` (device pixels) with `brush` through the alpha `mask`.
///
/// The mask is laid in device pixels, so the target's transform is set to
/// identity for the fill; the brush gets the target's transform folded into
/// its own so it paints exactly where it would have (a gradient keeps its
/// place). The fill needs aliased primitives. All three are restored.
fn fill_mask(rt: &ID2D1RenderTarget, brush: &ID2D1Brush, rect: Rect, mask: &[u8], ppd: f32) -> Option<()> {
    let (w, h) = (u32::try_from(rect.2 - rect.0).ok()?, u32::try_from(rect.3 - rect.1).ok()?);
    if w == 0 || h == 0 {
        return Some(());
    }
    let props = D2D1_BITMAP_PROPERTIES {
        pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
        dpiX: 96.0,
        dpiY: 96.0,
    };
    #[allow(clippy::cast_precision_loss)] // pixel coordinates, far below 2^24
    let (dest, src) = (
        D2D_RECT_F { left: rect.0 as f32 / ppd, top: rect.1 as f32 / ppd, right: rect.2 as f32 / ppd, bottom: rect.3 as f32 / ppd },
        D2D_RECT_F { left: 0.0, top: 0.0, right: w as f32, bottom: h as f32 },
    );
    // SAFETY: calls on a live render target and brush; `mask` holds w*h
    // bytes (one per pixel) and outlives the CreateBitmap call, which copies it.
    unsafe {
        let bitmap = rt.CreateBitmap(D2D_SIZE_U { width: w, height: h }, Some(mask.as_ptr().cast()), w, &raw const props).ok()?;
        // The target's transform and antialias mode and the brush's transform
        // are changed for the fill and put back. With a multithreaded factory
        // another thread may use the same brush or target meanwhile, so hold
        // Direct2D's own lock (reentrant) across the whole sequence.
        let lock = rt.GetFactory().ok().and_then(|f| f.cast::<ID2D1Multithread>().ok()).filter(|m| m.GetMultithreadProtected().as_bool());
        if let Some(m) = &lock {
            m.Enter();
        }
        let (mut t, mut bt) = (Matrix3x2::default(), Matrix3x2::default());
        rt.GetTransform(&raw mut t);
        brush.GetTransform(&raw mut bt);
        let aa = rt.GetAntialiasMode();
        let (folded, identity) = (bt * t, Matrix3x2::identity());
        brush.SetTransform(&raw const folded);
        rt.SetTransform(&raw const identity);
        rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_ALIASED);
        rt.FillOpacityMask(&bitmap, brush, D2D1_OPACITY_MASK_CONTENT_TEXT_NATURAL, Some(&raw const dest), Some(&raw const src));
        rt.SetAntialiasMode(aa);
        rt.SetTransform(&raw const t);
        brush.SetTransform(&raw const bt);
        if let Some(m) = &lock {
            m.Leave();
        }
    }
    Some(())
}

/// `DrawTextLayout` with render-core: the layout is walked with our own
/// `IDWriteTextRenderer`, which draws each glyph run like `draw_run`, fills
/// underlines and strikethroughs, and asks inline objects to draw themselves.
/// `None` leaves the whole layout to Direct2D: for a LCD profile, aliased
/// text, or colour glyphs with `ENABLE_COLOR_FONT` (which only Direct2D's
/// own layout drawing turns into colour layers).
fn draw_layout(rt: &ID2D1RenderTarget, origin: Vector2, layout: &IDWriteTextLayout, brush: &ID2D1Brush, options: i32) -> Option<()> {
    if !self_drawn(rt) {
        return None;
    }
    target_mapping(rt)?;
    let snap_off = options & D2D1_DRAW_TEXT_OPTIONS_NO_SNAP.0 != 0;
    if options & D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT.0 != 0 {
        let scan: IDWriteTextRenderer = Renderer::new(rt, brush, snap_off, true).into();
        // SAFETY: walking a live layout with our renderer.
        unsafe { layout.Draw(None, &scan, origin.X, origin.Y) }.ok()?;
        // SAFETY: `scan` is the Renderer made just above.
        if unsafe { scan.as_impl() }.scan.as_ref().is_some_and(Cell::get) {
            return None;
        }
    }
    let clip = options & D2D1_DRAW_TEXT_OPTIONS_CLIP.0 != 0;
    // SAFETY: getters and a push/pop pair on live objects.
    unsafe {
        if clip {
            let r = D2D_RECT_F { left: origin.X, top: origin.Y, right: origin.X + layout.GetMaxWidth(), bottom: origin.Y + layout.GetMaxHeight() };
            rt.PushAxisAlignedClip(&raw const r, D2D1_ANTIALIAS_MODE_ALIASED);
        }
        let renderer: IDWriteTextRenderer = Renderer::new(rt, brush, snap_off, false).into();
        let r = layout.Draw(None, &renderer, origin.X, origin.Y);
        if clip {
            rt.PopAxisAlignedClip();
        }
        // A walk that failed part-way has already drawn some of the layout;
        // handing it to Direct2D then would draw that part twice. Only a walk
        // that drew nothing falls back.
        // SAFETY: `renderer` is the Renderer made just above.
        (r.is_ok() || renderer.as_impl().drew.get()).then_some(())
    }
}

/// `DrawText` with render-core: lay the string out the way Direct2D does
/// (a text layout in the layout rectangle, GDI-compatible for the GDI
/// measuring modes) and draw it like `DrawTextLayout`.
fn draw_text(rt: &ID2D1RenderTarget, text: &[u16], format: &IDWriteTextFormat, rect: &D2D_RECT_F, brush: &ID2D1Brush, options: i32, measuring: i32) -> Option<()> {
    if !self_drawn(rt) {
        return None;
    }
    let (_, ppd) = target_mapping(rt)?;
    let (width, height) = ((rect.right - rect.left).max(0.0), (rect.bottom - rect.top).max(0.0));
    // SAFETY: creating a layout from the app's live format and string.
    let layout = unsafe {
        let factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).ok()?;
        if measuring == DWRITE_MEASURING_MODE_NATURAL.0 {
            factory.CreateTextLayout(text, format, width, height).ok()?
        } else {
            let mut t = Matrix3x2::default();
            rt.GetTransform(&raw mut t);
            let m = DWRITE_MATRIX { m11: t.M11, m12: t.M12, m21: t.M21, m22: t.M22, dx: t.M31, dy: t.M32 };
            let natural = measuring == DWRITE_MEASURING_MODE_GDI_NATURAL.0;
            factory.CreateGdiCompatibleTextLayout(text, format, width, height, ppd, Some(&raw const m), natural).ok()?
        }
    };
    draw_layout(rt, Vector2 { X: rect.left, Y: rect.top }, &layout, brush, options)
}

/// The `IDWriteTextRenderer` that `draw_layout` walks layouts with. In its
/// own module because the glue `windows::core::implement` generates trips
/// two pedantic lints (`#[inline(always)]` helpers, a reference cast to a raw
/// pointer) that are the macro's, not this code's.
mod text_renderer {
    #![allow(clippy::inline_always, clippy::ref_as_ptr)]

    use super::{
        draw_run, has_colour_glyphs, implement, line_rect, slot_orig, target_mapping, with_grid_fit_nudge, c_void, Cell,
        FnDrawGlyphRun, ID2D1Brush, ID2D1RenderTarget, IDWriteInlineObject, IDWritePixelSnapping_Impl, IDWriteTextRenderer,
        IDWriteTextRenderer_Impl, IUnknown, Interface, Matrix3x2, Ref, Vector2, BOOL, D2D_RECT_F, DWRITE_GLYPH_RUN,
        DWRITE_GLYPH_RUN_DESCRIPTION, DWRITE_MATRIX, DWRITE_MEASURING_MODE, DWRITE_STRIKETHROUGH, DWRITE_UNDERLINE,
        SLOT_DRAW_GLYPH_RUN,
    };

    /// Our `IDWriteTextRenderer`: draws what a text layout hands it onto `rt`.
    /// With `scan` set it draws nothing and only records whether any run has
    /// colour glyphs.
    #[implement(IDWriteTextRenderer)]
    pub(super) struct Renderer {
        pub(super) rt: ID2D1RenderTarget,
        pub(super) brush: ID2D1Brush,
        pub(super) snap_off: bool,
        pub(super) scan: Option<Cell<bool>>,
        /// Set once anything has been drawn.
        pub(super) drew: Cell<bool>,
    }

    impl Renderer {
        pub(super) fn new(rt: &ID2D1RenderTarget, brush: &ID2D1Brush, snap_off: bool, scan: bool) -> Renderer {
            Renderer { rt: rt.clone(), brush: brush.clone(), snap_off, scan: scan.then(|| Cell::new(false)), drew: Cell::new(false) }
        }

        /// The run's brush: its drawing effect when that is a brush (as in
        /// Direct2D's own layout drawing), else the default.
        fn brush_for(&self, effect: &Ref<IUnknown>) -> ID2D1Brush {
            effect.as_ref().and_then(|e| e.cast::<ID2D1Brush>().ok()).unwrap_or_else(|| self.brush.clone())
        }

        fn fill(&self, rect: D2D_RECT_F, effect: &Ref<IUnknown>) {
            if self.scan.is_none() {
                // SAFETY: a fill on a live target with a live brush.
                unsafe { self.rt.FillRectangle(&raw const rect, &self.brush_for(effect)) };
                self.drew.set(true);
            }
        }
    }

    impl IDWritePixelSnapping_Impl for Renderer_Impl {
        fn IsPixelSnappingDisabled(&self, _: *const c_void) -> windows::core::Result<BOOL> {
            Ok(self.snap_off.into())
        }

        fn GetCurrentTransform(&self, _: *const c_void, transform: *mut DWRITE_MATRIX) -> windows::core::Result<()> {
            let mut t = Matrix3x2::default();
            // SAFETY: a getter on a live target; `transform` is DirectWrite's out-parameter.
            unsafe {
                self.rt.GetTransform(&raw mut t);
                if let Some(out) = transform.as_mut() {
                    *out = DWRITE_MATRIX { m11: t.M11, m12: t.M12, m21: t.M21, m22: t.M22, dx: t.M31, dy: t.M32 };
                }
            }
            Ok(())
        }

        fn GetPixelsPerDip(&self, _: *const c_void) -> windows::core::Result<f32> {
            Ok(target_mapping(&self.rt).map_or(1.0, |(_, ppd)| ppd))
        }
    }

    impl IDWriteTextRenderer_Impl for Renderer_Impl {
        fn DrawGlyphRun(
            &self, _: *const c_void, x: f32, y: f32, measuring: DWRITE_MEASURING_MODE, run: *const DWRITE_GLYPH_RUN,
            desc: *const DWRITE_GLYPH_RUN_DESCRIPTION, effect: Ref<IUnknown>,
        ) -> windows::core::Result<()> {
            // SAFETY: `run` / `desc` are the layout's, live for the callback.
            let Some(r) = (unsafe { run.as_ref() }) else { return Ok(()) };
            let baseline = Vector2 { X: x, Y: y };
            if let Some(found) = &self.scan {
                if !found.get() {
                    found.set(has_colour_glyphs(&self.rt, baseline, r, desc, measuring.0));
                }
                return Ok(());
            }
            let brush = self.brush_for(&effect);
            self.drew.set(true);
            if draw_run(&self.rt, baseline, r, &brush, measuring.0).is_none() {
                // Not ours after all (an unreadable font): Direct2D's own draw.
                let this = self.rt.as_raw();
                // SAFETY: the original DrawGlyphRun of this target, with live arguments.
                unsafe {
                    if let Some(f) = slot_orig::<FnDrawGlyphRun>(this, SLOT_DRAW_GLYPH_RUN) {
                        with_grid_fit_nudge(&self.rt, || f(this, baseline, run, brush.as_raw(), measuring.0));
                    } else {
                        self.rt.DrawGlyphRun(baseline, run, &brush, measuring);
                    }
                }
            }
            Ok(())
        }

        fn DrawUnderline(&self, _: *const c_void, x: f32, y: f32, u: *const DWRITE_UNDERLINE, effect: Ref<IUnknown>) -> windows::core::Result<()> {
            // SAFETY: the layout's underline, live for the callback.
            if let Some(u) = unsafe { u.as_ref() } {
                self.fill(line_rect(x, y, u.width, u.offset, u.thickness, u.readingDirection), &effect);
            }
            Ok(())
        }

        fn DrawStrikethrough(&self, _: *const c_void, x: f32, y: f32, s: *const DWRITE_STRIKETHROUGH, effect: Ref<IUnknown>) -> windows::core::Result<()> {
            // SAFETY: the layout's strikethrough, live for the callback.
            if let Some(s) = unsafe { s.as_ref() } {
                self.fill(line_rect(x, y, s.width, s.offset, s.thickness, s.readingDirection), &effect);
            }
            Ok(())
        }

        fn DrawInlineObject(
            &self, ctx: *const c_void, x: f32, y: f32, object: Ref<IDWriteInlineObject>, sideways: BOOL, rtl: BOOL, effect: Ref<IUnknown>,
        ) -> windows::core::Result<()> {
            if self.scan.is_some() {
                return Ok(());
            }
            let Some(object) = object.as_ref() else { return Ok(()) };
            self.drew.set(true);
            let me: IDWriteTextRenderer = Renderer::new(&self.rt, &self.brush, self.snap_off, false).into();
            // SAFETY: the inline object draws itself through our renderer.
            unsafe { object.Draw(Some(ctx), &me, x, y, sideways.as_bool(), rtl.as_bool(), effect.as_ref()) }
        }
    }
}
use text_renderer::Renderer;

/// An underline / strikethrough box: `offset` below the baseline, extending
/// in the reading direction.
fn line_rect(x: f32, y: f32, width: f32, offset: f32, thickness: f32, dir: DWRITE_READING_DIRECTION) -> D2D_RECT_F {
    let (left, right) = if dir == DWRITE_READING_DIRECTION_RIGHT_TO_LEFT { (x - width, x) } else { (x, x + width) };
    D2D_RECT_F { left, top: y + offset, right, bottom: y + offset + thickness }
}

/// Does `run` hold colour glyphs (COLR / SVG / bitmap)? DirectWrite answers
/// `DWRITE_E_NOCOLOR` when it does not.
fn has_colour_glyphs(rt: &ID2D1RenderTarget, baseline: Vector2, run: &DWRITE_GLYPH_RUN, desc: *const DWRITE_GLYPH_RUN_DESCRIPTION, measuring: i32) -> bool {
    let mut t = Matrix3x2::default();
    // SAFETY: a getter, then a query on DirectWrite's shared factory with the
    // layout's live run.
    unsafe {
        rt.GetTransform(&raw mut t);
        let m = DWRITE_MATRIX { m11: t.M11, m12: t.M12, m21: t.M21, m22: t.M22, dx: t.M31, dy: t.M32 };
        DWriteCreateFactory::<IDWriteFactory2>(DWRITE_FACTORY_TYPE_SHARED).is_ok_and(|f| {
            f.TranslateColorGlyphRun(baseline.X, baseline.Y, run, Some(desc), DWRITE_MEASURING_MODE(measuring), Some(&raw const m), 0)
                .is_ok()
        })
    }
}

// ---- setup ----

/// Hook the three d2d1 exports that start the creation chains above.
pub(crate) fn setup_d2d_hook() {
    // SAFETY: loading/looking up a system DLL has no preconditions; d2d1
    // then stays loaded for the process's life.
    let loaded = unsafe { GetModuleHandleW(w!("d2d1.dll")).ok().filter(|h| !h.is_invalid()) };
    // SAFETY: as above.
    let d2d1 = loaded.or_else(|| unsafe { LoadLibraryW(w!("d2d1.dll")).ok() });
    let Some(d2d1) = d2d1 else {
        log("d2d1.dll not available");
        return;
    };
    // SAFETY: for all three, the export name is NUL-terminated, the export
    // stays mapped for the process's life, each detour matches its
    // signature, and each transmute types the trampoline, which continues
    // that same function.
    unsafe {
        hook_export(d2d1, "D2D1CreateFactory", s!("D2D1CreateFactory"), d2dcf_detour as *const (), |t| {
            let _ = ORIG_D2DCF.set(std::mem::transmute::<*const (), FnD2DCreateFactory>(t));
        });
        hook_export(d2d1, "D2D1CreateDevice", s!("D2D1CreateDevice"), d2dcd_detour as *const (), |t| {
            let _ = ORIG_D2DCD.set(std::mem::transmute::<*const (), FnD2DCreateDevice>(t));
        });
        hook_export(d2d1, "D2D1CreateDeviceContext", s!("D2D1CreateDeviceContext"), d2dcdc_detour as *const (), |t| {
            let _ = ORIG_D2DCDC.set(std::mem::transmute::<*const (), FnD2DCreateDevice>(t));
        });
    }
    if loaded.is_some() {
        probe_vtables();
    }
}

/// The app loaded Direct2D before the core arrived, so it may already hold
/// factories and render targets that never passed the creation hooks (the
/// core is injected at the first message pump; Scintilla, for one, creates
/// its factory before that - measured with Notepad++). Their vtables are the
/// ones every object of the class shares, so creating one of each here -
/// through the hooked entry points - patches them for the app's objects too.
/// Device contexts on DXGI surfaces would need a Direct3D device and are
/// not probed; they are still reached when created after the core arrives.
///
/// An HWND render target needs a window: a message-only one, made and
/// destroyed here on the attach thread (Scintilla creates its HWND target on
/// the first paint, which `UpdateWindow` sends before the message pump that
/// brings the core in - measured with Notepad++).
fn probe_vtables() {
    // Software targets: the probes must not bring up a GPU device (a driver
    // DLL load, under the loader lock, from every process that has Direct2D).
    let props = D2D1_RENDER_TARGET_PROPERTIES {
        r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
        pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_IGNORE },
        ..Default::default()
    };
    // SAFETY: a plain message-only window of a system class, destroyed below.
    let hwnd = unsafe { CreateWindowExW(WINDOW_EX_STYLE(0), w!("STATIC"), w!(""), WINDOW_STYLE(0), 0, 0, 8, 8, Some(HWND_MESSAGE), None, None, None) }.ok();
    let mut made = 0;
    for kind in [D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FACTORY_TYPE_MULTI_THREADED] {
        // SAFETY: creating a factory and render targets has no preconditions;
        // each goes through our detours, which patch its vtable, and all are
        // released at the end of the iteration.
        unsafe {
            let Ok(f) = D2D1CreateFactory::<ID2D1Factory>(kind, None) else { continue };
            made += u32::from(f.CreateDCRenderTarget(&raw const props).is_ok());
            if let Some(hwnd) = hwnd {
                let hp = D2D1_HWND_RENDER_TARGET_PROPERTIES { hwnd, pixelSize: D2D_SIZE_U { width: 8, height: 8 }, ..Default::default() };
                let base = D2D1_RENDER_TARGET_PROPERTIES { r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE, ..Default::default() };
                made += u32::from(f.CreateHwndRenderTarget(&raw const base, &raw const hp).is_ok());
            }
        }
    }
    if let Some(hwnd) = hwnd {
        // SAFETY: the window made above, on this thread.
        let _ = unsafe { DestroyWindow(hwnd) };
    }
    log(&format!("Direct2D was loaded before the core: patched through {made} probe render targets"));
}

/// Detour one export of `module`, logging the outcome.
///
/// # Safety
/// As `install_hook`, plus `sym` must be NUL-terminated.
unsafe fn hook_export(module: HMODULE, name: &str, sym: PCSTR, detour: *const (), publish: impl FnOnce(*const ())) {
    // SAFETY: forwarded from the caller's contract.
    let ok = unsafe { GetProcAddress(module, sym).is_some_and(|target| install_hook(target as *const (), detour, publish)) };
    log(&if ok { format!("hook installed on {name}") } else { format!("{name} hook failed") });
}
