//! Pure rendering helpers for `preview_x11`: format negotiation,
//! XRender transforms, color conversions, and font rasterization.
//! No `self`, no shared mutable state — everything here takes
//! arguments and returns data (or sends X11 requests through a
//! borrowed connection). Tested in detail by `super::tests`.

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use anyhow::Result;
use std::sync::OnceLock;

use x11rb::connection::Connection;
use x11rb::protocol::render::{
    self, Color as RenderColor, ConnectionExt as _, CreatePictureAux as RenderCreatePictureAux,
    Transform,
};
use x11rb::protocol::xproto::{ConnectionExt as _, CreateGCAux, ImageFormat, Window};
use x11rb::rust_connection::RustConnection;

use super::{BORDER_WIDTH, TITLE_STRIP_HEIGHT};

/// XFixed is signed 16.16 fixed-point. 1.0 == 0x10000.
pub(super) const XFIXED_ONE: i32 = 0x10000;

pub(super) fn fixed(value: f64) -> i32 {
    (value * 65536.0) as i32
}

/// Convert (R, G, B) in 0–255 to an XRender Color. XRender colors are
/// 16-bit per channel; multiply 8-bit by 257 to span 0..0xFFFF without
/// fractional drift (257 = 0x101 so 0xFF * 257 = 0xFFFF exactly).
pub(super) fn xcolor(rgb: (u8, u8, u8), alpha: u16) -> RenderColor {
    RenderColor {
        red: (rgb.0 as u16) * 257,
        green: (rgb.1 as u16) * 257,
        blue: (rgb.2 as u16) * 257,
        alpha,
    }
}

/// JetBrains Mono baked into the binary, parsed once and shared across
/// every preview's text rasterization. Bundled into preview chrome so we
/// don't depend on system fontconfig — same font shows up regardless of
/// the user's distro / Wayland font setup.
pub(super) fn jetbrains_mono() -> &'static FontRef<'static> {
    static FONT: OnceLock<FontRef<'static>> = OnceLock::new();
    FONT.get_or_init(|| {
        FontRef::try_from_slice(include_bytes!(
            "../../assets/fonts/JetBrainsMono-Regular.ttf"
        ))
        .expect("bundled JetBrains Mono parses")
    })
}

/// Marlboro display font for the list-view title strip ("Nicotine"
/// header). Same font the (now-removed) egui overlay used and the
/// Windows config panel uses, so all three Nicotine surfaces share a
/// logo treatment.
pub(super) fn marlboro() -> &'static FontRef<'static> {
    static FONT: OnceLock<FontRef<'static>> = OnceLock::new();
    FONT.get_or_init(|| {
        FontRef::try_from_slice(include_bytes!("../../assets/fonts/Marlboro.ttf"))
            .expect("bundled Marlboro parses")
    })
}

/// (Width, height) of the thumbnail area within a preview window.
/// Subtracts the title strip from the top and the border on left,
/// right, and bottom.
pub(super) fn thumbnail_size(window_w: u16, window_h: u16) -> (u16, u16) {
    let w = window_w.saturating_sub(BORDER_WIDTH * 2);
    let h = window_h
        .saturating_sub(TITLE_STRIP_HEIGHT)
        .saturating_sub(BORDER_WIDTH);
    (w.max(1), h.max(1))
}

/// Set the source picture's transform so a thumb_w × thumb_h destination
/// area samples the full source_w × source_h source. XRender transforms
/// are inverse-mapping — they map dest coords to source coords — hence
/// the source/thumb ratio.
pub(super) fn apply_scale_transform(
    conn: &RustConnection,
    src_picture: u32,
    source_w: u16,
    source_h: u16,
    thumb_w: u16,
    thumb_h: u16,
) -> Result<()> {
    let scale_x = source_w as f64 / thumb_w as f64;
    let scale_y = source_h as f64 / thumb_h as f64;
    let transform = Transform {
        matrix11: fixed(scale_x),
        matrix12: 0,
        matrix13: 0,
        matrix21: 0,
        matrix22: fixed(scale_y),
        matrix23: 0,
        matrix31: 0,
        matrix32: 0,
        matrix33: XFIXED_ONE,
    };
    conn.render_set_picture_transform(src_picture, transform)?;
    // Bilinear filtering on the downscale — Nearest is jaggy at typical
    // ~10× ratios.
    conn.render_set_picture_filter(src_picture, b"bilinear", &[])?;
    Ok(())
}

/// Find the Pictformat for a specific (visual, depth). Walks the
/// per-screen depth/visual map in QueryPictFormatsReply.
pub(super) fn pick_visual_format(
    reply: &render::QueryPictFormatsReply,
    visual_id: u32,
    depth: u8,
) -> Option<u32> {
    for screen in &reply.screens {
        for d in &screen.depths {
            if d.depth != depth {
                continue;
            }
            for v in &d.visuals {
                if v.visual == visual_id {
                    return Some(v.format);
                }
            }
        }
    }
    None
}

/// Find the standard ARGB32 PictFormat (depth 32, A R G B layout).
pub(super) fn pick_argb32_format(reply: &render::QueryPictFormatsReply) -> Option<u32> {
    reply.formats.iter().find_map(|f| {
        if f.type_ == render::PictType::DIRECT
            && f.depth == 32
            && f.direct.alpha_mask == 0xff
            && f.direct.red_mask == 0xff
            && f.direct.green_mask == 0xff
            && f.direct.blue_mask == 0xff
            && f.direct.alpha_shift == 24
            && f.direct.red_shift == 16
            && f.direct.green_shift == 8
            && f.direct.blue_shift == 0
        {
            Some(f.id)
        } else {
            None
        }
    })
}

/// Rasterize `text` at `font_px` into a packed alpha buffer using
/// `font`. Returns (bytes, width, height). Each byte is the coverage
/// (0–255) for one pixel; row-major, no padding.
pub(super) fn rasterize_text(font: &FontRef<'_>, text: &str, font_px: f32) -> (Vec<u8>, u16, u16) {
    let scaled = font.as_scaled(PxScale::from(font_px));
    let ascent = scaled.ascent();
    let descent = scaled.descent();
    let height = ((ascent - descent).ceil() as i32).max(1) as u16;

    let mut total_advance = 0.0f32;
    for c in text.chars() {
        total_advance += scaled.h_advance(scaled.glyph_id(c));
    }
    let width = (total_advance.ceil() as i32).max(1) as u16;

    let mut buf = vec![0u8; (width as usize) * (height as usize)];

    let mut x_cursor = 0.0f32;
    for c in text.chars() {
        let mut glyph = scaled.scaled_glyph(c);
        glyph.position = ab_glyph::point(x_cursor, ascent);
        if let Some(outlined) = scaled.font.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            outlined.draw(|gx, gy, alpha| {
                let px = (bounds.min.x as i32) + gx as i32;
                let py = (bounds.min.y as i32) + gy as i32;
                if px < 0 || py < 0 || px as u16 >= width || py as u16 >= height {
                    return;
                }
                let idx = py as usize * width as usize + px as usize;
                let new_a = (alpha * 255.0).clamp(0.0, 255.0) as u8;
                // Glyphs that overlap (e.g. kerning corner cases) — take
                // the max to avoid additive over-saturation that'd render
                // as a black rectangle.
                buf[idx] = buf[idx].max(new_a);
            });
        }
        x_cursor += scaled.h_advance(scaled.glyph_id(c));
    }

    (buf, width, height)
}

/// Build the per-preview title text resources: a depth-32 pixmap with
/// the character name pre-rasterized and color-baked, plus an XRender
/// Picture (ARGB32) wrapping it. Each pixel is premultiplied:
/// `(cream.r * a, cream.g * a, cream.b * a, a)` so PictOp::OVER blends
/// it directly onto whatever chrome color is underneath. Caller owns
/// both resources; drop via render_free_picture + free_pixmap.
pub(super) fn create_text_pixmap(
    conn: &RustConnection,
    parent_drawable: Window,
    text: &str,
    color: (u8, u8, u8),
    font: &FontRef<'_>,
    size_px: f32,
    argb32_format: u32,
) -> Result<(u32, u32, u16, u16)> {
    let (alpha_buf, w, h) = rasterize_text(font, text, size_px);

    // BGRA wire layout (X11 little-endian for ARGB32) with premultiplied
    // color. Shift-by-8 instead of /255 — visually identical, no FP,
    // and ~2× faster than dividing.
    let mut bgra = Vec::with_capacity((w as usize) * (h as usize) * 4);
    for &a in &alpha_buf {
        let a16 = a as u16;
        bgra.push(((color.2 as u16 * a16) >> 8) as u8); // B
        bgra.push(((color.1 as u16 * a16) >> 8) as u8); // G
        bgra.push(((color.0 as u16 * a16) >> 8) as u8); // R
        bgra.push(a);
    }

    let pixmap = conn.generate_id()?;
    conn.create_pixmap(32, pixmap, parent_drawable, w, h)?;

    let gc = conn.generate_id()?;
    conn.create_gc(gc, pixmap, &CreateGCAux::new())?;
    conn.put_image(ImageFormat::Z_PIXMAP, pixmap, gc, w, h, 0, 0, 0, 32, &bgra)?;
    conn.free_gc(gc)?;

    let picture = conn.generate_id()?;
    conn.render_create_picture(
        picture,
        pixmap,
        argb32_format,
        &RenderCreatePictureAux::new(),
    )?;

    Ok((pixmap, picture, w, h))
}
