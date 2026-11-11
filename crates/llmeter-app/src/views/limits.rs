use chrono::{DateTime, Local, Utc};
use gpui::{
    AnyElement, Context, FontWeight, IntoElement, ParentElement, Styled, div, prelude::*, px,
    relative,
};
use gpui_component::{Disableable, IconName, button::Button, h_flex, v_flex};
use llmeter_core::{LimitWindow, Provider, ProviderLimits, ResetCredit};
use rust_i18n::t;

use crate::{
    app::LLMeterView,
    views::{palette::Palette, provider_brand::provider_logo},
};

const LIMIT_PROVIDERS: [Provider; 8] = [
    Provider::Claude,
    Provider::Codex,
    Provider::Cursor,
    Provider::Qoder,
    Provider::Grok,
    Provider::Trae,
    Provider::ZCode,
    Provider::Antigravity,
];

/// A card appears only once it has quota windows to show. That keeps the page
/// blank instead of flashing "pending" cards that later turn out to be
/// unconfigured or unavailable: entries without windows (missing sign-in
/// state, failed checks) and not-yet-checked providers stay hidden until a
/// refresh delivers real data. Cached windows — including stale cache with an
/// error footer — render immediately on open.
fn visible_limit_providers(
    limits: &llmeter_core::LimitsSnapshot,
) -> impl Iterator<Item = Provider> + '_ {
    LIMIT_PROVIDERS.into_iter().filter(move |provider| {
        limits
            .provider(*provider)
            .is_some_and(|item| !item.windows.is_empty())
    })
}

pub(crate) fn limits_page(view: &LLMeterView, cx: &mut Context<LLMeterView>) -> impl IntoElement {
    let p = Palette::from_app(cx);
    let visible = visible_limit_providers(&view.limits).collect::<Vec<_>>();
    let all_hidden = visible.is_empty();
    let cards = visible.into_iter().map(|provider| {
        provider_card(
            provider,
            view.limits.provider(provider),
            view.limits_refreshing,
            p,
        )
    });
    let refresh_label = if view.limits_refreshing {
        t!("limits.refreshing").to_string()
    } else {
        t!("limits.refresh").to_string()
    };

    v_flex()
        .w_full()
        .px_6()
        .pb_6()
        .gap_6()
        .child(
            h_flex()
                .w_full()
                .items_start()
                .justify_between()
                .gap_4()
                .child(
                    v_flex()
                        .gap_2()
                        .child(
                            div()
                                .text_xl()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(p.foreground)
                                .child(t!("limits.title")),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(p.muted_foreground)
                                .child(t!("limits.subtitle")),
                        ),
                )
                .child(
                    Button::new("refresh-limits")
                        .outline()
                        .icon(IconName::Redo)
                        .label(refresh_label)
                        .disabled(view.limits_refreshing)
                        .on_click(cx.listener(|view, _, _, cx| view.refresh_limits(cx))),
                ),
        )
        .child(
            v_flex()
                .w_full()
                .gap_4()
                .children(cards)
                // While a refresh is running a blank list means "waiting",
                // not "nothing found"; hold the hint until the check settles.
                .when(all_hidden && !view.limits_refreshing, |this| {
                    this.child(
                        div()
                            .debug_selector(|| "limit-cards-empty".to_string())
                            .rounded_lg()
                            .border_1()
                            .border_color(p.border)
                            .bg(p.tiles)
                            .p_4()
                            .text_sm()
                            .text_color(p.muted_foreground)
                            .child(t!("limits.none_connected")),
                    )
                }),
        )
        .child(
            div()
                .pt_1()
                .text_xs()
                .text_color(p.muted_foreground)
                .child(t!("limits.privacy_note")),
        )
}

fn provider_card(
    provider: Provider,
    limits: Option<&ProviderLimits>,
    refreshing: bool,
    p: Palette,
) -> AnyElement {
    let has_windows = limits.is_some_and(|item| !item.windows.is_empty());
    let mut rows = v_flex()
        .gap_4()
        .when(has_windows, |this| this.pt_5())
        .when(!has_windows, |this| this.pt_3());
    if let Some(limits) = limits {
        for window in &limits.windows {
            rows = rows.child(window_row(window, p));
        }
        let cards = unexpired_reset_cards(limits, Utc::now());
        if !cards.is_empty() {
            rows = rows.child(reset_block(&cards, p, Utc::now()));
        }
    }
    if !has_windows {
        rows = rows.child(
            div()
                .text_sm()
                .text_color(p.muted_foreground)
                .child(empty_message(limits, refreshing)),
        );
    }

    let plan = limits
        .and_then(|item| item.plan.as_deref())
        .filter(|plan| !plan.is_empty())
        .map(str::to_string);
    let (status, status_color, status_background) = status_style(limits, refreshing, p);
    // An authentication check is not a quota update. Keep empty cards compact
    // and reserve the timestamp footer for cards that actually show limits.
    let captured = limits.filter(|_| has_windows).map(|item| {
        t!(
            "limits.updated",
            time = relative_time(item.captured_at, Utc::now())
        )
        .to_string()
    });

    v_flex()
        .debug_selector(move || format!("limit-card-{}", provider.as_str()))
        .w_full()
        .min_w(px(0.0))
        .rounded_lg()
        .border_1()
        .border_color(p.border)
        .bg(p.tiles)
        .when(has_windows, |this| this.p_5())
        .when(!has_windows, |this| this.p_4())
        .child(
            h_flex()
                .w_full()
                .items_start()
                .justify_between()
                .gap_3()
                .child(
                    h_flex()
                        .min_w(px(0.0))
                        .gap_3()
                        .child(provider_logo(provider, 30.0))
                        .child(
                            v_flex()
                                .min_w(px(0.0))
                                .gap_1()
                                .child(
                                    div()
                                        .truncate()
                                        .text_base()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(provider.display_name()),
                                )
                                .when_some(plan, |this, plan| {
                                    this.child(
                                        div()
                                            .truncate()
                                            .text_xs()
                                            .text_color(p.muted_foreground)
                                            .child(plan),
                                    )
                                }),
                        ),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .rounded_full()
                        .bg(status_background)
                        .px_2()
                        .py_1()
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(status_color)
                        .child(status),
                ),
        )
        .child(rows)
        .when_some(captured, |this, captured| {
            this.child(
                div()
                    .debug_selector(move || format!("limit-updated-{}", provider.as_str()))
                    .mt_5()
                    .pt_3()
                    .border_t_1()
                    .border_color(p.border.opacity(0.7))
                    .text_xs()
                    .text_color(p.muted_foreground)
                    .child(captured),
            )
        })
        .when_some(
            limits.and_then(|item| item.last_error.clone().or_else(|| item.error.clone())),
            |this, error| {
                this.child(div().pt_2().text_xs().text_color(p.warning).child(
                    if limits.is_some_and(|item| item.stale) {
                        format!("{} {error}", t!("limits.cached_warning"))
                    } else {
                        error
                    },
                ))
            },
        )
        .into_any_element()
}

fn window_row(window: &LimitWindow, p: Palette) -> AnyElement {
    let ratio = (window.used_percent / 100.0).clamp(0.0, 1.0) as f32;
    let color = if window.quota_exceeded || window.used_percent >= 90.0 {
        p.danger
    } else if window.used_percent >= 75.0 {
        p.warning
    } else {
        p.link
    };
    let reset = window.reset_at.map(|reset| {
        t!(
            "limits.resets_in",
            time = relative_duration(reset, Utc::now())
        )
        .to_string()
    });
    let amount = window
        .used_amount
        .zip(window.limit_amount)
        .map(|(used, limit)| format_amounts(used, limit, window.unit.as_deref()));
    let usage_label = if window.quota_exceeded {
        t!("limits.quota_exceeded").to_string()
    } else {
        format_percent(window.used_percent)
    };

    if !window.usage_known {
        let allowance = window
            .limit_amount
            .map(|limit| format_allowance(limit, window.unit.as_deref()))
            .unwrap_or_else(|| t!("limits.usage_unknown").to_string());
        return v_flex()
            .gap_2()
            .child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .min_w(px(0.0))
                            .truncate()
                            .text_sm()
                            .text_color(p.foreground)
                            .child(window_label(&window.key)),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.foreground)
                            .child(allowance),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(p.muted_foreground)
                    .child(t!("limits.allowance_only")),
            )
            .into_any_element();
    }

    v_flex()
        .gap_2()
        .child(
            h_flex()
                .w_full()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .min_w(px(0.0))
                        .truncate()
                        .text_sm()
                        .text_color(p.foreground)
                        .child(window_label(&window.key)),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(color)
                        .child(usage_label),
                ),
        )
        .child(
            div()
                .h(px(6.0))
                .w_full()
                .overflow_hidden()
                .rounded_full()
                .bg(p.muted)
                .child(div().h_full().w(relative(ratio)).rounded_full().bg(color)),
        )
        .when(amount.is_some() || reset.is_some(), |this| {
            this.child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .gap_2()
                    .text_xs()
                    .text_color(p.muted_foreground)
                    .when_some(amount, |this, amount| this.child(amount))
                    .when_some(reset, |this, reset| this.child(reset)),
            )
        })
        .into_any_element()
}

/// The provider's reset cards that have not expired yet, sorted by expiry
/// (soonest first). Expired entries from stale disk caches are dropped here
/// so the list never shows a card that already rolled off.
fn unexpired_reset_cards(limits: &ProviderLimits, now: DateTime<Utc>) -> Vec<ResetCredit> {
    let mut cards = limits
        .reset_cards
        .iter()
        .filter(|card| !card.is_expired(now))
        .cloned()
        .collect::<Vec<_>>();
    cards.sort_by_key(|card| card.expires_at);
    cards
}

/// Reset cards for one provider, embedded in its card below the usage rows:
/// a muted subheading plus numbered rows with a remaining-validity bar and
/// the absolute expiry time, mirroring the provider's own usage page.
fn reset_block(cards: &[ResetCredit], p: Palette, now: DateTime<Utc>) -> AnyElement {
    let mut list = v_flex().gap_2();
    for (index, card) in cards.iter().enumerate() {
        list = list.child(reset_row(index, card, p, now));
    }
    v_flex()
        .debug_selector(|| "limits-reset-block".to_string())
        .gap_2()
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(p.muted_foreground)
                .child(t!("limits.reset_title")),
        )
        .child(list)
        .into_any_element()
}

fn reset_row(index: usize, card: &ResetCredit, p: Palette, now: DateTime<Utc>) -> AnyElement {
    let label = format!("{} {}", t!("limits.reset_card"), index + 1);
    let expires = card
        .expires_at
        .with_timezone(&Local)
        .format("%-m/%-d %H:%M")
        .to_string();
    // The bar tracks how much of the card's validity window is left, the
    // same fill the provider's usage page draws; cards without a known
    // grant time (ZCode) render a full bar.
    let ratio = card.remaining_ratio(now).unwrap_or(1.0) as f32;

    h_flex()
        .w_full()
        .items_center()
        .gap_3()
        .child(
            div()
                .w(px(56.0))
                .flex_shrink_0()
                .truncate()
                .text_sm()
                .text_color(p.muted_foreground)
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .h(px(6.0))
                .overflow_hidden()
                .rounded_full()
                .bg(p.muted)
                .child(
                    div()
                        .h_full()
                        .w(relative(ratio))
                        .rounded_full()
                        .bg(p.muted_foreground.opacity(0.55)),
                ),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_xs()
                .text_color(p.muted_foreground)
                .child(expires),
        )
        .into_any_element()
}

fn status_style(
    limits: Option<&ProviderLimits>,
    refreshing: bool,
    p: Palette,
) -> (String, gpui::Hsla, gpui::Hsla) {
    match limits {
        None if refreshing => (
            t!("limits.checking").to_string(),
            p.muted_foreground,
            p.muted,
        ),
        None => (
            t!("limits.pending").to_string(),
            p.muted_foreground,
            p.muted,
        ),
        Some(item) if !item.configured => (
            t!("limits.not_connected").to_string(),
            p.muted_foreground,
            p.muted,
        ),
        Some(item) if item.stale => (
            t!("limits.cached").to_string(),
            p.warning,
            p.warning.opacity(0.12),
        ),
        Some(item) if item.error.is_some() => (
            t!("limits.unavailable").to_string(),
            p.danger,
            p.danger.opacity(0.12),
        ),
        Some(_) => (
            t!("limits.live").to_string(),
            p.success,
            p.success.opacity(0.1),
        ),
    }
}

fn empty_message(limits: Option<&ProviderLimits>, refreshing: bool) -> String {
    match limits {
        None if refreshing => t!("limits.checking_hint").to_string(),
        None => t!("limits.pending_hint").to_string(),
        Some(item) if !item.configured => t!("limits.not_connected_hint").to_string(),
        Some(item) if item.error.is_some() => item.error.clone().unwrap_or_default(),
        Some(_) => t!("limits.no_windows").to_string(),
    }
}

fn window_label(key: &str) -> String {
    match key {
        "five_hour" => t!("limits.window_5h").to_string(),
        "seven_day" => t!("limits.window_7d").to_string(),
        "spark_five_hour" => t!("limits.window_spark_5h").to_string(),
        "spark_seven_day" => t!("limits.window_spark_7d").to_string(),
        "daily" => t!("limits.window_daily").to_string(),
        "weekly" => t!("limits.window_weekly").to_string(),
        "monthly" => t!("limits.window_monthly").to_string(),
        "opus" => t!("limits.window_opus").to_string(),
        "credits" => t!("limits.window_credits").to_string(),
        "on_demand" => t!("limits.window_on_demand").to_string(),
        "cursor_plan" => t!("limits.window_cursor_plan").to_string(),
        "cursor_auto" => t!("limits.window_cursor_auto").to_string(),
        "cursor_api" => t!("limits.window_cursor_api").to_string(),
        "qoder_credits" => t!("limits.window_qoder_credits").to_string(),
        "qoder_cn_credits" => t!("limits.window_qoder_cn_credits").to_string(),
        "zcode_mcp_credits" => t!("limits.window_zcode_mcp_credits").to_string(),
        "cl_seven_day" => t!("limits.window_cl_7d").to_string(),
        "cl_five_hour" => t!("limits.window_cl_5h").to_string(),
        "gemini_seven_day" => t!("limits.window_gm_7d").to_string(),
        "gemini_five_hour" => t!("limits.window_gm_5h").to_string(),
        "trae_fast_requests" => t!("limits.window_trae_fast_requests").to_string(),
        key if key.starts_with("model:") => key.trim_start_matches("model:").to_string(),
        key => key.replace('_', " "),
    }
}

fn format_percent(value: f64) -> String {
    if (value - value.round()).abs() < 0.05 {
        format!("{value:.0}%")
    } else {
        format!("{value:.1}%")
    }
}

fn format_amounts(used: f64, limit: f64, unit: Option<&str>) -> String {
    let suffix = match unit {
        Some("credits") => t!("limits.credits_unit").to_string(),
        Some(unit) => unit.to_string(),
        None => String::new(),
    };
    format!(
        "{} / {} {suffix}",
        compact_number(used),
        compact_number(limit)
    )
    .trim_end()
    .to_string()
}

fn format_allowance(limit: f64, unit: Option<&str>) -> String {
    let unit = match unit {
        Some("requests/hour") => t!("limits.requests_per_hour").to_string(),
        Some(unit) => unit.to_string(),
        None => String::new(),
    };
    format!("{} {unit}", compact_number(limit))
        .trim_end()
        .to_string()
}

fn compact_number(value: f64) -> String {
    if value.abs() >= 1000.0 || (value - value.round()).abs() < 0.005 {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
    }
}

fn relative_time(value: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (now - value).num_seconds().max(0);
    if seconds < 60 {
        return t!("limits.just_now").to_string();
    }
    if seconds < 3600 {
        return t!("limits.minutes_ago", count = seconds / 60).to_string();
    }
    if seconds < 86_400 {
        return t!("limits.hours_ago", count = seconds / 3600).to_string();
    }
    value
        .with_timezone(&Local)
        .format("%m-%d %H:%M")
        .to_string()
}

fn relative_duration(value: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let minutes = (value - now).num_minutes().max(0);
    if minutes < 60 {
        t!("limits.minutes", count = minutes).to_string()
    } else if minutes < 24 * 60 {
        let hours = minutes / 60;
        let rest = minutes % 60;
        if rest == 0 {
            t!("limits.hours", count = hours).to_string()
        } else {
            t!("limits.hours_minutes", hours = hours, minutes = rest).to_string()
        }
    } else {
        let days = minutes / (24 * 60);
        let hours = minutes % (24 * 60) / 60;
        t!("limits.days_hours", days = days, hours = hours).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn labels_known_windows_and_preserves_model_names() {
        assert!(!window_label("five_hour").is_empty());
        assert!(!window_label("cursor_plan").is_empty());
        assert!(!window_label("qoder_credits").is_empty());
        assert!(!window_label("qoder_cn_credits").is_empty());
        assert!(!window_label("zcode_mcp_credits").is_empty());
        assert!(!window_label("trae_fast_requests").is_empty());
        assert_eq!(window_label("model:Claude Opus"), "Claude Opus");
    }

    #[test]
    fn percent_format_does_not_add_noise_to_whole_values() {
        assert_eq!(format_percent(42.0), "42%");
        assert_eq!(format_percent(42.25), "42.2%");
    }

    fn snapshot_entries(entries: Vec<ProviderLimits>) -> llmeter_core::LimitsSnapshot {
        llmeter_core::LimitsSnapshot {
            fetched_at: Some(Utc::now()),
            providers: entries,
        }
    }

    #[test]
    fn only_cards_with_windows_are_visible() {
        let mut stale = ProviderLimits::not_configured(Provider::Qoder, Utc::now());
        stale.configured = true;
        stale.stale = true;
        stale.error = Some("live request failed".into());
        stale.windows = vec![LimitWindow::new("seven_day", 40.0)];
        let snapshot = snapshot_entries(vec![
            // Signed out: no local credentials.
            ProviderLimits::not_configured(Provider::Grok, Utc::now()),
            // Signed in but the check failed with nothing to show.
            ProviderLimits::failed(
                Provider::Claude,
                Utc::now(),
                "Claude login expired. Run `claude` to sign in again.",
            ),
            // Live fetch failed but cached windows remain.
            stale,
        ]);
        let visible = visible_limit_providers(&snapshot).collect::<Vec<_>>();
        assert_eq!(
            visible,
            vec![Provider::Qoder],
            "stale cache with windows is the only card worth showing"
        );
        // Codex and Trae have no entry at all (check not run this session):
        // they start hidden instead of flashing a pending card.
        assert!(!visible.contains(&Provider::Codex));
        assert!(!visible.contains(&Provider::Trae));
    }

    #[test]
    fn blank_snapshot_shows_no_cards() {
        let snapshot = snapshot_entries(Vec::new());
        assert!(visible_limit_providers(&snapshot).next().is_none());
    }

    #[test]
    fn reset_cards_sort_by_expiry_and_skip_expired() {
        let now = Utc::now();
        let mut codex = ProviderLimits::not_configured(Provider::Codex, now);
        codex.configured = true;
        codex.reset_cards = vec![
            ResetCredit {
                granted_at: Some(now - Duration::days(1)),
                expires_at: now + Duration::days(20),
                title: "Full reset".into(),
            },
            ResetCredit {
                granted_at: Some(now - Duration::days(2)),
                expires_at: now + Duration::days(19),
                title: "Full reset".into(),
            },
            ResetCredit {
                granted_at: Some(now - Duration::days(31)),
                expires_at: now - Duration::minutes(5),
                title: "Expired card".into(),
            },
            ResetCredit {
                granted_at: Some(now),
                expires_at: now,
                title: "Boundary card".into(),
            },
        ];

        let cards = unexpired_reset_cards(&codex, now);

        assert_eq!(
            cards.iter().map(|card| card.expires_at).collect::<Vec<_>>(),
            vec![now + Duration::days(19), now + Duration::days(20)],
            "expired and boundary-expired cards are dropped, soonest expiry first"
        );
    }

    #[test]
    fn all_hidden_when_every_provider_lacks_credentials() {
        let snapshot = snapshot_entries(
            LIMIT_PROVIDERS
                .map(|provider| ProviderLimits::not_configured(provider, Utc::now()))
                .to_vec(),
        );
        assert!(visible_limit_providers(&snapshot).next().is_none());
    }
}
