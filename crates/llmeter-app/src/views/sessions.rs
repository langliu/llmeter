use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::Arc,
};

use chrono::{Datelike, Duration, Local, Timelike};
use gpui::{
    AnyElement, Bounds, Context, Entity, FocusHandle, FontWeight, HighlightStyle, Image,
    ImageFormat, InteractiveElement, IntoElement, ListAlignment, ListState, ObjectFit,
    ParentElement, Render, ScrollHandle, SharedString, StyledImage, Window, WindowBounds,
    WindowOptions, deferred, div, img, list, prelude::*, px, rems, size,
};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, Selectable, Sizable,
    button::{Button, ButtonGroup, ButtonVariants},
    h_flex,
    highlighter::HighlightTheme,
    input::Input,
    scroll::Scrollbar,
    sheet::Sheet,
    text::{TextView, TextViewStyle},
    v_flex, v_virtual_list,
};
use llmeter_collector::{SessionTranscript, TranscriptMessage, TranscriptPhase, TranscriptRole};
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
    let session_indices = view.visible_session_indices(cx);
    let visible_count = session_indices.len();
    let total_count = view.snapshot.sessions.len();
    let providers = view.session_providers();
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
                                .child(t!("sessions.title")),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(p.muted_foreground)
                                .child(t!("sessions.subtitle")),
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
                .child(provider_filter(
                    view.session_provider,
                    &providers,
                    provider_open,
                    p,
                    cx,
                ))
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
    available_providers: &[Provider],
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
            let mut filters = vec![SessionProviderFilter::All];
            for provider in available_providers {
                filters.push(SessionProviderFilter::Provider(*provider));
            }
            if let SessionProviderFilter::Provider(active) = selected
                && !available_providers.contains(&active)
            {
                filters.push(selected);
            }
            for (index, filter) in filters.into_iter().enumerate() {
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

/// A user message, final reply, or a whole turn's collapsible work history.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TranscriptRow {
    Message(usize),
    Steps {
        /// Inclusive index of the first work message.
        start: usize,
        /// Exclusive index past the last work message.
        end: usize,
    },
}

fn transcript_rows(messages: &[TranscriptMessage]) -> Vec<TranscriptRow> {
    let mut rows = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        if messages[index].role == TranscriptRole::User {
            rows.push(TranscriptRow::Message(index));
            index += 1;
            continue;
        }

        let start = index;
        while index < messages.len() && messages[index].role != TranscriptRole::User {
            index += 1;
        }
        let end = index;
        let has_work = messages[start..end].iter().any(|message| {
            message.phase == TranscriptPhase::Progress
                || matches!(
                    message.role,
                    TranscriptRole::Thinking | TranscriptRole::Tool
                )
        });
        if !has_work {
            rows.extend((start..end).map(TranscriptRow::Message));
            continue;
        }

        // Prefer the provider's explicit phase; older formats fall back to
        // a trailing assistant reply. Incomplete work remains together.
        let explicit_final =
            (start..end).find(|&index| messages[index].phase == TranscriptPhase::Final);
        let work_end = if let Some(index) = explicit_final {
            index
        } else if messages[end - 1].role == TranscriptRole::Assistant
            && messages[end - 1].phase != TranscriptPhase::Progress
        {
            end - 1
        } else {
            end
        };
        rows.push(TranscriptRow::Steps {
            start,
            end: work_end,
        });
        rows.extend((work_end..end).map(TranscriptRow::Message));
    }
    rows
}

/// Reuse encoded image objects across layout passes and fullscreen previews.
#[derive(Default)]
struct TranscriptImageCache(RefCell<HashMap<(usize, usize), Arc<Image>>>);

impl TranscriptImageCache {
    fn image(
        &self,
        message: usize,
        attachment: usize,
        format: ImageFormat,
        data: &[u8],
    ) -> Arc<Image> {
        self.0
            .borrow_mut()
            .entry((message, attachment))
            .or_insert_with(|| Arc::new(Image::from_bytes(format, data.to_vec())))
            .clone()
    }
}

pub(crate) struct SessionDetailView {
    palette: Palette,
    transcript: TranscriptLoadState,
    transcript_rows: Rc<Vec<TranscriptRow>>,
    /// Step-group start indices whose detail block the user expanded.
    expanded_transcript_items: HashSet<usize>,
    transcript_list: ListState,
    expanded_thinking: HashSet<usize>,
    thinking_scroll: HashMap<usize, ScrollHandle>,
    image_cache: TranscriptImageCache,
}

impl SessionDetailView {
    pub(crate) fn new(palette: Palette) -> Self {
        Self {
            palette,
            transcript: TranscriptLoadState::Loading,
            transcript_rows: Rc::new(Vec::new()),
            expanded_transcript_items: HashSet::new(),
            transcript_list: ListState::new(0, ListAlignment::Top, px(400.0)),
            expanded_thinking: HashSet::new(),
            thinking_scroll: HashMap::new(),
            image_cache: TranscriptImageCache::default(),
        }
    }

    pub(crate) fn set_transcript(
        &mut self,
        transcript: Result<SessionTranscript, String>,
        cx: &mut Context<Self>,
    ) {
        self.image_cache.0.borrow_mut().clear();
        self.transcript = match transcript {
            Ok(transcript) => {
                self.expanded_transcript_items.clear();
                self.expanded_thinking.clear();
                self.thinking_scroll.clear();
                self.transcript_rows = Rc::new(transcript_rows(&transcript.messages));
                self.transcript_list.reset(self.transcript_rows.len());
                TranscriptLoadState::Loaded(transcript)
            }
            Err(error) => {
                self.transcript_rows = Rc::new(Vec::new());
                self.transcript_list.reset(0);
                TranscriptLoadState::Failed(error)
            }
        };
        cx.notify();
    }

    fn toggle_thinking(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.expanded_thinking.remove(&index) {
            self.expanded_thinking.insert(index);
            self.thinking_scroll.entry(index).or_default();
        }
        if let Some(row_index) = self.transcript_rows.iter().position(|row| {
            matches!(row, TranscriptRow::Steps { start, end } if (*start..*end).contains(&index))
        }) {
            self.transcript_list.splice(row_index..row_index + 1, 1);
        }
        cx.notify();
    }

    fn toggle_transcript_item(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.expanded_transcript_items.remove(&index) {
            self.expanded_transcript_items.insert(index);
        }
        // Invalidate only this group's measured height, preserving the scroll position.
        if let Some(row_index) = self
            .transcript_rows
            .iter()
            .position(|row| row_key(row) == index)
        {
            self.transcript_list.splice(row_index..row_index + 1, 1);
        }
        cx.notify();
    }
}

/// Expansion key of a row; only step groups are toggleable.
fn row_key(row: &TranscriptRow) -> usize {
    match row {
        TranscriptRow::Message(index) => *index,
        TranscriptRow::Steps { start, .. } => *start,
    }
}

impl Render for SessionDetailView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        session_detail_content(self, cx.entity())
    }
}

pub(crate) fn session_detail_sheet(sheet: Sheet, detail: Entity<SessionDetailView>) -> Sheet {
    sheet
        .size(px(680.0))
        .title(t!("sessions.transcript_details").to_string())
        .child(detail)
}

fn session_detail_content(
    detail_view: &SessionDetailView,
    detail: Entity<SessionDetailView>,
) -> AnyElement {
    v_flex()
        .debug_selector(|| "session-detail-content".to_string())
        .size_full()
        .gap_3()
        .pb_4()
        .child(transcript_section(
            &detail_view.transcript,
            detail_view.transcript_list.clone(),
            detail,
            detail_view.palette,
        ))
        .into_any_element()
}

fn transcript_section(
    state: &TranscriptLoadState,
    list_state: ListState,
    detail: Entity<SessionDetailView>,
    p: Palette,
) -> impl IntoElement {
    let content: AnyElement = match state {
        TranscriptLoadState::Loading => div()
            .text_sm()
            .text_color(p.muted_foreground)
            .child(t!("sessions.transcript_loading"))
            .into_any_element(),
        TranscriptLoadState::Failed(error) => v_flex()
            .gap_1()
            .child(
                div()
                    .text_sm()
                    .text_color(p.muted_foreground)
                    .child(t!("sessions.transcript_unavailable")),
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
                    .child(t!("sessions.transcript_empty"))
                    .into_any_element()
            } else {
                let messages = list(list_state, move |row_index, _, cx| {
                    detail.update(cx, |detail, cx| {
                        let TranscriptLoadState::Loaded(transcript) = &detail.transcript else {
                            return div().into_any_element();
                        };
                        let row = &detail.transcript_rows[row_index];
                        let content = match row {
                            TranscriptRow::Message(index) => transcript_message(
                                &transcript.messages[*index],
                                *index,
                                p,
                                &detail.image_cache,
                            ),
                            TranscriptRow::Steps { start, end } => collapsible_steps(
                                &transcript.messages[*start..*end],
                                *start,
                                detail.expanded_transcript_items.contains(start),
                                &detail.expanded_thinking,
                                &detail.thinking_scroll,
                                &detail.image_cache,
                                p,
                                cx,
                            ),
                        };
                        // List measures the rendered row, including Markdown and spacing.
                        div().w_full().pb_3().child(content).into_any_element()
                    })
                })
                .size_full();

                let mut content = v_flex().size_full().gap_2().child(messages);
                if transcript.truncated {
                    content = content.child(
                        div()
                            .text_xs()
                            .text_color(p.muted_foreground)
                            .child(t!("sessions.transcript_truncated")),
                    );
                }
                content.into_any_element()
            }
        }
    };

    div().size_full().px_3().py_3().child(content)
}

/// Codex-style conversation rendering: user messages sit in a right-aligned
/// bubble, assistant text flows full width as rendered Markdown without a
/// card, and each run of thinking/tool steps collapses to one hairline-topped
/// row that expands in place.
fn transcript_message(
    message: &TranscriptMessage,
    index: usize,
    p: Palette,
    image_cache: &TranscriptImageCache,
) -> AnyElement {
    match message.role {
        TranscriptRole::User => user_bubble(message, index, p, image_cache),
        // Step messages reach the renderer only through a Steps row; keep a
        // safe fallback in case grouping ever misses one.
        TranscriptRole::Assistant | TranscriptRole::Thinking | TranscriptRole::Tool => {
            assistant_text(message, index, p, image_cache)
        }
    }
}

fn user_bubble(
    message: &TranscriptMessage,
    index: usize,
    p: Palette,
    image_cache: &TranscriptImageCache,
) -> AnyElement {
    // A foreground-tinted ground keeps the bubble clearly elevated on both
    // themes; muted is barely distinguishable from the sheet background.
    let ground = p.foreground.opacity(if p.is_dark { 0.14 } else { 0.08 });
    v_flex()
        .debug_selector(|| "transcript-user".to_string())
        .w_full()
        .items_end()
        .gap_2()
        .child(transcript_images(message, index, p, image_cache))
        .when(!message.content.is_empty(), |this| {
            this.child(
                div()
                    .max_w(px(560.0))
                    .rounded_2xl()
                    .bg(ground)
                    .px_3p5()
                    .py_2p5()
                    .text_sm()
                    .text_color(p.foreground)
                    .child(div().whitespace_normal().child(message.content.clone())),
            )
        })
        .into_any_element()
}

fn transcript_images(
    message: &TranscriptMessage,
    index: usize,
    p: Palette,
    image_cache: &TranscriptImageCache,
) -> AnyElement {
    // Scope attachment indices by message, including expanded work steps.
    let mut images = h_flex()
        .id(("transcript-images", index))
        .flex_wrap()
        .justify_end()
        .gap_2()
        .max_w_full();
    let gallery: Arc<[Arc<Image>]> = message
        .images
        .iter()
        .enumerate()
        .filter_map(|(attachment, image)| {
            Some(image_cache.image(
                index,
                attachment,
                ImageFormat::from_mime_type(&image.mime)?,
                image.data.as_ref()?,
            ))
        })
        .collect();
    let mut gallery_index = 0;
    for (image_index, image) in message.images.iter().enumerate() {
        if image.data.is_some() && ImageFormat::from_mime_type(&image.mime).is_some() {
            let thumbnail = gallery[gallery_index].clone();
            let selected = gallery_index;
            gallery_index += 1;
            let gallery = gallery.clone();
            images = images.child(
                div()
                    .id(("transcript-image", image_index))
                    .cursor_pointer()
                    .on_click(move |_, _, cx| {
                        let images = gallery.clone();
                        let bounds = Bounds::centered(None, size(px(1000.0), px(760.0)), cx);
                        if let Err(error) = cx.open_window(
                            WindowOptions {
                                window_bounds: Some(WindowBounds::Fullscreen(bounds)),
                                ..Default::default()
                            },
                            move |window, cx| {
                                cx.new(|cx| {
                                    let focus = cx.focus_handle();
                                    window.focus(&focus, cx);
                                    ImagePreview {
                                        images,
                                        selected,
                                        focus,
                                    }
                                })
                            },
                        ) {
                            tracing::warn!(%error, "could not open image preview");
                        }
                    })
                    .debug_selector(move || format!("transcript-image-{index}-{image_index}"))
                    .w(px(112.0))
                    .max_w_full()
                    .h(px(112.0))
                    .rounded_lg()
                    .overflow_hidden()
                    .child(
                        img(thumbnail)
                            .size_full()
                            .object_fit(ObjectFit::Cover)
                            .rounded_lg(),
                    ),
            );
        } else {
            images = images.child(
                div()
                    .text_xs()
                    .text_color(p.muted_foreground)
                    .child(t!("sessions.transcript_image_unavailable").to_string()),
            );
        }
    }
    images.into_any_element()
}

struct ImagePreview {
    images: Arc<[Arc<Image>]>,
    selected: usize,
    focus: FocusHandle,
}

impl ImagePreview {
    fn previous(&mut self, cx: &mut Context<Self>) {
        self.selected = (self.selected + self.images.len() - 1) % self.images.len();
        cx.notify();
    }

    fn next(&mut self, cx: &mut Context<Self>) {
        self.selected = (self.selected + 1) % self.images.len();
        cx.notify();
    }
}

impl Render for ImagePreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .relative()
            .bg(gpui::rgb(0x101010))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|view, event: &gpui::KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "escape" => window.remove_window(),
                    "left" => view.previous(cx),
                    "right" => view.next(cx),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .child(
                img(self.images[self.selected].clone())
                    .size_full()
                    .object_fit(ObjectFit::Contain),
            )
            .child(
                div()
                    .id("close-image-preview")
                    .absolute()
                    .top_4()
                    .right_4()
                    .p_2()
                    .rounded_full()
                    .bg(gpui::rgb(0x303030))
                    .text_color(gpui::rgb(0xffffff))
                    .cursor_pointer()
                    .on_click(|_, window, _| window.remove_window())
                    .child(Icon::new(IconName::Close).size_5()),
            )
            .when(self.images.len() > 1, |this| {
                this.child(
                    h_flex()
                        .absolute()
                        .bottom_4()
                        .w_full()
                        .justify_center()
                        .child(
                            h_flex()
                                .gap_3()
                                .items_center()
                                .p_2()
                                .rounded_lg()
                                .bg(gpui::rgb(0x303030))
                                .text_color(gpui::rgb(0xffffff))
                                .child(
                                    div()
                                        .id("previous-preview-image")
                                        .debug_selector(|| "previous-preview-image".to_string())
                                        .p_1()
                                        .cursor_pointer()
                                        .on_click(cx.listener(|view, _, _, cx| view.previous(cx)))
                                        .child(Icon::new(IconName::ChevronLeft).size_5()),
                                )
                                .child(format!("{} / {}", self.selected + 1, self.images.len()))
                                .child(
                                    div()
                                        .id("next-preview-image")
                                        .debug_selector(|| "next-preview-image".to_string())
                                        .p_1()
                                        .cursor_pointer()
                                        .on_click(cx.listener(|view, _, _, cx| view.next(cx)))
                                        .child(Icon::new(IconName::ChevronRight).size_5()),
                                ),
                        ),
                )
            })
    }
}

fn assistant_text(
    message: &TranscriptMessage,
    index: usize,
    p: Palette,
    image_cache: &TranscriptImageCache,
) -> AnyElement {
    // The renderer defaults shout: 1.5rem headings, 1rem paragraph gaps, and
    // inline code tinted with the theme accent. Codex keeps everything
    // compact — headings barely above body size, neutral chips, tight
    // paragraphs — so rein all three in.
    let chip_ground = p.foreground.opacity(if p.is_dark { 0.12 } else { 0.08 });
    let style = TextViewStyle {
        is_dark: p.is_dark,
        highlight_theme: if p.is_dark {
            HighlightTheme::default_dark()
        } else {
            HighlightTheme::default_light()
        },
        ..Default::default()
    };
    let style = style
        .paragraph_gap(rems(0.5))
        .heading_font_size(|level, base| match level {
            1 => base + px(3.0),
            2 => base + px(2.0),
            _ => base + px(1.0),
        })
        .inline_code(HighlightStyle {
            background_color: Some(chip_ground),
            ..HighlightStyle::default()
        });
    // Assistant replies carry Markdown (bold, headings, inline code, lists);
    // render them richly like Codex instead of showing the raw source.
    div()
        .w_full()
        .debug_selector(move || format!("transcript-assistant-{index}"))
        .child(
            TextView::markdown(
                SharedString::from(format!("assistant-md-{index}")),
                &message.content,
            )
            .style(style)
            .w_full()
            .text_sm()
            .text_color(p.foreground),
        )
        .child(transcript_images(message, index, p, image_cache))
        .into_any_element()
}

/// One collapsed entry for a whole turn’s work messages: a hairline
/// separator, a duration summary line, and an expandable step list underneath.
fn collapsible_steps(
    steps: &[TranscriptMessage],
    key: usize,
    expanded: bool,
    expanded_thinking: &HashSet<usize>,
    thinking_scroll: &HashMap<usize, ScrollHandle>,
    image_cache: &TranscriptImageCache,
    p: Palette,
    cx: &mut Context<SessionDetailView>,
) -> AnyElement {
    let chevron = if expanded {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    };
    let mut block = v_flex()
        .debug_selector(move || format!("transcript-work-{key}"))
        .w_full()
        .gap_1()
        .child(div().mt_1().h(px(1.0)).w_full().bg(p.border.opacity(0.6)))
        .child(
            h_flex()
                .id(SharedString::from(format!("transcript-steps-{key}")))
                .w_full()
                .cursor_pointer()
                .items_center()
                .gap_1p5()
                .rounded_md()
                .px_1p5()
                .py_1()
                .hover(|style| style.bg(p.muted.opacity(0.4)))
                .on_click(cx.listener(move |detail, _, _, cx| {
                    detail.toggle_transcript_item(key, cx);
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(p.muted_foreground)
                        .child(steps_label(steps)),
                )
                .child(Icon::new(chevron).size_3().text_color(p.muted_foreground)),
        );
    if expanded {
        let mut list = v_flex().w_full().gap_2().pl_1p5();
        for (offset, message) in steps.iter().enumerate() {
            list = list.child(if message.role == TranscriptRole::Assistant {
                assistant_text(message, key + offset, p, image_cache)
            } else if message.role == TranscriptRole::Thinking {
                let index = key + offset;
                thinking_detail(
                    message,
                    index,
                    expanded_thinking.contains(&index),
                    thinking_scroll.get(&index).cloned().unwrap_or_default(),
                    p,
                    cx,
                )
            } else {
                step_detail(message, key + offset, p, image_cache)
            });
        }
        block = block.child(list);
    }
    block.into_any_element()
}

const THINKING_MAX_HEIGHT: f32 = 320.0;

fn thinking_detail(
    message: &TranscriptMessage,
    index: usize,
    expanded: bool,
    scroll: ScrollHandle,
    p: Palette,
    cx: &mut Context<SessionDetailView>,
) -> AnyElement {
    let mut block = v_flex().w_full().gap_1().child(
        h_flex()
            .id(SharedString::from(format!("thinking-toggle-{index}")))
            .cursor_pointer()
            .items_center()
            .gap_1p5()
            .py_1()
            .on_click(cx.listener(move |detail, _, _, cx| detail.toggle_thinking(index, cx)))
            .child(
                Icon::new(IconName::Cpu)
                    .size_3()
                    .text_color(p.muted_foreground),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(p.muted_foreground)
                    .child(t!("sessions.transcript_thinking").to_string()),
            )
            .child(
                Icon::new(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size_3()
                .text_color(p.muted_foreground),
            ),
    );
    if expanded {
        block = block.child(
            div()
                .relative()
                .w_full()
                .ml_1p5()
                .border_l_1()
                .border_color(p.border)
                .child(
                    div()
                        .id(SharedString::from(format!("thinking-scroll-{index}")))
                        .debug_selector(move || format!("thinking-content-{index}"))
                        .w_full()
                        .max_h(px(THINKING_MAX_HEIGHT))
                        .overflow_y_scroll()
                        .track_scroll(&scroll)
                        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                        .px_3()
                        .py_1()
                        .text_sm()
                        .text_color(p.muted_foreground)
                        .child(div().whitespace_normal().child(message.content.clone())),
                )
                .child(Scrollbar::vertical(&scroll)),
        );
    }
    block.into_any_element()
}

/// Expanded view of one message inside a step group: an icon plus its title,
/// with the raw content underneath.
fn step_detail(
    message: &TranscriptMessage,
    index: usize,
    p: Palette,
    image_cache: &TranscriptImageCache,
) -> AnyElement {
    let (icon, title) = match message.role {
        TranscriptRole::Thinking => (
            IconName::Cpu,
            t!("sessions.transcript_thinking").to_string(),
        ),
        _ => (IconName::SquareTerminal, tool_summary(message)),
    };
    v_flex()
        .w_full()
        .gap_0p5()
        .child(
            h_flex()
                .items_center()
                .gap_1p5()
                .child(Icon::new(icon).size_3().text_color(p.muted_foreground))
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_xs()
                        .text_color(p.muted_foreground)
                        .child(title),
                ),
        )
        .child(
            div()
                .pl_5()
                .whitespace_normal()
                .text_xs()
                .text_color(p.muted_foreground)
                .child(message.content.clone()),
        )
        .child(transcript_images(message, index, p, image_cache))
        .into_any_element()
}

/// Collapsed summary of a turn’s work: the wall-clock span between its first and
/// last message when timestamps are available, the lone step's own title for
/// single untimed steps, otherwise the step count.
fn steps_label(steps: &[TranscriptMessage]) -> String {
    let first = steps.iter().filter_map(|message| message.timestamp).min();
    let last = steps.iter().filter_map(|message| message.timestamp).max();
    if let (Some(first), Some(last)) = (first, last)
        && last > first
    {
        let seconds = (last - first).num_seconds();
        return t!(
            "sessions.transcript_worked",
            duration = work_duration(seconds)
        )
        .to_string();
    }
    if steps.len() == 1 {
        return match steps[0].role {
            TranscriptRole::Thinking => t!("sessions.transcript_thinking").to_string(),
            _ => tool_summary(&steps[0]),
        };
    }
    t!("sessions.transcript_steps", count = steps.len() as i64).to_string()
}

/// Compact wall-clock span ("6 分 46 秒") for step-run labels.
fn work_duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds < 60 {
        t!("sessions.seconds", count = seconds).to_string()
    } else if seconds < 3600 {
        let minutes = seconds / 60;
        let remainder = seconds % 60;
        if remainder == 0 {
            t!("sessions.minutes", count = minutes).to_string()
        } else {
            t!(
                "sessions.minutes_seconds",
                minutes = minutes,
                seconds = remainder
            )
            .to_string()
        }
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

fn tool_summary(message: &TranscriptMessage) -> String {
    let first_line = message
        .content
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim();
    let mut summary: String = first_line.chars().take(80).collect();
    if first_line.chars().count() > summary.chars().count() {
        summary.push('…');
    }
    if summary.is_empty() {
        t!("sessions.transcript_tool").to_string()
    } else {
        summary
    }
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
        .child(t!("sessions.one_shot"))
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

#[cfg(test)]
mod transcript_tests {
    use super::*;

    fn message(
        role: TranscriptRole,
        timestamp: Option<chrono::DateTime<chrono::Utc>>,
    ) -> TranscriptMessage {
        TranscriptMessage {
            role,
            content: "step content".into(),
            timestamp,
            images: Vec::new(),
            phase: TranscriptPhase::Unspecified,
        }
    }

    #[test]
    fn unfinished_turn_keeps_all_work_in_one_group() {
        let base = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let messages = vec![
            message(TranscriptRole::User, Some(base)),
            message(TranscriptRole::Thinking, Some(base)),
            message(TranscriptRole::Tool, Some(base)),
            message(TranscriptRole::Thinking, Some(base)),
            message(TranscriptRole::Assistant, Some(base)),
            message(TranscriptRole::Tool, Some(base)),
        ];
        let rows = transcript_rows(&messages);
        assert_eq!(
            rows,
            vec![
                TranscriptRow::Message(0),
                TranscriptRow::Steps { start: 1, end: 6 },
            ]
        );
    }

    #[test]
    fn turn_folds_interleaved_progress_and_preserves_final_reply() {
        let roles = [
            TranscriptRole::User,
            TranscriptRole::Thinking,
            TranscriptRole::Assistant,
            TranscriptRole::Tool,
            TranscriptRole::Thinking,
            TranscriptRole::Assistant,
            TranscriptRole::Tool,
            TranscriptRole::Assistant,
            TranscriptRole::User,
            TranscriptRole::Thinking,
            TranscriptRole::Assistant,
        ];
        let messages = roles
            .into_iter()
            .map(|role| message(role, None))
            .collect::<Vec<_>>();
        assert_eq!(
            transcript_rows(&messages),
            vec![
                TranscriptRow::Message(0),
                TranscriptRow::Steps { start: 1, end: 7 },
                TranscriptRow::Message(7),
                TranscriptRow::Message(8),
                TranscriptRow::Steps { start: 9, end: 10 },
                TranscriptRow::Message(10),
            ]
        );
    }

    #[test]
    fn explicit_final_parts_stay_visible_after_progress() {
        let mut progress = message(TranscriptRole::Assistant, None);
        progress.phase = TranscriptPhase::Progress;
        let mut final_part = message(TranscriptRole::Assistant, None);
        final_part.phase = TranscriptPhase::Final;
        let messages = vec![
            message(TranscriptRole::User, None),
            progress,
            final_part.clone(),
            final_part,
        ];
        assert_eq!(
            transcript_rows(&messages),
            vec![
                TranscriptRow::Message(0),
                TranscriptRow::Steps { start: 1, end: 2 },
                TranscriptRow::Message(2),
                TranscriptRow::Message(3)
            ]
        );
    }

    #[test]
    fn replies_without_work_events_remain_visible() {
        let messages = [
            TranscriptRole::User,
            TranscriptRole::Assistant,
            TranscriptRole::Assistant,
        ]
        .into_iter()
        .map(|role| message(role, None))
        .collect::<Vec<_>>();
        assert_eq!(
            transcript_rows(&messages),
            vec![
                TranscriptRow::Message(0),
                TranscriptRow::Message(1),
                TranscriptRow::Message(2),
            ]
        );
    }

    #[test]
    fn steps_label_reports_span_then_falls_back_to_count() {
        let start = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let end = start + chrono::Duration::seconds(406);
        let timed = vec![
            message(TranscriptRole::Thinking, Some(start)),
            message(TranscriptRole::Tool, Some(end)),
        ];
        let label = steps_label(&timed);
        assert!(label.contains('6') && label.contains('4'), "label: {label}");

        let untimed = vec![
            message(TranscriptRole::Thinking, None),
            message(TranscriptRole::Tool, None),
        ];
        let count_label = steps_label(&untimed);
        assert!(count_label.contains('2'), "label: {count_label}");
    }

    #[gpui::test]
    fn whole_turn_has_one_disclosure_and_expands_progress_in_place(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let palette = cx.update(|cx| Palette::from_app(cx));
        let (view, cx) = cx.add_window_view(|_, _| SessionDetailView::new(palette));
        let roles = [
            TranscriptRole::User,
            TranscriptRole::Thinking,
            TranscriptRole::Assistant,
            TranscriptRole::Tool,
            TranscriptRole::Thinking,
            TranscriptRole::Assistant,
            TranscriptRole::Thinking,
            TranscriptRole::Assistant,
        ];
        let transcript = SessionTranscript {
            messages: roles.into_iter().map(|role| message(role, None)).collect(),
            truncated: false,
        };
        view.update(cx, |detail, cx| detail.set_transcript(Ok(transcript), cx));
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        assert!(cx.debug_bounds("transcript-work-1").is_some());
        assert!(cx.debug_bounds("transcript-work-4").is_none());
        assert!(cx.debug_bounds("transcript-work-6").is_none());
        assert!(cx.debug_bounds("transcript-assistant-2").is_none());
        assert!(cx.debug_bounds("transcript-assistant-5").is_none());
        assert!(cx.debug_bounds("transcript-assistant-7").is_some());

        view.update(cx, |detail, cx| detail.toggle_transcript_item(1, cx));
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        assert!(cx.debug_bounds("transcript-assistant-2").is_some());
        assert!(cx.debug_bounds("transcript-assistant-5").is_some());
        assert!(cx.debug_bounds("transcript-assistant-7").is_some());

        view.update(cx, |detail, cx| detail.toggle_transcript_item(1, cx));
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        assert!(cx.debug_bounds("transcript-assistant-2").is_none());
        assert!(cx.debug_bounds("transcript-assistant-7").is_some());
    }

    #[gpui::test]
    fn thinking_is_collapsed_then_scrolls_with_a_height_limit(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let palette = cx.update(|cx| Palette::from_app(cx));
        let (view, cx) = cx.add_window_view(|_, _| SessionDetailView::new(palette));
        let transcript = SessionTranscript {
            messages: vec![
                message(TranscriptRole::User, None),
                TranscriptMessage {
                    role: TranscriptRole::Thinking,
                    content: "A long reasoning line that wraps within the transcript.\n"
                        .repeat(100),
                    timestamp: None,
                    images: Vec::new(),
                    phase: TranscriptPhase::Unspecified,
                },
                message(TranscriptRole::Assistant, None),
            ],
            truncated: false,
        };
        view.update(cx, |detail, cx| {
            detail.set_transcript(Ok(transcript), cx);
            detail.toggle_transcript_item(1, cx);
        });
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        assert!(
            cx.debug_bounds("thinking-content-1").is_none(),
            "thinking defaults to collapsed even inside expanded work"
        );
        view.update(cx, |detail, cx| detail.toggle_thinking(1, cx));
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        let bounds = cx
            .debug_bounds("thinking-content-1")
            .expect("thinking expands");
        assert_eq!(bounds.size.height, px(THINKING_MAX_HEIGHT));
        let scroll = view.update(cx, |detail, _| detail.thinking_scroll[&1].clone());
        assert!(
            scroll.max_offset().y > px(0.0),
            "long thinking content has an internal scroll range"
        );
        scroll.set_offset(gpui::point(px(0.0), -px(100.0)));
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        assert_eq!(scroll.offset().y, -px(100.0));
        view.update(cx, |detail, cx| detail.toggle_thinking(1, cx));
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        assert!(cx.debug_bounds("thinking-content-1").is_none());
        assert!(cx.debug_bounds("transcript-assistant-2").is_some());
    }

    #[gpui::test]
    fn attachment_objects_are_reused_and_cleared_for_new_transcripts(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let palette = cx.update(|cx| Palette::from_app(cx));
        let view = cx.new(|_| SessionDetailView::new(palette));
        view.update(cx, |detail, cx| {
            let first = detail.image_cache.image(0, 0, ImageFormat::Png, b"first");
            let repeated = detail.image_cache.image(0, 0, ImageFormat::Png, b"first");
            assert!(Arc::ptr_eq(&first, &repeated));
            detail.set_transcript(Ok(SessionTranscript::default()), cx);
            let replacement = detail
                .image_cache
                .image(0, 0, ImageFormat::Png, b"replacement");
            assert!(!Arc::ptr_eq(&first, &replacement));
            assert_eq!(replacement.bytes, b"replacement");
        });
    }

    #[gpui::test]
    fn image_only_user_message_renders_attachment(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let palette = cx.update(|cx| Palette::from_app(cx));
        let (view, cx) = cx.add_window_view(|_, _| SessionDetailView::new(palette));
        let transcript = SessionTranscript {
            messages: vec![TranscriptMessage {
                phase: TranscriptPhase::Unspecified,
                role: TranscriptRole::User,
                content: String::new(),
                timestamp: None,
                images: vec![llmeter_collector::TranscriptImage {
                    source: "fixture".into(),
                    mime: "image/png".into(),
                    data: Some(Arc::from(&include_bytes!("../../assets/AppIcon.png")[..])),
                }],
            }],
            truncated: false,
        };
        view.update(cx, |detail, cx| detail.set_transcript(Ok(transcript), cx));
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        let bounds = cx
            .debug_bounds("transcript-image-0-0")
            .expect("image-only message has a visible attachment");
        assert_eq!(bounds.size.height, px(112.0));
        assert_eq!(bounds.size.width, px(112.0));
    }

    #[gpui::test]
    fn image_preview_navigates_with_keys_buttons_and_closes_with_escape(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let initial_windows = cx.update(|cx| cx.windows().len());
        let image = Arc::new(Image::from_bytes(
            ImageFormat::Png,
            include_bytes!("../../assets/AppIcon.png").to_vec(),
        ));
        let (view, visual) = cx.add_window_view(|window, cx| {
            let focus = cx.focus_handle();
            window.focus(&focus, cx);
            ImagePreview {
                images: Arc::from([image.clone(), image]),
                selected: 1,
                focus,
            }
        });
        visual.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        visual.simulate_keystrokes("left");
        view.update(visual, |view, _| assert_eq!(view.selected, 0));
        visual.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        let bounds = visual.debug_bounds("next-preview-image").unwrap();
        visual.simulate_click(bounds.center(), gpui::Modifiers::default());
        view.update(visual, |view, _| assert_eq!(view.selected, 1));
        visual.simulate_keystrokes("right");
        view.update(visual, |view, _| assert_eq!(view.selected, 0));
        visual.simulate_keystrokes("escape");
        assert_eq!(cx.update(|cx| cx.windows().len()), initial_windows);
    }

    #[gpui::test]
    fn images_in_different_messages_open_independent_previews(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let palette = cx.update(|cx| Palette::from_app(cx));
        let (view, cx) = cx.add_window_view(|_, _| SessionDetailView::new(palette));
        let mut image_message = message(TranscriptRole::User, None);
        image_message.content.clear();
        image_message
            .images
            .push(llmeter_collector::TranscriptImage {
                source: "fixture".into(),
                mime: "image/png".into(),
                data: Some(Arc::from(&include_bytes!("../../assets/AppIcon.png")[..])),
            });
        let transcript = SessionTranscript {
            messages: vec![image_message.clone(), image_message],
            truncated: false,
        };
        view.update(cx, |detail, cx| detail.set_transcript(Ok(transcript), cx));
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        let initial_windows = cx.update(|_, cx| cx.windows().len());
        for (offset, selector) in ["transcript-image-0-0", "transcript-image-1-0"]
            .into_iter()
            .enumerate()
        {
            let bounds = cx.debug_bounds(selector).expect("attachment is visible");
            cx.simulate_click(bounds.center(), gpui::Modifiers::default());
            assert_eq!(
                cx.update(|_, cx| cx.windows().len()),
                initial_windows + offset + 1,
                "each attachment opens its own preview: {selector}"
            );
        }
    }

    /// Markdown source whitespace must not reserve empty space between messages.
    /// Real row measurements must also follow changes in the viewport width.
    #[gpui::test]
    fn transcript_measures_markdown_and_reflows_on_resize(cx: &mut gpui::TestAppContext) {
        use gpui::size;

        cx.update(gpui_component::init);
        let palette = cx.update(|cx| Palette::from_app(cx));
        let (view, cx) = cx.add_window_view(|_, _| SessionDetailView::new(palette));

        let transcript = SessionTranscript {
            truncated: false,
            messages: vec![
                TranscriptMessage {
                    role: TranscriptRole::User,
                    content: "看一下有什么可以优化的地方".into(),
                    timestamp: None,
                    images: Vec::new(),
                    phase: TranscriptPhase::Unspecified,
                },
                TranscriptMessage {
                    role: TranscriptRole::Assistant,
                    content: format!(
                        "## 高影响\n\n**架构基础是好的**——增量 JSONL 读取、`WAL`、批量事务。{}继续优化。",
                        "\n".repeat(80)
                    ),
                    timestamp: None,
                    images: Vec::new(),
                    phase: TranscriptPhase::Unspecified,
                },
                TranscriptMessage {
                    role: TranscriptRole::User,
                    content: "开始优化".into(),
                    timestamp: None,
                    images: Vec::new(),
                    phase: TranscriptPhase::Unspecified,
                },
            ],
        };
        view.update(cx, |detail, cx| detail.set_transcript(Ok(transcript), cx));
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });

        let first = cx
            .debug_bounds("transcript-assistant-1")
            .expect("assistant row must paint");
        assert!(first.size.height > px(0.0), "painted row has height");
        let user = cx
            .debug_bounds("transcript-user")
            .expect("following user message must paint");
        let gap = user.origin.y - first.bottom();
        assert!(
            gap >= px(0.0) && gap <= px(16.0),
            "message gap follows rendered Markdown: {gap}"
        );

        cx.simulate_resize(size(px(1400.0), px(768.0)));
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        let second = cx
            .debug_bounds("transcript-assistant-1")
            .expect("assistant row must survive a width re-measurement");
        assert!(second.size.height > px(0.0), "resized row has height");
        assert!(
            second.size.width < first.size.width,
            "row follows the narrower window: {first} -> {second}"
        );
    }
}
