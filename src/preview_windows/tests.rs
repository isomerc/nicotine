//! Unit tests for `preview_windows` pure helpers. Currently scoped to
//! `unpack_xy` (sign-extension on Win32 LPARAM packed coordinates) —
//! the other items in this manager are window-procedure callbacks
//! tied to live Win32 state and don't unit-test cleanly without
//! significant mocking.

use super::*;

/// Pack (x, y) into an LPARAM the way Win32 message loops do:
/// low 16 bits = x, high 16 bits = y, each as a signed i16.
/// This builds the test inputs for `unpack_xy`.
fn pack(x: i16, y: i16) -> LPARAM {
    let raw: u32 = ((y as u16 as u32) << 16) | (x as u16 as u32);
    LPARAM(raw as isize)
}

#[test]
fn unpack_xy_positive_coords_round_trip() {
    let (x, y) = unpack_xy(pack(100, 200));
    assert_eq!((x, y), (100, 200));
}

#[test]
fn unpack_xy_zero() {
    let (x, y) = unpack_xy(pack(0, 0));
    assert_eq!((x, y), (0, 0));
}

#[test]
fn unpack_xy_negative_x_sign_extends() {
    // The reason this function exists in the first place: a captured
    // drag that crosses to the left of the window delivers x as a
    // small negative. Without the `as u16 as i16 as i32` cast chain,
    // u16's 0xFFFB would zero-extend to 65531 instead of -5.
    let (x, y) = unpack_xy(pack(-5, 100));
    assert_eq!((x, y), (-5, 100));
}

#[test]
fn unpack_xy_negative_y_sign_extends() {
    let (x, y) = unpack_xy(pack(100, -5));
    assert_eq!((x, y), (100, -5));
}

#[test]
fn unpack_xy_both_negative() {
    let (x, y) = unpack_xy(pack(-1, -1));
    assert_eq!((x, y), (-1, -1));
}

#[test]
fn unpack_xy_i16_extremes() {
    // Boundaries of the 16-bit signed range — if anything goes
    // wrong with the cast chain, these are where it shows first.
    let (x, y) = unpack_xy(pack(i16::MIN, i16::MAX));
    assert_eq!((x, y), (i16::MIN as i32, i16::MAX as i32));

    let (x, y) = unpack_xy(pack(i16::MAX, i16::MIN));
    assert_eq!((x, y), (i16::MAX as i32, i16::MIN as i32));
}

/// On 64-bit Windows, LPARAM is i64. The function only consumes
/// the low 32 bits (`lparam.0 as u32`); upper bits should not
/// bleed into the result. This pins that down so a future
/// `lparam.0 as i64` rewrite would be caught. Gated to 64-bit
/// targets because the construction below relies on `isize`
/// being i64.
#[cfg(target_pointer_width = "64")]
#[test]
fn unpack_xy_ignores_upper_lparam_bits() {
    let raw_low: u32 = ((200i16 as u16 as u32) << 16) | (100i16 as u16 as u32);
    let with_high_bits = LPARAM(((0xDEADBEEF_u64 as isize) << 32) | (raw_low as isize));
    let (x, y) = unpack_xy(with_high_bits);
    assert_eq!((x, y), (100, 200));
}
