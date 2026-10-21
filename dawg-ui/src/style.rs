//! Shared colors and control states for the native server workspace.
use iced::{
    Background, Border, Color, Theme,
    widget::{button, container, text_editor, text_input},
};

pub const BG: Color = Color::from_rgb8(36, 39, 43);
pub const SIDE: Color = Color::from_rgb8(28, 31, 35);
pub const PANEL: Color = Color::from_rgb8(42, 46, 51);
pub const RAISED: Color = Color::from_rgb8(49, 54, 60);
pub const CONSOLE: Color = Color::from_rgb8(31, 34, 38);
pub const LINE: Color = Color::from_rgb8(63, 70, 78);
pub const INK: Color = Color::from_rgb8(235, 239, 243);
pub const MUTED: Color = Color::from_rgb8(173, 182, 192);
pub const ACCENT: Color = Color::from_rgb8(148, 183, 212);
pub const SUCCESS: Color = Color::from_rgb8(137, 190, 152);
pub const RED: Color = Color::from_rgb8(239, 153, 153);
pub const AMBER: Color = Color::from_rgb8(224, 193, 139);

const PRIMARY: Color = Color::from_rgb8(62, 88, 110);
const SELECTION: Color = Color::from_rgb8(48, 64, 79);

pub fn surface(color: Color, radius: f32) -> container::Style {
    container::Style {
        background: Some(color.into()),
        text_color: Some(INK),
        border: Border {
            color: LINE,
            width: 1.0,
            radius: radius.into(),
        },
        ..Default::default()
    }
}

pub fn plain(color: Color) -> container::Style {
    container::Style {
        background: Some(color.into()),
        text_color: Some(INK),
        ..Default::default()
    }
}

#[derive(Clone, Copy)]
pub enum ButtonKind {
    Primary,
    Secondary,
    Ghost,
    Danger,
    Selected,
}

pub fn action(kind: ButtonKind) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let (mut background, mut text_color, mut border) = match kind {
            ButtonKind::Primary => (PRIMARY, INK, Color::from_rgb8(96, 126, 151)),
            ButtonKind::Secondary => (RAISED, INK, LINE),
            ButtonKind::Ghost => (Color::TRANSPARENT, INK, Color::TRANSPARENT),
            ButtonKind::Danger => (Color::TRANSPARENT, RED, LINE),
            ButtonKind::Selected => (
                SELECTION,
                Color::from_rgb8(220, 235, 247),
                Color::TRANSPARENT,
            ),
        };
        match status {
            button::Status::Hovered => {
                background = match kind {
                    ButtonKind::Ghost => PANEL,
                    ButtonKind::Danger => Color::from_rgb8(69, 44, 48),
                    _ => Color {
                        r: (background.r + 0.035).min(1.0),
                        g: (background.g + 0.035).min(1.0),
                        b: (background.b + 0.035).min(1.0),
                        a: 1.0,
                    },
                };
                if !matches!(kind, ButtonKind::Ghost | ButtonKind::Selected) {
                    border = MUTED;
                }
            }
            button::Status::Pressed => {
                background = SELECTION;
                border = ACCENT;
            }
            button::Status::Disabled => {
                background = PANEL;
                text_color = MUTED;
                border = LINE;
            }
            button::Status::Active => {}
        }
        button::Style {
            background: Some(Background::Color(background)),
            text_color,
            border: Border {
                color: border,
                width: 1.0,
                radius: 4.0.into(),
            },
            ..Default::default()
        }
    }
}

pub fn input(_: &Theme, status: text_input::Status) -> text_input::Style {
    text_input::Style {
        background: if matches!(status, text_input::Status::Disabled) {
            PANEL
        } else {
            CONSOLE
        }
        .into(),
        border: Border {
            color: if matches!(status, text_input::Status::Focused { .. }) {
                ACCENT
            } else {
                LINE
            },
            width: 1.0,
            radius: 4.0.into(),
        },
        icon: MUTED,
        placeholder: MUTED,
        value: if matches!(status, text_input::Status::Disabled) {
            MUTED
        } else {
            INK
        },
        selection: PRIMARY,
    }
}

pub fn editor(_: &Theme, status: text_editor::Status) -> text_editor::Style {
    text_editor::Style {
        background: CONSOLE.into(),
        border: Border {
            color: if matches!(status, text_editor::Status::Focused { .. }) {
                ACCENT
            } else {
                LINE
            },
            width: 1.0,
            radius: 4.0.into(),
        },
        placeholder: MUTED,
        value: INK,
        selection: PRIMARY,
    }
}
