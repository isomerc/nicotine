//! View helpers for the iced config panel: the branded header/footer and
//! the four body sections (display mode, cycle order, hotkeys, previews).

use iced::widget::{
    button, checkbox, column, container, pick_list, radio, row, slider, text, text_input, Space,
};
use iced::{Alignment, Background, Element, Font, Length};

use crate::config::DisplayMode;

use super::{
    code_to_label, CaptureTarget, Message, Panel, MODIFIER_CHOICES, NICOTINE_BLACK, NICOTINE_CREAM,
    NICOTINE_GOLD, NICOTINE_GREEN, NICOTINE_RED,
};

const LOGO_FONT: Font = Font::with_name("Marlboro");
const BOLD: Font = Font {
    weight: iced::font::Weight::Bold,
    ..Font::with_name("JetBrains Mono")
};

pub(super) fn header() -> Element<'static, Message> {
    container(
        text("Nicotine")
            .font(LOGO_FONT)
            .size(48)
            .color(NICOTINE_CREAM),
    )
    .center_x(Length::Fill)
    .center_y(Length::Fixed(72.0))
    .style(|_theme| container::Style {
        background: Some(Background::Color(NICOTINE_RED)),
        ..container::Style::default()
    })
    .into()
}

pub(super) fn footer() -> Element<'static, Message> {
    let links = row![
        link_button("GITHUB", "https://github.com/isomerc"),
        text("•").color(NICOTINE_GOLD),
        link_button(
            "ILLUMINATED IS RECRUITING",
            "https://www.illuminatedcorp.com"
        ),
    ]
    .spacing(14)
    .align_y(Alignment::Center);

    let badge: Element<'static, Message> = match crate::version_check::get_update_status() {
        Some(crate::version_check::UpdateStatus::Outdated { version, url }) => {
            link_button(format!("NEW VERSION AVAILABLE (v{version})"), url)
        }
        Some(crate::version_check::UpdateStatus::UpToDate) => text("LATEST VERSION")
            .color(NICOTINE_GREEN)
            .font(BOLD)
            .into(),
        None => Space::new().into(),
    };

    container(row![links, Space::new().width(Length::Fill), badge].align_y(Alignment::Center))
        .width(Length::Fill)
        .height(Length::Fixed(40.0))
        .padding([8, 16])
        .style(|_theme| container::Style {
            background: Some(Background::Color(NICOTINE_CREAM)),
            ..container::Style::default()
        })
        .into()
}

fn link_button(label: impl Into<String>, url: impl Into<String>) -> Element<'static, Message> {
    let url = url.into();
    button(text(label.into()).color(NICOTINE_RED).font(BOLD).size(13))
        .style(button::text)
        .padding(0)
        .on_press(Message::OpenLink(url))
        .into()
}

pub(super) fn body(panel: &Panel) -> Element<'_, Message> {
    container(
        column![
            display_mode_section(panel),
            characters_section(panel),
            hotkeys_section(panel),
            previews_section(panel),
        ]
        .spacing(20),
    )
    .padding([12, 16])
    .into()
}

fn section_header(label: &'static str) -> Element<'static, Message> {
    column![
        text(label).size(16).color(NICOTINE_RED).font(BOLD),
        divider(),
    ]
    .spacing(4)
    .into()
}

fn divider() -> Element<'static, Message> {
    container(Space::new().height(Length::Fixed(1.0)).width(Length::Fill))
        .width(Length::Fill)
        .style(|_theme| container::Style {
            background: Some(Background::Color(NICOTINE_GOLD)),
            ..container::Style::default()
        })
        .into()
}

fn caption(s: &'static str) -> Element<'static, Message> {
    text(s).size(11).color(NICOTINE_BLACK).into()
}

fn display_mode_section(panel: &Panel) -> Element<'_, Message> {
    column![
        section_header("Display Mode"),
        caption(
            "How Nicotine shows your running clients on screen. Preview windows mirror each \
             client live; the list view is a compact always-on-top window of names."
        ),
        row![
            radio(
                "Preview windows",
                DisplayMode::Previews,
                Some(panel.config.display_mode),
                Message::DisplayModeChanged,
            ),
            radio(
                "Client list",
                DisplayMode::List,
                Some(panel.config.display_mode),
                Message::DisplayModeChanged,
            ),
        ]
        .spacing(16),
        checkbox(panel.config.positions_locked)
            .label("Lock positions (drag disabled on previews and list)")
            .on_toggle(Message::LockToggled),
        button(text("Restack EVE Windows")).on_press(Message::RestackClicked),
    ]
    .spacing(8)
    .into()
}

fn characters_section(panel: &Panel) -> Element<'_, Message> {
    let mut col = column![
        section_header("Cycle Order"),
        caption(
            "Characters cycle in the order shown. Names must match EVE's window title exactly \
             (the part after \"EVE - \")."
        ),
    ]
    .spacing(8);

    for (i, name) in panel.config.characters.iter().enumerate() {
        let row1 = row![
            text(format!("{}.", i + 1)),
            text_input("character name", name)
                .on_input(move |s| Message::CharacterNameChanged(i, s))
                .width(Length::Fill),
            button(text("↑")).on_press(Message::MoveCharacterUp(i)),
            button(text("↓")).on_press(Message::MoveCharacterDown(i)),
            button(text("✕")).on_press(Message::RemoveCharacter(i)),
        ]
        .spacing(6)
        .align_y(Alignment::Center);

        let current_mod = panel
            .config
            .character_hotkeys
            .get(name)
            .and_then(|h| h.modifier);
        let selected = MODIFIER_CHOICES
            .iter()
            .copied()
            .find(|m| m.code == current_mod);
        let name_for_mod = name.clone();
        let modifier_pick = pick_list(MODIFIER_CHOICES, selected, move |c| {
            Message::CharacterModifierChanged(name_for_mod.clone(), c)
        })
        .width(Length::Fixed(80.0));

        let binding_label = panel
            .config
            .character_hotkeys
            .get(name)
            .filter(|h| h.vk != 0)
            .map(|h| code_to_label(h.vk))
            .unwrap_or_else(|| "none".into());

        let mut row2 = row![
            Space::new().width(Length::Fixed(20.0)),
            text("Hotkey:"),
            modifier_pick,
            bind_button(
                panel,
                CaptureTarget::Character(name.clone()),
                binding_label,
                110.0
            ),
        ]
        .spacing(6)
        .align_y(Alignment::Center);

        if panel.config.character_hotkeys.contains_key(name) {
            let n = name.clone();
            row2 = row2.push(button(text("✕")).on_press(Message::ClearCharacterHotkey(n)));
        }

        col = col.push(column![row1, row2].spacing(2));
    }

    col = col.push(
        row![
            text("Add:"),
            text_input("new character", &panel.new_character_buffer)
                .on_input(Message::NewCharacterChanged)
                .on_submit(Message::AddCharacter)
                .width(Length::Fill),
            button(text("+")).on_press(Message::AddCharacter),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    );

    col.into()
}

fn hotkeys_section(panel: &Panel) -> Element<'_, Message> {
    let forward = row![
        text("Forward:").width(Length::Fixed(90.0)),
        bind_button(
            panel,
            CaptureTarget::ForwardKey,
            code_to_label(panel.config.forward_key),
            200.0,
        ),
    ]
    .spacing(6)
    .align_y(Alignment::Center);

    let backward = row![
        text("Backward:").width(Length::Fixed(90.0)),
        bind_button(
            panel,
            CaptureTarget::BackwardKey,
            code_to_label(panel.config.backward_key),
            200.0,
        ),
    ]
    .spacing(6)
    .align_y(Alignment::Center);

    let modifier_label = match panel.config.modifier_key {
        Some(vk) => code_to_label(vk),
        None => "None".to_string(),
    };
    let mut modifier = row![
        text("Modifier:").width(Length::Fixed(90.0)),
        bind_button(panel, CaptureTarget::ModifierKey, modifier_label, 200.0),
    ]
    .spacing(6)
    .align_y(Alignment::Center);
    if panel.config.modifier_key.is_some() {
        modifier = modifier.push(button(text("Clear")).on_press(Message::ClearModifier));
    }

    column![
        section_header("Keyboard Hotkeys"),
        checkbox(panel.config.enable_keyboard_buttons)
            .label("Enable keyboard cycling")
            .on_toggle(Message::KeyboardEnabledToggled),
        forward,
        backward,
        modifier,
        text(
            "Click a binding to record the next key you press. Esc cancels. Set both keys to the \
             same value with a modifier to cycle backward via modifier+key (e.g. Tab + Shift+Tab)."
        )
        .size(10)
        .color(NICOTINE_BLACK),
        checkbox(panel.config.enable_mouse_buttons)
            .label("Cycle on mouse side buttons (XBUTTON1/XBUTTON2)")
            .on_toggle(Message::MouseEnabledToggled),
        text(
            "Off by default. Turn on only if you don't already remap your mouse side buttons via \
             driver software (Logi Options+, Razer Synapse, etc.)."
        )
        .size(10)
        .color(NICOTINE_BLACK),
    ]
    .spacing(8)
    .into()
}

fn previews_section(panel: &Panel) -> Element<'_, Message> {
    column![
        section_header("Preview Windows"),
        checkbox(panel.config.show_previews)
            .label("Show preview windows")
            .on_toggle(Message::ShowPreviewsToggled),
        row![
            text("Width:").width(Length::Fixed(70.0)),
            slider(
                120..=800u32,
                panel.config.preview_width,
                Message::PreviewWidthChanged
            )
            .step(1u32),
            text(format!("{} px", panel.config.preview_width)),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
        row![
            text("Height:").width(Length::Fixed(70.0)),
            slider(
                80..=600u32,
                panel.config.preview_height,
                Message::PreviewHeightChanged
            )
            .step(1u32),
            text(format!("{} px", panel.config.preview_height)),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    ]
    .spacing(8)
    .into()
}

fn bind_button(
    panel: &Panel,
    target: CaptureTarget,
    label: String,
    width: f32,
) -> Element<'static, Message> {
    let is_capturing = panel.capturing.as_ref() == Some(&target);
    let txt = if is_capturing {
        "[press key — Esc]".to_string()
    } else {
        label
    };
    button(text(txt))
        .width(Length::Fixed(width))
        .on_press(Message::StartCapture(target))
        .into()
}
