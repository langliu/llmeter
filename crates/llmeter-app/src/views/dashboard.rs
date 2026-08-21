use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use chrono::{Datelike, Duration, Local, NaiveDate};
use gpui::{
    Bounds, Context, FontWeight, Hsla, IntoElement, MouseMoveEvent, Render, Rgba, canvas, div,
    fill, point, prelude::*, px, relative, rgb, size, svg,
};
use gpui_component::{
    ActiveTheme, IconName, Selectable, Sizable, TITLE_BAR_HEIGHT,
    button::{Button, ButtonCustomVariant, ButtonGroup, ButtonVariants},
    date_picker::DatePicker,
};
use llmeter_core::{Provider, ProviderDetection, ProviderStatus};
use llmeter_storage::{DailyModelUsage, ModelUsage, ProjectUsage, ProviderUsage, RecentActivity};
use rust_i18n::t;

use crate::{
    app::{LLMeterView, OverviewPeriod},
    state::UiSnapshot,
    views::{
        limits::limits_page, palette::Palette, provider_brand::provider_logo,
        sessions::sessions_page, settings::settings_page,
    },
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum DashboardPage {
    #[default]
    Overview,
    Sessions,
    Limits,
    Providers,
    Projects,
    Settings,
}

impl DashboardPage {
    const ALL: [Self; 6] = [
        Self::Overview,
        Self::Sessions,
        Self::Limits,
        Self::Providers,
        Self::Projects,
        Self::Settings,
    ];

    fn label(self) -> String {
        match self {
            Self::Overview => t!("nav.overview").to_string(),
            Self::Sessions => t!("nav.sessions").to_string(),
            Self::Limits => t!("nav.limits").to_string(),
            Self::Providers => t!("nav.providers").to_string(),
            Self::Projects => t!("nav.projects").to_string(),
            Self::Settings => t!("nav.settings").to_string(),
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::Overview => IconName::LayoutDashboard,
            Self::Sessions => IconName::Inbox,
            Self::Limits => IconName::ChartPie,
            Self::Providers => IconName::Bot,
            Self::Projects => IconName::Folder,
            Self::Settings => IconName::Settings,
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
        DashboardPage::Overview => overview_page(view, snapshot, activity_rows, p, cx),
        DashboardPage::Sessions => div()
            .size_full()
            .child(sessions_page(view, cx))
            .into_any_element(),
        DashboardPage::Limits => div()
            .w_full()
            .child(limits_page(view, cx))
            .into_any_element(),
        DashboardPage::Providers => div()
            .flex()
            .flex_col()
            .gap_4()
            .px_4()
            .py_1()
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
            ))
            .into_any_element(),
        DashboardPage::Projects => div()
            .flex()
            .flex_col()
            .gap_4()
            .px_4()
            .py_1()
            .child(page_heading(
                t!("projects.title").to_string(),
                t!("projects.subtitle").to_string(),
                p,
            ))
            .child(panel(t!("projects.usage_30").to_string(), project_rows, p))
            .into_any_element(),
        DashboardPage::Settings => div()
            .size_full()
            .child(settings_page(view, cx))
            .into_any_element(),
    };

    let main = div().flex().flex_1().min_w(px(0.0)).p_3().child(
        div()
            .flex()
            .flex_col()
            .size_full()
            // GPUI's overflow mask is rectangular, so keep the scroll viewport
            // inside the rounded corners instead of relying on rounded clipping.
            .py_3()
            .rounded_3xl()
            .bg(p.background.opacity(0.96))
            .text_color(p.foreground)
            .overflow_hidden()
            .child(
                div()
                    .id("main-scroll")
                    .w_full()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .on_scroll_wheel(cx.listener(|view, _, _, cx| {
                        view.heatmap
                            .update(cx, |heatmap, cx| heatmap.clear_hover(cx));
                    }))
                    .child(content),
            ),
    );

    div()
        .flex()
        .size_full()
        .bg(p.background.opacity(0.55))
        .child(sidebar(active_page, cx))
        .child(main)
}

fn overview_page(
    view: &LLMeterView,
    snapshot: &UiSnapshot,
    activity_rows: impl IntoElement,
    p: Palette,
    cx: &mut Context<LLMeterView>,
) -> gpui::AnyElement {
    let range = &snapshot.overview_range;
    let model_total = snapshot
        .overview_range
        .models
        .iter()
        .map(|model| model.total_tokens)
        .sum::<u64>()
        .max(1);
    let mut ranking = div().flex().flex_col();
    for (index, model) in range.models.iter().take(3).enumerate() {
        let percent = model.total_tokens as f64 / model_total as f64 * 100.0;
        ranking = ranking.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .py_2()
                .child(
                    div()
                        .flex()
                        .min_w(px(0.0))
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .size_5()
                                .rounded_full()
                                .bg(p.muted)
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_xs()
                                .text_color(p.muted_foreground)
                                .child((index + 1).to_string()),
                        )
                        .child(
                            div()
                                .min_w(px(0.0))
                                .truncate()
                                .text_sm()
                                .child(model.model.clone()),
                        ),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_sm()
                        .text_color(p.muted_foreground)
                        .child(format!("{percent:.1}%")),
                ),
        );
    }
    if range.models.is_empty() {
        ranking = ranking.child(empty_state(t!("overview.models_pending").to_string(), p));
    }

    let started = snapshot
        .sessions
        .iter()
        .map(|session| session.started_at)
        .min()
        .map(|value| value.with_timezone(&Local).format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "-".into());
    let active_days = snapshot
        .heatmap_daily
        .iter()
        .filter(|day| day.total_tokens > 0)
        .count();
    let stats = glass_card(p)
        .child(
            div()
                .flex()
                .gap_2()
                .child(stat_chip(
                    format_tokens(snapshot.today.total_tokens),
                    t!("overview.today").to_string(),
                    p,
                ))
                .child(stat_chip(
                    format_tokens(snapshot.seven_days.total_tokens),
                    t!("overview.days_7").to_string(),
                    p,
                ))
                .child(stat_chip(
                    format_tokens(snapshot.thirty_days.total_tokens),
                    t!("overview.days_30").to_string(),
                    p,
                ))
                .child(stat_chip(
                    format_tokens(snapshot.sessions.len() as u64),
                    t!("overview.conversations").to_string(),
                    p,
                )),
        )
        .child(
            div()
                .mt_3()
                .border_t_1()
                .border_color(p.border)
                .pt_2()
                .child(ranking),
        )
        .child(
            div()
                .mt_3()
                .pt_3()
                .border_t_1()
                .border_color(p.border)
                .flex()
                .justify_between()
                .text_xs()
                .text_color(p.muted_foreground)
                .child(format!("{}  {}", t!("overview.started"), started))
                .child(t!("overview.active_days", days = active_days).to_string()),
        );

    let provider_total = snapshot
        .overview_range
        .providers
        .iter()
        .map(|provider| provider.total_tokens)
        .sum::<u64>()
        .max(1);
    let mut model_counts = HashMap::<Provider, usize>::new();
    for model in &range.models {
        *model_counts.entry(model.provider).or_default() += 1;
    }
    let mut share_bar = div()
        .flex()
        .h(px(8.0))
        .w_full()
        .overflow_hidden()
        .rounded_full()
        .bg(p.muted);
    for provider in &range.providers {
        let ratio = provider.total_tokens as f32 / provider_total as f32;
        if ratio <= 0.0 {
            continue;
        }
        share_bar = share_bar.child(
            div()
                .h_full()
                .w(relative(ratio))
                .bg(provider_color(provider.provider)),
        );
    }
    let all_models = aggregate_model_usage(&range.models);
    let mut provider_cards = div().flex().flex_wrap().gap_3();
    provider_cards = provider_cards.child(share_card(
        t!("overview.share_all").to_string(),
        100.0,
        all_models.len(),
        None,
        p,
        view.overview_provider.is_none(),
        cx,
    ));
    for provider in &range.providers {
        let percent = provider.total_tokens as f64 / provider_total as f64 * 100.0;
        provider_cards = provider_cards.child(share_card(
            provider.provider.to_string(),
            percent,
            model_counts.get(&provider.provider).copied().unwrap_or(0),
            Some(provider.provider),
            p,
            view.overview_provider == Some(provider.provider),
            cx,
        ));
    }

    let provider_detail = match view.overview_provider {
        None => Some(all_model_detail(
            &all_models,
            range.overview.total_tokens,
            p,
        )),
        Some(selected) => range
            .providers
            .iter()
            .find(|usage| usage.provider == selected)
            .map(|usage| provider_model_detail(usage, &range.models, p)),
    };

    let hero = glass_card(p)
        .child(overview_period_filter(view, cx))
        .flex_1()
        .child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .items_center()
                .pt_4()
                .child(
                    div()
                        .text_xs()
                        .text_color(p.muted_foreground)
                        .child(t!("overview.token_total").to_string()),
                )
                .child(
                    div()
                        .pt_3()
                        .text_3xl()
                        .font_weight(FontWeight::BOLD)
                        .child(format_full_number(range.overview.total_tokens)),
                )
                .child(
                    div()
                        .pt_2()
                        .text_base()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(p.success)
                        .child(format_cost(range.overview.estimated_cost_usd)),
                ),
        )
        .child(div().pt_6().child(share_bar))
        .child(div().pt_5().child(provider_cards))
        .when_some(provider_detail, |this, detail| this.child(detail));

    div()
        .flex()
        .items_start()
        .gap_3()
        .px_3()
        .child(
            div()
                .w(px(380.0))
                .flex_shrink_0()
                .flex()
                .flex_col()
                .gap_3()
                .child(stats)
                .child(heatmap(
                    view,
                    &snapshot.heatmap_daily,
                    &snapshot.heatmap_models,
                    p,
                    cx,
                ))
                .child(panel(
                    t!(
                        "overview.token_trend_period",
                        period = view.overview_period.label()
                    )
                    .to_string(),
                    trend(&range.daily, p),
                    p,
                )),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .gap_3()
                .child(hero)
                .child(panel(
                    t!("overview.recent_activity").to_string(),
                    activity_rows,
                    p,
                )),
        )
        .into_any_element()
}

fn overview_period_filter(view: &LLMeterView, cx: &mut Context<LLMeterView>) -> impl IntoElement {
    let mut group = ButtonGroup::new("overview-period-filter")
        .compact()
        .outline();
    for (index, period) in OverviewPeriod::ALL.into_iter().enumerate() {
        group = group.child(
            Button::new(("overview-period", index))
                .label(period.label())
                .selected(view.overview_period == period),
        );
    }

    div()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_2()
        .pb_3()
        .child(
            group.on_click(cx.listener(|view, clicks: &Vec<usize>, _, cx| {
                if let Some(&index) = clicks.first()
                    && let Some(period) = OverviewPeriod::ALL.get(index).copied()
                {
                    view.set_overview_period(period, cx);
                }
            })),
        )
        .when(view.overview_period == OverviewPeriod::Custom, |this| {
            this.child(
                div().w(px(224.0)).child(
                    DatePicker::new(&view.overview_date_range)
                        .small()
                        .number_of_months(1)
                        .placeholder(t!("overview.custom_range_placeholder").to_string()),
                ),
            )
        })
}

fn glass_card(p: Palette) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .p_4()
        .rounded_2xl()
        .bg(p.tiles.opacity(0.98))
        .border_1()
        .border_color(p.border.opacity(0.7))
}

fn stat_chip(value: String, label: String, p: Palette) -> impl IntoElement {
    let (background, border): (gpui::Hsla, gpui::Hsla) = if p.is_dark {
        (p.muted.opacity(0.65), p.border.opacity(0.35))
    } else {
        (rgb(0xf1f5f9).into(), rgb(0xe2e8f0).into())
    };
    div()
        .flex_1()
        .min_w(px(0.0))
        .rounded_xl()
        .bg(background)
        .border_1()
        .border_color(border)
        .px_2()
        .py_3()
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .whitespace_nowrap()
                .child(value),
        )
        .child(
            div()
                .pt_1()
                .text_xs()
                .text_color(p.muted_foreground)
                .child(label),
        )
}

fn share_card(
    title: String,
    percent: f64,
    models: usize,
    provider: Option<Provider>,
    p: Palette,
    selected: bool,
    cx: &mut Context<LLMeterView>,
) -> impl IntoElement {
    let marker = match provider {
        Some(provider) => provider_logo(provider, 20.0),
        None => all_models_icon(20.0, p.foreground),
    };
    let provider_key = provider.map(Provider::as_str).unwrap_or("all");
    div()
        .id(format!("overview-provider-card-{provider_key}"))
        .w(px(168.0))
        .rounded_2xl()
        .border_1()
        .border_color(if selected {
            p.link.opacity(0.72)
        } else {
            p.border.opacity(0.7)
        })
        .bg(if selected {
            p.accent.opacity(0.5)
        } else {
            p.background.opacity(0.94)
        })
        .px_3()
        .py_3()
        .cursor_pointer()
        .hover(move |style| {
            style
                .border_color(p.link.opacity(0.5))
                .bg(p.accent.opacity(0.38))
        })
        .on_click(cx.listener(move |view, _, _, cx| {
            view.set_overview_provider(provider, cx);
        }))
        .child(
            div().flex().items_center().gap_2().child(marker).child(
                div()
                    .min_w(px(0.0))
                    .truncate()
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .child(title),
            ),
        )
        .child(
            div()
                .pt_2()
                .text_lg()
                .font_weight(FontWeight::SEMIBOLD)
                .child(format!("{percent:.2}%")),
        )
        .child(
            div()
                .pt_1()
                .text_xs()
                .text_color(p.muted_foreground)
                .child(t!("overview.model_count", count = models).to_string()),
        )
}

#[derive(Clone, Debug, PartialEq)]
struct DetailModelUsage {
    model: String,
    total_tokens: u64,
    estimated_cost_usd: Option<f64>,
}

fn aggregate_model_usage(models: &[ModelUsage]) -> Vec<DetailModelUsage> {
    let mut aggregated = HashMap::<String, DetailModelUsage>::new();
    for model in models {
        let entry = aggregated
            .entry(model.model.clone())
            .or_insert_with(|| DetailModelUsage {
                model: model.model.clone(),
                total_tokens: 0,
                estimated_cost_usd: None,
            });
        entry.total_tokens = entry.total_tokens.saturating_add(model.total_tokens);
        if let Some(cost) = model.estimated_cost_usd {
            entry.estimated_cost_usd = Some(entry.estimated_cost_usd.unwrap_or_default() + cost);
        }
    }
    let mut models = aggregated.into_values().collect::<Vec<_>>();
    models.sort_by(|left, right| {
        right
            .total_tokens
            .cmp(&left.total_tokens)
            .then_with(|| left.model.cmp(&right.model))
    });
    models
}

fn all_model_detail(
    models: &[DetailModelUsage],
    total_tokens: u64,
    p: Palette,
) -> gpui::AnyElement {
    div()
        .mt_5()
        .pt_5()
        .border_t_1()
        .border_color(p.border)
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(all_models_icon(22.0, p.foreground))
                .child(
                    div()
                        .text_base()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(t!("overview.all_models").to_string()),
                ),
        )
        .child(
            div()
                .pt_2()
                .text_xs()
                .text_color(p.muted_foreground)
                .child(t!("overview.all_models_description").to_string()),
        )
        .child(
            div()
                .pt_2()
                .child(model_detail_rows(models, total_tokens, rgb(0x16a34a), p)),
        )
        .into_any_element()
}

fn all_models_icon(size: f32, color: Hsla) -> gpui::AnyElement {
    div()
        .size(px(size))
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .child(
            svg()
                .path("icons/layers.svg")
                .size(px(size * 0.72))
                .text_color(color),
        )
        .into_any_element()
}

fn provider_model_detail(
    provider: &ProviderUsage,
    models: &[ModelUsage],
    p: Palette,
) -> gpui::AnyElement {
    let provider_models = models
        .iter()
        .filter(|model| model.provider == provider.provider)
        .map(|model| DetailModelUsage {
            model: model.model.clone(),
            total_tokens: model.total_tokens,
            estimated_cost_usd: model.estimated_cost_usd,
        })
        .collect::<Vec<_>>();
    let total_prompt_input = total_prompt_input(
        provider.provider,
        provider.input_tokens,
        provider.cached_input_tokens,
        provider.cache_creation_input_tokens,
    );
    let cache_rate = cache_hit_rate(provider.cached_input_tokens, total_prompt_input);
    let rows = model_detail_rows(
        &provider_models,
        provider.total_tokens,
        provider_color(provider.provider),
        p,
    );

    div()
        .mt_5()
        .pt_5()
        .border_t_1()
        .border_color(p.border)
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(provider_logo(provider.provider, 22.0))
                .child(
                    div().text_base().font_weight(FontWeight::SEMIBOLD).child(
                        t!(
                            "overview.agent_breakdown",
                            provider = provider.provider.display_name()
                        )
                        .to_string(),
                    ),
                ),
        )
        .child(
            div().pt_3().text_xs().text_color(p.muted_foreground).child(
                t!(
                    "overview.cache_summary",
                    rate = format_detail_percentage(cache_rate),
                    cached = format_full_number(provider.cached_input_tokens),
                    input = format_full_number(total_prompt_input)
                )
                .to_string(),
            ),
        )
        .child(div().pt_2().child(rows))
        .into_any_element()
}

fn model_detail_rows(
    models: &[DetailModelUsage],
    total_tokens: u64,
    color: Rgba,
    p: Palette,
) -> gpui::AnyElement {
    let mut rows = div().flex().flex_col();
    for model in models {
        let ratio = model.total_tokens as f32 / total_tokens.max(1) as f32;
        let percentage = ratio as f64 * 100.0;
        rows = rows.child(
            div()
                .py_3()
                .border_b_1()
                .border_color(p.border.opacity(0.7))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_4()
                        .child(
                            div()
                                .min_w(px(0.0))
                                .flex_1()
                                .truncate()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .child(model.model.clone()),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_shrink_0()
                                .items_center()
                                .gap_4()
                                .text_sm()
                                .child(
                                    div()
                                        .w(px(108.0))
                                        .text_right()
                                        .text_color(p.muted_foreground)
                                        .child(format_full_number(model.total_tokens)),
                                )
                                .child(
                                    div()
                                        .w(px(76.0))
                                        .text_right()
                                        .text_color(p.muted_foreground)
                                        .child(format_cost(model.estimated_cost_usd)),
                                )
                                .child(
                                    div()
                                        .w(px(54.0))
                                        .text_right()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(format_detail_percentage(percentage)),
                                ),
                        ),
                )
                .child(
                    div()
                        .mt_2()
                        .h(px(4.0))
                        .w_full()
                        .overflow_hidden()
                        .rounded_full()
                        .bg(p.muted)
                        .child(
                            div()
                                .h_full()
                                .w(relative(ratio.clamp(0.0, 1.0)))
                                .rounded_full()
                                .bg(color),
                        ),
                ),
        );
    }
    if models.is_empty() {
        rows = rows.child(empty_state(
            t!("overview.agent_models_empty").to_string(),
            p,
        ));
    }
    rows.into_any_element()
}

fn total_prompt_input(
    provider: Provider,
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_creation_input_tokens: u64,
) -> u64 {
    match provider {
        // Codex reports cached input as a subset of input_tokens. The other
        // supported providers report uncached, cache-read, and cache-write
        // input as separate categories.
        Provider::Codex => input_tokens,
        _ => input_tokens
            .saturating_add(cached_input_tokens)
            .saturating_add(cache_creation_input_tokens),
    }
}

fn cache_hit_rate(cached_input_tokens: u64, total_prompt_input: u64) -> f64 {
    if total_prompt_input == 0 {
        0.0
    } else {
        (cached_input_tokens as f64 / total_prompt_input as f64 * 100.0).min(100.0)
    }
}

fn format_detail_percentage(value: f64) -> String {
    if (value - value.round()).abs() < 0.05 {
        format!("{value:.0}%")
    } else {
        format!("{value:.1}%")
    }
}

fn sidebar(active_page: DashboardPage, cx: &mut Context<LLMeterView>) -> impl IntoElement {
    let sidebar_foreground = cx.theme().sidebar_foreground;
    let sidebar_muted = sidebar_foreground.opacity(0.65);

    let mut navigation = div().flex().flex_col().gap_1().pt_5();
    for page in DashboardPage::ALL {
        let item = page.label();
        let is_active = page == active_page;
        let variant = if is_active {
            ButtonCustomVariant::new(cx)
                .color(cx.theme().background.opacity(0.78))
                .foreground(sidebar_foreground)
                .hover(cx.theme().background.opacity(0.88))
                .active(cx.theme().background.opacity(0.88))
        } else {
            ButtonCustomVariant::new(cx)
                .color(gpui::transparent_black())
                .foreground(sidebar_foreground)
                .hover(cx.theme().sidebar_accent.opacity(0.55))
                .active(cx.theme().sidebar_accent.opacity(0.55))
        };
        navigation = navigation.child(
            Button::new(item.clone())
                .custom(variant)
                .selected(is_active)
                .icon(page.icon())
                .w_full()
                .justify_start()
                .child(div().flex_1().min_w(px(0.0)).child(item))
                .on_click(cx.listener(move |view, _, _, cx| view.navigate(page, cx))),
        );
    }
    div()
        .flex()
        .flex_col()
        .min_w(px(200.0))
        .px_4()
        .pb_4()
        .when(cfg!(target_os = "macos"), |this| this.pt(TITLE_BAR_HEIGHT))
        .when(!cfg!(target_os = "macos"), |this| this.pt_4())
        .bg(gpui::transparent_black())
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
        statuses = statuses.child(provider_status_row(detection, p));
    }
    if snapshot.detections.is_empty() {
        statuses = statuses.child(empty_state(
            t!("providers.detection_unavailable").to_string(),
            p,
        ));
    }
    statuses
}

fn provider_status_row(detection: &ProviderDetection, p: Palette) -> impl IntoElement {
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
        .items_center()
        .justify_between()
        .gap_4()
        .rounded_lg()
        .bg(p.muted.opacity(0.34))
        .px_3()
        .py_2()
        .text_xs()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(provider_logo(detection.provider, 22.0))
                .child(detection.provider.display_name()),
        )
        .child(div().text_color(color).child(label))
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
                        .child(provider_logo(provider.provider, 24.0))
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(p.foreground)
                                .child(provider.provider.display_name()),
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
                .child(provider_logo(activity.provider, 24.0))
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
                                    activity.provider.display_name(),
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
                .child(provider_logo(model.provider, 24.0))
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
                                .child(model.provider.display_name()),
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
        Provider::Zed => rgb(0x111827),
        Provider::Grok => rgb(0x64748b),
        Provider::Hermes => rgb(0x475569),
    }
}

const HEATMAP_WEEKS: i64 = 20;
const HEATMAP_CELL: f32 = 12.0;
const HEATMAP_GAP: f32 = 3.0;

#[derive(Clone, Debug, PartialEq)]
struct HeatmapCell {
    date_text: String,
    value: u64,
    level: usize,
    models: Vec<(String, u64)>,
}

/// Paints the 20×7 weekly calendar as a single canvas so scrolling the overview
/// does not thrash 140+ stateful hover/tooltip elements.
pub(crate) struct HeatmapView {
    cells: Rc<Vec<HeatmapCell>>,
    colors: [Hsla; 5],
    hover: Option<usize>,
}

impl Default for HeatmapView {
    fn default() -> Self {
        Self {
            cells: Rc::new(Vec::new()),
            colors: [gpui::black(); 5],
            hover: None,
        }
    }
}

impl HeatmapView {
    fn sync(&mut self, cells: Rc<Vec<HeatmapCell>>, colors: [Hsla; 5], cx: &mut Context<Self>) {
        if self.cells.as_ref() != cells.as_ref() || self.colors != colors {
            self.cells = cells;
            self.colors = colors;
            cx.notify();
        }
    }

    pub(crate) fn clear_hover(&mut self, cx: &mut Context<Self>) {
        if self.hover.take().is_some() {
            cx.notify();
        }
    }
}

impl Render for HeatmapView {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cell = HEATMAP_CELL;
        let gap = HEATMAP_GAP;
        let width = px(HEATMAP_WEEKS as f32 * cell + (HEATMAP_WEEKS - 1) as f32 * gap);
        let height = px(7.0 * cell + 6.0 * gap);
        let colors = self.colors;
        let levels = self.cells.iter().map(|item| item.level).collect::<Vec<_>>();
        let grid_bounds = Rc::new(Cell::new(Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(width, height),
        }));

        let mut grid = div()
            .id("heatmap-grid")
            .relative()
            .w(width)
            .h(height)
            .flex_shrink_0()
            .child(
                canvas(
                    {
                        let grid_bounds = grid_bounds.clone();
                        move |bounds, _, _| grid_bounds.set(bounds)
                    },
                    move |bounds, _, window, _| {
                        let hovered = heatmap_hit_index(window.mouse_position(), bounds, cell, gap)
                            .filter(|index| *index < levels.len());
                        for (index, level) in levels.iter().copied().enumerate() {
                            let week = (index / 7) as f32;
                            let weekday = (index % 7) as f32;
                            let origin = point(
                                bounds.origin.x + px(week * (cell + gap)),
                                bounds.origin.y + px(weekday * (cell + gap)),
                            );
                            let mut color = colors[level.min(4)];
                            if hovered == Some(index) {
                                color = color.opacity(0.78);
                            }
                            window.paint_quad(
                                fill(
                                    Bounds {
                                        origin,
                                        size: size(px(cell), px(cell)),
                                    },
                                    color,
                                )
                                .corner_radii(px(4.0)),
                            );
                        }
                    },
                )
                .size_full(),
            )
            .on_mouse_move(cx.listener({
                let grid_bounds = grid_bounds.clone();
                move |this, event: &MouseMoveEvent, _, cx| {
                    let next = heatmap_hit_index(event.position, grid_bounds.get(), cell, gap)
                        .filter(|index| *index < this.cells.len());
                    if this.hover != next {
                        this.hover = next;
                        cx.notify();
                    }
                }
            }))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !*hovered && this.hover.take().is_some() {
                    cx.notify();
                }
            }));

        if let Some(index) = self.hover
            && let Some(item) = self.cells.get(index).cloned()
        {
            let week = (index / 7) as f32;
            let weekday = (index % 7) as f32;
            grid = grid.child(
                div()
                    .id(("heatmap-hover", index))
                    .absolute()
                    .left(px(week * (cell + gap)))
                    .top(px(weekday * (cell + gap)))
                    .size(px(cell))
                    .tooltip_show_delay(std::time::Duration::ZERO)
                    .tooltip(move |_, cx| {
                        cx.new(|_| HeatmapTooltip {
                            date: item.date_text.clone(),
                            level: item.level,
                            total_tokens: item.value,
                            models: item.models.clone(),
                        })
                        .into()
                    }),
            );
        }

        grid
    }
}

fn heatmap_hit_index(
    position: gpui::Point<gpui::Pixels>,
    bounds: Bounds<gpui::Pixels>,
    cell: f32,
    gap: f32,
) -> Option<usize> {
    if !bounds.contains(&position) {
        return None;
    }
    let local_x = (position.x - bounds.origin.x).as_f32();
    let local_y = (position.y - bounds.origin.y).as_f32();
    let stride = cell + gap;
    let week = (local_x / stride).floor();
    let weekday = (local_y / stride).floor();
    if week < 0.0 || weekday < 0.0 {
        return None;
    }
    let week = week as i64;
    let weekday = weekday as i64;
    if week >= HEATMAP_WEEKS || weekday >= 7 {
        return None;
    }
    let x_in_cell = local_x - week as f32 * stride;
    let y_in_cell = local_y - weekday as f32 * stride;
    if x_in_cell > cell || y_in_cell > cell {
        return None;
    }
    Some((week * 7 + weekday) as usize)
}

fn heatmap(
    view: &LLMeterView,
    daily: &[llmeter_storage::DailyUsage],
    model_usage: &[DailyModelUsage],
    p: Palette,
    cx: &mut Context<LLMeterView>,
) -> gpui::AnyElement {
    let cell_size = px(HEATMAP_CELL);
    let gap = px(HEATMAP_GAP);
    let label_width = px(18.0);
    let today = Local::now().date_naive();
    let current_week_start =
        today - Duration::days(i64::from(today.weekday().num_days_from_sunday()));
    let start = current_week_start - Duration::days((HEATMAP_WEEKS - 1) * 7);

    let totals = daily
        .iter()
        .filter_map(|day| {
            NaiveDate::parse_from_str(&day.day, "%Y-%m-%d")
                .ok()
                .map(|date| (date, day.total_tokens))
        })
        .collect::<HashMap<_, _>>();
    let mut models_by_day = HashMap::<NaiveDate, Vec<(String, u64)>>::new();
    for usage in model_usage {
        if let Ok(date) = NaiveDate::parse_from_str(&usage.day, "%Y-%m-%d") {
            models_by_day
                .entry(date)
                .or_default()
                .push((usage.model.clone(), usage.total_tokens));
        }
    }

    let mut values = Vec::with_capacity((HEATMAP_WEEKS * 7) as usize);
    for week in 0..HEATMAP_WEEKS {
        for weekday in 0..7 {
            let date = start + Duration::days(week * 7 + weekday);
            if date > today {
                continue;
            }
            values.push((date, totals.get(&date).copied().unwrap_or_default()));
        }
    }
    let max_value = values.iter().map(|(_, value)| *value).max().unwrap_or(0);
    let cells = Rc::new(
        values
            .into_iter()
            .map(|(date, value)| HeatmapCell {
                date_text: date.format("%Y-%m-%d").to_string(),
                value,
                level: heatmap_level(value, max_value),
                models: models_by_day.remove(&date).unwrap_or_default(),
            })
            .collect::<Vec<_>>(),
    );
    view.heatmap.update(cx, |heatmap, cx| {
        heatmap.sync(
            cells,
            [
                heatmap_level_color(0, p),
                heatmap_level_color(1, p),
                heatmap_level_color(2, p),
                heatmap_level_color(3, p),
                heatmap_level_color(4, p),
            ],
            cx,
        );
    });

    let mut month_grid = div().flex().gap(gap).flex_shrink_0();
    for week in 0..HEATMAP_WEEKS {
        let week_start = start + Duration::days(week * 7);
        let month = if week == 0 {
            Some(week_start.month())
        } else {
            (0..7).find_map(|offset| {
                let date = week_start + Duration::days(offset);
                (date <= today && date.day() == 1).then_some(date.month())
            })
        };
        let month_label = month
            .map(|month| t!("overview.heatmap_month", month = month).to_string())
            .unwrap_or_default();
        month_grid = month_grid.child(
            div().w(cell_size).h(px(18.0)).flex_shrink_0().child(
                div()
                    .whitespace_nowrap()
                    .text_xs()
                    .text_color(p.muted_foreground)
                    .child(month_label),
            ),
        );
    }

    let weekday_labels = [
        t!("overview.heatmap_sun").to_string(),
        t!("overview.heatmap_mon").to_string(),
        t!("overview.heatmap_tue").to_string(),
        t!("overview.heatmap_wed").to_string(),
        t!("overview.heatmap_thu").to_string(),
        t!("overview.heatmap_fri").to_string(),
        t!("overview.heatmap_sat").to_string(),
    ];
    let mut weekday_column = div()
        .flex()
        .flex_col()
        .gap(gap)
        .w(label_width)
        .flex_shrink_0();
    for label in weekday_labels {
        weekday_column = weekday_column.child(
            div()
                .w(label_width)
                .h(cell_size)
                .flex()
                .items_center()
                .text_xs()
                .text_color(p.muted_foreground)
                .child(label),
        );
    }

    let week_row = div()
        .flex()
        .gap(gap)
        .items_end()
        .child(div().w(label_width).h(px(18.0)).flex_shrink_0())
        .child(month_grid);
    let grid = div()
        .flex()
        .items_start()
        .gap(gap)
        .child(weekday_column)
        .child(view.heatmap.clone());

    let legend_colors = [
        heatmap_level_color(0, p),
        heatmap_level_color(1, p),
        heatmap_level_color(2, p),
        heatmap_level_color(3, p),
        heatmap_level_color(4, p),
    ];
    let mut legend = div()
        .flex()
        .items_center()
        .justify_center()
        .gap(px(6.0))
        .pt_3()
        .text_xs()
        .text_color(p.muted_foreground)
        .child(t!("overview.heatmap_less").to_string());
    for color in legend_colors {
        legend = legend.child(div().size(px(16.0)).rounded_sm().bg(color));
    }
    legend = legend.child(t!("overview.heatmap_more").to_string());

    div()
        .w_full()
        .p_4()
        .rounded_2xl()
        .bg(p.tiles.opacity(0.98))
        .border_1()
        .border_color(p.border.opacity(0.7))
        .child(
            div()
                .text_base()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.foreground)
                .child(t!("overview.heatmap").to_string()),
        )
        .child(div().pt_3().child(week_row).child(div().pt_2().child(grid)))
        .child(legend)
        .into_any_element()
}

fn heatmap_level(value: u64, max_value: u64) -> usize {
    if value == 0 {
        return 0;
    }
    let ratio = value as f64 / max_value.max(1) as f64;
    if ratio <= 0.05 {
        1
    } else if ratio <= 0.2 {
        2
    } else if ratio <= 0.5 {
        3
    } else {
        4
    }
}

fn heatmap_level_color(level: usize, p: Palette) -> gpui::Hsla {
    match level {
        0 => p.muted,
        1 => p.success.opacity(0.22),
        2 => p.success.opacity(0.42),
        3 => p.success.opacity(0.68),
        _ => p.success,
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

struct HeatmapTooltip {
    date: String,
    level: usize,
    total_tokens: u64,
    models: Vec<(String, u64)>,
}

impl Render for HeatmapTooltip {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let total_tokens = self.total_tokens.max(1);
        let mut model_rows = div().flex().flex_col().gap_3();
        for (model, tokens) in self.models.iter().take(5) {
            let ratio = (*tokens as f32 / total_tokens as f32).min(1.0);
            let percentage = (*tokens as f64 / total_tokens as f64 * 100.0).round() as u64;
            model_rows = model_rows.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .truncate()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(model.clone()),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .whitespace_nowrap()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child(format!(
                                        "{} · {}%",
                                        format_heatmap_tokens(*tokens),
                                        percentage
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .h(px(6.0))
                            .w_full()
                            .overflow_hidden()
                            .rounded_full()
                            .bg(theme.muted)
                            .child(
                                div()
                                    .h_full()
                                    .w(relative(ratio))
                                    .rounded_full()
                                    .bg(theme.success),
                            ),
                    ),
            );
        }
        if self.models.is_empty() {
            model_rows = model_rows.child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(t!("overview.heatmap_no_models").to_string()),
            );
        }

        div()
            .w(px(280.0))
            .rounded_xl()
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .px_4()
            .py_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.muted_foreground)
                            .child(self.date.clone()),
                    )
                    .child(
                        div()
                            .rounded_full()
                            .border_1()
                            .border_color(theme.success.opacity(0.3))
                            .bg(theme.success.opacity(0.1))
                            .px_2()
                            .py_0p5()
                            .text_xs()
                            .whitespace_nowrap()
                            .text_color(theme.success)
                            .child(t!("overview.heatmap_level", level = self.level).to_string()),
                    ),
            )
            .child(
                div()
                    .pt_2()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .child(
                        div()
                            .text_2xl()
                            .font_weight(FontWeight::BOLD)
                            .line_height(relative(1.0))
                            .child(format_heatmap_tokens(self.total_tokens)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.muted_foreground)
                            .child(t!("overview.heatmap_token_unit").to_string()),
                    ),
            )
            .child(
                div()
                    .mt_3()
                    .pt_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .pb_2()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(t!("overview.heatmap_models").to_string()),
                    )
                    .child(model_rows),
            )
    }
}

fn empty_state(text: String, p: Palette) -> impl IntoElement {
    div().text_sm().text_color(p.muted_foreground).child(text)
}

fn format_full_number(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::new();
    for (index, character) in digits.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(character);
    }
    grouped.chars().rev().collect()
}

fn format_tokens(value: u64) -> String {
    if value >= 1_000_000_000 {
        format!("{:.2}B", value as f64 / 1_000_000_000.0)
    } else if value >= 1_000_000 {
        format!("{:.2}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn format_heatmap_tokens(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
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

#[cfg(test)]
mod tests {
    use super::{
        HEATMAP_CELL, HEATMAP_GAP, HEATMAP_WEEKS, aggregate_model_usage, cache_hit_rate,
        format_detail_percentage, heatmap_hit_index, total_prompt_input,
    };
    use gpui::{Bounds, point, px, size};
    use llmeter_core::Provider;
    use llmeter_storage::ModelUsage;

    fn grid_bounds() -> Bounds<gpui::Pixels> {
        let width = HEATMAP_WEEKS as f32 * HEATMAP_CELL + (HEATMAP_WEEKS - 1) as f32 * HEATMAP_GAP;
        let height = 7.0 * HEATMAP_CELL + 6.0 * HEATMAP_GAP;
        Bounds {
            origin: point(px(10.0), px(20.0)),
            size: size(px(width), px(height)),
        }
    }

    #[test]
    fn heatmap_hit_index_maps_cells_and_skips_gaps() {
        let bounds = grid_bounds();
        assert_eq!(
            heatmap_hit_index(point(px(10.0), px(20.0)), bounds, HEATMAP_CELL, HEATMAP_GAP),
            Some(0)
        );
        assert_eq!(
            heatmap_hit_index(point(px(21.0), px(31.0)), bounds, HEATMAP_CELL, HEATMAP_GAP),
            Some(0)
        );
        assert_eq!(
            heatmap_hit_index(point(px(23.0), px(20.0)), bounds, HEATMAP_CELL, HEATMAP_GAP),
            None
        );
        assert_eq!(
            heatmap_hit_index(point(px(25.0), px(20.0)), bounds, HEATMAP_CELL, HEATMAP_GAP),
            Some(7)
        );
        assert_eq!(
            heatmap_hit_index(point(px(9.0), px(20.0)), bounds, HEATMAP_CELL, HEATMAP_GAP),
            None
        );
    }

    #[test]
    fn cache_hit_rate_handles_usage_and_zero_input() {
        let codex_input = total_prompt_input(Provider::Codex, 388, 370, 0);
        assert_eq!(codex_input, 388);
        assert_eq!(cache_hit_rate(370, codex_input), 370.0 / 388.0 * 100.0);
        assert_eq!(
            format_detail_percentage(cache_hit_rate(370, codex_input)),
            "95.4%"
        );

        let pi_input = total_prompt_input(Provider::Pi, 100, 300, 20);
        assert_eq!(pi_input, 420);
        assert_eq!(cache_hit_rate(300, pi_input), 300.0 / 420.0 * 100.0);
        assert_eq!(cache_hit_rate(10, 0), 0.0);
        assert_eq!(format_detail_percentage(87.0), "87%");
    }

    #[test]
    fn all_model_usage_merges_names_across_providers_and_sorts_by_tokens() {
        let models = vec![
            ModelUsage {
                provider: Provider::Codex,
                model: "shared-model".into(),
                total_tokens: 100,
                estimated_cost_usd: Some(0.4),
            },
            ModelUsage {
                provider: Provider::Pi,
                model: "shared-model".into(),
                total_tokens: 50,
                estimated_cost_usd: Some(0.1),
            },
            ModelUsage {
                provider: Provider::Grok,
                model: "larger-model".into(),
                total_tokens: 200,
                estimated_cost_usd: None,
            },
        ];

        let aggregated = aggregate_model_usage(&models);
        assert_eq!(aggregated.len(), 2);
        assert_eq!(aggregated[0].model, "larger-model");
        assert_eq!(aggregated[0].total_tokens, 200);
        assert_eq!(aggregated[0].estimated_cost_usd, None);
        assert_eq!(aggregated[1].model, "shared-model");
        assert_eq!(aggregated[1].total_tokens, 150);
        assert_eq!(aggregated[1].estimated_cost_usd, Some(0.5));
    }
}
