//! FreeType driver: direct FFI to the fork's static `freetype64.lib` (no C
//! shim). FreeType does the glyph rasterisation; this module drives it with the
//! load flags / render mode that the upstream `FreeTypePrepare` selects for a
//! profile.
//!
//! Only the leading fields of `FT_FaceRec` / `FT_GlyphSlotRec` are declared:
//! FreeType allocates both, we only read through pointers, so trailing fields
//! can be left out. Offsets follow the fork's headers
//! (`vendor/freetype/include/freetype/{freetype,ftimage}.h`). Note that
//! `FT_Long` / `FT_Pos` / `FT_Fixed` are C `long`, i.e. **32-bit on Windows
//! x64** — hence `c_long`, never `i64`.
//!
//! The `FT_*` type names mirror the C ABI, and the numeric `as` casts here sit
//! at the FFI boundary (Rust ints ↔ C `long`/`uint`); the layout tests and the
//! render tests guard them. Hence the scoped allows.
#![allow(
    non_camel_case_types,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_lossless
)]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;
use std::ffi::{CStr, CString};
use std::os::raw::{c_long, c_uint, c_ulong};

use crate::config::{Aa, Profile};

// ---- FreeType ABI ---------------------------------------------------------

mod sys {
    use std::os::raw::{c_char, c_int, c_long, c_short, c_uchar, c_uint, c_ulong, c_ushort, c_void};

    pub type FT_Library = *mut c_void;
    pub type FT_Face = *mut FT_FaceRec;
    pub type FT_GlyphSlot = *mut FT_GlyphSlotRec;

    #[repr(C)]
    pub struct FT_Generic {
        pub data: *mut c_void,
        pub finalizer: Option<unsafe extern "C" fn(*mut c_void)>,
    }

    #[repr(C)]
    pub struct FT_BBox {
        pub x_min: c_long,
        pub y_min: c_long,
        pub x_max: c_long,
        pub y_max: c_long,
    }

    #[repr(C)]
    pub struct FT_Vector {
        pub x: c_long,
        pub y: c_long,
    }

    #[repr(C)]
    pub struct FT_Bitmap {
        pub rows: c_uint,
        pub width: c_uint,
        pub pitch: c_int,
        pub buffer: *mut c_uchar,
        pub num_grays: c_ushort,
        pub pixel_mode: c_uchar,
        pub palette_mode: c_uchar,
        pub palette: *mut c_void,
    }

    #[repr(C)]
    pub struct FT_Outline {
        pub n_contours: c_ushort,
        pub n_points: c_ushort,
        pub points: *mut FT_Vector,
        pub tags: *mut c_uchar,
        pub contours: *mut c_ushort,
        pub flags: c_int,
    }

    #[repr(C)]
    pub struct FT_Glyph_Metrics {
        pub width: c_long,
        pub height: c_long,
        pub hori_bearing_x: c_long,
        pub hori_bearing_y: c_long,
        pub hori_advance: c_long,
        pub vert_bearing_x: c_long,
        pub vert_bearing_y: c_long,
        pub vert_advance: c_long,
    }

    /// Leading fields of `FT_GlyphSlotRec` (through `outline`).
    #[repr(C)]
    pub struct FT_GlyphSlotRec {
        pub library: FT_Library,
        pub face: FT_Face,
        pub next: FT_GlyphSlot,
        pub glyph_index: c_uint,
        pub generic: FT_Generic,
        pub metrics: FT_Glyph_Metrics,
        pub linear_hori_advance: c_long,
        pub linear_vert_advance: c_long,
        pub advance: FT_Vector,
        pub format: c_uint,
        pub bitmap: FT_Bitmap,
        pub bitmap_left: c_int,
        pub bitmap_top: c_int,
        pub outline: FT_Outline,
    }

    /// Leading fields of `FT_FaceRec` (through `charmap`).
    #[repr(C)]
    pub struct FT_FaceRec {
        pub num_faces: c_long,
        pub face_index: c_long,
        pub face_flags: c_long,
        pub style_flags: c_long,
        pub num_glyphs: c_long,
        pub family_name: *mut c_char,
        pub style_name: *mut c_char,
        pub num_fixed_sizes: c_int,
        pub available_sizes: *mut c_void,
        pub num_charmaps: c_int,
        pub charmaps: *mut c_void,
        pub generic: FT_Generic,
        pub bbox: FT_BBox,
        pub units_per_em: c_ushort,
        pub ascender: c_short,
        pub descender: c_short,
        pub height: c_short,
        pub max_advance_width: c_short,
        pub max_advance_height: c_short,
        pub underline_position: c_short,
        pub underline_thickness: c_short,
        pub glyph: FT_GlyphSlot,
        pub size: *mut c_void,
        pub charmap: *mut c_void,
    }

    /// `FT_ENC_TAG('u','n','i','c')`.
    pub const FT_ENCODING_UNICODE: c_uint = 0x756E_6963;
    /// `FT_IMAGE_TAG('o','u','t','l')`.
    pub const FT_GLYPH_FORMAT_OUTLINE: c_uint = 0x6F75_746C;

    extern "C" {
        pub fn FT_Init_FreeType(alibrary: *mut FT_Library) -> c_int;
        pub fn FT_Done_FreeType(library: FT_Library) -> c_int;
        pub fn FT_New_Face(library: FT_Library, path: *const c_char, face_index: c_long, aface: *mut FT_Face) -> c_int;
        pub fn FT_New_Memory_Face(library: FT_Library, base: *const c_uchar, size: c_long, face_index: c_long, aface: *mut FT_Face) -> c_int;
        pub fn FT_Done_Face(face: FT_Face) -> c_int;
        pub fn FT_Select_Charmap(face: FT_Face, encoding: c_uint) -> c_int;
        pub fn FT_Set_Pixel_Sizes(face: FT_Face, pixel_width: c_uint, pixel_height: c_uint) -> c_int;
        pub fn FT_Set_Char_Size(face: FT_Face, char_width: c_long, char_height: c_long, horz_resolution: c_uint, vert_resolution: c_uint) -> c_int;
        pub fn FT_Set_Transform(face: FT_Face, matrix: *mut FT_Matrix, delta: *mut FT_Vector);
        pub fn FT_Load_Glyph(face: FT_Face, glyph_index: c_uint, load_flags: i32) -> c_int;
        pub fn FT_Get_Char_Index(face: FT_Face, charcode: c_ulong) -> c_uint;
        pub fn FT_Outline_EmboldenXY(outline: *mut FT_Outline, xstrength: c_long, ystrength: c_long) -> c_int;
        pub fn FT_Render_Glyph(slot: FT_GlyphSlot, render_mode: c_uint) -> c_int;
        pub fn FT_Library_SetLcdFilter(library: FT_Library, filter: c_uint) -> c_int;
        pub fn FT_Get_Sfnt_Name_Count(face: FT_Face) -> c_uint;
        pub fn FT_Get_Sfnt_Name(face: FT_Face, idx: c_uint, aname: *mut FT_SfntName) -> c_int;
    }

    /// `FT_Matrix` (fttypes.h): 16.16 fixed-point 2x2, applied as
    /// `x' = xx*x + xy*y`, `y' = yx*x + yy*y` in FreeType's y-up space.
    #[repr(C)]
    pub struct FT_Matrix {
        pub xx: c_long,
        pub xy: c_long,
        pub yx: c_long,
        pub yy: c_long,
    }

    /// `FT_SfntName` (ftsnames.h): one entry of the OpenType `name` table.
    #[repr(C)]
    pub struct FT_SfntName {
        pub platform_id: c_ushort,
        pub encoding_id: c_ushort,
        pub language_id: c_ushort,
        pub name_id: c_ushort,
        pub string: *mut c_uchar, // NOT NUL-terminated
        pub string_len: c_uint,   // bytes
    }
}

use sys::{
    FT_Done_Face, FT_Done_FreeType, FT_Face, FT_Get_Char_Index, FT_Get_Sfnt_Name, FT_Get_Sfnt_Name_Count, FT_Init_FreeType, FT_Library,
    FT_Library_SetLcdFilter, FT_SfntName,
    FT_Load_Glyph, FT_New_Face, FT_New_Memory_Face, FT_Outline_EmboldenXY, FT_Render_Glyph, FT_Select_Charmap,
    FT_Set_Pixel_Sizes, FT_Set_Char_Size, FT_Set_Transform, FT_Matrix, FT_ENCODING_UNICODE, FT_GLYPH_FORMAT_OUTLINE,
};

// FreeType constants (stable public ABI).
const FT_LOAD_NO_HINTING: i32 = 0x2;
const FT_LOAD_NO_BITMAP: i32 = 0x8;
const FT_LOAD_FORCE_AUTOHINT: i32 = 0x20;
const FT_LOAD_IGNORE_GLOBAL_ADVANCE_WIDTH: i32 = 0x200;
const FT_LOAD_TARGET_NORMAL: i32 = 0;
const FT_LOAD_TARGET_LIGHT: i32 = 1 << 16;
const FT_LOAD_TARGET_LCD: i32 = 3 << 16;
const FT_RENDER_MODE_NORMAL: i32 = 0;
const FT_RENDER_MODE_LCD: i32 = 3;
/// FreeType `FT_PIXEL_MODE_GRAY`.
pub const PIXEL_MODE_GRAY: i32 = 2;
/// FreeType `FT_PIXEL_MODE_LCD`.
pub const PIXEL_MODE_LCD: i32 = 5;

/// What a DirectWrite glyph run asks of a glyph beyond its index: a
/// fractional em size, a sideways rotation and DirectWrite's synthetic styles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlyphStyle {
    /// Em size in 1/64 pixel (26.6).
    pub size_26_6: i64,
    /// `isSideways`: the glyph is turned 90 degrees counter-clockwise.
    pub sideways: bool,
    /// `DWRITE_FONT_SIMULATIONS_BOLD`.
    pub bold: bool,
    /// `DWRITE_FONT_SIMULATIONS_OBLIQUE`.
    pub oblique: bool,
}

impl GlyphStyle {
    /// A plain glyph at `size_26_6`.
    pub fn at(size_26_6: i64) -> GlyphStyle {
        GlyphStyle { size_26_6, sideways: false, bold: false, oblique: false }
    }
}

/// Horizontal shear of DirectWrite's oblique simulation, as `x += y * 1/3`.
/// Measured against DirectWrite (verify/dwrite-probe, Yu Gothic UI "l"):
/// 29px of lean over 89px of stem at em 120, 15 over 44 at em 60.
const OBLIQUE_SHEAR_16_16: c_long = 0x1_0000 / 3;
/// DirectWrite's bold simulation widens by about em/40 and heightens by about
/// em/60 (measured the same way: +3 / +2 px at em 120, +2 / +1 at em 60).
const BOLD_X_DIV: i64 = 40;
const BOLD_Y_DIV: i64 = 60;

/// A rendered glyph: coverage bitmap plus placement. Owned (the bytes are
/// copied out of FreeType's glyph slot), so it can be cached and shared.
pub struct Glyph {
    pub width: i32,
    pub rows: i32,
    /// Bytes per row of `buffer`.
    pub pitch: i32,
    pub pixel_mode: i32,
    pub left: i32,
    pub top: i32,
    pub advance_px: i32, // integer pixels (26.6 >> 6)
    pub buffer: Vec<u8>,
}

/// Does `face` carry `want` as a family name? Checks FreeType's ASCII
/// `family_name` and then every Windows-platform (3) `name` table entry with
/// a family name id: 1 (family), 16 (typographic family), 21 (WWS family).
/// Those are UTF-16BE, which is how GDI's localized face names are stored.
/// Case-insensitive via Unicode simple lowercase.
///
/// # Safety
/// `face` must be a live `FT_Face`.
unsafe fn face_has_family(face: FT_Face, want: &str) -> bool {
    let want_lc: String = want.chars().flat_map(char::to_lowercase).collect();
    // SAFETY: caller guarantees a live face; `family_name` is null or NUL-terminated.
    let ascii = unsafe { (*face).family_name };
    if !ascii.is_null() {
        // SAFETY: as above.
        let bytes = unsafe { CStr::from_ptr(ascii) }.to_bytes();
        if bytes.eq_ignore_ascii_case(want.as_bytes()) {
            return true;
        }
    }
    // SAFETY: live face.
    let count = unsafe { FT_Get_Sfnt_Name_Count(face) };
    for idx in 0..count {
        let mut entry = FT_SfntName { platform_id: 0, encoding_id: 0, language_id: 0, name_id: 0, string: std::ptr::null_mut(), string_len: 0 };
        // SAFETY: `entry` is an out-param; FreeType fills it on 0.
        if unsafe { FT_Get_Sfnt_Name(face, idx, &raw mut entry) } != 0 { continue; }
        if entry.platform_id != 3 || !matches!(entry.name_id, 1 | 16 | 21) || entry.string.is_null() { continue; }
        // SAFETY: `string` points at `string_len` bytes owned by the face,
        // valid until the face is closed.
        let raw = unsafe { std::slice::from_raw_parts(entry.string, entry.string_len as usize) };
        let units: Vec<u16> = raw.as_chunks::<2>().0.iter().map(|b| u16::from_be_bytes(*b)).collect();
        let name_lc: String = char::decode_utf16(units.iter().copied()).filter_map(Result::ok).flat_map(char::to_lowercase).collect();
        if name_lc == want_lc {
            return true;
        }
    }
    false
}

/// An open face and what keeps it readable: the font-file bytes for a face
/// opened from memory (FreeType reads them for the face's whole life), or
/// nothing for one opened from a path (FreeType reads the file on demand).
struct FaceSlot {
    key: u64,
    face: FT_Face,
    bytes: Vec<u8>,
}

/// At most this many faces stay open; the least recently used is closed.
const MAX_FACES: usize = 8;
/// Faces opened from memory hold a copy of the whole font file (a CJK font
/// is 10-20 MB), so their bytes are capped too.
const MAX_FACE_BYTES: usize = 64 << 20;
/// Rendered glyphs are kept up to this many bytes, then dropped wholesale.
const MAX_GLYPH_BYTES: usize = 8 << 20;
/// Keys handed out for faces opened without one (never looked up again).
const UNKEYED: u64 = 1 << 63;

/// What a cached glyph was rendered from: the face, the glyph, its size and
/// style, and every FreeType setting that changes the bitmap.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphKey {
    face: u64,
    gi: u32,
    /// 26.6 em size for styled glyphs; whole pixels (negated) otherwise.
    size: i64,
    style: u8,
    load_flags: i32,
    render_mode: i32,
    lcd_filter: i32,
    embolden: i32,
}

/// Owned FreeType library, a handful of open faces (one of them active) and
/// a cache of rendered glyphs. `&self` methods mutate FreeType state (the
/// glyph slot, the active face) exactly as the C globals did; callers
/// serialise access (render-inject keeps the `Ft` behind a mutex).
pub struct Ft {
    lib: FT_Library,
    /// The active face (one of `faces`), or null.
    face: Cell<FT_Face>,
    active: Cell<u64>,
    /// Open faces, least recently used first.
    faces: RefCell<Vec<FaceSlot>>,
    next_unkeyed: Cell<u64>,
    glyphs: RefCell<HashMap<GlyphKey, Arc<Glyph>>>,
    glyph_bytes: Cell<usize>,
}

impl Ft {
    /// Initialise FreeType with no face yet.
    pub fn new() -> Result<Ft, i32> {
        let mut lib: FT_Library = std::ptr::null_mut();
        // SAFETY: `lib` is an out-param FreeType fills; checked before use.
        let r = unsafe { FT_Init_FreeType(&raw mut lib) };
        if r != 0 { return Err(r); }
        Ok(Ft {
            lib,
            face: Cell::new(std::ptr::null_mut()),
            active: Cell::new(0),
            faces: RefCell::new(Vec::new()),
            next_unkeyed: Cell::new(UNKEYED),
            glyphs: RefCell::new(HashMap::new()),
            glyph_bytes: Cell::new(0),
        })
    }

    /// Initialise FreeType and open a face from a font file.
    pub fn open(path: &str, face_index: i64) -> Result<Ft, i32> {
        let ft = Ft::new()?;
        let key = ft.unkeyed();
        ft.open_path(key, path, face_index)?;
        Ok(ft)
    }

    fn unkeyed(&self) -> u64 {
        let k = self.next_unkeyed.get();
        self.next_unkeyed.set(k.wrapping_add(1) | UNKEYED);
        k
    }

    /// Make the face opened under `key` the active one, if it is still open.
    /// Callers key a face by what identifies its font (a file path and face
    /// index), so the file is opened - or read - only once.
    pub fn activate(&self, key: u64) -> bool {
        let mut faces = self.faces.borrow_mut();
        let Some(i) = faces.iter().position(|s| s.key == key) else { return false };
        let slot = faces.remove(i);
        self.face.set(slot.face);
        self.active.set(key);
        faces.push(slot);
        true
    }

    /// Close the face opened under `key`, if open (it is no longer active),
    /// and forget the glyphs rendered from it. For a key that names an object
    /// whose identity may be reused: the next face under that key may be
    /// another font, and must not be handed this one's glyphs.
    pub fn close(&self, key: u64) {
        self.forget_glyphs(key);
        let mut faces = self.faces.borrow_mut();
        let Some(i) = faces.iter().position(|s| s.key == key) else { return };
        let slot = faces.remove(i);
        if self.active.get() == key {
            self.face.set(std::ptr::null_mut());
            self.active.set(0);
        }
        // SAFETY: the face we owned, no longer active or listed; closed once.
        unsafe { FT_Done_Face(slot.face); }
    }

    /// Open face `index` of the font file at `path` under `key` and make it
    /// active. FreeType reads the file on demand; nothing is copied.
    pub fn open_path(&self, key: u64, path: &str, index: i64) -> Result<(), i32> {
        let c = CString::new(path).map_err(|_| -1)?;
        let mut face: FT_Face = std::ptr::null_mut();
        // SAFETY: `lib` is live, `c` is a NUL-terminated path, `face` is an
        // out-param; a non-zero return means `face` was not set.
        let r = unsafe { FT_New_Face(self.lib, c.as_ptr(), index as c_long, &raw mut face) };
        if r != 0 { return Err(r); }
        self.adopt(key, face, Vec::new());
        Ok(())
    }

    /// Open face `index` of the in-memory font file `data` under `key` and
    /// make it active. The bytes are kept for the face's life.
    pub fn open_memory(&self, key: u64, data: Vec<u8>, index: i64) -> Result<(), i32> {
        let mut face: FT_Face = std::ptr::null_mut();
        // SAFETY: `data` is moved into the face's slot below and never
        // reallocated, so the pointer stays valid for the face's life.
        let r = unsafe { FT_New_Memory_Face(self.lib, data.as_ptr(), data.len() as c_long, index as c_long, &raw mut face) };
        if r != 0 { return Err(r); }
        self.adopt(key, face, data);
        Ok(())
    }

    /// Like `open_memory`, but for a TTC pick the face whose family is
    /// `want_family` (case-insensitive), checking every `name` table family
    /// entry so a localized GDI face name ("BIZ UDPゴシック", "游ゴシック")
    /// finds its face too; face 0 otherwise.
    pub fn open_memory_family(&self, key: u64, data: Vec<u8>, want_family: &str) -> Result<(), i32> {
        let (base, len) = (data.as_ptr(), data.len() as c_long);
        let mut chosen: c_long = 0;
        if !want_family.is_empty() {
            // Probe face -1 for the face count, then open each to compare names.
            let mut probe: FT_Face = std::ptr::null_mut();
            // SAFETY: `base`/`len` describe `data`, live for this call; face
            // index -1 asks FreeType for the face count.
            let perr = unsafe { FT_New_Memory_Face(self.lib, base, len, -1, &raw mut probe) };
            // SAFETY: `probe` is live iff the call returned 0.
            let n = if perr == 0 { unsafe { (*probe).num_faces } } else { 1 };
            if perr == 0 {
                // SAFETY: closing the probe face we opened.
                unsafe { FT_Done_Face(probe); }
            }
            for i in 0..n {
                let mut f: FT_Face = std::ptr::null_mut();
                // SAFETY: as for the probe; `f` is set only on a 0 return.
                if unsafe { FT_New_Memory_Face(self.lib, base, len, i, &raw mut f) } != 0 { continue; }
                // SAFETY: `f` is a live face for the duration of the check.
                let matched = unsafe { face_has_family(f, want_family) };
                // SAFETY: closing the face we opened for the name compare.
                unsafe { FT_Done_Face(f); }
                if matched { chosen = i; break; }
            }
        }
        self.open_memory(key, data, i64::from(chosen))
    }

    /// Swap the active face to an in-memory font file (e.g. GDI `GetFontData`
    /// bytes), choosing a TTC face by family as `open_memory_family`. The
    /// face is not looked up again: callers that repeat a font use a key.
    pub fn reface_memory(&self, data: &[u8], want_family: &str) -> Result<(), i32> {
        let key = self.unkeyed();
        self.open_memory_family(key, data.to_vec(), want_family)
    }

    /// Swap the active face to an in-memory font file at a specific face index.
    pub fn reface_memory_index(&self, data: &[u8], index: i64) -> Result<(), i32> {
        let key = self.unkeyed();
        self.open_memory(key, data.to_vec(), index)
    }

    /// Take `face` (already open) as the active one under `key`, closing the
    /// least recently used faces past the limits. A face opened again under a
    /// key already present replaces the old one.
    fn adopt(&self, key: u64, face: FT_Face, bytes: Vec<u8>) {
        // SAFETY: `face` is a live FT_Face we just took ownership of.
        unsafe { FT_Select_Charmap(face, FT_ENCODING_UNICODE); }
        // A face (re)opened under a key starts with no glyphs: whatever was
        // cached under it came from the previous face.
        self.forget_glyphs(key);
        let mut faces = self.faces.borrow_mut();
        if let Some(i) = faces.iter().position(|s| s.key == key) {
            let old = faces.remove(i);
            // SAFETY: the face we owned; the active pointer is replaced below.
            unsafe { FT_Done_Face(old.face); }
        }
        faces.push(FaceSlot { key, face, bytes });
        self.face.set(face);
        self.active.set(key);
        let mut bytes: usize = faces.iter().map(|s| s.bytes.len()).sum();
        while faces.len() > 1 && (faces.len() > MAX_FACES || bytes > MAX_FACE_BYTES) {
            let old = faces.remove(0);
            bytes -= old.bytes.len();
            // SAFETY: not the active face (that is the last one); closed once.
            unsafe { FT_Done_Face(old.face); }
        }
    }

    /// filter: FT_LCD_FILTER_* (0 NONE, 1 DEFAULT, 2 LIGHT, 3 LEGACY1, 16 LEGACY)
    fn set_lcd_filter(&self, filter: i32) {
        // SAFETY: `self.lib` is a live FreeType library.
        unsafe { FT_Library_SetLcdFilter(self.lib, filter as c_uint); }
    }

    /// FreeType load flags + render mode for a profile's AA + hinting, as
    /// upstream `FreeTypePrepare` (ft.cpp): AntiAliasMode 2/3 load with
    /// `FT_LOAD_TARGET_LCD`, 4/5 (LightLCD) with `FT_LOAD_TARGET_LIGHT` — the
    /// light autohinter, vertical snapping only — and both render LCD.
    fn flags(p: &Profile) -> (i32, i32) {
        let base = FT_LOAD_NO_BITMAP | FT_LOAD_IGNORE_GLOBAL_ADVANCE_WIDTH;
        let target = if p.aa.is_light() { FT_LOAD_TARGET_LIGHT } else if p.aa.is_lcd() { FT_LOAD_TARGET_LCD } else { FT_LOAD_TARGET_NORMAL };
        let render = if p.aa.is_lcd() { FT_RENDER_MODE_LCD } else { FT_RENDER_MODE_NORMAL };
        let mut flags = base | target;
        match p.hinting {
            1 => flags |= FT_LOAD_NO_HINTING,
            2 => flags |= FT_LOAD_FORCE_AUTOHINT,
            _ => {}
        }
        (flags, render)
    }

    /// Prepare the library-global LCD filter for a profile (call before a run).
    pub fn prepare(&self, p: &Profile) {
        if p.aa.is_lcd() {
            self.set_lcd_filter(p.lcd_filter);
        }
    }

    /// Render one character at `px` pixels through `p`. Returns `None` only on
    /// a hard error; a missing/empty glyph yields an empty `Glyph` (advance only).
    pub fn render(&self, ch: char, px: i32, p: &Profile) -> Option<Arc<Glyph>> {
        let face = self.face.get();
        if face.is_null() { return None; }
        // SAFETY: `face` is a live FT_Face (null-checked above).
        let gi = unsafe { FT_Get_Char_Index(face, ch as c_ulong) };
        self.emit(gi, px, p)
    }

    /// Render a glyph by its font glyph index (for ETO_GLYPH_INDEX draws).
    pub fn render_glyph(&self, gi: u16, px: i32, p: &Profile) -> Option<Arc<Glyph>> {
        self.emit(gi as c_uint, px, p)
    }

    /// The cache key for glyph `gi` of the active face at `size` / `style`
    /// through profile `p`.
    fn key(&self, gi: c_uint, size: i64, style: u8, p: &Profile) -> GlyphKey {
        let (load_flags, render_mode) = Self::flags(p);
        GlyphKey {
            face: self.active.get(),
            gi,
            size,
            style,
            load_flags,
            render_mode,
            lcd_filter: if p.aa.is_lcd() { p.lcd_filter } else { 0 },
            embolden: p.embolden,
        }
    }

    /// Drop the cached glyphs rendered from the face under `face`.
    fn forget_glyphs(&self, face: u64) {
        let mut map = self.glyphs.borrow_mut();
        let before = map.len();
        map.retain(|k, _| k.face != face);
        if map.len() != before {
            let bytes = map.values().map(|g| g.buffer.len() + core::mem::size_of::<Glyph>()).sum();
            self.glyph_bytes.set(bytes);
        }
    }

    /// The glyph for `key`, rendering it with `render` on a miss.
    fn cached(&self, key: GlyphKey, render: impl FnOnce() -> Option<Glyph>) -> Option<Arc<Glyph>> {
        if let Some(g) = self.glyphs.borrow().get(&key) {
            return Some(Arc::clone(g));
        }
        let g = Arc::new(render()?);
        let size = g.buffer.len() + core::mem::size_of::<Glyph>();
        let mut map = self.glyphs.borrow_mut();
        if self.glyph_bytes.get() + size > MAX_GLYPH_BYTES {
            map.clear();
            self.glyph_bytes.set(0);
        }
        map.insert(key, Arc::clone(&g));
        self.glyph_bytes.set(self.glyph_bytes.get() + size);
        Some(g)
    }

    /// Render glyph `gi` the way a DirectWrite run asks for it: at a
    /// fractional size, turned sideways and/or with the synthetic styles.
    /// The transform is reset afterwards, so `render`/`render_glyph` are
    /// unaffected.
    pub fn render_glyph_styled(&self, gi: u16, style: &GlyphStyle, p: &Profile) -> Option<Arc<Glyph>> {
        let face = self.face.get();
        if face.is_null() || style.size_26_6 <= 0 { return None; }
        let bits = u8::from(style.sideways) | u8::from(style.bold) << 1 | u8::from(style.oblique) << 2;
        self.cached(self.key(c_uint::from(gi), style.size_26_6, bits, p), || {
            // Shear first (the oblique lean is in the glyph's own frame), then
            // the quarter turn: rotate(90 ccw) * shear.
            let (mut xx, mut xy, mut yx, mut yy): (c_long, c_long, c_long, c_long) =
                (0x1_0000, if style.oblique { OBLIQUE_SHEAR_16_16 } else { 0 }, 0, 0x1_0000);
            if style.sideways {
                (xx, xy, yx, yy) = (-yx, -yy, xx, xy);
            }
            let mut m = FT_Matrix { xx, xy, yx, yy };
            let mut identity = FT_Matrix { xx: 0x1_0000, xy: 0, yx: 0, yy: 0x1_0000 };
            let bold = style.bold.then_some((style.size_26_6 / BOLD_X_DIV, style.size_26_6 / BOLD_Y_DIV));
            // SAFETY: `face` is live; the matrix pointers are valid for the
            // calls and FreeType copies them.
            unsafe {
                if FT_Set_Char_Size(face, 0, style.size_26_6 as c_long, 72, 72) != 0 { return None; }
                FT_Set_Transform(face, &raw mut m, std::ptr::null_mut());
            }
            let g = self.load_render(c_uint::from(gi), p, bold);
            // SAFETY: as above.
            unsafe { FT_Set_Transform(face, &raw mut identity, std::ptr::null_mut()); }
            g
        })
    }

    /// Load + render glyph `gi` at `px` pixels, through the cache.
    fn emit(&self, gi: c_uint, px: i32, p: &Profile) -> Option<Arc<Glyph>> {
        let face = self.face.get();
        if face.is_null() { return None; }
        // Whole-pixel sizes are keyed negated, apart from 26.6 ones.
        self.cached(self.key(gi, -i64::from(px), 0, p), || {
            // SAFETY: face is a live FT_Face from FreeType.
            if unsafe { FT_Set_Pixel_Sizes(face, 0, px as c_uint) } != 0 { return None; }
            self.load_render(gi, p, None)
        })
    }

    /// Load + render `gi` at the face's current size and transform, and copy
    /// the bitmap out of the slot. `bold` is DirectWrite's synthetic
    /// emboldening (x, y strength in 26.6), applied on top of the profile's own.
    fn load_render(&self, gi: c_uint, p: &Profile, bold: Option<(i64, i64)>) -> Option<Glyph> {
        let face = self.face.get();
        if face.is_null() { return None; }
        let (flags, render_mode) = Self::flags(p);
        // SAFETY: face is a live FT_Face from FreeType; slot is owned by it.
        unsafe {
            if FT_Load_Glyph(face, gi, flags) != 0 { return None; }
            let slot = (*face).glyph;
            if (*slot).format == FT_GLYPH_FORMAT_OUTLINE {
                let (bx, by) = bold.unwrap_or((0, 0));
                let (ex, ey) = (p.embolden as i64 + bx, p.embolden as i64 + by);
                if ex != 0 || ey != 0 {
                    FT_Outline_EmboldenXY(&raw mut (*slot).outline, ex as c_long, ey as c_long);
                }
            }
            if FT_Render_Glyph(slot, render_mode as c_uint) != 0 { return None; }
            let s = &*slot;
            let (left, top, advance_px) = (s.bitmap_left, s.bitmap_top, s.advance.x >> 6);
            let b = &s.bitmap;
            if b.buffer.is_null() || b.rows == 0 {
                return Some(Glyph {
                    width: 0, rows: 0, pitch: 0, pixel_mode: b.pixel_mode as i32,
                    left, top, advance_px, buffer: Vec::new(),
                });
            }
            // Copy row by row: a negative pitch means bottom-up rows.
            let stride = b.pitch.unsigned_abs() as usize;
            let rows = b.rows as usize;
            let mut buffer = Vec::with_capacity(stride * rows);
            for r in 0..rows {
                let row = if b.pitch >= 0 { b.buffer.add(r * stride) } else { b.buffer.add((rows - 1 - r) * stride) };
                buffer.extend_from_slice(std::slice::from_raw_parts(row, stride));
            }
            Some(Glyph {
                width: b.width as i32, rows: b.rows as i32, pitch: stride as i32, pixel_mode: b.pixel_mode as i32,
                left, top, advance_px, buffer,
            })
        }
    }
}

impl Drop for Ft {
    fn drop(&mut self) {
        for slot in self.faces.get_mut().drain(..) {
            // SAFETY: each face is ours and closed once, before the library.
            unsafe { FT_Done_Face(slot.face); }
        }
        // SAFETY: called once, from Drop, after the faces are released.
        unsafe { FT_Done_FreeType(self.lib); }
    }
}

/// Convenience: does this profile need BGR subpixel order?
pub fn is_bgr(aa: Aa) -> bool {
    matches!(aa, Aa::LcdBgr | Aa::LightLcdBgr)
}

#[cfg(test)]
mod family_tests {
    use super::*;

    /// A TTC face must be found by its localized (Japanese) GDI face name,
    /// not only the ASCII family name; otherwise GDI's "BIZ UDPゴシック"
    /// (proportional) fell back to face 0 = BIZ UDGothic (monospace), which
    /// is what made menu text look evenly-spaced and off. Skips without the
    /// font.
    #[test]
    fn ttc_face_by_localized_name() {
        const FONT: &str = r"C:\Windows\Fonts\BIZ-UDGothicR.ttc";
        let Ok(bytes) = std::fs::read(FONT) else { eprintln!("skip: no {FONT}"); return; };
        let Ok(ft) = Ft::new() else { eprintln!("skip: FT init failed"); return; };
        let p = Profile::clean_greyscale();
        let adv = |ft: &Ft, ch: char| ft.render(ch, 24, &p).map_or(0, |g| g.advance_px);

        assert!(ft.reface_memory(&bytes, "BIZ UDPゴシック").is_ok());
        let (i_p, m_p) = (adv(&ft, 'i'), adv(&ft, 'm'));
        assert!(i_p < m_p, "proportional face expected: i={i_p} m={m_p}");

        assert!(ft.reface_memory(&bytes, "BIZ UDGothic").is_ok());
        let (i_m, m_m) = (adv(&ft, 'i'), adv(&ft, 'm'));
        assert_eq!(i_m, m_m, "monospace face expected: i={i_m} m={m_m}");

        // ASCII name still works, case-insensitively.
        assert!(ft.reface_memory(&bytes, "biz udpgothic").is_ok());
        assert!(adv(&ft, 'i') < adv(&ft, 'm'));
    }
}

#[cfg(test)]
mod flag_tests {
    use super::*;
    use crate::config::Aa;

    /// Load targets follow upstream `FreeTypePrepare`: Grey → NORMAL,
    /// LCD → TARGET_LCD, LightLCD → TARGET_LIGHT (still rendered LCD).
    #[test]
    fn load_target_per_aa_mode_matches_upstream() {
        let mut p = Profile::clean_greyscale();
        let (f, r) = Ft::flags(&p);
        assert_eq!(f & (0xF << 16), FT_LOAD_TARGET_NORMAL);
        assert_eq!(r, FT_RENDER_MODE_NORMAL);
        p.aa = Aa::LcdRgb;
        let (f, r) = Ft::flags(&p);
        assert_eq!(f & (0xF << 16), FT_LOAD_TARGET_LCD);
        assert_eq!(r, FT_RENDER_MODE_LCD);
        for aa in [Aa::LightLcdRgb, Aa::LightLcdBgr] {
            p.aa = aa;
            let (f, r) = Ft::flags(&p);
            assert_eq!(f & (0xF << 16), FT_LOAD_TARGET_LIGHT, "{aa:?}");
            assert_eq!(r, FT_RENDER_MODE_LCD);
        }
        p.hinting = 1;
        assert_ne!(Ft::flags(&p).0 & FT_LOAD_NO_HINTING, 0);
        p.hinting = 2;
        assert_ne!(Ft::flags(&p).0 & FT_LOAD_FORCE_AUTOHINT, 0);
    }
}

#[cfg(test)]
mod layout_tests {
    use super::sys::*;
    use std::mem::{offset_of, size_of};

    // Offsets on x64 Windows (long = 4 bytes), derived from the fork's headers.
    #[test]
    fn face_rec_offsets() {
        assert_eq!(offset_of!(FT_FaceRec, family_name), 24);
        assert_eq!(offset_of!(FT_FaceRec, glyph), 24 + 8 + 8 + 4 + 4 + 8 + 4 + 4 + 8 + 16 + 16 + 2 + 14);
    }

    #[test]
    fn glyph_slot_offsets() {
        assert_eq!(size_of::<FT_Bitmap>(), 40);
        assert_eq!(offset_of!(FT_GlyphSlotRec, metrics), 48);
        assert_eq!(offset_of!(FT_GlyphSlotRec, advance), 48 + 32 + 8);
        assert_eq!(offset_of!(FT_GlyphSlotRec, format), 96);
        assert_eq!(offset_of!(FT_GlyphSlotRec, bitmap), 104);
        assert_eq!(offset_of!(FT_GlyphSlotRec, bitmap_left), 144);
        assert_eq!(offset_of!(FT_GlyphSlotRec, outline), 152);
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    /// Faces stay open under their keys and come back without reopening;
    /// past `MAX_FACES` the least recently used one is closed.
    #[test]
    fn faces_are_kept_by_key() {
        const FONT: &str = r"C:\Windows\Fonts\meiryo.ttc";
        let Ok(ft) = Ft::new() else { return };
        if ft.open_path(1, FONT, 0).is_err() {
            eprintln!("skip: no {FONT}");
            return;
        }
        assert!(ft.open_path(2, FONT, 1).is_ok());
        assert!(ft.activate(1), "still open");
        assert!(!ft.activate(99), "never opened");
        for k in 10..10 + MAX_FACES as u64 {
            assert!(ft.open_path(k, FONT, 0).is_ok());
        }
        assert!(!ft.activate(2), "the least recently used face was closed");
        ft.close(10);
        assert!(!ft.activate(10));
        assert!(ft.activate(11));
    }

    /// A glyph is rasterised once per face / size / style / settings: the
    /// second request returns the cached bitmap itself.
    #[test]
    fn glyphs_are_cached() {
        const FONT: &str = r"C:\Windows\Fonts\meiryo.ttc";
        let Ok(ft) = Ft::open(FONT, 0) else { eprintln!("skip: no {FONT}"); return };
        let p = Profile::clean_greyscale();
        let a = ft.render_glyph(36, 20, &p).expect("glyph");
        let b = ft.render_glyph(36, 20, &p).expect("glyph");
        assert!(Arc::ptr_eq(&a, &b), "cached");
        let c = ft.render_glyph(36, 21, &p).expect("glyph");
        assert!(!Arc::ptr_eq(&a, &c), "another size is another glyph");
        // At 100px the synthetic bold adds 2.5px (em/40): enough to show.
        let s1 = ft.render_glyph_styled(36, &GlyphStyle::at(100 * 64), &p).expect("glyph");
        let s2 = ft.render_glyph_styled(36, &GlyphStyle { bold: true, ..GlyphStyle::at(100 * 64) }, &p).expect("glyph");
        assert!(!Arc::ptr_eq(&s1, &s2), "a style is part of the key");
        assert!(s2.width > s1.width, "synthetic bold widens");
    }

    /// Closing a face, or opening another under its key, forgets the glyphs
    /// cached under that key: the next face there may be another font.
    #[test]
    fn a_reused_key_does_not_serve_the_old_glyphs() {
        const FONT: &str = r"C:\Windows\Fonts\meiryo.ttc";
        let Ok(ft) = Ft::new() else { return };
        if ft.open_path(7, FONT, 0).is_err() {
            eprintln!("skip: no {FONT}");
            return;
        }
        let p = Profile::clean_greyscale();
        let first = ft.render_glyph(36, 20, &p).expect("glyph");
        ft.close(7);
        assert!(ft.open_path(7, FONT, 1).is_ok(), "another face under the same key");
        let other = ft.render_glyph(36, 20, &p).expect("glyph");
        assert!(!Arc::ptr_eq(&first, &other), "rendered again, not served from the closed face");
        let again = ft.render_glyph(36, 20, &p).expect("glyph");
        assert!(ft.open_path(7, FONT, 0).is_ok(), "reopened under the key without closing");
        let reopened = ft.render_glyph(36, 20, &p).expect("glyph");
        assert!(!Arc::ptr_eq(&again, &reopened));
    }
}
