use std::rc::Rc;

use chrono::{Datelike, Duration, Local, Timelike};
use gpui::{
    AnyElement, Context, Entity, FontWeight, InteractiveElement, IntoElement, ParentElement,
    Pixels, Render, SharedString, Size, Window, deferred, div, prelude::*, px, size,
};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, Selectable, Sizable, VirtualListScrollHandle,
    button::{Button, ButtonGroup, ButtonVariants},
    h_flex,
    input::Input,
    sheet::Sheet,
    v_flex, v_virtual_list,
};
use llmeter_collector::{SessionTranscript, TranscriptMessage, TranscriptRole};
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
    const ALL: [Self; 12] = [
        Self::All,
        Self::Provider(Provider::Claude),
        Self::Provider(Provider::Codex),
        Self::Provider(Provider::Cursor),
        Self::Provider(Provider::Qoder),
        Self::Provider(Provider::Trae),
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
    let provider_open = view.session_provider_open;
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
                        Some(session_row(view, session, copied, position == 0, p, cx))
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
        .px_6()
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
                                .text_xl()
                                .font_weight(FontWeight::SEMIBOLD)
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
                .child(provider_filter(view.session_provider, provider_open, p, cx))
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
    open: bool,
    p: Palette,
    cx: &mut Context<LLMeterView>,
) -> impl IntoElement {
    v_flex()
        .relative()
        .child(
            Button::new("session-provider-filter")
                .icon(IconName::Bot)
                .label(selected.label())
                .compact()
                .on_click(cx.listener(|view, _, _, cx| view.toggle_session_providers(cx))),
        )
        .when(open, |this| {
            let mut menu = v_flex()
                .absolute()
                .top_full()
                .left_0()
                .mt_1()
                .min_w(px(180.0))
                .max_h(px(320.0))
                .id("session-provider-menu")
                .overflow_y_scroll()
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .rounded_lg()
                .border_1()
                .border_color(p.border)
                .bg(p.popover)
                .shadow_sm()
                .p_1()
                .occlude();
            for (index, filter) in SessionProviderFilter::ALL.into_iter().enumerate() {
                menu = menu.child(provider_menu_item(filter, selected == filter, index, p, cx));
            }
            this.child(deferred(menu).with_priority(1))
        })
}

fn provider_menu_item(
    filter: SessionProviderFilter,
    selected: bool,
    index: usize,
    p: Palette,
    cx: &mut Context<LLMeterView>,
) -> impl IntoElement {
    let label = filter.label();
    Button::new(("session-provider-item", index))
        .ghost()
        .compact()
        .w_full()
        .justify_start()
        .selected(selected)
        .child(
            h_flex()
                .w_full()
                .items_center()
                .gap_2()
                .when_some(
                    match filter {
                        SessionProviderFilter::Provider(provider) => Some(provider),
                        SessionProviderFilter::All => None,
                    },
                    |this, provider| this.child(provider_logo(provider, 14.0)),
                )
                .child(
                    div()
                        .flex_1()
                        .text_left()
                        .text_color(if selected {
                            p.foreground
                        } else {
                            p.muted_foreground
                        })
                        .child(label),
                ),
        )
        .on_click(cx.listener(move |view, _, _, cx| {
            view.set_session_provider(filter, cx);
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
    view: &LLMeterView,
    session: SessionSummary,
    copied: bool,
    first: bool,
    p: Palette,
    cx: &mut Context<LLMeterView>,
) -> AnyElement {
    let title = session_display_title(&session);
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
    let detail_session = owned.clone();
    let row_id = format!("session-row-{}", crate::app::session_key(&owned));

    h_flex()
        .id(SharedString::from(row_id.clone()))
        .debug_selector(move || row_id)
        .w_full()
        .cursor_pointer()
        .on_click(cx.listener(move |view, _, window, cx| {
            view.show_session_detail(detail_session.clone(), window, cx);
        }))
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
                    view.format_cost(estimated_cost_usd),
                    t!("sessions.cost").to_string(),
                    144.0,
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

#[derive(Clone, Debug)]
enum TranscriptLoadState {
    Loading,
    Loaded(SessionTranscript),
    Failed(String),
}

pub(crate) struct SessionDetailView {
    palette: Palette,
    transcript: TranscriptLoadState,
    transcript_item_sizes: Rc<Vec<Size<Pixels>>>,
    transcript_scroll: VirtualListScrollHandle,
}

impl SessionDetailView {
    pub(crate) fn new(palette: Palette) -> Self {
        Self {
            palette,
            transcript: TranscriptLoadState::Loading,
            transcript_item_sizes: Rc::new(Vec::new()),
            transcript_scroll: VirtualListScrollHandle::new(),
        }
    }

    pub(crate) fn set_transcript(
        &mut self,
        transcript: Result<SessionTranscript, String>,
        cx: &mut Context<Self>,
    ) {
        self.transcript = match transcript {
            Ok(transcript) => {
                self.transcript_item_sizes = Rc::new(
                    transcript
                        .messages
                        .iter()
                        .map(estimated_transcript_message_size)
                        .collect(),
                );
                TranscriptLoadState::Loaded(transcript)
            }
            Err(error) => {
                self.transcript_item_sizes = Rc::new(Vec::new());
                TranscriptLoadState::Failed(error)
            }
        };
        cx.notify();
    }
}

impl Render for SessionDetailView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        session_detail_content(self, cx.entity())
    }
}

pub(crate) fn session_detail_sheet(sheet: Sheet, detail: Entity<SessionDetailView>) -> Sheet {
    sheet
        .size(px(520.0))
        .title(t!("sessions.transcript_details").to_string())
        .child(detail)
}

fn session_detail_content(
    detail_view: &SessionDetailView,
    detail: Entity<SessionDetailView>,
) -> AnyElement {
    v_flex()
        .debug_selector(|| "session-detail-content".to_string())
        .w_full()
        .gap_3()
        .pb_6()
        .child(transcript_section(
            &detail_view.transcript,
            detail_view.transcript_item_sizes.clone(),
            detail_view.transcript_scroll.clone(),
            detail,
            detail_view.palette,
        ))
        .into_any_element()
}

fn transcript_section(
    state: &TranscriptLoadState,
    item_sizes: Rc<Vec<Size<Pixels>>>,
    scroll: VirtualListScrollHandle,
    detail: Entity<SessionDetailView>,
    p: Palette,
) -> impl IntoElement {
    let content: AnyElement = match state {
        TranscriptLoadState::Loading => div()
            .text_sm()
            .text_color(p.muted_foreground)
            .child(t!("sessions.transcript_loading").to_string())
            .into_any_element(),
        TranscriptLoadState::Failed(error) => v_flex()
            .gap_1()
            .child(
                div()
                    .text_sm()
                    .text_color(p.muted_foreground)
                    .child(t!("sessions.transcript_unavailable").to_string()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(p.muted_foreground)
                    .child(error.clone()),
            )
            .into_any_element(),
        TranscriptLoadState::Loaded(transcript) => {
            if transcript.messages.is_empty() {
                div()
                    .text_sm()
                    .text_color(p.muted_foreground)
                    .child(t!("sessions.transcript_empty").to_string())
                    .into_any_element()
            } else {
                let messages = v_virtual_list(
                    detail,
                    "session-transcript-items",
                    item_sizes,
                    move |detail, visible_range, _, _| {
                        let TranscriptLoadState::Loaded(transcript) = &detail.transcript else {
                            return Vec::new();
                        };
                        visible_range
                            .filter_map(|index| {
                                transcript
                                    .messages
                                    .get(index)
                                    .map(|message| transcript_message(message, p))
                            })
                            .collect()
                    },
                )
                .track_scroll(&scroll)
                .h(px(480.0))
                .w_full()
                .gap_3();

                let mut content = v_flex().gap_2().child(messages);
                if transcript.truncated {
                    content = content.child(
                        div()
                            .text_xs()
                            .text_color(p.muted_foreground)
                            .child(t!("sessions.transcript_truncated").to_string()),
                    );
                }
                content.into_any_element()
            }
        }
    };

    div()
        .w_full()
        .rounded_lg()
        .border_1()
        .border_color(p.border.opacity(0.7))
        .bg(p.tiles)
        .px_3()
        .py_3()
        .child(content)
}

fn estimated_transcript_message_size(message: &TranscriptMessage) -> Size<Pixels> {
    const CHARS_PER_LINE: usize = 52;
    const LINE_HEIGHT: f32 = 20.0;
    const FIXED_HEIGHT: f32 = 56.0;

    let lines = message
        .content
        .lines()
        .map(|line| {
            let width = line
                .chars()
                .map(|character| if character.is_ascii() { 1 } else { 2 })
                .sum::<usize>();
            width.max(1).div_ceil(CHARS_PER_LINE)
        })
        .sum::<usize>()
        .max(1);
    size(px(1.0), px(FIXED_HEIGHT + lines as f32 * LINE_HEIGHT))
}

fn transcript_message(message: &TranscriptMessage, p: Palette) -> AnyElement {
    let (label, foreground, background) = match message.role {
        TranscriptRole::User => (
            t!("sessions.transcript_user").to_string(),
            p.link,
            p.link.opacity(0.1),
        ),
        TranscriptRole::Assistant => (
            t!("sessions.transcript_assistant").to_string(),
            p.success,
            p.success.opacity(0.1),
        ),
        TranscriptRole::Thinking => (
            t!("sessions.transcript_thinking").to_string(),
            p.muted_foreground,
            p.muted.opacity(0.42),
        ),
        TranscriptRole::Tool => (
            t!("sessions.transcript_tool").to_string(),
            p.foreground,
            p.accent.opacity(0.28),
        ),
    };

    let timestamp = message.timestamp.map(format_session_time);
    v_flex()
        .gap_2()
        .rounded_lg()
        .border_1()
        .border_color(p.border.opacity(0.68))
        .bg(background)
        .px_3()
        .py_3()
        .child(
            h_flex()
                .items_center()
                .justify_between()
                .gap_2()
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(foreground)
                        .child(label),
                )
                .when_some(timestamp, |this, timestamp| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(p.muted_foreground)
                            .child(timestamp),
                    )
                }),
        )
        .child(
            div()
                .whitespace_normal()
                .text_sm()
                .text_color(p.foreground)
                .child(message.content.clone()),
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
        .flex_shrink_0()
        .items_center()
        .child(
            div()
                .whitespace_nowrap()
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
        cx.stop_propagation();
        view.copy_resume_command(&session, cx);
    }))
}

fn session_display_title(session: &SessionSummary) -> String {
    let title = session.title();
    if title.trim().is_empty() {
        t!("sessions.untitled").to_string()
    } else {
        title
    }
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
