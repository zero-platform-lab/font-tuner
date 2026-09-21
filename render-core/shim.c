/* Minimal FreeType shim: link the MacType fork (freetype64.lib) via its own
   headers, render one glyph, hand the coverage bitmap back to Rust. The Rust
   side models no FreeType ABI; it only sees the small ShimGlyph struct below.
   A single library/face is kept as process-global state (this is an offline
   single-threaded tool). */
#include <ft2build.h>
#include FT_FREETYPE_H
#include FT_OUTLINE_H
#include FT_LCD_FILTER_H
#include <string.h>
#include <stdlib.h>

typedef struct {
    int width, rows, pitch, pixel_mode;
    int left, top;
    int advance_x;               /* 26.6 fixed */
    const unsigned char* buffer; /* valid until the next shim_render / shim_done */
} ShimGlyph;

static FT_Library    g_lib = 0;
static FT_Face       g_face = 0;
static unsigned char* g_membuf = 0; /* kept alive for FT_New_Memory_Face */

int shim_init(void) {
    if (g_lib) return 0; /* idempotent */
    return FT_Init_FreeType(&g_lib);
}

int shim_open(const char* path, long face_index) {
    if (g_face) { FT_Done_Face(g_face); g_face = 0; }
    int err = FT_New_Face(g_lib, path, face_index, &g_face);
    if (!err && g_face) FT_Select_Charmap(g_face, FT_ENCODING_UNICODE);
    return err;
}

/* Load a face from an in-memory font file (e.g. GDI GetFontData bytes). For a
   TrueType Collection, pick the face whose family name matches want_family
   (case-insensitive); falls back to face 0. The buffer is copied and kept until
   the next reface / shim_done, since FreeType references it for the face's life. */
int shim_reface_memory(const unsigned char* data, long len, const char* want_family) {
    if (g_face) { FT_Done_Face(g_face); g_face = 0; }
    if (g_membuf) { free(g_membuf); g_membuf = 0; }
    g_membuf = (unsigned char*)malloc(len);
    if (!g_membuf) return -1;
    memcpy(g_membuf, data, len);

    long chosen = 0;
    if (want_family && *want_family) {
        FT_Face probe;
        int perr = FT_New_Memory_Face(g_lib, g_membuf, len, -1, &probe);
        long n = perr ? 1 : probe->num_faces;
        if (!perr) FT_Done_Face(probe);
        for (long i = 0; i < n; ++i) {
            FT_Face f;
            if (FT_New_Memory_Face(g_lib, g_membuf, len, i, &f) == 0) {
                int match = f->family_name && _stricmp(f->family_name, want_family) == 0;
                FT_Done_Face(f);
                if (match) { chosen = i; break; }
            }
        }
    }
    int err = FT_New_Memory_Face(g_lib, g_membuf, len, chosen, &g_face);
    if (!err && g_face) FT_Select_Charmap(g_face, FT_ENCODING_UNICODE);
    return err;
}

/* filter: FT_LCD_FILTER_* (0 NONE, 1 DEFAULT, 2 LIGHT, 3 LEGACY1, 16 LEGACY) */
int shim_set_lcd_filter(int filter) {
    if (!g_lib) return -1;
    return FT_Library_SetLcdFilter(g_lib, (FT_LcdFilter)filter);
}

/* Load + render a specific glyph index into `out`. */
static int emit_glyph(FT_UInt gi, int pixel_height, int load_flags, int render_mode,
                      int embolden_x, int embolden_y, ShimGlyph* out) {
    if (!g_face) return -1;
    int err = FT_Set_Pixel_Sizes(g_face, 0, pixel_height);
    if (err) return err;
    err = FT_Load_Glyph(g_face, gi, load_flags);
    if (err) return err;
    FT_GlyphSlot slot = g_face->glyph;
    if (slot->format == FT_GLYPH_FORMAT_OUTLINE && (embolden_x || embolden_y)) {
        FT_Outline_EmboldenXY(&slot->outline, embolden_x, embolden_y);
    }
    err = FT_Render_Glyph(slot, render_mode);
    if (err) return err;
    out->width      = (int)slot->bitmap.width;
    out->rows       = (int)slot->bitmap.rows;
    out->pitch      = (int)slot->bitmap.pitch;
    out->pixel_mode = (int)slot->bitmap.pixel_mode;
    out->left       = slot->bitmap_left;
    out->top        = slot->bitmap_top;
    out->advance_x  = (int)slot->advance.x;
    out->buffer     = slot->bitmap.buffer;
    return 0;
}

/* charcode: unicode. load_flags/render_mode: FT_LOAD_* / FT_RENDER_MODE_*.
   embolden_x/y: 26.6 outline embolden strength (0 = none). */
int shim_render(unsigned int charcode, int pixel_height,
                int load_flags, int render_mode,
                int embolden_x, int embolden_y, ShimGlyph* out) {
    if (!g_face) return -1;
    FT_UInt gi = FT_Get_Char_Index(g_face, charcode);
    return emit_glyph(gi, pixel_height, load_flags, render_mode, embolden_x, embolden_y, out);
}

/* Same, but render a glyph index directly (for ETO_GLYPH_INDEX draws). */
int shim_render_glyph(unsigned int glyph_index, int pixel_height,
                      int load_flags, int render_mode,
                      int embolden_x, int embolden_y, ShimGlyph* out) {
    return emit_glyph((FT_UInt)glyph_index, pixel_height, load_flags, render_mode,
                      embolden_x, embolden_y, out);
}

void shim_done(void) {
    if (g_face) { FT_Done_Face(g_face); g_face = 0; }
    if (g_lib)  { FT_Done_FreeType(g_lib); g_lib = 0; }
    if (g_membuf) { free(g_membuf); g_membuf = 0; }
}
