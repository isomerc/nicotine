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

use crate::config::{Hotkey, TriggerKind};
use std::collections::{HashMap, HashSet};

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
    Win,
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
        0x5B | 0x5C => Some(ModifierKind::Win),
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

/// Map a Hotkey's modifier codes to RegisterHotKey modifier kinds, dropping
/// any code that isn't a recognized modifier.
fn mods_to_kinds(mods: &[u16]) -> Vec<ModifierKind> {
    mods.iter().filter_map(|&m| modifier_kind(m)).collect()
}

/// A planned cycle (forward/backward) hotkey registration. Output of
/// `plan_cycle_hotkeys`; the Windows consumer turns each entry into a
/// `RegisterHotKey` call (its modifiers OR'd into the fsModifiers mask).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleHotkeyPlan {
    pub id: i32,
    pub modifiers: Vec<ModifierKind>,
    pub vk: u16,
}

/// Plan which cycle hotkeys to register. Only key-triggered bindings are
/// RegisterHotKey-able; mouse/wheel cycle bindings are dispatched by the
/// low-level mouse hook, so they're skipped here. Each emitted plan carries
/// the binding's full modifier chord (e.g. Shift+Tab for backward).
pub fn plan_cycle_hotkeys(
    enable: bool,
    forward: &Hotkey,
    backward: &Hotkey,
    forward_id: i32,
    backward_id: i32,
) -> Vec<CycleHotkeyPlan> {
    if !enable {
        return Vec::new();
    }
    let mut plans = Vec::new();
    for (hk, id) in [(forward, forward_id), (backward, backward_id)] {
        if hk.kind == TriggerKind::Key && hk.code != 0 {
            plans.push(CycleHotkeyPlan {
                id,
                modifiers: mods_to_kinds(&hk.mods),
                vk: hk.code,
            });
        }
    }
    plans
}

/// A planned per-character hotkey registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharacterHotkeyPlan {
    pub id: i32,
    pub character_name: String,
    pub modifiers: Vec<ModifierKind>,
    pub vk: u16,
}

/// Plan per-character key hotkeys in the user-configured character order.
/// Skips characters with no entry, unbound entries (key code 0), and
/// mouse/wheel entries (the mouse hook handles those). IDs are dense.
pub fn plan_character_hotkeys(
    characters: &[String],
    character_hotkeys: &HashMap<String, Hotkey>,
    base_id: i32,
) -> Vec<CharacterHotkeyPlan> {
    let mut out = Vec::new();
    let mut next_id = base_id;
    for name in characters {
        let Some(hk) = character_hotkeys.get(name) else {
            continue;
        };
        if hk.kind != TriggerKind::Key || hk.code == 0 {
            continue;
        }
        out.push(CharacterHotkeyPlan {
            id: next_id,
            character_name: name.clone(),
            modifiers: mods_to_kinds(&hk.mods),
            vk: hk.code,
        });
        next_id += 1;
    }
    out
}

/// What a mouse-button / wheel trigger maps to, resolved from the configured
/// bindings. `SwitchCharacter` carries the index into the `characters` slice
/// so the (Win32-side) caller can pass it through a thread message without
/// allocating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MouseHotkeyAction {
    CycleForward,
    CycleBackward,
    ToggleOverlay,
    SwitchCharacter(usize),
}

/// Resolve a mouse/wheel trigger (button or wheel code + held modifiers) to an
/// action, mirroring the Linux mouse listener's priority: toggle, then
/// backward, then forward, then per-character (most-specific chord wins). Only
/// `TriggerKind::Mouse` / `TriggerKind::Wheel` bindings can match here; key
/// bindings go through RegisterHotKey.
#[allow(clippy::too_many_arguments)]
pub fn resolve_mouse_action(
    kind: TriggerKind,
    code: u16,
    held: &HashSet<u16>,
    forward: &Hotkey,
    backward: &Hotkey,
    toggle: &Hotkey,
    characters: &[String],
    character_hotkeys: &HashMap<String, Hotkey>,
) -> Option<MouseHotkeyAction> {
    if toggle.matches(kind, code, held) {
        return Some(MouseHotkeyAction::ToggleOverlay);
    }
    if backward.matches(kind, code, held) {
        return Some(MouseHotkeyAction::CycleBackward);
    }
    if forward.matches(kind, code, held) {
        return Some(MouseHotkeyAction::CycleForward);
    }
    let mut best: Option<(usize, usize)> = None; // (character index, mods len)
    for (i, name) in characters.iter().enumerate() {
        if let Some(hk) = character_hotkeys.get(name) {
            if hk.matches(kind, code, held) && best.is_none_or(|(_, b)| hk.mods.len() > b) {
                best = Some((i, hk.mods.len()));
            }
        }
    }
    best.map(|(i, _)| MouseHotkeyAction::SwitchCharacter(i))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WHEEL_UP;

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
        let plans = plan_cycle_hotkeys(false, &Hotkey::key(0x70), &Hotkey::key(0x71), 1, 2);
        assert!(plans.is_empty());
    }

    #[test]
    fn plan_cycle_hotkeys_emits_both_when_key_bound() {
        let plans = plan_cycle_hotkeys(true, &Hotkey::key(0x70), &Hotkey::key(0x71), 1, 2);
        assert_eq!(
            plans,
            vec![
                CycleHotkeyPlan { id: 1, modifiers: vec![], vk: 0x70 },
                CycleHotkeyPlan { id: 2, modifiers: vec![], vk: 0x71 },
            ]
        );
    }

    #[test]
    fn plan_cycle_hotkeys_carries_backward_chord() {
        // Classic Tab forward, Shift+Tab backward.
        let plans = plan_cycle_hotkeys(
            true,
            &Hotkey::key(0x09),
            &Hotkey::key_with_mods(0x09, vec![0x10]),
            1,
            2,
        );
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].modifiers, vec![]);
        assert_eq!(plans[1].modifiers, vec![ModifierKind::Shift]);
        assert_eq!(plans[1].vk, 0x09);
    }

    #[test]
    fn plan_cycle_hotkeys_skips_unbound_and_mouse_bindings() {
        // Forward unbound; backward bound to a mouse button — neither is
        // RegisterHotKey-able, so nothing is planned.
        let mouse = Hotkey { mods: vec![], kind: TriggerKind::Mouse, code: 5 };
        let plans = plan_cycle_hotkeys(true, &Hotkey::default(), &mouse, 1, 2);
        assert!(plans.is_empty());
    }

    // ---- plan_character_hotkeys ----------------------------------------

    fn hk(vk: u16, mods: Vec<u16>) -> Hotkey {
        Hotkey::key_with_mods(vk, mods)
    }

    #[test]
    fn plan_character_hotkeys_emits_entries_in_character_order() {
        let characters = vec!["Beta".to_string(), "Alpha".to_string()];
        let mut hotkeys = HashMap::new();
        hotkeys.insert("Alpha".to_string(), hk(0x70, vec![]));
        hotkeys.insert("Beta".to_string(), hk(0x71, vec![]));
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
        hotkeys.insert("Beta".to_string(), hk(0x71, vec![]));
        let plans = plan_character_hotkeys(&characters, &hotkeys, 2000);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].character_name, "Beta");
        assert_eq!(plans[0].id, 2000);
    }

    #[test]
    fn plan_character_hotkeys_skips_unbound_key() {
        let characters = vec!["Alpha".to_string()];
        let mut hotkeys = HashMap::new();
        hotkeys.insert("Alpha".to_string(), hk(0, vec![0x10]));
        let plans = plan_character_hotkeys(&characters, &hotkeys, 2000);
        assert!(plans.is_empty());
    }

    #[test]
    fn plan_character_hotkeys_skips_mouse_bindings() {
        let characters = vec!["Alpha".to_string()];
        let mut hotkeys = HashMap::new();
        hotkeys.insert(
            "Alpha".to_string(),
            Hotkey { mods: vec![], kind: TriggerKind::Mouse, code: 5 },
        );
        let plans = plan_character_hotkeys(&characters, &hotkeys, 2000);
        assert!(plans.is_empty());
    }

    #[test]
    fn plan_character_hotkeys_translates_modifier_chord() {
        let characters = vec!["Alpha".to_string()];
        let mut hotkeys = HashMap::new();
        hotkeys.insert("Alpha".to_string(), hk(0x70, vec![0x11, 0x10])); // Ctrl+Shift
        let plans = plan_character_hotkeys(&characters, &hotkeys, 2000);
        // key_with_mods sorts by code: 0x10 (Shift) before 0x11 (Ctrl).
        assert_eq!(plans[0].modifiers, vec![ModifierKind::Shift, ModifierKind::Ctrl]);
    }

    #[test]
    fn plan_character_hotkeys_drops_non_modifier_codes() {
        let characters = vec!["Alpha".to_string()];
        let mut hotkeys = HashMap::new();
        hotkeys.insert("Alpha".to_string(), hk(0x70, vec![0x41])); // 'A' isn't a modifier
        let plans = plan_character_hotkeys(&characters, &hotkeys, 2000);
        assert_eq!(plans[0].modifiers, vec![]);
    }

    // ---- resolve_mouse_action ------------------------------------------

    fn mouse(code: u16, mods: Vec<u16>) -> Hotkey {
        Hotkey { mods, kind: TriggerKind::Mouse, code }
    }

    #[test]
    fn resolve_mouse_action_prioritises_toggle_then_cycle() {
        let none = HashSet::new();
        let fwd = mouse(1, vec![]);
        let back = mouse(2, vec![]);
        let toggle = mouse(3, vec![]);
        let chars: Vec<String> = vec![];
        let map = HashMap::new();
        assert_eq!(
            resolve_mouse_action(TriggerKind::Mouse, 3, &none, &fwd, &back, &toggle, &chars, &map),
            Some(MouseHotkeyAction::ToggleOverlay)
        );
        assert_eq!(
            resolve_mouse_action(TriggerKind::Mouse, 2, &none, &fwd, &back, &toggle, &chars, &map),
            Some(MouseHotkeyAction::CycleBackward)
        );
        assert_eq!(
            resolve_mouse_action(TriggerKind::Mouse, 1, &none, &fwd, &back, &toggle, &chars, &map),
            Some(MouseHotkeyAction::CycleForward)
        );
    }

    #[test]
    fn resolve_mouse_action_matches_wheel_and_character_index() {
        let none = HashSet::new();
        let unbound = Hotkey::default();
        let chars = vec!["Alpha".to_string(), "Bravo".to_string()];
        let mut map = HashMap::new();
        map.insert("Bravo".to_string(), Hotkey { mods: vec![], kind: TriggerKind::Wheel, code: WHEEL_UP });
        assert_eq!(
            resolve_mouse_action(
                TriggerKind::Wheel, WHEEL_UP, &none, &unbound, &unbound, &unbound, &chars, &map,
            ),
            Some(MouseHotkeyAction::SwitchCharacter(1))
        );
        // Wrong kind (a key) never matches a mouse/wheel resolve.
        assert_eq!(
            resolve_mouse_action(
                TriggerKind::Key, WHEEL_UP, &none, &unbound, &unbound, &unbound, &chars, &map,
            ),
            None
        );
    }

    #[test]
    fn resolve_mouse_action_requires_modifier_chord() {
        let unbound = Hotkey::default();
        let chars: Vec<String> = vec![];
        let map = HashMap::new();
        // Ctrl(0x11)+Mouse4 backward.
        let back = mouse(4, vec![0x11]);
        let none = HashSet::new();
        assert_eq!(
            resolve_mouse_action(TriggerKind::Mouse, 4, &none, &unbound, &back, &unbound, &chars, &map),
            None
        );
        let ctrl: HashSet<u16> = [0x11].into_iter().collect();
        assert_eq!(
            resolve_mouse_action(TriggerKind::Mouse, 4, &ctrl, &unbound, &back, &unbound, &chars, &map),
            Some(MouseHotkeyAction::CycleBackward)
        );
    }
}