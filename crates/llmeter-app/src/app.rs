use std::time::Duration;

use chrono::Utc;
use gpui::{AppContext, ClipboardItem, Context, Entity, Render, Subscription, Window, point, px};
use gpui_component::{
    VirtualListScrollHandle,
    input::{InputEvent, InputState},
    theme::{Theme as UiTheme, ThemeMode},
};
use llmeter_collector::{Collector, CollectorEvent};
use llmeter_storage::{SessionSummary, UsageRepository};
use rust_i18n::t;

use crate::{
    i18n::{self, LocalePreference},
    state::UiSnapshot,
    views::dashboard::{DashboardPage, HeatmapView, dashboard},
    views::sessions::{SessionProviderFilter, SessionRangeFilter},
    views::settings::SettingsSection,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ThemePreference {
    Light,
    Dark,
    #[default]
    System,
}

impl ThemePreference {
    pub(crate) const ALL: [Self; 3] = [Self::Light, Self::Dark, Self::System];

    pub(crate) fn label(self) -> String {
        match self {
            Self::Light => t!("settings.theme_light").to_string(),
            Self::Dark => t!("settings.theme_dark").to_string(),
            Self::System => t!("settings.system").to_string(),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
            Self::System => "system",
        }
    }

    fn from_setting(value: Option<String>) -> Self {
        match value.as_deref() {
            Some("light") => Self::Light,
            Some("dark") => Self::Dark,
            _ => Self::System,
        }
    }
}

pub struct LLMeterView {
    collector: Collector,
    pub(crate) snapshot: UiSnapshot,
    pub(crate) active_page: DashboardPage,
    pub(crate) session_provider: SessionProviderFilter,
    pub(crate) session_range: SessionRangeFilter,
    pub(crate) session_project: Option<String>,
    pub(crate) session_search: Entity<InputState>,
    pub(crate) session_scroll: VirtualListScrollHandle,
    pub(crate) session_project_open: bool,
    pub(crate) copied_session: Option<String>,
    pub(crate) theme_pref: ThemePreference,
    pub(crate) locale_pref: LocalePreference,
    pub(crate) settings_section: SettingsSection,
    pub(crate) heatmap: Entity<HeatmapView>,
    _search_subscription: Subscription,
    _appearance_subscription: Subscription,
    refresh_task: Option<gpui::Task<()>>,
}

impl LLMeterView {
    pub fn new(collector: Collector, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let repository = UsageRepository::new(collector.engine().database().clone());
        let theme_pref = ThemePreference::from_setting(
            repository.database().get_setting("theme").ok().flatten(),
        );
        let locale_pref = LocalePreference::from_setting(
            repository.database().get_setting("locale").ok().flatten(),
        );
        i18n::apply(locale_pref);
        let detections = collector.detect_all();
        let snapshot = UiSnapshot::load(&repository)
            .map(|snapshot| snapshot.with_detections(detections.clone()))
            .unwrap_or_else(|error| UiSnapshot {
                today: Default::default(),
                seven_days: Default::default(),
                thirty_days: Default::default(),
                all_time: Default::default(),
                daily: Vec::new(),
                heatmap_daily: Vec::new(),
                heatmap_models: Vec::new(),
                providers: Vec::new(),
                models: Vec::new(),
                projects: Vec::new(),
                recent: Vec::new(),
                sessions: Vec::new(),
                detections,
                database_path: repository.database().path().to_path_buf(),
                last_sync: None,
                warnings: vec![format!("database query failed: {error}")],
            });
        let session_search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("sessions.search_placeholder").to_string())
        });
        let _search_subscription = cx.subscribe(&session_search, |this, _input, event, cx| {
            if matches!(event, InputEvent::Change) {
                this.reset_session_scroll();
                cx.notify();
            }
        });
        let weak = cx.entity().downgrade();
        let _appearance_subscription = window.observe_window_appearance(move |window, cx| {
            let _ = weak.update(cx, |view, cx| {
                if view.theme_pref == ThemePreference::System {
                    view.apply_theme(window, cx);
                }
            });
        });
        let mut view = Self {
            collector,
            snapshot,
            active_page: DashboardPage::Overview,
            session_provider: SessionProviderFilter::All,
            session_range: SessionRangeFilter::All,
            session_project: None,
            session_search,
            session_scroll: VirtualListScrollHandle::new(),
            session_project_open: false,
            copied_session: None,
            theme_pref,
            locale_pref,
            settings_section: SettingsSection::default(),
            heatmap: cx.new(|_| HeatmapView::default()),
            _search_subscription,
            _appearance_subscription,
            refresh_task: None,
        };
        view.apply_theme(window, cx);
        let task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this
                    .update(cx, |view, cx| {
                        if view.refresh_from_events() {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        view.refresh_task = Some(task);
        view
    }

    pub(crate) fn apply_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.theme_pref {
            ThemePreference::Light => UiTheme::change(ThemeMode::Light, Some(window), cx),
            ThemePreference::Dark => UiTheme::change(ThemeMode::Dark, Some(window), cx),
            ThemePreference::System => UiTheme::sync_system_appearance(Some(window), cx),
        }
    }

    pub(crate) fn set_theme_preference(
        &mut self,
        preference: ThemePreference,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.theme_pref == preference {
            return;
        }
        self.theme_pref = preference;
        let database = self.collector.engine().database().clone();
        let _ = database.set_setting("theme", preference.as_str());
        self.apply_theme(window, cx);
        cx.notify();
    }

    pub(crate) fn set_locale_preference(
        &mut self,
        preference: LocalePreference,
        cx: &mut Context<Self>,
    ) {
        if self.locale_pref == preference {
            return;
        }
        self.locale_pref = preference;
        let database = self.collector.engine().database().clone();
        let _ = database.set_setting("locale", preference.as_str());
        i18n::apply(preference);
        cx.notify();
    }

    pub(crate) fn set_settings_section(
        &mut self,
        section: SettingsSection,
        cx: &mut Context<Self>,
    ) {
        if self.settings_section != section {
            self.settings_section = section;
            cx.notify();
        }
    }

    pub(crate) fn navigate(&mut self, page: DashboardPage, cx: &mut Context<Self>) {
        if self.active_page != page {
            self.active_page = page;
            self.session_project_open = false;
            cx.notify();
        }
    }

    pub(crate) fn set_session_provider(
        &mut self,
        filter: SessionProviderFilter,
        cx: &mut Context<Self>,
    ) {
        self.session_provider = filter;
        self.reset_session_scroll();
        cx.notify();
    }

    pub(crate) fn set_session_range(&mut self, filter: SessionRangeFilter, cx: &mut Context<Self>) {
        self.session_range = filter;
        self.reset_session_scroll();
        cx.notify();
    }

    pub(crate) fn set_session_project(&mut self, project: Option<String>, cx: &mut Context<Self>) {
        self.session_project = project;
        self.session_project_open = false;
        self.reset_session_scroll();
        cx.notify();
    }

    fn reset_session_scroll(&self) {
        self.session_scroll.set_offset(point(px(0.0), px(0.0)));
    }

    pub(crate) fn toggle_session_projects(&mut self, cx: &mut Context<Self>) {
        self.session_project_open = !self.session_project_open;
        cx.notify();
    }

    pub(crate) fn copy_resume_command(&mut self, session: &SessionSummary, cx: &mut Context<Self>) {
        let Some(command) = session.resume_command() else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(command));
        self.copied_session = Some(session_key(session));
        cx.notify();
    }

    pub(crate) fn is_copied(&self, session: &SessionSummary) -> bool {
        self.copied_session
            .as_deref()
            .is_some_and(|value| value == session_key(session))
    }

    pub(crate) fn refresh_sessions(&mut self, cx: &mut Context<Self>) {
        let collector = self.collector.clone();
        std::thread::Builder::new()
            .name("llmeter-manual-sync".into())
            .spawn(move || {
                let _ = collector.sync_now();
            })
            .ok();
        self.reload_snapshot(None);
        cx.notify();
    }

    pub(crate) fn visible_session_indices(&self, cx: &gpui::App) -> Vec<usize> {
        let query = self.session_search.read(cx).value();
        let query = query.to_string();
        let now = Utc::now();
        let range_start = self.session_range.start(now);
        self.snapshot
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| self.session_provider.matches(session.provider))
            .filter(|(_, session)| range_start.is_none_or(|start| session.ended_at >= start))
            .filter(|(_, session)| {
                self.session_project.as_ref().is_none_or(|project| {
                    session.project_label().as_deref() == Some(project.as_str())
                })
            })
            .filter(|(_, session)| session.matches_query(&query))
            .map(|(index, _)| index)
            .collect()
    }

    pub(crate) fn session_projects(&self) -> Vec<String> {
        let mut names = self
            .snapshot
            .sessions
            .iter()
            .filter_map(SessionSummary::project_label)
            .collect::<Vec<_>>();
        names.sort();
        names.dedup();
        names
    }

    fn refresh_from_events(&mut self) -> bool {
        let mut changed = false;
        while let Some(event) = self.collector.try_recv() {
            let CollectorEvent::UsageChanged(result) = event;
            self.reload_snapshot(Some((Utc::now(), result.warnings)));
            changed = true;
        }
        changed
    }

    fn reload_snapshot(&mut self, sync: Option<(chrono::DateTime<Utc>, Vec<String>)>) {
        let repository = UsageRepository::new(self.collector.engine().database().clone());
        if let Ok(snapshot) = UiSnapshot::load(&repository) {
            let mut snapshot = snapshot.with_detections(self.snapshot.detections.clone());
            if let Some((timestamp, warnings)) = sync {
                snapshot = snapshot.with_sync(timestamp, warnings);
            } else {
                snapshot.last_sync = self.snapshot.last_sync;
                snapshot.warnings = self.snapshot.warnings.clone();
            }
            self.snapshot = snapshot;
        }
    }
}

pub(crate) fn session_key(session: &SessionSummary) -> String {
    format!(
        "{}:{}:{}",
        session.provider.as_str(),
        session.session_id.as_deref().unwrap_or_default(),
        session.source_file.as_deref().unwrap_or_default()
    )
}

impl Render for LLMeterView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        dashboard(self, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::{LLMeterView, LocalePreference};
    use crate::views::dashboard::DashboardPage;
    use crate::views::sessions::SessionProviderFilter;
    use chrono::{Duration, Utc};
    use gpui::{Modifiers, ScrollDelta, ScrollWheelEvent, TestAppContext, TouchPhase, point, px};
    use gpui_component::ActiveTheme;
    use llmeter_collector::Collector;
    use llmeter_core::Provider;
    use llmeter_storage::{Database, SessionSummary};

    fn scroll(
        view_position: gpui::Point<gpui::Pixels>,
        delta: gpui::Point<gpui::Pixels>,
        cx: &mut gpui::VisualTestContext,
    ) {
        cx.simulate_event(ScrollWheelEvent {
            position: view_position,
            delta: ScrollDelta::Pixels(delta),
            modifiers: Default::default(),
            touch_phase: TouchPhase::Moved,
        });
    }

    fn fake_sessions(count: usize) -> Vec<SessionSummary> {
        let now = Utc::now();
        (0..count)
            .map(|index| SessionSummary {
                provider: Provider::Claude,
                session_id: Some(format!("session-{index}")),
                source_file: Some(format!("/tmp/project-{index}/session.jsonl")),
                project_name: Some(format!("Project {index}")),
                project_path: None,
                model: Some("claude-opus".into()),
                started_at: now - Duration::hours(index as i64 + 1),
                ended_at: now - Duration::hours(index as i64),
                turn_count: 4,
                total_tokens: 1_000 + index as u64,
                estimated_cost_usd: Some(0.01),
            })
            .collect()
    }

    fn setup_sessions_page(
        cx: &mut TestAppContext,
    ) -> (gpui::Entity<LLMeterView>, &mut gpui::VisualTestContext) {
        let database = Database::open_in_memory().expect("in-memory database");
        let collector = Collector::new(database);
        cx.update(gpui_component::init);
        let (view, cx) = cx.add_window_view(|window, cx| LLMeterView::new(collector, window, cx));
        view.update(cx, |view, _| {
            view.snapshot.sessions = fake_sessions(60);
            view.active_page = DashboardPage::Sessions;
        });
        (view, cx)
    }

    /// Regression test: scrolling the session list must survive re-renders.
    /// Previously the virtual list created a fresh scroll handle on every
    /// render, so each wheel event re-rendered the view and snapped the list
    /// back to the top.
    #[gpui::test]
    fn session_list_scroll_persists_across_renders(cx: &mut TestAppContext) {
        let (view, cx) = setup_sessions_page(cx);

        // Position the mouse inside the session list area (window is 1024x768).
        let list_position = point(px(600.0), px(560.0));
        cx.simulate_mouse_move(list_position, None, Modifiers::default());
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });

        scroll(list_position, point(px(0.0), px(-300.0)), cx);
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });

        let offset = view.read_with(cx, |view, _| view.session_scroll.offset());
        assert!(
            offset.y < px(0.0),
            "session list should be scrolled down, got offset {:?}",
            offset
        );
        let first = offset;

        // A second wheel event must accumulate instead of resetting to the top.
        scroll(list_position, point(px(0.0), px(-160.0)), cx);
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });

        let offset = view.read_with(cx, |view, _| view.session_scroll.offset());
        assert!(
            offset.y < first.y,
            "scroll position did not accumulate: {:?} after starting from {:?}",
            offset,
            first
        );
    }

    #[gpui::test]
    fn session_list_scroll_resets_when_filter_changes(cx: &mut TestAppContext) {
        let (view, cx) = setup_sessions_page(cx);

        view.update(cx, |view, _| {
            view.session_scroll.set_offset(point(px(0.0), px(-240.0)));
        });
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });

        view.update(cx, |view, cx| {
            view.set_session_provider(SessionProviderFilter::Provider(Provider::Codex), cx);
        });
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });

        let offset = view.read_with(cx, |view, _| view.session_scroll.offset());
        assert_eq!(
            offset,
            point(px(0.0), px(0.0)),
            "filter change should scroll back to the top"
        );
    }

    #[gpui::test]
    fn theme_preference_applies_and_persists(cx: &mut TestAppContext) {
        let database = Database::open_in_memory().expect("in-memory database");
        let collector = Collector::new(database.clone());
        cx.update(gpui_component::init);
        let (view, cx) = cx.add_window_view(|window, cx| LLMeterView::new(collector, window, cx));

        // Default preference is System; the test platform reports a light appearance.
        cx.update(|_, cx| assert!(!cx.theme().is_dark()));

        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.set_theme_preference(super::ThemePreference::Dark, window, cx);
            });
        });
        cx.update(|_, cx| assert!(cx.theme().is_dark()));
        assert_eq!(
            database.get_setting("theme").unwrap().as_deref(),
            Some("dark")
        );

        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.set_theme_preference(super::ThemePreference::Light, window, cx);
            });
        });
        cx.update(|_, cx| assert!(!cx.theme().is_dark()));
        assert_eq!(
            database.get_setting("theme").unwrap().as_deref(),
            Some("light")
        );

        // A new view over the same database restores the stored preference.
        let collector = Collector::new(database.clone());
        let (restored, _cx) =
            cx.add_window_view(|window, cx| LLMeterView::new(collector, window, cx));
        restored.read_with(cx, |view, _| {
            assert_eq!(view.theme_pref, super::ThemePreference::Light);
        });
    }

    #[test]
    fn locale_preference_from_setting_parses_values() {
        assert_eq!(
            LocalePreference::from_setting(Some("zh".into())),
            LocalePreference::Zh
        );
        assert_eq!(
            LocalePreference::from_setting(Some("en".into())),
            LocalePreference::En
        );
        assert_eq!(
            LocalePreference::from_setting(Some("system".into())),
            LocalePreference::System
        );
        assert_eq!(
            LocalePreference::from_setting(None),
            LocalePreference::System
        );
    }

    #[test]
    fn theme_preference_from_setting_parses_values() {
        assert_eq!(
            super::ThemePreference::from_setting(Some("dark".into())),
            super::ThemePreference::Dark
        );
        assert_eq!(
            super::ThemePreference::from_setting(Some("light".into())),
            super::ThemePreference::Light
        );
        assert_eq!(
            super::ThemePreference::from_setting(Some("system".into())),
            super::ThemePreference::System
        );
        assert_eq!(
            super::ThemePreference::from_setting(None),
            super::ThemePreference::System
        );
    }

    #[gpui::test]
    fn settings_page_renders_both_sections(cx: &mut TestAppContext) {
        let (view, cx) = setup_sessions_page(cx);

        // Navigate to Settings (Appearance section by default) and draw.
        view.update(cx, |view, cx| {
            view.navigate(crate::views::dashboard::DashboardPage::Settings, cx);
        });
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });

        // Switch to the Data & Sync section and draw again.
        view.update(cx, |view, cx| {
            view.set_settings_section(crate::views::settings::SettingsSection::Data, cx);
        });
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
    }

    #[gpui::test]
    fn overview_renders_trend_with_data(cx: &mut TestAppContext) {
        let (view, cx) = setup_sessions_page(cx);
        view.update(cx, |view, _| {
            view.snapshot.daily = (0..30)
                .map(|index| llmeter_storage::DailyUsage {
                    day: format!("2026-01-{:02}", index + 1),
                    total_tokens: (index as u64 + 1) * 1_000,
                    estimated_cost_usd: Some(0.1 * (index as f64 + 1.0)),
                })
                .collect();
            view.active_page = crate::views::dashboard::DashboardPage::Overview;
        });
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
    }
}
