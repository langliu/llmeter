use gpui::Hsla;
use gpui_component::{ActiveTheme, theme::Theme};

/// Copied theme colors so views can hold them across `cx.listener` calls
/// without keeping a borrow of `cx` alive.
#[derive(Clone, Copy)]
pub(crate) struct Palette {
    pub is_dark: bool,
    pub background: Hsla,
    pub foreground: Hsla,
    pub muted: Hsla,
    pub muted_foreground: Hsla,
    pub border: Hsla,
    pub tiles: Hsla,
    pub accent: Hsla,
    pub transparent: Hsla,
    pub success: Hsla,
    pub link: Hsla,
    pub popover: Hsla,
}

impl Palette {
    pub(crate) fn from_theme(theme: &Theme) -> Self {
        Self {
            is_dark: theme.is_dark(),
            background: theme.background,
            foreground: theme.foreground,
            muted: theme.muted,
            muted_foreground: theme.muted_foreground,
            border: theme.border,
            tiles: theme.tiles,
            accent: theme.accent,
            transparent: theme.transparent,
            success: theme.success,
            link: theme.link,
            popover: theme.popover,
        }
    }

    pub(crate) fn from_app(cx: &gpui::App) -> Self {
        Self::from_theme(cx.theme())
    }
}
