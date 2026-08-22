use chrono::Local;
use gpui::{
    AnyElement, Context, FontWeight, IntoElement, ParentElement, Styled, div, prelude::*, px,
};
use gpui_component::{
    Icon, IconName, Selectable,
    button::{Button, ButtonCustomVariant, ButtonGroup, ButtonVariants},
    h_flex,
    switch::Switch,
    v_flex,
};
use llmeter_core::ProviderStatus;
use rust_i18n::t;

use crate::{
    app::{LLMeterView, ThemePreference},
    i18n::LocalePreference,
    views::palette::Palette,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SettingsSection {
    #[default]
    Appearance,
    Data,
}

impl SettingsSection {
    const ALL: [Self; 2] = [Self::Appearance, Self::Data];

    fn label(self) -> String {
        match self {
            Self::Appearance => t!("settings.appearance").to_string(),
            Self::Data => t!("settings.data").to_string(),
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::Appearance => IconName::Palette,
            Self::Data => IconName::HardDrive,
        }
    }
}

pub(crate) fn settings_page(view: &LLMeterView, cx: &mut Context<LLMeterView>) -> impl IntoElement {
    let section = view.settings_section;
    let p = Palette::from_app(cx);

    v_flex()
        .size_full()
        .px_8()
        .pt_3()
        .pb_3()
        .child(
            div()
                .text_3xl()
                .font_weight(FontWeight::BOLD)
                .text_color(p.foreground)
                .child(t!("settings.title").to_string()),
        )
        .child(
            h_flex()
                .flex_1()
                .min_h(px(0.0))
                .items_start()
                .pt_6()
                .gap_8()
                .child(section_nav(section, p, cx))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w(px(0.0))
                        .child(match section {
                            SettingsSection::Appearance => {
                                appearance_card(view, p, cx).into_any_element()
                            }
                            SettingsSection::Data => data_card(view, p, cx).into_any_element(),
                        })
                        .child(footer(view, p)),
                ),
        )
}

fn section_nav(
    section: SettingsSection,
    p: Palette,
    cx: &mut Context<LLMeterView>,
) -> impl IntoElement {
    let mut nav = v_flex().w(px(200.0)).flex_shrink_0().gap_1();
    for item in SettingsSection::ALL {
        let active = item == section;
        let variant = if active {
            ButtonCustomVariant::new(cx)
                .color(p.accent)
                .foreground(p.foreground)
                .hover(p.accent)
                .active(p.accent)
        } else {
            ButtonCustomVariant::new(cx)
                .color(p.transparent)
                .foreground(p.foreground)
                .hover(p.accent)
                .active(p.accent)
        };
        nav = nav.child(
            Button::new(("settings-section", item as usize))
                .custom(variant)
                .selected(active)
                .icon(item.icon())
                .w_full()
                .justify_start()
                .child(div().flex_1().min_w(px(0.0)).child(item.label()))
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.set_settings_section(item, cx);
                })),
        );
    }
    nav.child(
        v_flex()
            .gap_1()
            .pt_4()
            .text_xs()
            .text_color(p.muted_foreground)
            .child(t!("settings.note_1").to_string())
            .child(t!("settings.note_2").to_string()),
    )
}

fn card(p: Palette) -> gpui::Div {
    v_flex()
        .w_full()
        .rounded_xl()
        .border_1()
        .border_color(p.border)
        .bg(p.tiles)
        .px_6()
        .pb_3()
        .pt_5()
}

fn section_header(title: String, p: Palette) -> gpui::Div {
    div()
        .pb_2()
        .text_base()
        .text_color(p.muted_foreground)
        .child(title)
}

fn setting_row(
    title: String,
    subtitle: impl IntoElement,
    control: Option<AnyElement>,
    first: bool,
    p: Palette,
) -> gpui::Div {
    let left = v_flex()
        .flex_1()
        .min_w(px(0.0))
        .gap_1()
        .pr_6()
        .child(
            div()
                .text_base()
                .font_weight(FontWeight::MEDIUM)
                .text_color(p.foreground)
                .child(title),
        )
        .child(
            div()
                .text_sm()
                .text_color(p.muted_foreground)
                .child(subtitle),
        );
    let mut row = h_flex()
        .w_full()
        .items_center()
        .justify_between()
        .py_5()
        .when(!first, |this| this.border_t_1().border_color(p.border))
        .child(left);
    if let Some(control) = control {
        row = row.child(control);
    }
    row
}

fn appearance_card(
    view: &LLMeterView,
    p: Palette,
    cx: &mut Context<LLMeterView>,
) -> impl IntoElement {
    card(p)
        .child(section_header(t!("settings.appearance").to_string(), p))
        .child(setting_row(
            t!("settings.theme").to_string(),
            t!("settings.theme_subtitle").to_string(),
            Some(theme_control(view, cx).into_any_element()),
            true,
            p,
        ))
        .child(setting_row(
            t!("settings.language").to_string(),
            t!("settings.language_subtitle").to_string(),
            Some(locale_control(view, cx).into_any_element()),
            false,
            p,
        ))
}

fn locale_control(view: &LLMeterView, cx: &mut Context<LLMeterView>) -> impl IntoElement {
    let selected = view.locale_pref;
    let mut group = ButtonGroup::new("locale-preference").compact();
    for (index, preference) in LocalePreference::ALL.into_iter().enumerate() {
        group = group.child(
            Button::new(("locale-preference", index))
                .label(preference.label())
                .selected(selected == preference),
        );
    }
    group.on_click(cx.listener(|view, clicks: &Vec<usize>, _, cx| {
        if let Some(&index) = clicks.first()
            && let Some(preference) = LocalePreference::ALL.get(index).copied()
        {
            view.set_locale_preference(preference, cx);
        }
    }))
}

fn theme_control(view: &LLMeterView, cx: &mut Context<LLMeterView>) -> impl IntoElement {
    let selected = view.theme_pref;
    let icons = [IconName::Sun, IconName::Moon, IconName::WindowRestore];
    let mut group = ButtonGroup::new("theme-preference").compact();
    for (index, preference) in ThemePreference::ALL.into_iter().enumerate() {
        group = group.child(
            Button::new(("theme-preference", index))
                .icon(icons[index].clone())
                .label(preference.label())
                .selected(selected == preference),
        );
    }
    group.on_click(cx.listener(|view, clicks: &Vec<usize>, window, cx| {
        if let Some(&index) = clicks.first()
            && let Some(preference) = ThemePreference::ALL.get(index).copied()
        {
            view.set_theme_preference(preference, window, cx);
        }
    }))
}

fn data_card(view: &LLMeterView, p: Palette, cx: &mut Context<LLMeterView>) -> impl IntoElement {
    let snapshot = &view.snapshot;

    let sync_subtitle = match snapshot.last_sync {
        Some(timestamp) => t!(
            "settings.last_sync",
            time = timestamp.with_timezone(&Local).format("%H:%M:%S")
        )
        .to_string(),
        None => t!("settings.waiting_sync").to_string(),
    };
    let sync_subtitle = if snapshot.warnings.is_empty() {
        sync_subtitle
    } else {
        format!(
            "{sync_subtitle} · {}",
            t!("settings.warnings_count", count = snapshot.warnings.len())
        )
    };

    let detections = if snapshot.detections.is_empty() {
        t!("providers.detection_unavailable").to_string()
    } else {
        snapshot
            .detections
            .iter()
            .map(|detection| {
                let status = match detection.status {
                    ProviderStatus::DataFound => t!("providers.data_found"),
                    ProviderStatus::Installed => t!("providers.installed"),
                    ProviderStatus::NotInstalled => t!("providers.not_detected"),
                    ProviderStatus::UnsupportedVersion => t!("providers.unsupported"),
                };
                format!("{} {status}", detection.provider)
            })
            .collect::<Vec<_>>()
            .join(" · ")
    };

    let refresh = Button::new("refresh-data")
        .ghost()
        .icon(IconName::Redo)
        .label(t!("settings.refresh").to_string())
        .on_click(cx.listener(|view, _, _, cx| view.refresh_sessions(cx)));

    card(p)
        .child(section_header(t!("settings.data").to_string(), p))
        .child(setting_row(
            t!("settings.database").to_string(),
            snapshot.database_path.display().to_string(),
            None,
            true,
            p,
        ))
        .child(setting_row(
            t!("settings.sync").to_string(),
            sync_subtitle,
            Some(refresh.into_any_element()),
            false,
            p,
        ))
        .child(setting_row(
            t!("settings.detection").to_string(),
            detections,
            None,
            false,
            p,
        ))
        .child(setting_row(
            t!("settings.trae_cn_usage").to_string(),
            t!("settings.trae_cn_usage_subtitle").to_string(),
            Some(
                Switch::new("trae-cn-usage")
                    .checked(view.trae_cn_usage_enabled)
                    .on_click(cx.listener(|view, enabled, _, cx| {
                        view.set_trae_cn_usage_enabled(*enabled, cx);
                    }))
                    .into_any_element(),
            ),
            false,
            p,
        ))
        .child(setting_row(
            t!("settings.hooks").to_string(),
            v_flex()
                .gap_1()
                .child(t!("settings.hooks_subtitle").to_string())
                .child(hook_line(
                    t!("settings.install").to_string(),
                    "llmeter hook install --provider codex",
                ))
                .child(hook_line(
                    t!("settings.status").to_string(),
                    "llmeter hook status --provider codex",
                ))
                .child(hook_line(
                    t!("settings.remove").to_string(),
                    "llmeter hook uninstall --provider codex",
                )),
            None,
            false,
            p,
        ))
        .child(setting_row(
            t!("settings.privacy").to_string(),
            t!("settings.privacy_subtitle").to_string(),
            Some(
                Icon::new(IconName::Info)
                    .text_color(p.muted_foreground)
                    .into_any_element(),
            ),
            false,
            p,
        ))
}

fn hook_line(label: String, command: &str) -> gpui::Div {
    div().text_xs().child(format!("{label}: {command}"))
}

fn footer(view: &LLMeterView, p: Palette) -> impl IntoElement {
    h_flex()
        .w_full()
        .justify_center()
        .gap_2()
        .pt_6()
        .text_sm()
        .text_color(p.muted_foreground)
        .child(format!("LLMeter v{}", env!("CARGO_PKG_VERSION")))
        .child("·")
        .child(t!("settings.footer_local").to_string())
        .child("·")
        .child(
            t!(
                "settings.footer_warnings",
                count = view.snapshot.warnings.len()
            )
            .to_string(),
        )
}
