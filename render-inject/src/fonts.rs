//! Which font file a draw uses, and opening it in `render-core` once.
//!
//! `Ft` keeps a few faces open under keys (see `Ft::activate`). A key names
//! the font, not the object a draw handed us, so the file is opened - or
//! read - only the first time:
//!
//! * A local font file (system and user fonts) is opened by path: FreeType
//!   reads it on demand and nothing is copied. Its key is the path plus the
//!   face index. FreeType opens paths as ANSI, so a path with other
//!   characters is read into memory instead.
//! * Anything else (a font an app supplies from memory) is copied into memory.
//!
//! GDI draws key their font the way they always did (face name and data
//! size) and read it with `GetFontData` on a miss; see `gdi.rs`.

use core::ffi::c_void;
use std::hash::{DefaultHasher, Hash, Hasher};

use render_core::Ft;
use windows::core::Interface;
use windows::Win32::Graphics::DirectWrite::{IDWriteFontFace, IDWriteFontFile, IDWriteLocalFontFileLoader};

use crate::state::{DwFace, RenderState};

/// DirectWrite faces remembered by object (and kept alive by the clone).
const MAX_DW_FACES: usize = 32;

/// A face key from what identifies a font. Below 2^63: keys at or above it
/// are `Ft`'s own unkeyed ones.
pub(crate) fn hash_of(parts: impl Hash) -> u64 {
    let mut h = DefaultHasher::new();
    parts.hash(&mut h);
    h.finish() >> 1
}

/// Where a DirectWrite face's bytes come from.
enum FontSource {
    /// A local file and the face index in it.
    Path(String, u32),
    /// The file's bytes and the face index.
    Memory(Vec<u8>, u32),
}

/// Open `src` in `ft` under `key` (and make it active).
fn open(ft: &Ft, key: u64, src: FontSource) -> Option<()> {
    match src {
        FontSource::Path(path, index) => ft.open_path(key, &path, i64::from(index)).ok(),
        FontSource::Memory(bytes, index) => ft.open_memory(key, bytes, i64::from(index)).ok(),
    }
}

// ---- DirectWrite faces ----

/// Make `st`'s active face `face`'s font, opening it the first time.
pub(crate) fn reface(st: &mut RenderState, face: &IDWriteFontFace) -> Option<()> {
    let raw = face.as_raw();
    if let Some(i) = st.dw_faces.iter().position(|f| f.face.as_raw() == raw) {
        let known = st.dw_faces.remove(i);
        let ok = st.ft.activate(known.key) || source_of(face).is_some_and(|src| open(&st.ft, known.key, src).is_some());
        st.dw_faces.push(known);
        return ok.then_some(());
    }
    let (key, memory, src) = match local_font(face) {
        Some((path, index)) if path.is_ascii() => (hash_of(("path", path.to_lowercase(), index)), false, None),
        _ => {
            // Keyed by the object, which the clone below keeps alive, so the
            // address cannot come back as another font while remembered.
            // SAFETY: a getter on a live face.
            (hash_of(("mem", raw.addr(), unsafe { face.GetIndex() })), true, Some(source_of(face)?))
        }
    };
    if !st.ft.activate(key) {
        open(&st.ft, key, src.or_else(|| source_of(face))?)?;
    }
    st.dw_faces.push(DwFace { face: face.clone(), key, memory });
    if st.dw_faces.len() > MAX_DW_FACES {
        let old = st.dw_faces.remove(0);
        if old.memory {
            // The object goes; its address may return as another font.
            st.ft.close(old.key);
        }
    }
    Some(())
}

/// How to open `face`: by path when it is a local file with an ANSI path,
/// else from its bytes.
fn source_of(face: &IDWriteFontFace) -> Option<FontSource> {
    // SAFETY: a getter on a live face.
    let index = unsafe { face.GetIndex() };
    match local_font(face) {
        Some((path, index)) if path.is_ascii() => Some(FontSource::Path(path, index)),
        _ => Some(FontSource::Memory(font_bytes(face)?, index)),
    }
}

/// The first font file behind `face`.
fn first_file(face: &IDWriteFontFace) -> Option<IDWriteFontFile> {
    // SAFETY: COM calls on a live face; `files` is sized by the first call.
    unsafe {
        let mut n = 0u32;
        face.GetFiles(&raw mut n, None).ok()?;
        if n == 0 {
            return None;
        }
        let mut files: Vec<Option<IDWriteFontFile>> = vec![None; n as usize];
        face.GetFiles(&raw mut n, Some(files.as_mut_ptr())).ok()?;
        files.into_iter().next()?
    }
}

/// The local file path and face index of `face`, when its file comes from
/// DirectWrite's local file loader (system and user fonts).
pub(crate) fn local_font(face: &IDWriteFontFace) -> Option<(String, u32)> {
    let file = first_file(face)?;
    // SAFETY: COM calls on live objects; the key is DirectWrite's, valid
    // while `file` lives, and `buf` holds the reported length plus the NUL.
    unsafe {
        let mut key: *mut c_void = core::ptr::null_mut();
        let mut size = 0u32;
        file.GetReferenceKey(&raw mut key, &raw mut size).ok()?;
        let loader: IDWriteLocalFontFileLoader = file.GetLoader().ok()?.cast().ok()?;
        let len = loader.GetFilePathLengthFromKey(key.cast_const(), size).ok()? as usize;
        let mut buf = vec![0u16; len + 1];
        loader.GetFilePathFromKey(key.cast_const(), size, &mut buf).ok()?;
        Some((String::from_utf16_lossy(&buf[..len]), face.GetIndex()))
    }
}

/// The font-file bytes behind a DirectWrite font face.
pub(crate) fn font_bytes(face: &IDWriteFontFace) -> Option<Vec<u8>> {
    let file = first_file(face)?;
    // SAFETY: COM calls on live objects; the fragment is copied out before
    // it is released.
    unsafe {
        let mut key: *mut c_void = core::ptr::null_mut();
        let mut keysz = 0u32;
        file.GetReferenceKey(&raw mut key, &raw mut keysz).ok()?;
        let stream = file.GetLoader().ok()?.CreateStreamFromKey(key.cast_const(), keysz).ok()?;
        let size = stream.GetFileSize().ok()?;
        let mut frag: *mut c_void = core::ptr::null_mut();
        let mut ctx: *mut c_void = core::ptr::null_mut();
        stream.ReadFileFragment(&raw mut frag, 0, size, &raw mut ctx).ok()?;
        let bytes = core::slice::from_raw_parts(frag.cast_const().cast::<u8>(), usize::try_from(size).ok()?).to_vec();
        stream.ReleaseFileFragment(ctx);
        Some(bytes)
    }
}
