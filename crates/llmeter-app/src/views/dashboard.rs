use chrono::Local;
use gpui::{Context, FontWeight, IntoElement, Render, Rgba, div, prelude::*, px, relative, rgb};
use gpui_component::{
    ActiveTheme, Selectable,
    button::{Button, ButtonCustomVariant, ButtonVariants},
};
use llmeter_core::{Provider, ProviderDetection, ProviderStatus};
use llmeter_storage::{ModelUsage, ProjectUsage, ProviderUsage, RecentActivity};
use rust_i18n::t;

use crate::{
    app::LLMeterView,
    state::UiSnapshot,
    views::{palette::Palette, sessions::sessions_page, settings::settings_page},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum DashboardPage {
    #[default]
    Overview,
    Sessions,
    Providers,
    Projects,
    Settings,
}

impl DashboardPage {
    const ALL: [Self; 5] = [
        Self::Overview,
        Self::Sessions,
        Self::Providers,
        Self::Projects,
        Self::Settings,
    ];

    fn label(self) -> String {
        match self {
            Self::Overview => t!("nav.overview").to_string(),
            Self::Sessions => t!("nav.sessions").to_string(),
            Self::Providers => t!("nav.providers").to_string(),
            Self::Projects => t!("nav.projects").to_string(),
            Self::Settings => t!("nav.settings").to_string(),
        }
    }
}

pub fn dashboard(view: &LLMeterView, cx: &mut Context<LLMeterView>) -> impl IntoElement {
    let snapshot = &view.snapshot;
    let active_page = view.active_page;
    let p = Palette::from_app(cx);

    let mut provider_rows = div().flex().flex_col().gap_2();
    for provider in &snapshot.providers {
        provider_rows = provider_rows.child(provider_row(provider, p));
    }
    if snapshot.providers.is_empty() {
        provider_rows = provider_rows.child(empty_state(t!("overview.no_usage").to_string(), p));
    }

    let mut model_rows = div().flex().flex_col().gap_2();
    for model in snapshot.models.iter().take(6) {
        model_rows = model_rows.child(model_row(model, p));
    }
    if snapshot.models.is_empty() {
        model_rows = model_rows.child(empty_state(t!("overview.models_pending").to_string(), p));
    }

    let mut activity_rows = div().flex().flex_col();
    for activity in snapshot.recent.iter().take(6) {
        activity_rows = activity_rows.child(activity_row(activity, p));
    }
    if snapshot.recent.is_empty() {
        activity_rows =
            activity_rows.child(empty_state(t!("overview.waiting_logs").to_string(), p));
    }

    let mut project_rows = div().flex().flex_col();
    for project in snapshot.projects.iter().take(6) {
        project_rows = project_rows.child(project_row(project, p));
    }
    if snapshot.projects.is_empty() {
        project_rows =
            project_rows.child(empty_state(t!("overview.projects_pending").to_string(), p));
    }

    let content = match active_page {
        DashboardPage::Overview => div()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .gap_4()
                    .p_4()
                    .child(summary_card(
                        t!("overview.today").to_string(),
                        &snapshot.today,
                        rgb(0x2563eb),
                        p,
                    ))
                    .child(summary_card(
                        t!("overview.days_7").to_string(),
                        &snapshot.seven_days,
                        rgb(0x7c3aed),
                        p,
                    ))
                    .child(summary_card(
                        t!("overview.days_30").to_string(),
                        &snapshot.thirty_days,
                        rgb(0x0891b2),
                        p,
                    ))
                    .child(summary_card(
                        t!("overview.all_time").to_string(),
                        &snapshot.all_time,
                        rgb(0x475569),
                        p,
                    )),
            )
            .child(
                div()
                    .flex()
                    .gap_4()
                    .px_4()
                    .child(panel(
                        t!("overview.provider_usage").to_string(),
                        provider_rows,
                        p,
                    ))
                    .child(panel(
                        t!("overview.recent_activity").to_string(),
                        activity_rows,
                        p,
                    )),
            )
            .child(
                div()
                    .flex()
                    .gap_4()
                    .px_4()
                    .pt_4()
                    .child(panel(
                        t!("overview.token_trend").to_string(),
                        trend(&snapshot.daily, p),
                        p,
                    ))
                    .child(panel(t!("overview.model_usage").to_string(), model_rows, p)),
            )
            .child(
                div()
                    .flex()
                    .gap_4()
                    .px_4()
                    .pt_4()
                    .child(panel(t!("overview.projects").to_string(), project_rows, p))
                    .child(panel(
                        t!("overview.local_setup").to_string(),
                        local_setup_panel(snapshot, p),
                        p,
                    )),
            ),
        DashboardPage::Sessions => div().size_full().child(sessions_page(view, cx)),
        DashboardPage::Providers => div()
            .flex()
            .flex_col()
            .gap_4()
            .p_4()
            .child(page_heading(
                t!("providers.title").to_string(),
                t!("providers.subtitle").to_string(),
                p,
            ))
            .child(
                div()
                    .flex()
                    .gap_4()
                    .child(panel(
                        t!("providers.usage_30").to_string(),
                        provider_rows,
                        p,
                    ))
                    .child(panel(t!("providers.model_30").to_string(), model_rows, p)),
            )
            .child(panel(
                t!("providers.status").to_string(),
                provider_statuses(snapshot, p),
                p,
            )),
        DashboardPage::Projects => div()
            .flex()
            .flex_col()
            .gap_4()
            .p_4()
            .child(page_heading(
                t!("projects.title").to_string(),
                t!("projects.subtitle").to_string(),
                p,
            ))
            .child(panel(t!("projects.usage_30").to_string(), project_rows, p)),
        DashboardPage::Settings => div().size_full().child(settings_page(view, cx)),
    };

    let main = div()
        .flex()
        .flex_1()
        .min_w(px(0.0))
        .flex_col()
        .bg(p.background)
        .text_color(p.foreground)
        .child(header(&snapshot.today, p))
        .child(
            div()
                .id("main-scroll")
                .flex_1()
                .overflow_y_scroll()
                .child(content),
        );

    div()
        .flex()
        .size_full()
        .child(sidebar(active_page, cx))
        .child(main)
}

fn sidebar(active_page: DashboardPage, cx: &mut Context<LLMeterView>) -> impl IntoElement {
    let sidebar_background = cx.theme().sidebar;
    let sidebar_foreground = cx.theme().sidebar_foreground;
    let sidebar_muted = sidebar_foreground.opacity(0.65);

    let mut navigation = div().flex().flex_col().gap_1().pt_5();
    for page in DashboardPage::ALL {
        let item = page.label();
        let is_active = page == active_page;
        let variant = if is_active {
            ButtonCustomVariant::new(cx)
                .color(cx.theme().sidebar_primary)
                .foreground(cx.theme().sidebar_primary_foreground)
                .hover(cx.theme().sidebar_primary)
                .active(cx.theme().sidebar_primary)
        } else {
            ButtonCustomVariant::new(cx)
                .color(sidebar_background)
                .foreground(sidebar_foreground)
                .hover(cx.theme().sidebar_accent)
                .active(cx.theme().sidebar_accent)
        };
        navigation = navigation.child(
            Button::new(item.clone())
                .custom(variant)
                .selected(is_active)
                .w_full()
                .justify_start()
                .label(item)
                .on_click(cx.listener(move |view, _, _, cx| view.navigate(page, cx))),
        );
    }
    div()
        .flex()
        .flex_col()
        .min_w(px(176.0))
        .p_4()
        .bg(sidebar_background)
        .text_color(sidebar_foreground)
        .child(div().text_lg().child(t!("app.name").to_string()))
        .child(
            div()
                .pt_1()
                .text_xs()
                .text_color(sidebar_muted)
                .child(t!("app.tagline").to_string()),
        )
        .child(navigation)
        .child(
            div()
                .pt_6()
                .text_xs()
                .text_color(sidebar_muted)
                .child(t!("app.no_cloud").to_string()),
        )
}

fn page_heading(title: String, subtitle: String, p: Palette) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_2xl().child(title))
        .child(
            div()
                .text_sm()
                .text_color(p.muted_foreground)
                .child(subtitle),
        )
}

fn provider_statuses(snapshot: &UiSnapshot, p: Palette) -> impl IntoElement {
    let mut statuses = div().flex().flex_col().gap_2();
    for detection in &snapshot.detections {
        statuses = statuses.child(provider_status_row(detection));
    }
    if snapshot.detections.is_empty() {
        statuses = statuses.child(empty_state(
            t!("providers.detection_unavailable").to_string(),
            p,
        ));
    }
    statuses
}

fn local_setup_panel(snapshot: &UiSnapshot, p: Palette) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .text_sm()
        .child(t!("providers.status").to_string())
        .child(provider_statuses(snapshot, p))
        .child(
            t!(
                "local.database",
                path = snapshot.database_path.display().to_string()
            )
            .to_string(),
        )
        .child(
            t!(
                "local.last_sync",
                time = snapshot
                    .last_sync
                    .map(|value| value.with_timezone(&Local).format("%H:%M:%S").to_string())
                    .unwrap_or_else(|| t!("local.pending").to_string())
            )
            .to_string(),
        )
        .child(t!("local.warnings", count = snapshot.warnings.len()).to_string())
        .child(t!("local.incremental").to_string())
        .child(t!("local.privacy").to_string())
        .child(
            div()
                .pt_2()
                .text_xs()
                .text_color(p.muted_foreground)
                .child(t!("local.unknown_pricing").to_string()),
        )
}

fn provider_status_row(detection: &ProviderDetection) -> impl IntoElement {
    let (label, color) = match detection.status {
        ProviderStatus::DataFound => (t!("providers.data_found").to_string(), rgb(0x16a34a)),
        ProviderStatus::Installed => (t!("providers.installed").to_string(), rgb(0x2563eb)),
        ProviderStatus::NotInstalled => (t!("providers.not_detected").to_string(), rgb(0x94a3b8)),
        ProviderStatus::UnsupportedVersion => {
            (t!("providers.unsupported").to_string(), rgb(0xd97706))
        }
    };
    div()
        .flex()
        .justify_between()
        .text_xs()
        .child(detection.provider.to_string())
        .child(div().text_color(color).child(label))
}

fn header(today: &llmeter_storage::Overview, p: Palette) -> impl IntoElement {
    div()
        .flex()
        .justify_between()
        .items_center()
        .px_5()
        .py_4()
        .bg(p.background)
        .border_b_1()
        .border_color(p.border)
        .child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(div().size_3().rounded_full().bg(rgb(0x22c55e)))
                .child(div().text_xl().child(t!("app.name").to_string())),
        )
        .child(
            div()
                .flex()
                .gap_5()
                .text_sm()
                .child(format!(
                    "{}  {}",
                    t!("header.today"),
                    format_tokens(today.total_tokens)
                ))
                .child(format_cost(today.estimated_cost_usd)),
        )
}

fn summary_card(
    title: String,
    overview: &llmeter_storage::Overview,
    color: Rgba,
    p: Palette,
) -> impl IntoElement {
    div()
        .flex_1()
        .min_h(px(122.0))
        .p_5()
        .rounded_xl()
        .bg(p.tiles)
        .border_1()
        .border_color(p.border)
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .text_color(p.muted_foreground)
                .child(title),
        )
        .child(
            div()
                .pt_3()
                .text_2xl()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(color)
                .child(format_tokens(overview.total_tokens)),
        )
        .child(
            div()
                .pt_2()
                .text_xs()
                .text_color(p.muted_foreground)
                .child(format!(
                    "{} events · {}",
                    overview.event_count,
                    format_cost(overview.estimated_cost_usd)
                )),
        )
}

fn panel(title: String, content: impl IntoElement, p: Palette) -> impl IntoElement {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .min_w(px(0.0))
        .min_h(px(220.0))
        .p_5()
        .rounded_xl()
        .bg(p.tiles)
        .border_1()
        .border_color(p.border)
        .child(
            div()
                .text_base()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.foreground)
                .child(title),
        )
        .child(
            div()
                .pt_3()
                .flex_1()
                .min_h(px(0.0))
                .flex()
                .flex_col()
                .child(content),
        )
}

fn provider_row(provider: &ProviderUsage, p: Palette) -> impl IntoElement {
    let color = provider_color(provider.provider);
    div()
        .flex()
        .flex_col()
        .gap_3()
        .py_3()
        .border_b_1()
        .border_color(p.border)
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_4()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().size_2().rounded_full().bg(color))
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(p.foreground)
                                .child(provider.provider.to_string()),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(format_tokens(provider.total_tokens)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(p.muted_foreground)
                                .child(format_cost(provider.estimated_cost_usd)),
                        ),
                ),
        )
        .child(
            div()
                .flex()
                .gap_2()
                .child(metric_chip(
                    t!("overview.input").to_string(),
                    provider.input_tokens,
                    p,
                ))
                .child(metric_chip(
                    t!("overview.output").to_string(),
                    provider.output_tokens,
                    p,
                ))
                .child(metric_chip(
                    t!("overview.cached").to_string(),
                    provider.cached_input_tokens,
                    p,
                )),
        )
}

fn activity_row(activity: &RecentActivity, p: Palette) -> impl IntoElement {
    let model = activity
        .model
        .clone()
        .unwrap_or_else(|| t!("overview.unknown_model").to_string());
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_4()
        .py_3()
        .border_b_1()
        .border_color(p.border)
        .child(
            div()
                .flex()
                .min_w(px(0.0))
                .items_center()
                .gap_3()
                .child(
                    div()
                        .size_2()
                        .flex_shrink_0()
                        .rounded_full()
                        .bg(provider_color(activity.provider)),
                )
                .child(
                    div()
                        .flex()
                        .min_w(px(0.0))
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .truncate()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(p.foreground)
                                .child(model),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(p.muted_foreground)
                                .child(format!(
                                    "{} · {}",
                                    activity.provider,
                                    activity.timestamp.with_timezone(&Local).format("%H:%M")
                                )),
                        ),
                ),
        )
        .child(token_pill(activity.total_tokens, p))
}

fn project_row(project: &ProjectUsage, p: Palette) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_4()
        .py_3()
        .border_b_1()
        .border_color(p.border)
        .child(
            div()
                .min_w(px(0.0))
                .truncate()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .child(project.project_name.clone()),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_xs()
                .text_color(p.muted_foreground)
                .child(format!(
                    "{} · {}",
                    format_tokens(project.total_tokens),
                    format_cost(project.estimated_cost_usd)
                )),
        )
}

fn model_row(model: &ModelUsage, p: Palette) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_4()
        .py_3()
        .border_b_1()
        .border_color(p.border)
        .child(
            div()
                .flex()
                .min_w(px(0.0))
                .items_center()
                .gap_3()
                .child(
                    div()
                        .size_2()
                        .flex_shrink_0()
                        .rounded_full()
                        .bg(provider_color(model.provider)),
                )
                .child(
                    div()
                        .flex()
                        .min_w(px(0.0))
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .truncate()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(p.foreground)
                                .child(model.model.clone()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(p.muted_foreground)
                                .child(model.provider.to_string()),
                        ),
                ),
        )
        .child(
            div()
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap_2()
                .child(token_pill(model.total_tokens, p))
                .child(pricing_pill(model.estimated_cost_usd, p)),
        )
}

fn metric_chip(label: String, value: u64, p: Palette) -> impl IntoElement {
    div()
        .flex_1()
        .min_w(px(0.0))
        .rounded_md()
        .bg(p.muted)
        .px_3()
        .py_2()
        .child(div().text_xs().text_color(p.muted_foreground).child(label))
        .child(
            div()
                .pt_1()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .text_color(p.foreground)
                .child(format_tokens(value)),
        )
}

fn token_pill(value: u64, p: Palette) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .rounded_full()
        .bg(rgb(0x2563eb).opacity(0.12))
        .px_3()
        .py_1()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(p.link)
        .child(format_tokens(value))
}

fn pricing_pill(value: Option<f64>, p: Palette) -> impl IntoElement {
    let (label, background, foreground): (String, gpui::Hsla, gpui::Hsla) = match value {
        Some(value) => (
            format!("$ {:.2}", value),
            rgb(0x22c55e).opacity(0.12).into(),
            p.success,
        ),
        None => (
            t!("overview.unpriced").to_string(),
            p.muted,
            p.muted_foreground,
        ),
    };
    div()
        .flex_shrink_0()
        .rounded_full()
        .bg(background)
        .px_3()
        .py_1()
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .text_color(foreground)
        .child(label)
}

fn provider_color(provider: Provider) -> Rgba {
    match provider {
        Provider::Codex => rgb(0x2563eb),
        Provider::Claude => rgb(0xd97706),
        Provider::OpenCode => rgb(0x0891b2),
        Provider::Pi => rgb(0x7c3aed),
    }
}

fn trend(daily: &[llmeter_storage::DailyUsage], p: Palette) -> gpui::AnyElement {
    if daily.is_empty() {
        return div()
            .size_full()
            .child(empty_state(t!("overview.no_daily").to_string(), p))
            .into_any_element();
    }
    let max = daily.iter().map(|day| day.total_tokens).max().unwrap_or(1);
    let mut chart = div()
        .flex()
        .items_end()
        .gap_1()
        .flex_1()
        .min_h(px(96.0))
        .w_full();
    for (index, day) in daily.iter().rev().take(30).rev().enumerate() {
        let ratio = (day.total_tokens as f32 / max as f32).max(if day.total_tokens > 0 {
            0.04
        } else {
            0.01
        });
        let tooltip_text = trend_tooltip_text(day);
        chart = chart.child(
            div()
                .id(("trend-bar", index))
                .flex_1()
                .min_w(px(4.0))
                .h(relative(ratio))
                .min_h(px(2.0))
                .rounded_sm()
                .bg(rgb(0x60a5fa))
                .hover(|style| style.bg(rgb(0x93c5fd)))
                .tooltip(move |_, cx| cx.new(|_| TrendTooltip(tooltip_text.clone())).into()),
        );
    }
    let first_day = daily.first().expect("daily is not empty");
    let last_day = daily.last().expect("daily is not empty");
    div()
        .size_full()
        .flex()
        .flex_col()
        .gap_2()
        .child(chart)
        .child(
            div()
                .flex()
                .justify_between()
                .text_xs()
                .text_color(p.muted_foreground)
                .child(first_day.day.clone())
                .child(t!("overview.trend_peak", value = format_tokens(max)).to_string())
                .child(last_day.day.clone()),
        )
        .into_any_element()
}

fn trend_tooltip_text(day: &llmeter_storage::DailyUsage) -> String {
    let mut text = format!("{} · {} tokens", day.day, format_tokens(day.total_tokens));
    if let Some(cost) = day.estimated_cost_usd {
        text.push_str(&format!(" · ${cost:.2}"));
    }
    text
}

struct TrendTooltip(String);

impl Render for TrendTooltip {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .px_2()
            .py_1()
            .text_xs()
            .child(self.0.clone())
    }
}

fn empty_state(text: String, p: Palette) -> impl IntoElement {
    div().text_sm().text_color(p.muted_foreground).child(text)
}

fn format_tokens(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.2}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn format_cost(value: Option<f64>) -> String {
    value
        .map(|value| format!("$ {:.2}", value))
        .unwrap_or_else(|| t!("overview.unpriced").to_string())
}
