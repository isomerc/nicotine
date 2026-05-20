//! Cross-platform helpers for the preview-window managers
//! (`preview_x11.rs` on Linux, `preview_windows.rs` on Windows). Both
//! managers implement the same UX — drag to position, snap to dock,
//! click vs drag distinguished by pixel-distance threshold — so the
//! geometry pieces live here once.

/// Axis-aligned rectangle in root/screen coordinates. `right` and
/// `bottom` are exclusive (right = left + width). Same shape as Win32
/// `RECT`; the Windows backend converts at the call boundary.
#[derive(Debug, Clone, Copy)]
pub struct DragRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// Press/drag/release bookkeeping for a draggable preview. `active`
/// is true between mouse-down and mouse-up. `dragged` flips once
/// motion exceeds `DRAG_THRESHOLD_PX` — at release it distinguishes
/// "moved → persist new position" from "clicked → activate source
/// EVE client". `origin_screen` and `origin_window` are captured at
/// press time so the motion delta can drive both threshold detection
/// and position computation.
#[derive(Debug, Default, Clone, Copy)]
pub struct DragState {
    pub active: bool,
    pub dragged: bool,
    pub origin_screen: (i32, i32),
    pub origin_window: (i32, i32),
}

/// Pointer must move at least this many reference pixels between
/// press and release for the gesture to count as a drag. Below the
/// threshold the release activates the source EVE client; above it,
/// the new window position is persisted. Windows wraps this through
/// the DPI helper (`px(DRAG_THRESHOLD_PX)`); Linux uses it raw.
pub const DRAG_THRESHOLD_PX: i32 = 4;

/// Edges within this distance during a drag snap to the matching
/// edge of another preview window. Generous enough that docking
/// feels deliberate, tight enough that you can place freely between
/// docked previews. DPI-scaled on Windows; raw on Linux.
pub const SNAP_THRESHOLD_PX: i32 = 12;

/// Adjust a proposed (x, y) so the dragged window's edges snap to
/// nearby preview windows' edges when within `snap_threshold` pixels.
/// Each axis snaps independently so you can dock right-against-left
/// while also aligning tops. The first match per axis wins so densely
/// packed snap targets don't oscillate.
///
/// `snap_threshold` is a parameter rather than a constant because
/// Windows applies DPI scaling before calling.
pub fn snap_position(
    proposed_x: i32,
    proposed_y: i32,
    width: i32,
    height: i32,
    others: &[DragRect],
    snap_threshold: i32,
) -> (i32, i32) {
    let mut x = proposed_x;
    let mut y = proposed_y;
    let mut x_snapped = false;
    let mut y_snapped = false;
    let drag_right = proposed_x + width;
    let drag_bottom = proposed_y + height;
    let snap = snap_threshold;

    for other in others {
        // Edge-to-edge docking on the X axis (right of dragged ↔ left
        // of other, then left of dragged ↔ right of other).
        if !x_snapped && (drag_right - other.left).abs() <= snap {
            x = other.left - width;
            x_snapped = true;
        }
        if !x_snapped && (proposed_x - other.right).abs() <= snap {
            x = other.right;
            x_snapped = true;
        }
        // Same for the Y axis.
        if !y_snapped && (drag_bottom - other.top).abs() <= snap {
            y = other.top - height;
            y_snapped = true;
        }
        if !y_snapped && (proposed_y - other.bottom).abs() <= snap {
            y = other.bottom;
            y_snapped = true;
        }
        // Parallel-edge alignment so docked previews share a baseline.
        if !y_snapped && (proposed_y - other.top).abs() <= snap {
            y = other.top;
            y_snapped = true;
        }
        if !y_snapped && (drag_bottom - other.bottom).abs() <= snap {
            y = other.bottom - height;
            y_snapped = true;
        }
        if !x_snapped && (proposed_x - other.left).abs() <= snap {
            x = other.left;
            x_snapped = true;
        }
        if !x_snapped && (drag_right - other.right).abs() <= snap {
            x = other.right - width;
            x_snapped = true;
        }
    }

    (x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reference geometry used across the snap tests. The dragged window
    // is 100×100. With snap_threshold = 12 (the production value), edges
    // within 12px of a neighbor's edge should snap; edges 13px+ away
    // should not. `neighbor` sits far enough away that placing it at
    // various offsets exercises one snap condition at a time.
    const DRAG_W: i32 = 100;
    const DRAG_H: i32 = 100;
    const SNAP: i32 = 12;

    fn neighbor(left: i32, top: i32) -> DragRect {
        DragRect {
            left,
            top,
            right: left + 100,
            bottom: top + 100,
        }
    }

    #[test]
    fn no_others_returns_proposed_unchanged() {
        let (x, y) = snap_position(100, 100, DRAG_W, DRAG_H, &[], SNAP);
        assert_eq!((x, y), (100, 100));
    }

    #[test]
    fn empty_others_at_extreme_proposed_still_identity() {
        let (x, y) = snap_position(-50_000, 32_000, DRAG_W, DRAG_H, &[], SNAP);
        assert_eq!((x, y), (-50_000, 32_000));
    }

    #[test]
    fn no_snap_when_all_edges_beyond_threshold() {
        // Neighbor at (213, 213) — closest edge (left=213) is 13px from
        // drag's right edge (200). Past the inclusive threshold of 12.
        let others = [neighbor(213, 213)];
        let (x, y) = snap_position(100, 100, DRAG_W, DRAG_H, &others, SNAP);
        assert_eq!((x, y), (100, 100));
    }

    #[test]
    fn dock_drag_right_to_other_left() {
        // drag right (200) is 10px from other.left (210). Snaps so drag's
        // right edge equals other's left edge.
        let others = [neighbor(210, 100)];
        let (x, _) = snap_position(100, 100, DRAG_W, DRAG_H, &others, SNAP);
        assert_eq!(x, 110); // x + DRAG_W == other.left
    }

    #[test]
    fn dock_drag_left_to_other_right() {
        // drag left (100) is 10px from other.right (90). Snaps so drag's
        // left equals other's right.
        let others = [DragRect {
            left: -10,
            top: 100,
            right: 90,
            bottom: 200,
        }];
        let (x, _) = snap_position(100, 100, DRAG_W, DRAG_H, &others, SNAP);
        assert_eq!(x, 90);
    }

    #[test]
    fn dock_drag_bottom_to_other_top() {
        let others = [neighbor(100, 210)]; // other.top = 210, drag.bottom = 200
        let (_, y) = snap_position(100, 100, DRAG_W, DRAG_H, &others, SNAP);
        assert_eq!(y, 110); // y + DRAG_H == other.top
    }

    #[test]
    fn dock_drag_top_to_other_bottom() {
        let others = [DragRect {
            left: 100,
            top: -10,
            right: 200,
            bottom: 90,
        }];
        let (_, y) = snap_position(100, 100, DRAG_W, DRAG_H, &others, SNAP);
        assert_eq!(y, 90);
    }

    #[test]
    fn align_top_to_top() {
        // Y offset by 5 — within snap range but neither edge-to-edge
        // condition matches (other is far below drag's bottom and far
        // above drag's top). Top-alignment kicks in.
        let others = [neighbor(400, 105)];
        let (_, y) = snap_position(100, 100, DRAG_W, DRAG_H, &others, SNAP);
        assert_eq!(y, 105); // proposed_y snaps to other.top
    }

    #[test]
    fn align_bottom_to_bottom() {
        // drag.bottom = 200, other.bottom = 205. Difference 5 ≤ snap.
        // No edge-to-edge match (other is far away in X), so
        // bottom-alignment wins. New y = other.bottom - drag_height.
        let others = [DragRect {
            left: 400,
            top: 105,
            right: 500,
            bottom: 205,
        }];
        let (_, y) = snap_position(100, 100, DRAG_W, DRAG_H, &others, SNAP);
        assert_eq!(y, 105);
    }

    #[test]
    fn align_left_to_left() {
        // X offset of 5 with neighbor far below — only alignment paths
        // can match. proposed_x snaps to other.left.
        let others = [neighbor(105, 400)];
        let (x, _) = snap_position(100, 100, DRAG_W, DRAG_H, &others, SNAP);
        assert_eq!(x, 105);
    }

    #[test]
    fn align_right_to_right() {
        // drag.right = 200, other.right = 195. Difference 5 ≤ snap.
        let others = [DragRect {
            left: 95,
            top: 400,
            right: 195,
            bottom: 500,
        }];
        let (x, _) = snap_position(100, 100, DRAG_W, DRAG_H, &others, SNAP);
        assert_eq!(x, 95); // x = other.right - drag_width
    }

    #[test]
    fn x_and_y_snap_independently_in_one_call() {
        // Neighbor positioned to dock right-to-left on X AND align tops
        // on Y. Both axes should snap from the same neighbor.
        let others = [neighbor(210, 105)];
        let (x, y) = snap_position(100, 100, DRAG_W, DRAG_H, &others, SNAP);
        assert_eq!((x, y), (110, 105));
    }

    #[test]
    fn x_axis_inclusive_at_threshold() {
        // Distance == snap should still snap (the check is `<= snap`).
        // drag.right = 200, other.left = 212 → distance 12.
        let others = [neighbor(212, 100)];
        let (x, _) = snap_position(100, 100, DRAG_W, DRAG_H, &others, SNAP);
        assert_eq!(x, 112); // snapped: x + width = other.left
    }

    #[test]
    fn x_axis_no_snap_one_past_threshold() {
        // Distance == snap + 1 should not snap.
        let others = [neighbor(213, 100)];
        let (x, _) = snap_position(100, 100, DRAG_W, DRAG_H, &others, SNAP);
        assert_eq!(x, 100); // unchanged
    }

    #[test]
    fn first_x_match_wins_among_multiple_neighbors() {
        // Both neighbors are within snap range on X. A is processed
        // first and snaps the X axis; B should be ignored for X even
        // though its edge would also qualify. If first-match-wins
        // breaks, the test catches B's snap result (x=112) instead of
        // A's (x=110).
        let a = neighbor(210, 400); // far enough on Y to not snap Y
        let b = neighbor(212, 400);
        let (x, _) = snap_position(100, 100, DRAG_W, DRAG_H, &[a, b], SNAP);
        assert_eq!(x, 110);
    }

    #[test]
    fn first_y_match_wins_among_multiple_neighbors() {
        let a = neighbor(400, 210);
        let b = neighbor(400, 212);
        let (_, y) = snap_position(100, 100, DRAG_W, DRAG_H, &[a, b], SNAP);
        assert_eq!(y, 110);
    }

    #[test]
    fn x_snaps_from_first_other_y_from_second() {
        // First neighbor is positioned to only snap X; second is
        // positioned to only snap Y. Combined call should pick up
        // both snaps cumulatively.
        let x_only = neighbor(210, 400);
        let y_only = neighbor(400, 210);
        let (x, y) = snap_position(100, 100, DRAG_W, DRAG_H, &[x_only, y_only], SNAP);
        assert_eq!((x, y), (110, 110));
    }

    #[test]
    fn edge_to_edge_preferred_over_alignment_within_same_other() {
        // Geometry where BOTH y-axis snap rules would match from the
        // same neighbor; the loop body checks edge-to-edge first, so
        // it wins.
        //
        // drag (100, 105), 100×100 → drag.top = 105, drag.bottom = 205.
        // other.top = 200, other.bottom = 210.
        //   edge-to-edge bottom↔top: |205 - 200| = 5 ≤ snap → y = 200 - 100 = 100.
        //   alignment bottom↔bottom: |205 - 210| = 5 ≤ snap → y = 210 - 100 = 110.
        //   alignment top↔top: |105 - 200| = 95 → no.
        //   edge top↔bottom: |105 - 210| = 105 → no.
        // Outcome 100 (edge wins) is distinguishable from both
        // 105 (no snap) and 110 (alignment wins).
        let other = DragRect {
            left: 400, // far on X so X axis stays unchanged
            top: 200,
            right: 500,
            bottom: 210,
        };
        let (x, y) = snap_position(100, 105, DRAG_W, DRAG_H, &[other], SNAP);
        assert_eq!((x, y), (100, 100));
    }

    #[test]
    fn works_with_negative_coordinates() {
        // Secondary monitors on multi-display setups commonly map to
        // negative root coordinates. Snap should work the same regardless
        // of sign.
        let others = [DragRect {
            left: -90,
            top: -90,
            right: -10, // drag.left (-100) is 10px from this. Snap.
            bottom: 10,
        }];
        let (x, _) = snap_position(-100, -100, DRAG_W, DRAG_H, &others, SNAP);
        // drag.left (-100) vs other.right (-10): diff 90 → no edge dock.
        // drag.right (0) vs other.left (-90): diff 90 → no edge dock.
        // drag.left vs other.left (-90): diff 10 → alignment-left match.
        // Result: x = other.left = -90.
        assert_eq!(x, -90);
    }

    #[test]
    fn zero_snap_threshold_disables_all_snapping() {
        // With snap_threshold = 0, only exactly-coincident edges match
        // (diff.abs() <= 0 means diff == 0). The test uses a clearly
        // non-coincident neighbor; no snap should occur.
        let others = [neighbor(210, 100)];
        let (x, y) = snap_position(100, 100, DRAG_W, DRAG_H, &others, 0);
        assert_eq!((x, y), (100, 100));
    }
}
