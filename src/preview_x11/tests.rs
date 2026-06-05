//! Unit tests for the pure helpers in `preview_x11`. Items under test
//! live in `super` (the parent module's other files). They're private
//! to the module; this sibling submodule sees them because child
//! modules inherit access to their parent's private items.

use super::*;
use x11rb::protocol::render::{
    Directformat, PictType, Pictdepth, Pictforminfo, Pictscreen, Pictvisual, QueryPictFormatsReply,
};

// --- fixed -------------------------------------------------------

#[test]
fn fixed_one_equals_xfixed_one_constant() {
    // The whole point of 16.16: f64 1.0 maps to integer 0x10000.
    // Used as matrix33 in the identity row of every XRender
    // transform we build (see apply_scale_transform).
    assert_eq!(fixed(1.0), XFIXED_ONE);
    assert_eq!(fixed(1.0), 65536);
}

#[test]
fn fixed_zero_and_negatives() {
    assert_eq!(fixed(0.0), 0);
    assert_eq!(fixed(-1.0), -65536);
    assert_eq!(fixed(-2.5), -163_840);
}

#[test]
fn fixed_simple_fractions() {
    assert_eq!(fixed(0.5), 32_768);
    assert_eq!(fixed(2.0), 131_072);
    assert_eq!(fixed(13.0), 13 * 65536); // a typical EVE-to-thumb downscale ratio
}

#[test]
fn fixed_truncates_toward_zero_not_rounds() {
    // (0.99 * 65536) ≈ 64880.64 — truncates to 64880. If someone
    // switched the cast to .round(), this would be 64881. Pins
    // down the rounding direction (matters for transform jitter).
    assert_eq!(fixed(0.99), 64_880);
    // Negative side: -64880.64 truncates toward zero → -64880,
    // not -64881 (which would be floor / round-down).
    assert_eq!(fixed(-0.99), -64_880);
}

// --- thumbnail_size ---------------------------------------------

#[test]
fn thumbnail_size_normal_dimensions() {
    // Standard preview: subtract border on both sides for width
    // (3 * 2 = 6); subtract title strip + bottom border for height
    // (24 + 3 = 27).
    assert_eq!(thumbnail_size(480, 270), (474, 243));
}

#[test]
fn thumbnail_size_floors_at_one_not_zero() {
    // Zero-dim drawables make XRender unhappy. The `.max(1)` clamp
    // on each axis is the safety floor; this catches anyone
    // removing it.
    assert_eq!(thumbnail_size(0, 0), (1, 1));
    assert_eq!(thumbnail_size(1, 1), (1, 1));
    assert_eq!(
        thumbnail_size(BORDER_WIDTH * 2, TITLE_STRIP_HEIGHT + BORDER_WIDTH),
        (1, 1)
    );
}

#[test]
fn thumbnail_size_saturating_sub_doesnt_underflow() {
    // Without saturating_sub this would underflow on u16 and
    // either panic (debug) or wrap to ~65535 (release).
    let (w, h) = thumbnail_size(2, 20);
    assert_eq!((w, h), (1, 1));
}

#[test]
fn thumbnail_size_one_pixel_above_chrome() {
    // Just past the chrome floor — the first size where saturation
    // doesn't clip the result.
    let min_w = BORDER_WIDTH * 2 + 1;
    let min_h = TITLE_STRIP_HEIGHT + BORDER_WIDTH + 1;
    assert_eq!(thumbnail_size(min_w, min_h), (1, 1));
    assert_eq!(thumbnail_size(min_w + 1, min_h + 1), (2, 2));
}

// --- pick_visual_format -----------------------------------------

fn empty_direct() -> Directformat {
    Directformat {
        red_shift: 0,
        red_mask: 0,
        green_shift: 0,
        green_mask: 0,
        blue_shift: 0,
        blue_mask: 0,
        alpha_shift: 0,
        alpha_mask: 0,
    }
}

/// Build a `QueryPictFormatsReply` with the screens/depths/visuals
/// shape that `pick_visual_format` walks. `formats` and the other
/// reply fields not consulted by the function are left empty.
fn reply_with_screens(screens: Vec<Pictscreen>) -> QueryPictFormatsReply {
    QueryPictFormatsReply {
        sequence: 0,
        length: 0,
        num_depths: 0,
        num_visuals: 0,
        formats: Vec::new(),
        screens,
        subpixels: Vec::new(),
    }
}

fn screen(depths: Vec<Pictdepth>) -> Pictscreen {
    Pictscreen {
        depths,
        fallback: 0,
    }
}

fn depth(depth_val: u8, visuals: Vec<Pictvisual>) -> Pictdepth {
    Pictdepth {
        depth: depth_val,
        visuals,
    }
}

#[test]
fn pick_visual_format_returns_id_on_match() {
    let reply = reply_with_screens(vec![screen(vec![depth(
        24,
        vec![Pictvisual {
            visual: 0xAA_BB_CC,
            format: 42,
        }],
    )])]);
    assert_eq!(pick_visual_format(&reply, 0xAA_BB_CC, 24), Some(42));
}

#[test]
fn pick_visual_format_none_when_visual_absent() {
    let reply = reply_with_screens(vec![screen(vec![depth(
        24,
        vec![Pictvisual {
            visual: 0x111,
            format: 42,
        }],
    )])]);
    assert_eq!(pick_visual_format(&reply, 0x222, 24), None);
}

#[test]
fn pick_visual_format_none_when_depth_mismatches() {
    // Same visual id at depth 32 should NOT match a depth-24 query —
    // X server may have multiple visuals with the same id at
    // different depths.
    let reply = reply_with_screens(vec![screen(vec![depth(
        32,
        vec![Pictvisual {
            visual: 0x123,
            format: 99,
        }],
    )])]);
    assert_eq!(pick_visual_format(&reply, 0x123, 24), None);
}

#[test]
fn pick_visual_format_searches_across_screens_and_depths() {
    // Real servers can present multiple screens with multiple
    // depths. The function must walk all of them, not just the
    // first.
    let reply = reply_with_screens(vec![
        screen(vec![depth(
            24,
            vec![Pictvisual {
                visual: 0x100,
                format: 1,
            }],
        )]),
        screen(vec![
            depth(
                24,
                vec![Pictvisual {
                    visual: 0x200,
                    format: 2,
                }],
            ),
            depth(
                32,
                vec![Pictvisual {
                    visual: 0x300,
                    format: 3,
                }],
            ),
        ]),
    ]);
    assert_eq!(pick_visual_format(&reply, 0x300, 32), Some(3));
    assert_eq!(pick_visual_format(&reply, 0x200, 24), Some(2));
}

// --- pick_argb32_format -----------------------------------------

/// Standard 8-8-8-8 ARGB direct format. All four masks `0xFF`,
/// shifts at 0/8/16/24. The function should accept this.
fn argb32_direct() -> Directformat {
    Directformat {
        red_shift: 16,
        red_mask: 0xFF,
        green_shift: 8,
        green_mask: 0xFF,
        blue_shift: 0,
        blue_mask: 0xFF,
        alpha_shift: 24,
        alpha_mask: 0xFF,
    }
}

fn reply_with_formats(formats: Vec<Pictforminfo>) -> QueryPictFormatsReply {
    QueryPictFormatsReply {
        sequence: 0,
        length: 0,
        num_depths: 0,
        num_visuals: 0,
        formats,
        screens: Vec::new(),
        subpixels: Vec::new(),
    }
}

#[test]
fn pick_argb32_format_returns_id_for_matching_layout() {
    let reply = reply_with_formats(vec![Pictforminfo {
        id: 7,
        type_: PictType::DIRECT,
        depth: 32,
        direct: argb32_direct(),
        colormap: 0,
    }]);
    assert_eq!(pick_argb32_format(&reply), Some(7));
}

#[test]
fn pick_argb32_format_rejects_wrong_depth() {
    let mut f = Pictforminfo {
        id: 7,
        type_: PictType::DIRECT,
        depth: 24, // not 32
        direct: argb32_direct(),
        colormap: 0,
    };
    assert_eq!(pick_argb32_format(&reply_with_formats(vec![f])), None);
    f.depth = 32;
    assert_eq!(pick_argb32_format(&reply_with_formats(vec![f])), Some(7));
}

#[test]
fn pick_argb32_format_rejects_indexed_type() {
    let reply = reply_with_formats(vec![Pictforminfo {
        id: 7,
        type_: PictType::INDEXED,
        depth: 32,
        direct: argb32_direct(),
        colormap: 0,
    }]);
    assert_eq!(pick_argb32_format(&reply), None);
}

#[test]
fn pick_argb32_format_rejects_wrong_shifts() {
    // ABGR layout (alpha-blue-green-red) at depth 32 should not be
    // picked — XRender consumers downstream assume the standard
    // ARGB shift positions.
    let abgr = Directformat {
        red_shift: 0,
        red_mask: 0xFF,
        green_shift: 8,
        green_mask: 0xFF,
        blue_shift: 16,
        blue_mask: 0xFF,
        alpha_shift: 24,
        alpha_mask: 0xFF,
    };
    let reply = reply_with_formats(vec![Pictforminfo {
        id: 7,
        type_: PictType::DIRECT,
        depth: 32,
        direct: abgr,
        colormap: 0,
    }]);
    assert_eq!(pick_argb32_format(&reply), None);
}

#[test]
fn pick_argb32_format_finds_correct_among_many() {
    // Three formats: depth-24 RGB, depth-32 ABGR, depth-32 ARGB.
    // Only the last should match.
    let rgb24 = Pictforminfo {
        id: 1,
        type_: PictType::DIRECT,
        depth: 24,
        direct: Directformat {
            red_shift: 16,
            red_mask: 0xFF,
            green_shift: 8,
            green_mask: 0xFF,
            blue_shift: 0,
            blue_mask: 0xFF,
            alpha_shift: 0,
            alpha_mask: 0,
        },
        colormap: 0,
    };
    let abgr32 = Pictforminfo {
        id: 2,
        type_: PictType::DIRECT,
        depth: 32,
        direct: Directformat {
            red_shift: 0,
            red_mask: 0xFF,
            green_shift: 8,
            green_mask: 0xFF,
            blue_shift: 16,
            blue_mask: 0xFF,
            alpha_shift: 24,
            alpha_mask: 0xFF,
        },
        colormap: 0,
    };
    let argb32 = Pictforminfo {
        id: 3,
        type_: PictType::DIRECT,
        depth: 32,
        direct: argb32_direct(),
        colormap: 0,
    };
    let reply = reply_with_formats(vec![rgb24, abgr32, argb32]);
    assert_eq!(pick_argb32_format(&reply), Some(3));
}

#[test]
fn pick_argb32_format_none_on_empty_reply() {
    let reply = reply_with_formats(vec![]);
    assert_eq!(pick_argb32_format(&reply), None);
}

// --- opacity_to_cardinal ----------------------------------------

#[test]
fn opacity_cardinal_boundaries() {
    // 100% is the default value, so it must map to *exactly* u32::MAX.
    // Anything short and KWin reads the window as slightly translucent —
    // every preview would be subtly dimmed out of the box with nobody
    // having touched the slider.
    assert_eq!(opacity_to_cardinal(100), u32::MAX);
    assert_eq!(opacity_to_cardinal(0), 0);
    // The slider can't exceed 100, but the float→u32 cast must saturate
    // rather than wrap if a bad value ever reached it.
    assert_eq!(opacity_to_cardinal(150), u32::MAX);
}

#[test]
fn opacity_cardinal_scales_monotonically() {
    // Higher percent → more opaque. Pins the scaling direction and that
    // the 10% slider floor is a small but non-zero fraction of opaque.
    let ten = opacity_to_cardinal(10);
    assert!(ten > 0);
    assert!(ten < opacity_to_cardinal(50));
    assert!(opacity_to_cardinal(50) < u32::MAX);
}

#[test]
fn empty_direct_format_is_zero_everywhere() {
    // Sanity for the test fixture helper itself — used elsewhere
    // when the function under test ignores the direct format.
    let d = empty_direct();
    assert_eq!(
        (d.red_mask, d.green_mask, d.blue_mask, d.alpha_mask),
        (0, 0, 0, 0)
    );
}
