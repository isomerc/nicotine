//! View helpers for the iced config panel: the branded header/footer and
//! the four body sections (display mode, cycle order, hotkeys, previews).

use iced::widget::{
    button, checkbox, column, container, mouse_area, radio, row, slider, text, text_input,
    tooltip, Space,
};
use iced::{Alignment, Background, Border, Color, Element, Font, Length, Shadow, Vector};

use crate::config::{DisplayMode, Hotkey};

use super::{
    hotkey_label, CaptureTarget, Message, Panel, SliderField, Tab, CAPTION_SIZE, LOGO_SIZE,
    NICOTINE_BLACK, NICOTINE_CREAM, NICOTINE_GOLD, NICOTINE_GREEN, NICOTINE_RED,
    SECTION_SIZE,
};

const LOGO_FONT: Font = Font::with_name("Marlboro");
const BOLD: Font = Font {
    weight: iced::font::Weight::Bold,
    ..Font::with_name("JetBrains Mono")
};

/// A container style that's just a solid fill — the common case for the
/// header strip, footer, sidebar, and dividers.
fn filled(color: Color) -> container::Style {
    container::Style {
        background: Some(Background::Color(color)),
        ..container::Style::default()
    }
}

pub(super) fn header() -> Element<'static, Message> {
    container(
        mouse_area(
            text("Nicotine")
                .font(LOGO_FONT)
                .size(LOGO_SIZE)
                .color(NICOTINE_CREAM),
        )
        .on_press(Message::LogoClicked),
    )
    .center_x(Length::Fill)
    .center_y(Length::Fixed(72.0))
    .style(|_| filled(NICOTINE_RED))
    .into()
}

pub(super) fn footer() -> Element<'static, Message> {
    let links = row![
        link_button("GITHUB", "https://github.com/isomerc"),
        text("•").size(CAPTION_SIZE).color(NICOTINE_GOLD),
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
            .size(CAPTION_SIZE)
            .color(NICOTINE_GREEN)
            .font(BOLD)
            .into(),
        None => Space::new().into(),
    };

    let bar =
        container(row![links, Space::new().width(Length::Fill), badge].align_y(Alignment::Center))
            .width(Length::Fill)
            .padding([10, 16])
            .style(|_| filled(NICOTINE_CREAM));

    column![divider(), bar].into()
}

fn link_button(label: impl Into<String>, url: impl Into<String>) -> Element<'static, Message> {
    let url = url.into();
    button(
        text(label.into())
            .size(CAPTION_SIZE)
            .color(NICOTINE_RED)
            .font(BOLD),
    )
    .style(button::text)
    .padding(0)
    .on_press(Message::OpenLink(url))
    .into()
}

pub(super) fn tab_sidebar(panel: &Panel) -> Element<'_, Message> {
    let col = column![
        tab_button("Display", Tab::Display, panel.active_tab == Tab::Display),
        tab_button(
            "Characters",
            Tab::Characters,
            panel.active_tab == Tab::Characters
        ),
        tab_button("Hotkeys", Tab::Hotkeys, panel.active_tab == Tab::Hotkeys),
    ]
    .spacing(4);

    container(col)
        .padding(8)
        .width(Length::Fixed(150.0))
        .height(Length::Fill)
        .style(|_| filled(NICOTINE_CREAM))
        .into()
}

fn tab_button(label: &'static str, tab: Tab, active: bool) -> Element<'static, Message> {
    button(text(label))
        .width(Length::Fill)
        .padding([8, 12])
        .on_press(Message::TabSelected(tab))
        .style(move |_theme, status| {
            // Press is styled like hover, so clicking a tab doesn't flash.
            let (bg, fg) = if active {
                (NICOTINE_RED, NICOTINE_CREAM)
            } else if matches!(status, button::Status::Hovered | button::Status::Pressed) {
                (NICOTINE_GOLD, NICOTINE_BLACK)
            } else {
                (NICOTINE_CREAM, NICOTINE_BLACK)
            };
            button::Style {
                background: Some(Background::Color(bg)),
                text_color: fg,
                ..button::Style::default()
            }
        })
        .into()
}

pub(super) fn vdivider() -> Element<'static, Message> {
    container(Space::new().width(Length::Fixed(1.0)).height(Length::Fill))
        .height(Length::Fill)
        .style(|_| filled(NICOTINE_GOLD))
        .into()
}

pub(super) fn tab_content(panel: &Panel) -> Element<'_, Message> {
    let inner: Element<'_, Message> = match panel.active_tab {
        Tab::Display => column![display_mode_section(panel), previews_section(panel)]
            .spacing(20)
            .into(),
        Tab::Characters => characters_section(panel),
        Tab::Hotkeys => hotkeys_section(panel),
    };
    container(inner)
        .padding([12, 16])
        .width(Length::Fill)
        .into()
}

fn section_header(label: &'static str) -> Element<'static, Message> {
    column![
        text(label)
            .size(SECTION_SIZE)
            .color(NICOTINE_RED)
            .font(BOLD),
        divider(),
    ]
    .spacing(4)
    .into()
}

fn divider() -> Element<'static, Message> {
    container(Space::new().height(Length::Fixed(1.0)).width(Length::Fill))
        .width(Length::Fill)
        .style(|_| filled(NICOTINE_GOLD))
        .into()
}

fn caption(s: &'static str) -> Element<'static, Message> {
    text(s).size(CAPTION_SIZE).color(NICOTINE_BLACK).into()
}

/// Drag handle for a cycle-list row — press and drag it to reorder.
fn grip(i: usize) -> Element<'static, Message> {
    mouse_area(text("≡").size(SECTION_SIZE).color(Color {
        a: 0.55,
        ..NICOTINE_BLACK
    }))
    .on_press(Message::GrabRow(i))
    .into()
}

/// Container style for a cycle-list row mid-drag: the grabbed row lifts off
/// as a white card; the row under the cursor becomes a soft gold drop slot.
fn drag_row_style(is_dragged: bool, is_target: bool) -> container::Style {
    let gold_border = Border {
        color: NICOTINE_GOLD,
        width: 1.0,
        radius: 6.0_f32.into(),
    };
    if is_dragged {
        container::Style {
            background: Some(Background::Color(Color::WHITE)),
            border: gold_border,
            shadow: Shadow {
                color: Color {
                    a: 0.18,
                    ..NICOTINE_BLACK
                },
                offset: Vector { x: 0.0, y: 2.0 },
                blur_radius: 8.0,
            },
            ..container::Style::default()
        }
    } else if is_target {
        container::Style {
            background: Some(Background::Color(Color {
                a: 0.15,
                ..NICOTINE_GOLD
            })),
            border: gold_border,
            ..container::Style::default()
        }
    } else {
        container::Style::default()
    }
}

fn display_mode_section(panel: &Panel) -> Element<'_, Message> {
    let mut col = column![
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
    ]
    .spacing(8);

    // Hidden on GNOME Wayland, where the compositor blocks window
    // positioning so restacking can't do anything.
    if panel.restack_supported {
        col = col.push(button(text("Restack EVE Windows")).on_press(Message::RestackClicked));
    }

    col.into()
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
        // One compact keybind chip: left-click to record (a key chord, a
        // mouse button, or a wheel notch), right-click to clear. The tooltip
        // spells that out.
        let label = panel
            .config
            .character_hotkeys
            .get(name)
            .map(hotkey_label)
            .unwrap_or_else(|| "unbound".into());
        let keybind = tooltip(
            mouse_area(bind_button(
                panel,
                CaptureTarget::Character(name.clone()),
                label,
                130.0,
            ))
            .on_right_press(Message::ClearBinding(CaptureTarget::Character(name.clone()))),
            container(
                text("Left-click: set  ·  Right-click: clear")
                    .size(CAPTION_SIZE)
                    .color(NICOTINE_BLACK),
            )
            .padding(6)
            .style(|_| filled(NICOTINE_CREAM)),
            tooltip::Position::Top,
        );

        let row = row![
            grip(i),
            text(format!("{}.", i + 1)),
            text_input("character name", name)
                .on_input(move |s| Message::CharacterNameChanged(i, s))
                .width(Length::Fill),
            keybind,
            button(text("↑")).on_press(Message::MoveCharacterUp(i)),
            button(text("↓")).on_press(Message::MoveCharacterDown(i)),
            button(text("✕")).on_press(Message::RemoveCharacter(i)),
        ]
        .spacing(6)
        .align_y(Alignment::Center);

        let is_dragged = panel.dragging == Some(i);
        let is_target = panel.dragging.is_some() && panel.drag_hover == Some(i) && !is_dragged;
        let block = container(row)
            .padding(6)
            .style(move |_| drag_row_style(is_dragged, is_target));
        col = col.push(
            mouse_area(block)
                .on_enter(Message::HoverRow(i))
                .on_exit(Message::UnhoverRow(i)),
        );
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
    column![
        section_header("Keyboard Hotkeys"),
        checkbox(panel.config.enable_keyboard_buttons)
            .label("Enable keyboard cycling")
            .on_toggle(Message::KeyboardEnabledToggled),
        keybind_chip_row(panel, "Forward:", CaptureTarget::ForwardKey, &panel.config.forward_key),
        keybind_chip_row(
            panel,
            "Backward:",
            CaptureTarget::BackwardKey,
            &panel.config.backward_key,
        ),
        caption(
            "Left-click a binding to record a key chord, mouse button, or wheel notch \
             (Esc cancels). Right-click clears it. Backward defaults to Shift+forward."
        ),
        keybind_chip_row(
            panel,
            "Toggle overlay:",
            CaptureTarget::TogglePreviews,
            &panel.config.toggle_previews_key,
        ),
        caption(
            "Toggle overlay shows/hides the whole overlay with one binding — independent of \
             keyboard cycling. Leave unbound if you don't want it."
        ),
        checkbox(panel.config.enable_mouse_buttons)
            .label("Cycle on mouse side buttons (XBUTTON1/XBUTTON2)")
            .on_toggle(Message::MouseEnabledToggled),
        caption(
            "Off by default. Turn on only if you don't already remap your mouse side buttons via \
             driver software (Logi Options+, Razer Synapse, etc.)."
        ),
    ]
    .spacing(8)
    .into()
}

/// A labelled keybind chip: left-click to record (key chord / mouse button /
/// wheel), right-click to clear, with a tooltip. Shared by every bind site.
fn keybind_chip_row(
    panel: &Panel,
    label: &'static str,
    target: CaptureTarget,
    hk: &Hotkey,
) -> Element<'static, Message> {
    let text_label = if hk.is_bound() {
        hotkey_label(hk)
    } else {
        "unbound".into()
    };
    let chip = tooltip(
        mouse_area(bind_button(panel, target.clone(), text_label, 150.0))
            .on_right_press(Message::ClearBinding(target)),
        container(
            text("Left-click: set  ·  Right-click: clear")
                .size(CAPTION_SIZE)
                .color(NICOTINE_BLACK),
        )
        .padding(6)
        .style(|_| filled(NICOTINE_CREAM)),
        tooltip::Position::Top,
    );
    row![
        text(label).width(Length::Fixed(120.0)),
        chip,
    ]
    .spacing(6)
    .align_y(Alignment::Center)
    .into()
}

fn previews_section(panel: &Panel) -> Element<'_, Message> {
    let mut col = column![
        section_header("Preview Windows"),
        checkbox(panel.config.show_previews)
            .label("Show preview windows")
            .on_toggle(Message::ShowPreviewsToggled),
        checkbox(panel.config.hide_active_preview)
            .label("Hide the active client's preview")
            .on_toggle(Message::HideActivePreviewToggled),
        checkbox(panel.config.constrain_aspect)
            .label("Constrain aspect ratio")
            .on_toggle(Message::ConstrainAspectToggled),
    ]
    .spacing(8);

    // One "size" slider when the ratio is locked; otherwise independent
    // width + height sliders.
    if panel.config.constrain_aspect {
        col = col.push(slider_field(
            panel,
            SliderField::PreviewSize,
            "Size:",
            panel.config.preview_width,
            "px",
        ));
    } else {
        col = col.push(slider_field(
            panel,
            SliderField::PreviewWidth,
            "Width:",
            panel.config.preview_width,
            "px",
        ));
        col = col.push(slider_field(
            panel,
            SliderField::PreviewHeight,
            "Height:",
            panel.config.preview_height,
            "px",
        ));
    }

    col = col.push(slider_field(
        panel,
        SliderField::PreviewOpacity,
        "Opacity:",
        panel.config.preview_opacity,
        "%",
    ));
    col.into()
}

/// A slider with a typable numeric readout: click the value, type an exact
/// number, press Enter and the slider snaps to it. The text mirrors the
/// value except while you're mid-edit. Reused for every slider.
fn slider_field<'a>(
    panel: &'a Panel,
    field: SliderField,
    label: &'static str,
    value: u32,
    unit: &'static str,
) -> Element<'a, Message> {
    let buffer = panel
        .slider_text
        .get(&field)
        .map(String::as_str)
        .unwrap_or("");
    row![
        text(label).width(Length::Fixed(70.0)),
        slider(field.range(), value, move |v| field.change_message(v)).step(1u32),
        text_input("", buffer)
            .on_input(move |s| Message::SliderTextChanged(field, s))
            .on_submit(Message::SliderTextCommit(field))
            .width(Length::Fixed(56.0)),
        text(unit),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
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
        .style(move |_theme, status| {
            if is_capturing {
                // Armed and listening for the next key.
                button::Style {
                    background: Some(Background::Color(NICOTINE_GOLD)),
                    text_color: NICOTINE_BLACK,
                    border: Border {
                        color: NICOTINE_RED,
                        width: 1.5,
                        radius: 4.0_f32.into(),
                    },
                    ..button::Style::default()
                }
            } else {
                // Neutral outlined chip showing the current binding.
                let background =
                    matches!(status, button::Status::Hovered).then_some(Background::Color(Color {
                        a: 0.18,
                        ..NICOTINE_GOLD
                    }));
                button::Style {
                    background,
                    text_color: NICOTINE_BLACK,
                    border: Border {
                        color: NICOTINE_GOLD,
                        width: 1.0,
                        radius: 4.0_f32.into(),
                    },
                    ..button::Style::default()
                }
            }
        })
        .into()
}
