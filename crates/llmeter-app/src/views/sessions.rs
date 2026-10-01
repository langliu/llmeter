use std::{collections::HashSet, rc::Rc};

use chrono::{Datelike, Duration, Local, Timelike};
use gpui::{
    AnyElement, Context, Entity, FontWeight, HighlightStyle, InteractiveElement, IntoElement,
    ParentElement, Pixels, Render, SharedString, Size, Window, deferred, div, prelude::*, px, rems,
    size,
};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, Selectable, Sizable, VirtualListScrollHandle,
    button::{Button, ButtonGroup, ButtonVariants},
    h_flex,
    highlighter::HighlightTheme,
    input::Input,
    sheet::Sheet,
    text::{TextView, TextViewStyle},
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

/// One renderable row of the transcript. Consecutive thinking/tool messages
/// collapse into a single `Steps` row so a work phase reads as one entry
/// ("worked 6m 46s") instead of one line per step, matching ZCode's layout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TranscriptRow {
    Message(usize),
    Steps {
        /// Inclusive index of the first thinking/tool message.
        start: usize,
        /// Exclusive index past the last thinking/tool message.
        end: usize,
    },
}

fn transcript_rows(messages: &[TranscriptMessage]) -> Vec<TranscriptRow> {
    let mut rows = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        if matches!(
            messages[index].role,
            TranscriptRole::Thinking | TranscriptRole::Tool
        ) {
            let start = index;
            while index < messages.len()
                && matches!(
                    messages[index].role,
                    TranscriptRole::Thinking | TranscriptRole::Tool
                )
            {
                index += 1;
            }
            rows.push(TranscriptRow::Steps { start, end: index });
        } else {
            rows.push(TranscriptRow::Message(index));
            index += 1;
        }
    }
    rows
}

pub(crate) struct SessionDetailView {
    palette: Palette,
    transcript: TranscriptLoadState,
    transcript_rows: Rc<Vec<TranscriptRow>>,
    transcript_item_sizes: Rc<Vec<Size<Pixels>>>,
    /// Step-group start indices whose detail block the user expanded.
    expanded_transcript_items: HashSet<usize>,
    /// Content width the row-size estimates were computed for. The sheet is
    /// user-resizable, so this is measured from the laid-out list and the
    /// size table is rebuilt whenever it moves.
    transcript_width: Pixels,
    transcript_scroll: VirtualListScrollHandle,
}

/// Row-height estimates before the list has been laid out once: the default
/// 680px sheet minus its horizontal padding.
const TRANSCRIPT_FALLBACK_WIDTH: f32 = 656.0;
/// Average glyph width at the transcript's 14px body size, slightly wide so
/// estimates err toward a few spare pixels rather than clipped rows.
const TRANSCRIPT_GLYPH_UNIT: f32 = 7.5;

impl SessionDetailView {
    pub(crate) fn new(palette: Palette) -> Self {
        Self {
            palette,
            transcript: TranscriptLoadState::Loading,
            transcript_rows: Rc::new(Vec::new()),
            transcript_item_sizes: Rc::new(Vec::new()),
            expanded_transcript_items: HashSet::new(),
            transcript_width: px(TRANSCRIPT_FALLBACK_WIDTH),
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
                self.expanded_transcript_items.clear();
                self.transcript_rows = Rc::new(transcript_rows(&transcript.messages));
                self.rebuild_transcript_sizes();
                TranscriptLoadState::Loaded(transcript)
            }
            Err(error) => {
                self.transcript_rows = Rc::new(Vec::new());
                self.transcript_item_sizes = Rc::new(Vec::new());
                TranscriptLoadState::Failed(error)
            }
        };
        cx.notify();
    }

    fn toggle_transcript_item(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.expanded_transcript_items.remove(&index) {
            self.expanded_transcript_items.insert(index);
        }
        // Collapsing changes a row's height, so the virtual list's size
        // table has to move with the expanded set.
        self.rebuild_transcript_sizes();
        cx.notify();
    }

    /// The virtual list positions rows by estimated size, and text wraps by
    /// the real container width — which the user can drag wider or narrower.
    /// Measure it from the laid-out list each frame and rebuild the size
    /// table when it moved; the notify settles after one correction frame.
    fn sync_transcript_width(&mut self, width: Pixels, cx: &mut Context<Self>) {
        if width <= px(0.0) || (width - self.transcript_width).abs() <= px(0.5) {
            return;
        }
        self.transcript_width = width;
        self.rebuild_transcript_sizes();
        cx.notify();
    }

    fn rebuild_transcript_sizes(&mut self) {
        if let TranscriptLoadState::Loaded(transcript) = &self.transcript {
            let rows = self.transcript_rows.clone();
            self.transcript_item_sizes = Rc::new(
                rows.iter()
                    .map(|row| {
                        estimated_transcript_row_size(
                            row,
                            &transcript.messages,
                            self.expanded_transcript_items.contains(&row_key(row)),
                            self.transcript_width,
                        )
                    })
                    .collect(),
            );
        }
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
                let messages = v_virtual_list(
                    detail,
                    "session-transcript-items",
                    item_sizes,
                    move |detail, visible_range, _, cx| {
                        // The list's laid-out width is the ground truth for
                        // text wrapping; pick up resizes here so the size
                        // table follows the dragged sheet width.
                        detail.sync_transcript_width(
                            detail.transcript_scroll.base_handle().bounds().size.width,
                            cx,
                        );
                        let TranscriptLoadState::Loaded(transcript) = &detail.transcript else {
                            return Vec::new();
                        };
                        let expanded = &detail.expanded_transcript_items;
                        visible_range
                            .filter_map(|row_index| {
                                let row = detail.transcript_rows.get(row_index)?;
                                Some(match row {
                                    TranscriptRow::Message(index) => transcript_message(
                                        transcript.messages.get(*index)?,
                                        *index,
                                        p,
                                    ),
                                    TranscriptRow::Steps { start, end } => collapsible_steps(
                                        transcript.messages.get(*start..*end)?,
                                        *start,
                                        expanded.contains(start),
                                        p,
                                        cx,
                                    ),
                                })
                            })
                            .collect()
                    },
                )
                .track_scroll(&scroll)
                .size_full()
                .gap_3();

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

    div()
        .size_full()
        .rounded_lg()
        .border_1()
        .border_color(p.border.opacity(0.7))
        .bg(p.tiles)
        .px_3()
        .py_3()
        .child(content)
}

fn estimated_transcript_row_size(
    row: &TranscriptRow,
    messages: &[TranscriptMessage],
    expanded: bool,
    content_width: Pixels,
) -> Size<Pixels> {
    const LINE_HEIGHT: f32 = 20.0;
    const COLLAPSED_ROW_HEIGHT: f32 = 30.0;
    const EXPANDED_LINE_HEIGHT: f32 = 18.0;
    const EXPANDED_PADDING: f32 = 10.0;
    const BUBBLE_PADDING: f32 = 22.0;
    const BUBBLE_HORIZONTAL_PADDING: f32 = 28.0;
    const BUBBLE_MAX_WIDTH: f32 = 560.0;
    const SEPARATOR_HEIGHT: f32 = 5.0;
    const STEP_HEADER_HEIGHT: f32 = 20.0;
    const STEP_GAP: f32 = 8.0;
    const MARKDOWN_BLOCK_PADDING: f32 = 12.0;
    /// Small safety margin so slight wrapping differences leave a hair of
    /// space instead of clipping the last line.
    const WIDTH_MARGIN: f32 = 6.0;

    let width = f32::from(content_width);
    let full_units = ((width - WIDTH_MARGIN).max(120.0) / TRANSCRIPT_GLYPH_UNIT) as usize;
    let bubble_units = (((width.min(BUBBLE_MAX_WIDTH) - BUBBLE_HORIZONTAL_PADDING).max(120.0))
        / TRANSCRIPT_GLYPH_UNIT) as usize;
    let step_units = ((width - WIDTH_MARGIN - 12.0).max(120.0) / TRANSCRIPT_GLYPH_UNIT) as usize;

    match row {
        TranscriptRow::Message(index) => {
            let Some(message) = messages.get(*index) else {
                return size(px(1.0), px(0.0));
            };
            match message.role {
                TranscriptRole::User => {
                    let lines = content_lines(&message.content, bubble_units);
                    size(px(1.0), px(BUBBLE_PADDING + lines as f32 * LINE_HEIGHT))
                }
                // Markdown blocks (headings, code fences, lists) add vertical
                // padding a plain line count misses.
                TranscriptRole::Assistant => {
                    let lines = content_lines(&message.content, full_units);
                    size(
                        px(1.0),
                        px(lines as f32 * LINE_HEIGHT + MARKDOWN_BLOCK_PADDING),
                    )
                }
                TranscriptRole::Thinking | TranscriptRole::Tool => {
                    let lines = content_lines(&message.content, full_units);
                    size(px(1.0), px(lines as f32 * LINE_HEIGHT))
                }
            }
        }
        TranscriptRow::Steps { start, end } => {
            let mut height = SEPARATOR_HEIGHT + COLLAPSED_ROW_HEIGHT;
            if expanded {
                if let Some(steps) = messages.get(*start..*end) {
                    for message in steps {
                        let lines = content_lines(&message.content, step_units);
                        height +=
                            STEP_HEADER_HEIGHT + lines as f32 * EXPANDED_LINE_HEIGHT + STEP_GAP;
                    }
                }
                height += EXPANDED_PADDING;
            }
            size(px(1.0), px(height))
        }
    }
}

fn content_lines(content: &str, chars_per_line: usize) -> usize {
    content
        .lines()
        .map(|line| {
            let width = line
                .chars()
                .map(|character| if character.is_ascii() { 1 } else { 2 })
                .sum::<usize>()
                .max(1);
            width.div_ceil(chars_per_line)
        })
        .sum::<usize>()
        .max(1)
}

/// ZCode-style conversation rendering: user messages sit in a right-aligned
/// bubble, assistant text flows full width as rendered Markdown without a
/// card, and each run of thinking/tool steps collapses to one hairline-topped
/// row that expands in place.
fn transcript_message(message: &TranscriptMessage, index: usize, p: Palette) -> AnyElement {
    match message.role {
        TranscriptRole::User => user_bubble(message, p),
        // Step messages reach the renderer only through a Steps row; keep a
        // safe fallback in case grouping ever misses one.
        TranscriptRole::Assistant | TranscriptRole::Thinking | TranscriptRole::Tool => {
            assistant_text(message, index, p)
        }
    }
}

fn user_bubble(message: &TranscriptMessage, p: Palette) -> AnyElement {
    // A foreground-tinted ground keeps the bubble clearly elevated on both
    // themes; muted is barely distinguishable from the sheet background.
    let ground = p.foreground.opacity(if p.is_dark { 0.14 } else { 0.08 });
    h_flex()
        .w_full()
        .justify_end()
        .child(
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
        .into_any_element()
}

fn assistant_text(message: &TranscriptMessage, index: usize, p: Palette) -> AnyElement {
    // The renderer defaults shout: 1.5rem headings, 1rem paragraph gaps, and
    // inline code tinted with the theme accent. ZCode keeps everything
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
    // render them richly like ZCode instead of showing the raw source.
    TextView::markdown(
        SharedString::from(format!("assistant-md-{index}")),
        &message.content,
    )
    .style(style)
    .w_full()
    .text_sm()
    .text_color(p.foreground)
    .into_any_element()
}

/// One collapsed entry for a whole run of thinking/tool messages: a hairline
/// separator, a duration summary line, and an expandable step list underneath.
fn collapsible_steps(
    steps: &[TranscriptMessage],
    key: usize,
    expanded: bool,
    p: Palette,
    cx: &mut Context<SessionDetailView>,
) -> AnyElement {
    let chevron = if expanded {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    };
    let mut block = v_flex()
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
        for message in steps {
            list = list.child(step_detail(message, p));
        }
        block = block.child(list);
    }
    block.into_any_element()
}

/// Expanded view of one message inside a step group: an icon plus its title,
/// with the raw content underneath.
fn step_detail(message: &TranscriptMessage, p: Palette) -> AnyElement {
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
        .into_any_element()
}

/// Collapsed summary of a step run: the wall-clock span between its first and
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
        }
    }

    #[test]
    fn consecutive_step_messages_group_into_single_rows() {
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
                TranscriptRow::Steps { start: 1, end: 4 },
                TranscriptRow::Message(4),
                TranscriptRow::Steps { start: 5, end: 6 },
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

    #[test]
    fn expanded_step_rows_estimate_taller_than_collapsed() {
        let messages = vec![
            message(TranscriptRole::Thinking, None),
            message(TranscriptRole::Tool, None),
        ];
        let row = TranscriptRow::Steps { start: 0, end: 2 };
        let width = px(656.0);
        let collapsed = estimated_transcript_row_size(&row, &messages, false, width);
        let expanded = estimated_transcript_row_size(&row, &messages, true, width);
        assert!(expanded.height > collapsed.height);
    }

    #[test]
    fn wider_content_estimates_fewer_lines() {
        let long_assistant = TranscriptMessage {
            role: TranscriptRole::Assistant,
            content: "三路深入审查完成。".repeat(60),
            timestamp: None,
        };
        let row = TranscriptRow::Message(0);
        let messages = std::slice::from_ref(&long_assistant);
        let narrow = estimated_transcript_row_size(&row, messages, false, px(500.0));
        let wide = estimated_transcript_row_size(&row, messages, false, px(900.0));
        assert!(
            wide.height < narrow.height,
            "same text at a wider container must estimate shorter: {wide} vs {narrow}"
        );
    }
}
