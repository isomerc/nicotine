//! Pure logic extracted from `windows_input` / `windows_manager` so it
//! can be unit-tested on any host — including from a Linux dev box that
//! can't run the Win32 calls these helpers feed into. The functions
//! here take primitive inputs (u16 VK codes, u32 thread IDs, plain
//! structs) and return data the Windows-only consumers then act on.
//!
//! Compiled unconditionally; on Linux only the tests use it (the
//! actual call sites in `windows_input` / `windows_manager` are
//! cfg-gated and never linked). The module-wide `dead_code` allow
//! covers that intentional Linux-side unused-ness — the unit tests
//! still exercise every item below.
#![allow(dead_code)]

use crate::config::CharacterHotkey;
use std::collections::HashMap;

/// Direction of a cycle action. Used by `classify_xbutton` and by the
/// Windows input listener's internal message dispatch. Cross-platform
/// to keep tests free of Win32 imports.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum CycleDirection {
    Forward,
    Backward,
}

/// The three keyboard modifiers Win32 RegisterHotKey accepts as a
/// global hotkey qualifier. `None` is represented by `Option<ModifierKind>::None`
/// at the use site, not a fourth variant — matches the existing
/// `Option<u16> modifier_key` shape in `Config`.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ModifierKind {
    Shift,
    Ctrl,
    Alt,
}

/// Map a Win32 VK code to the modifier it represents, or None if the
/// code isn't a modifier. Covers both unified (`VK_SHIFT`) and
/// left/right (`VK_LSHIFT` / `VK_RSHIFT`) variants since Config may
/// store either depending on how the bind was captured.
pub fn modifier_kind(vk: u16) -> Option<ModifierKind> {
    // Values hard-coded against the Win32 VK_ constants so this file
    // doesn't need the `windows` crate (which only links on Windows).
    // Verified against `windows::Win32::UI::Input::KeyboardAndMouse`:
    //   VK_SHIFT   = 0x10, VK_LSHIFT = 0xA0, VK_RSHIFT = 0xA1
    //   VK_CONTROL = 0x11, VK_LCONTROL = 0xA2, VK_RCONTROL = 0xA3
    //   VK_MENU    = 0x12, VK_LMENU = 0xA4, VK_RMENU = 0xA5
    match vk {
        0x10 | 0xA0 | 0xA1 => Some(ModifierKind::Shift),
        0x11 | 0xA2 | 0xA3 => Some(ModifierKind::Ctrl),
        0x12 | 0xA4 | 0xA5 => Some(ModifierKind::Alt),
        _ => None,
    }
}

/// Pure classification of a raw mouse XBUTTON code into a cycle
/// direction, given the user-configured forward/backward button codes.
/// Returns None when the press isn't bound to either direction —
/// caller passes through to the next hook with no side effect.
pub fn classify_xbutton(
    xbutton: u16,
    forward_button: u16,
    backward_button: u16,
) -> Option<CycleDirection> {
    if xbutton == forward_button {
        Some(CycleDirection::Forward)
    } else if xbutton == backward_button {
        Some(CycleDirection::Backward)
    } else {
        None
    }
}

/// Whether the low-level focus-stealing fallback in `force_activate`
/// should attach a foreign thread's input queue to ours. The Win32
/// `AttachThreadInput` call is a no-op (and returns false) if you ask
/// it to attach a thread to itself, and there's no point attaching a
/// thread that's already attached or one we don't know exists (id 0).
///
/// `exclude` lists thread IDs we've already attached or otherwise
/// shouldn't attempt — typically the target thread when evaluating
/// whether to also attach the previous foreground thread.
pub fn should_attach_thread_input(thread: u32, current: u32, exclude: &[u32]) -> bool {
    thread != 0 && thread != current && !exclude.contains(&thread)
}

/// A planned cycle (forward/backward) hotkey registration. Output of
/// `plan_cycle_hotkeys`; the Windows consumer turns each entry into a
/// `RegisterHotKey` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleHotkeyPlan {
    pub id: i32,
    pub modifier: Option<ModifierKind>,
    pub vk: u16,
}

/// Plan which cycle hotkeys to register given the user's config.
///
/// Three relevant shapes:
/// * Cycle hotkeys disabled → no entries.
/// * Forward == Backward key, no modifier configured → forward only,
///   no modifier (the backward registration is unreachable so we skip
///   it; the modifier-bearing match in the modifier-key gating path
///   below is what makes shift-tab work).
/// * Forward == Backward key, modifier configured → forward (no mod)
///   AND backward (with modifier).
/// * Forward != Backward → both registered, neither with a modifier.
///   The `modifier_key` setting is intentionally ignored in this case;
///   it's only meaningful when forward/backward overlap.
pub fn plan_cycle_hotkeys(
    enable: bool,
    forward_vk: u16,
    backward_vk: u16,
    modifier_vk: Option<u16>,
    forward_id: i32,
    backward_id: i32,
) -> Vec<CycleHotkeyPlan> {
    if !enable {
        return Vec::new();
    }
    let mut plans = vec![CycleHotkeyPlan {
        id: forward_id,
        modifier: None,
        vk: forward_vk,
    }];
    let same_key = forward_vk == backward_vk;
    let backward_modifier = if same_key {
        modifier_vk.and_then(modifier_kind)
    } else {
        None
    };
    if !same_key || backward_modifier.is_some() {
        plans.push(CycleHotkeyPlan {
            id: backward_id,
            modifier: backward_modifier,
            vk: backward_vk,
        });
    }
    plans
}

/// A planned per-character hotkey registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharacterHotkeyPlan {
    pub id: i32,
    pub character_name: String,
    pub modifier: Option<ModifierKind>,
    pub vk: u16,
}

/// Plan per-character hotkey registrations in the user-configured
/// character order. Skips characters that have no hotkey entry or
/// whose VK is the placeholder 0 (the user picked a modifier but
/// hasn't captured a key yet). IDs start at `base_id` and advance by
/// one for every emitted plan — entries that are skipped don't burn an
/// ID, so the ID sequence is dense.
pub fn plan_character_hotkeys(
    characters: &[String],
    character_hotkeys: &HashMap<String, CharacterHotkey>,
    base_id: i32,
) -> Vec<CharacterHotkeyPlan> {
    let mut out = Vec::new();
    let mut next_id = base_id;
    for name in characters {
        let Some(hk) = character_hotkeys.get(name) else {
            continue;
        };
        if hk.vk == 0 {
            continue;
        }
        out.push(CharacterHotkeyPlan {
            id: next_id,
            character_name: name.clone(),
            modifier: hk.modifier.and_then(modifier_kind),
            vk: hk.vk,
        });
        next_id += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- modifier_kind --------------------------------------------------

    #[test]
    fn modifier_kind_recognises_unified_vk_constants() {
        assert_eq!(modifier_kind(0x10), Some(ModifierKind::Shift)); // VK_SHIFT
        assert_eq!(modifier_kind(0x11), Some(ModifierKind::Ctrl)); // VK_CONTROL
        assert_eq!(modifier_kind(0x12), Some(ModifierKind::Alt)); // VK_MENU
    }

    #[test]
    fn modifier_kind_recognises_left_and_right_variants() {
        assert_eq!(modifier_kind(0xA0), Some(ModifierKind::Shift));
        assert_eq!(modifier_kind(0xA1), Some(ModifierKind::Shift));
        assert_eq!(modifier_kind(0xA2), Some(ModifierKind::Ctrl));
        assert_eq!(modifier_kind(0xA3), Some(ModifierKind::Ctrl));
        assert_eq!(modifier_kind(0xA4), Some(ModifierKind::Alt));
        assert_eq!(modifier_kind(0xA5), Some(ModifierKind::Alt));
    }

    #[test]
    fn modifier_kind_returns_none_for_non_modifier_keys() {
        assert_eq!(modifier_kind(0x70), None); // F1
        assert_eq!(modifier_kind(0x41), None); // A
        assert_eq!(modifier_kind(0), None);
        assert_eq!(modifier_kind(0xFF), None);
    }

    // ---- classify_xbutton ----------------------------------------------

    #[test]
    fn classify_xbutton_returns_forward_when_press_matches_forward_binding() {
        assert_eq!(classify_xbutton(2, 2, 1), Some(CycleDirection::Forward));
    }

    #[test]
    fn classify_xbutton_returns_backward_when_press_matches_backward_binding() {
        assert_eq!(classify_xbutton(1, 2, 1), Some(CycleDirection::Backward));
    }

    #[test]
    fn classify_xbutton_returns_none_for_unbound_button() {
        assert_eq!(classify_xbutton(3, 2, 1), None);
    }

    #[test]
    fn classify_xbutton_forward_wins_when_forward_and_backward_collide() {
        // If a user managed to bind both directions to the same button,
        // forward shadows backward — that's the order the consumer
        // expects.
        assert_eq!(classify_xbutton(2, 2, 2), Some(CycleDirection::Forward));
    }

    // ---- should_attach_thread_input ------------------------------------

    #[test]
    fn should_attach_thread_input_rejects_zero_id() {
        assert!(!should_attach_thread_input(0, 100, &[]));
    }

    #[test]
    fn should_attach_thread_input_rejects_self_attach() {
        assert!(!should_attach_thread_input(100, 100, &[]));
    }

    #[test]
    fn should_attach_thread_input_rejects_already_excluded() {
        assert!(!should_attach_thread_input(200, 100, &[200]));
    }

    #[test]
    fn should_attach_thread_input_allows_valid_distinct_thread() {
        assert!(should_attach_thread_input(200, 100, &[]));
        assert!(should_attach_thread_input(200, 100, &[300]));
    }

    // ---- plan_cycle_hotkeys --------------------------------------------

    #[test]
    fn plan_cycle_hotkeys_returns_empty_when_disabled() {
        let plans = plan_cycle_hotkeys(false, 0x70, 0x70, Some(0x10), 1, 2);
        assert!(plans.is_empty());
    }

    #[test]
    fn plan_cycle_hotkeys_emits_forward_and_backward_when_keys_differ() {
        let plans = plan_cycle_hotkeys(true, 0x70, 0x71, None, 1, 2);
        assert_eq!(
            plans,
            vec![
                CycleHotkeyPlan {
                    id: 1,
                    modifier: None,
                    vk: 0x70
                },
                CycleHotkeyPlan {
                    id: 2,
                    modifier: None,
                    vk: 0x71
                },
            ]
        );
    }

    #[test]
    fn plan_cycle_hotkeys_ignores_modifier_when_keys_differ() {
        // The modifier_key field is only meaningful when forward and
        // backward share a VK. With distinct keys, modifier must NOT
        // apply to the backward entry — matches the original
        // do_register_hotkeys behavior.
        let plans = plan_cycle_hotkeys(true, 0x70, 0x71, Some(0x10), 1, 2);
        assert_eq!(plans[1].modifier, None);
    }

    #[test]
    fn plan_cycle_hotkeys_drops_backward_when_same_key_and_no_modifier() {
        let plans = plan_cycle_hotkeys(true, 0x09, 0x09, None, 1, 2);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].id, 1);
    }

    #[test]
    fn plan_cycle_hotkeys_uses_modifier_on_backward_when_keys_match() {
        // The classic "Tab forward, Shift+Tab backward" config.
        let plans = plan_cycle_hotkeys(true, 0x09, 0x09, Some(0x10), 1, 2);
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].modifier, None);
        assert_eq!(plans[1].modifier, Some(ModifierKind::Shift));
        assert_eq!(plans[1].vk, 0x09);
    }

    #[test]
    fn plan_cycle_hotkeys_drops_backward_when_modifier_vk_isnt_a_modifier() {
        // Defensive: if Config somehow stores a non-modifier VK as the
        // modifier_key, the plan treats it as no modifier — same key +
        // no modifier ⇒ skip backward.
        let plans = plan_cycle_hotkeys(true, 0x09, 0x09, Some(0x41), 1, 2); // 'A'
        assert_eq!(plans.len(), 1);
    }

    // ---- plan_character_hotkeys ----------------------------------------

    fn hk(vk: u16, modifier: Option<u16>) -> CharacterHotkey {
        CharacterHotkey { vk, modifier }
    }

    #[test]
    fn plan_character_hotkeys_emits_entries_in_character_order() {
        let characters = vec!["Beta".to_string(), "Alpha".to_string()];
        let mut hotkeys = HashMap::new();
        hotkeys.insert("Alpha".to_string(), hk(0x70, None));
        hotkeys.insert("Beta".to_string(), hk(0x71, None));
        let plans = plan_character_hotkeys(&characters, &hotkeys, 2000);
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].character_name, "Beta");
        assert_eq!(plans[0].id, 2000);
        assert_eq!(plans[1].character_name, "Alpha");
        assert_eq!(plans[1].id, 2001);
    }

    #[test]
    fn plan_character_hotkeys_skips_characters_without_a_binding() {
        let characters = vec!["Alpha".to_string(), "Beta".to_string()];
        let mut hotkeys = HashMap::new();
        hotkeys.insert("Beta".to_string(), hk(0x71, None));
        let plans = plan_character_hotkeys(&characters, &hotkeys, 2000);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].character_name, "Beta");
        // No ID burned for the skipped character.
        assert_eq!(plans[0].id, 2000);
    }

    #[test]
    fn plan_character_hotkeys_skips_placeholder_vk_zero() {
        let characters = vec!["Alpha".to_string()];
        let mut hotkeys = HashMap::new();
        hotkeys.insert("Alpha".to_string(), hk(0, Some(0x10)));
        let plans = plan_character_hotkeys(&characters, &hotkeys, 2000);
        assert!(plans.is_empty());
    }

    #[test]
    fn plan_character_hotkeys_translates_modifier_vk_into_modifier_kind() {
        let characters = vec!["Alpha".to_string()];
        let mut hotkeys = HashMap::new();
        hotkeys.insert("Alpha".to_string(), hk(0x70, Some(0x11))); // Ctrl
        let plans = plan_character_hotkeys(&characters, &hotkeys, 2000);
        assert_eq!(plans[0].modifier, Some(ModifierKind::Ctrl));
    }

    #[test]
    fn plan_character_hotkeys_treats_non_modifier_vk_as_no_modifier() {
        let characters = vec!["Alpha".to_string()];
        let mut hotkeys = HashMap::new();
        hotkeys.insert("Alpha".to_string(), hk(0x70, Some(0x41))); // 'A' isn't a modifier
        let plans = plan_character_hotkeys(&characters, &hotkeys, 2000);
        assert_eq!(plans[0].modifier, None);
    }
}
