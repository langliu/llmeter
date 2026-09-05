//! Opaque, neutral surfaces and a restrained violet accent, shared by native
//! controls and application views. Install configs rather than patching colors
//! after Theme::change so GPUI's semantic tokens stay in sync as well.
use std::rc::Rc;

use gpui::App;
use gpui_component::theme::{Theme, ThemeConfig, ThemeConfigColors, ThemeMode};

pub(crate) fn install(cx: &mut App) {
    let theme = Theme::global_mut(cx);
    theme.light_theme = Rc::new(config(false));
    theme.dark_theme = Rc::new(config(true));
}

fn config(dark: bool) -> ThemeConfig {
    let (
        background,
        surface,
        sidebar,
        muted,
        border,
        foreground,
        secondary,
        accent,
        violet,
        success,
    ) = if dark {
        (
            "#141518", "#1B1C20", "#101114", "#26272D", "#303138", "#EEEFF2", "#989AA6", "#2C2942",
            "#A49BFF", "#62C99A",
        )
    } else {
        (
            "#FAFAFB", "#FFFFFF", "#F3F3F5", "#EDEEF1", "#E3E4E8", "#22232A", "#686B78", "#EEEBFC",
            "#6554C0", "#207A54",
        )
    };
    let color = |value: &str| Some(gpui::SharedString::from(value.to_string()));
    // ThemeConfigColors contains private ANSI fields, so populate its public
    // semantic fields on the default value rather than using struct update.
    macro_rules! colors {
        ($($field:ident: $value:expr),* $(,)?) => {{
            let mut colors = ThemeConfigColors::default();
            $(colors.$field = $value;)*
            colors
        }};
    }
    ThemeConfig {
        name: if dark {
            "LLMeter Dark"
        } else {
            "LLMeter Light"
        }
        .into(),
        mode: if dark {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        },
        font_size: Some(14.0),
        radius: Some(6),
        radius_lg: Some(10),
        shadow: Some(false),
        colors: colors! {
            background: color(background),
            foreground: color(foreground),
            tiles: color(surface),
            popover: color(surface),
            popover_foreground: color(foreground),
            muted: color(muted),
            muted_foreground: color(secondary),
            border: color(border),
            input: color(border),
            accent: color(accent),
            accent_foreground: color(foreground),
            primary: color("#7160D5"),
            primary_hover: color("#7463D7"),
            primary_active: color("#6250C2"),
            primary_foreground: color("#FFFFFF"),
            secondary: color(muted),
            secondary_hover: color(border),
            secondary_active: color(accent),
            secondary_foreground: color(foreground),
            button: color(surface),
            button_foreground: color(foreground),
            button_hover: color(muted),
            button_active: color(accent),
            link: color(violet),
            link_hover: color(violet),
            link_active: color(violet),
            ring: color(violet),
            caret: color(violet),
            selection: color(accent),
            success: color(success),
            warning: color(if dark { "#D7AE68" } else { "#926100" }),
            danger: color(if dark { "#F0808B" } else { "#BC3D49" }),
            sidebar: color(sidebar),
            sidebar_foreground: color(foreground),
            sidebar_border: color(border),
            sidebar_accent: color(accent),
            sidebar_accent_foreground: color(violet),
            sidebar_primary: color(violet),
            sidebar_primary_foreground: color(surface),
            title_bar: color(background),
            title_bar_border: color(border),
            list: color(surface),
            list_head: color(background),
            list_hover: color(muted),
            list_active: color(accent),
            list_active_border: color(violet),
            table: color(surface),
            table_head: color(background),
            table_head_foreground: color(secondary),
            table_hover: color(muted),
            table_active: color(accent),
            table_active_border: color(violet),
            table_row_border: color(border),
            tab: color(background),
            tab_bar: color(background),
            tab_bar_segmented: color(muted),
            tab_active: color(surface),
            tab_active_foreground: color(foreground),
            tab_foreground: color(secondary),
            progress_bar: color(violet),
            slider_bar: color(violet),
            switch: color("#7160D5"),
            scrollbar_thumb: color(border),
            scrollbar_thumb_hover: color(secondary),
        },
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Hsla, Rgba, rgb};

    fn luminance(color: Hsla) -> f32 {
        let c = Rgba::from(color);
        let linear = |v: f32| {
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(c.r) + 0.7152 * linear(c.g) + 0.0722 * linear(c.b)
    }

    fn contrast(a: Hsla, b: Hsla) -> f32 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn both_modes_have_readable_text_and_consistent_surfaces() {
        for dark in [false, true] {
            let mut theme = Theme::default();
            theme.apply_config(&Rc::new(config(dark)));
            assert_eq!(theme.is_dark(), dark);
            assert_eq!(theme.tiles, theme.popover);
            for surface in [theme.background, theme.tiles, theme.sidebar] {
                for text in [
                    theme.foreground,
                    theme.muted_foreground,
                    theme.link,
                    theme.success,
                    theme.warning,
                    theme.danger,
                ] {
                    assert!(
                        contrast(text, surface) >= 4.5,
                        "dark={dark}, contrast={}",
                        contrast(text, surface)
                    );
                }
            }
            for background in [theme.primary, theme.primary_hover, theme.primary_active] {
                assert!(contrast(background, rgb(0xffffff).into()) >= 4.5);
            }
            assert!(contrast(theme.link, theme.accent) >= 4.5);
            assert_eq!(theme.semantic_tokens().colors.background, theme.background);
        }
    }
}
