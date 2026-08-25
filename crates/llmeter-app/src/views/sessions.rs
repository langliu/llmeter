use std::rc::Rc;

use chrono::{Datelike, Duration, Local, Timelike};
use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, ParentElement, SharedString,
    deferred, div, prelude::*, px, size,
};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, Selectable, Sizable,
    button::{Button, ButtonGroup, ButtonVariants},
    h_flex,
    input::Input,
    v_flex, v_virtual_list,
};
use llmeter_core::Provider;
use llmeter_storage::SessionSummary;
use rust_i18n::t;

use crate::{
    app::LLMeterView,
    views::{palette::Palette, provider_brand::provider_logo},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SessionProviderFilter {
    #[default]
    All,
    Provider(Provider),
}

impl SessionProviderFilter {
    const ALL: [Self; 9] = [
        Self::All,
        Self::Provider(Provider::Claude),
        Self::Provider(Provider::Codex),
        Self::Provider(Provider::Pi),
        Self::Provider(Provider::Omp),
        Self::Provider(Provider::OpenCode),
        Self::Provider(Provider::Zed),
        Self::Provider(Provider::Grok),
        Self::Provider(Provider::Hermes),
    ];

    fn label(self) -> String {
        match self {
            Self::All => t!("sessions.all").to_string(),
            Self::Provider(provider) => provider.display_name().to_string(),
        }
    }

    pub(crate) fn matches(self, provider: Provider) -> bool {
        match self {
            Self::All => true,
            Self::Provider(expected) => provider == expected,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SessionRangeFilter {
    #[default]
    All,
    Days7,
    Days30,
    Days90,
}

impl SessionRangeFilter {
    const ALL: [Self; 4] = [Self::All, Self::Days7, Self::Days30, Self::Days90];

    fn label(self) -> String {
        match self {
            Self::All => t!("sessions.all_time").to_string(),
            Self::Days7 => t!("sessions.days_7").to_string(),
            Self::Days30 => t!("sessions.days_30").to_string(),
            Self::Days90 => t!("sessions.days_90").to_string(),
        }
    }

    pub(crate) fn start(
        self,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Option<chrono::DateTime<chrono::Utc>> {
        match self {
            Self::All => None,
            Self::Days7 => Some(now - Duration::days(7)),
            Self::Days30 => Some(now - Duration::days(30)),
            Self::Days90 => Some(now - Duration::days(90)),
        }
    }
}

pub(crate) fn sessions_page(view: &LLMeterView, cx: &mut Context<LLMeterView>) -> impl IntoElement {
    let p = Palette::from_app(cx);
    let session_indices = Rc::new(view.visible_session_indices(cx));
    let visible_count = session_indices.len();
    let total_count = view.snapshot.sessions.len();
    let projects = view.session_projects();
    let project_open = view.session_project_open;
    let selected_project = view.session_project.clone();

    let rows: AnyElement = if session_indices.is_empty() {
        div()
            .size_full()
            .child(empty_state(t!("sessions.empty").to_string(), p))
            .into_any_element()
    } else {
        let item_sizes = Rc::new(vec![size(px(1.0), px(78.0)); visible_count]);
        v_virtual_list(
            cx.entity().clone(),
            "session-items",
            item_sizes,
            move |view, visible_range, _, cx| {
                visible_range
                    .filter_map(|position| {
                        let session_index = *session_indices.get(position)?;
                        let session = view.snapshot.sessions.get(session_index)?.clone();
                        let copied = view.is_copied(&session);
                        Some(session_row(session, copied, position == 0, p, cx))
                    })
                    .collect()
            },
        )
        .track_scroll(&view.session_scroll)
        .size_full()
        .into_any_element()
    };

    v_flex()
        .size_full()
        .px_8()
        .pt_3()
        .pb_5()
        .child(
            h_flex()
                .w_full()
                .items_start()
                .justify_between()
                .child(
                    v_flex()
                        .gap_2()
                        .child(
                            div()
                                .text_3xl()
                                .font_weight(FontWeight::BOLD)
                                .text_color(p.foreground)
                                .child(t!("sessions.title").to_string()),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(p.muted_foreground)
                                .child(t!("sessions.subtitle").to_string()),
                        ),
                )
                .child(
                    Button::new("refresh-sessions")
                        .outline()
                        .icon(IconName::Redo)
                        .label(format!("{visible_count} / {total_count}"))
                        .tooltip(t!("sessions.refresh").to_string())
                        .on_click(cx.listener(|view, _, _, cx| view.refresh_sessions(cx))),
                ),
        )
        .child(
            h_flex()
                .w_full()
                .pt_6()
                .gap_2()
                .flex_wrap()
                .items_center()
                .child(provider_filter(view.session_provider, cx))
                .child(range_filter(view.session_range, cx))
                .child(project_filter(
                    selected_project.as_deref(),
                    &projects,
                    project_open,
                    p,
                    cx,
                ))
                .child(
                    div().flex_1().min_w(px(180.0)).child(
                        Input::new(&view.session_search).cleanable(true).prefix(
                            Icon::new(IconName::Search)
                                .text_color(p.muted_foreground)
                                .with_size(px(14.)),
                        ),
                    ),
                ),
        )
        .child(
            div()
                .flex_1()
                .min_h(px(0.0))
                .mt_4()
                .overflow_hidden()
                .child(rows),
        )
}

fn provider_filter(
    selected: SessionProviderFilter,
    cx: &mut Context<LLMeterView>,
) -> impl IntoElement {
    let mut group = ButtonGroup::new("session-provider-filter")
        .compact()
        .outline();
    for (index, filter) in SessionProviderFilter::ALL.into_iter().enumerate() {
        group = group.child(
            Button::new(("session-provider", index))
                .label(filter.label())
                .selected(selected == filter),
        );
    }
    group.on_click(cx.listener(|view, clicks: &Vec<usize>, _, cx| {
        if let Some(&index) = clicks.first()
            && let Some(filter) = SessionProviderFilter::ALL.get(index).copied()
        {
            view.set_session_provider(filter, cx);
        }
    }))
}

fn range_filter(selected: SessionRangeFilter, cx: &mut Context<LLMeterView>) -> impl IntoElement {
    let mut group = ButtonGroup::new("session-range-filter").compact().outline();
    for (index, filter) in SessionRangeFilter::ALL.into_iter().enumerate() {
        let button = Button::new(("session-range", index))
            .label(filter.label())
            .selected(selected == filter);
        group = group.child(if index == 0 {
            button.icon(IconName::Calendar)
        } else {
            button
        });
    }
    group.on_click(cx.listener(|view, clicks: &Vec<usize>, _, cx| {
        if let Some(&index) = clicks.first()
            && let Some(filter) = SessionRangeFilter::ALL.get(index).copied()
        {
            view.set_session_range(filter, cx);
        }
    }))
}

fn project_filter(
    selected: Option<&str>,
    projects: &[String],
    open: bool,
    p: Palette,
    cx: &mut Context<LLMeterView>,
) -> impl IntoElement {
    let label = selected
        .map(str::to_owned)
        .unwrap_or_else(|| t!("sessions.all_projects").to_string());
    v_flex()
        .relative()
        .child(
            Button::new("session-project-filter")
                .icon(IconName::Folder)
                .label(label.clone())
                .compact()
                .on_click(cx.listener(|view, _, _, cx| view.toggle_session_projects(cx))),
        )
        .when(open, |this| {
            let mut menu = v_flex()
                .absolute()
                .top_full()
                .left_0()
                .mt_1()
                .min_w(px(180.0))
                .max_h(px(280.0))
                .id("session-project-menu")
                .overflow_y_scroll()
                // Keep wheel events inside the nested project picker. The session list is
                // another scroll container underneath the popover and would otherwise also
                // receive the bubbled event.
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .rounded_lg()
                .border_1()
                .border_color(p.border)
                .bg(p.popover)
                .shadow_sm()
                .p_1()
                .occlude()
                .child(project_menu_item(
                    t!("sessions.all_projects").as_ref(),
                    selected.is_none(),
                    None,
                    cx,
                ));
            for project in projects.iter().take(24) {
                menu = menu.child(project_menu_item(
                    project,
                    selected == Some(project.as_str()),
                    Some(project.clone()),
                    cx,
                ));
            }
            // This menu overlaps the virtualized session rows. Defer its paint so it is
            // rendered above the rows instead of being covered by later siblings in the page.
            this.child(deferred(menu).with_priority(1))
        })
}

fn project_menu_item(
    label: &str,
    selected: bool,
    value: Option<String>,
    cx: &mut Context<LLMeterView>,
) -> impl IntoElement {
    let label = label.to_string();
    Button::new(SharedString::from(format!("session-project-item-{label}")))
        .ghost()
        .compact()
        .w_full()
        .justify_start()
        .selected(selected)
        // Button centers its built-in label. Use a full-width child so the
        // label itself can be aligned to the left inside the menu item.
        .child(div().w_full().text_left().child(label))
        .on_click(cx.listener(move |view, _, _, cx| {
            view.set_session_project(value.clone(), cx);
        }))
}

fn session_row(
    session: SessionSummary,
    copied: bool,
    first: bool,
    p: Palette,
    cx: &mut Context<LLMeterView>,
) -> AnyElement {
    let title = {
        let title = session.title();
        if title.trim().is_empty() {
            t!("sessions.untitled").to_string()
        } else {
            title
        }
    };
    let model = session
        .model
        .clone()
        .unwrap_or_else(|| t!("sessions.unknown_model").to_string());
    let project = session.project_label();
    let meta = session_meta(&session, &model, project.as_deref());
    let command = session.resume_command();
    let one_shot = session.is_one_shot();
    let provider = session.provider;
    let total_tokens = session.total_tokens;
    let estimated_cost_usd = session.estimated_cost_usd;
    let turn_count = session.turn_count;
    let mono_font = cx.theme().mono_font_family.clone();
    let owned = session;

    h_flex()
        .id(SharedString::from(format!(
            "session-row-{}",
            crate::app::session_key(&owned)
        )))
        .w_full()
        .items_center()
        .justify_between()
        .h(px(78.0))
        .gap_4()
        .px_2()
        .py_3()
        .rounded_lg()
        .hover(|style| style.bg(p.muted.opacity(0.42)))
        .when(!first, |this| this.border_t_1().border_color(p.border))
        .child(
            h_flex()
                .min_w(px(0.0))
                .flex_1()
                .items_start()
                .gap_3()
                .child(provider_logo(provider, 28.0))
                .child(
                    v_flex()
                        .min_w(px(0.0))
                        .gap_1()
                        .child(
                            h_flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .truncate()
                                        .text_base()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(p.foreground)
                                        .child(title),
                                )
                                .when(one_shot, |this| this.child(one_shot_badge(p))),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_sm()
                                .text_color(p.muted_foreground)
                                .child(meta),
                        ),
                ),
        )
        .child(
            h_flex()
                .flex_shrink_0()
                .items_center()
                .gap_3()
                .child(metric_cell(
                    format_tokens(total_tokens),
                    t!("sessions.tokens").to_string(),
                    78.0,
                    mono_font.clone(),
                    p,
                ))
                .child(metric_cell(
                    format_cost(estimated_cost_usd),
                    t!("sessions.cost").to_string(),
                    72.0,
                    mono_font.clone(),
                    p,
                ))
                .child(metric_cell(
                    turn_count.to_string(),
                    t!("sessions.turns").to_string(),
                    50.0,
                    mono_font,
                    p,
                ))
                .child(copy_button(command.is_some(), copied, owned, cx)),
        )
        .into_any_element()
}

fn session_meta(session: &SessionSummary, model: &str, project: Option<&str>) -> String {
    let timestamp = format_session_time(session.started_at);
    let duration = format_duration(session.duration_secs());
    match project {
        Some(project) if project != session.title() => {
            format!("{project}  ·  {model}  ·  {timestamp}  ·  {duration}")
        }
        _ => format!("{model}  ·  {timestamp}  ·  {duration}"),
    }
}

fn one_shot_badge(p: Palette) -> impl IntoElement {
    div()
        .rounded_full()
        .bg(p.success.opacity(0.15))
        .px_2()
        .py_0p5()
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .text_color(p.success)
        .child(t!("sessions.one_shot").to_string())
}

fn metric_cell(
    value: String,
    label: String,
    width: f32,
    mono_font: SharedString,
    p: Palette,
) -> impl IntoElement {
    v_flex()
        .w(px(width))
        .items_center()
        .child(
            div()
                .text_base()
                .font_family(mono_font)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.foreground)
                .child(value),
        )
        .child(
            div()
                .pt_0p5()
                .text_xs()
                .text_color(p.muted_foreground)
                .child(label),
        )
}

fn copy_button(
    available: bool,
    copied: bool,
    session: SessionSummary,
    cx: &mut Context<LLMeterView>,
) -> impl IntoElement {
    let label = if !available {
        t!("sessions.no_command").to_string()
    } else if copied {
        t!("sessions.copied").to_string()
    } else {
        t!("sessions.copy_command").to_string()
    };
    Button::new(SharedString::from(format!(
        "copy-session-{}",
        crate::app::session_key(&session)
    )))
    .ghost()
    .compact()
    .w(px(110.0))
    .justify_end()
    .icon(if copied {
        IconName::Check
    } else {
        IconName::SquareTerminal
    })
    .label(label)
    .disabled(!available)
    .on_click(cx.listener(move |view, _, _, cx| {
        view.copy_resume_command(&session, cx);
    }))
}

fn empty_state(text: String, p: Palette) -> impl IntoElement {
    div()
        .pt_16()
        .text_sm()
        .text_color(p.muted_foreground)
        .child(text)
}

fn format_tokens(value: u64) -> String {
    if value >= 1_000_000 {
        trim_decimal(value as f64 / 1_000_000.0, "M")
    } else if value >= 1_000 {
        trim_decimal(value as f64 / 1_000.0, "K")
    } else {
        value.to_string()
    }
}

fn trim_decimal(value: f64, suffix: &str) -> String {
    let text = format!("{value:.1}");
    if text.ends_with(".0") {
        format!("{}{suffix}", &text[..text.len() - 2])
    } else {
        format!("{text}{suffix}")
    }
}

fn format_cost(value: Option<f64>) -> String {
    value
        .map(|value| format!("${value:.2}"))
        .unwrap_or_else(|| "$0.00".into())
}

fn format_session_time(timestamp: chrono::DateTime<chrono::Utc>) -> String {
    let local = timestamp.with_timezone(&Local);
    t!(
        "sessions.date_time",
        year = local.year(),
        month = local.month(),
        day = local.day(),
        hour = format!("{:02}", local.hour()),
        minute = format!("{:02}", local.minute())
    )
    .to_string()
}

fn format_duration(seconds: i64) -> String {
    if seconds < 60 {
        t!("sessions.seconds", count = seconds.max(0)).to_string()
    } else if seconds < 3600 {
        t!("sessions.minutes", count = seconds / 60).to_string()
    } else {
        let hours = seconds / 3600;
        let minutes = (seconds % 3600) / 60;
        if minutes == 0 {
            t!("sessions.hours", count = hours).to_string()
        } else {
            t!("sessions.hours_minutes", hours = hours, minutes = minutes).to_string()
        }
    }
}
