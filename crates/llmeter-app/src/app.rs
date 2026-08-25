use std::time::{Duration, Instant};

use chrono::{DateTime, Datelike, Days, Duration as ChronoDuration, Local, NaiveDate, Utc};
use gpui::{AppContext, ClipboardItem, Context, Entity, Render, Subscription, Window, point, px};
use gpui_component::{
    VirtualListScrollHandle,
    calendar::Date,
    date_picker::{DatePickerEvent, DatePickerState},
    input::{InputEvent, InputState},
    theme::{Theme as UiTheme, ThemeMode},
};
use llmeter_collector::{
    Collector, CollectorEvent, LimitCollector, providers::TRAE_CN_USAGE_SETTING,
};
use llmeter_core::LimitsSnapshot;
use llmeter_storage::{SessionSummary, UsageRepository};
use rust_i18n::t;

use crate::{
    i18n::{self, LocalePreference},
    state::{OverviewRangeSnapshot, UiSnapshot, local_midnight},
    views::dashboard::{DashboardPage, HeatmapView, dashboard},
    views::sessions::{SessionProviderFilter, SessionRangeFilter},
    views::settings::SettingsSection,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum OverviewPeriod {
    #[default]
    Day,
    Week,
    Month,
    AllTime,
    Custom,
}

impl OverviewPeriod {
    pub(crate) const ALL: [Self; 5] = [
        Self::Day,
        Self::Week,
        Self::Month,
        Self::AllTime,
        Self::Custom,
    ];

    pub(crate) fn label(self) -> String {
        match self {
            Self::Day => t!("overview.period_day").to_string(),
            Self::Week => t!("overview.period_week").to_string(),
            Self::Month => t!("overview.period_month").to_string(),
            Self::AllTime => t!("overview.period_total").to_string(),
            Self::Custom => t!("overview.period_custom").to_string(),
        }
    }

    fn bounds(
        self,
        now: DateTime<Local>,
        custom: (NaiveDate, NaiveDate),
    ) -> (DateTime<Utc>, DateTime<Utc>) {
        let today = now.date_naive();
        let current_end = now.with_timezone(&Utc) + ChronoDuration::seconds(1);
        match self {
            Self::Day => (local_midnight(today), current_end),
            Self::Week => {
                let start =
                    today - ChronoDuration::days(i64::from(today.weekday().num_days_from_monday()));
                (local_midnight(start), current_end)
            }
            Self::Month => {
                let start =
                    NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap_or(today);
                (local_midnight(start), current_end)
            }
            Self::AllTime => (
                DateTime::<Utc>::from_timestamp(0, 0).unwrap_or(current_end),
                current_end,
            ),
            Self::Custom => {
                let (start, end) = if custom.0 <= custom.1 {
                    custom
                } else {
                    (custom.1, custom.0)
                };
                let exclusive_end = end.checked_add_days(Days::new(1)).unwrap_or(end);
                (local_midnight(start), local_midnight(exclusive_end))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum OverviewProviderFilter {
    #[default]
    None,
    All,
    Provider(llmeter_core::Provider),
}

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

const SNAPSHOT_LOAD_ERROR_PREFIX: &str = "snapshot query failed";

struct SnapshotUpdate {
    snapshot: Option<UiSnapshot>,
    load_error: Option<String>,
    sync: Option<(chrono::DateTime<Utc>, Vec<String>)>,
    include_sessions: bool,
    period: OverviewPeriod,
    custom_range: (NaiveDate, NaiveDate),
    generation: u64,
}

pub struct LLMeterView {
    collector: Collector,
    limit_collector: LimitCollector,
    limit_sender: std::sync::mpsc::Sender<LimitsSnapshot>,
    limit_receiver: std::sync::mpsc::Receiver<LimitsSnapshot>,
    snapshot_sender: std::sync::mpsc::Sender<SnapshotUpdate>,
    snapshot_receiver: std::sync::mpsc::Receiver<SnapshotUpdate>,
    limit_refresh_started_at: Option<Instant>,
    pub(crate) limits: LimitsSnapshot,
    pub(crate) limits_refreshing: bool,
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
    pub(crate) trae_cn_usage_enabled: bool,
    pub(crate) settings_section: SettingsSection,
    pub(crate) heatmap: Entity<HeatmapView>,
    pub(crate) overview_period: OverviewPeriod,
    pub(crate) overview_date_range: Entity<DatePickerState>,
    pub(crate) overview_provider: OverviewProviderFilter,
    overview_custom_range: (NaiveDate, NaiveDate),
    pub(crate) overview_cards_width: f32,
    snapshot_generation: u64,
    applied_snapshot_generation: u64,
    _search_subscription: Subscription,
    _overview_date_subscription: Subscription,
    _appearance_subscription: Subscription,
    refresh_task: Option<gpui::Task<()>>,
}

impl LLMeterView {
    pub fn new(collector: Collector, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let repository = UsageRepository::new(collector.engine().database().clone());
        let limit_collector = LimitCollector::new(collector.engine().database().clone());
        let limits = limit_collector.cached_snapshot();
        let (limit_sender, limit_receiver) = std::sync::mpsc::channel();
        let (snapshot_sender, snapshot_receiver) = std::sync::mpsc::channel();
        let theme_pref = ThemePreference::from_setting(
            repository.database().get_setting("theme").ok().flatten(),
        );
        let locale_pref = LocalePreference::from_setting(
            repository.database().get_setting("locale").ok().flatten(),
        );
        let trae_cn_usage_enabled = repository
            .database()
            .get_setting(TRAE_CN_USAGE_SETTING)
            .ok()
            .flatten()
            .is_some_and(|value| matches!(value.trim(), "1" | "true"));
        i18n::apply(locale_pref);
        let detections = collector.detect_all();
        let today = Local::now().date_naive();
        let overview_period = OverviewPeriod::default();
        let overview_custom_range = (today, today);
        let (overview_start, overview_end) =
            overview_period.bounds(Local::now(), overview_custom_range);
        let snapshot = UiSnapshot::load(&repository, overview_start, overview_end, false)
            .map(|snapshot| snapshot.with_detections(detections.clone()))
            .unwrap_or_else(|error| UiSnapshot {
                today: Default::default(),
                seven_days: Default::default(),
                thirty_days: Default::default(),
                overview_range: Default::default(),
                heatmap_daily: Vec::new(),
                heatmap_models: Vec::new(),
                providers: Vec::new(),
                models: Vec::new(),
                projects: Vec::new(),
                recent: Vec::new(),
                sessions: Vec::new(),
                session_count: 0,
                detections,
                database_path: repository.database().path().to_path_buf(),
                last_sync: None,
                warnings: vec![format!("database query failed: {error}")],
            });
        let overview_date_range = cx.new(|cx| {
            let mut picker = DatePickerState::range(window, cx).date_format("%Y-%m-%d");
            picker.set_date(overview_custom_range, window, cx);
            picker
        });
        let _overview_date_subscription =
            cx.subscribe(&overview_date_range, |this, _, event, cx| {
                let DatePickerEvent::Change(Date::Range(Some(start), Some(end))) = event else {
                    return;
                };
                this.set_overview_custom_range(*start, *end, cx);
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
            limit_collector,
            limit_sender,
            limit_receiver,
            snapshot_sender,
            snapshot_receiver,
            limit_refresh_started_at: None,
            limits,
            limits_refreshing: false,
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
            trae_cn_usage_enabled,
            settings_section: SettingsSection::default(),
            heatmap: cx.new(|_| HeatmapView::default()),
            overview_period,
            overview_date_range,
            overview_provider: OverviewProviderFilter::None,
            overview_custom_range,
            overview_cards_width: 0.0,
            snapshot_generation: 0,
            applied_snapshot_generation: 0,
            _search_subscription,
            _overview_date_subscription,
            _appearance_subscription,
            refresh_task: None,
        };
        view.apply_theme(window, cx);
        if !cfg!(test) {
            view.collector.start_background();
        }
        let task = cx.spawn(async move |this, cx| {
            loop {
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
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
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

    pub(crate) fn set_trae_cn_usage_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.trae_cn_usage_enabled == enabled {
            return;
        }
        let database = self.collector.engine().database().clone();
        if let Err(error) = database.set_setting(
            TRAE_CN_USAGE_SETTING,
            if enabled { "true" } else { "false" },
        ) {
            self.snapshot
                .warnings
                .push(format!("failed to save TRAE CN usage preference: {error}"));
            cx.notify();
            return;
        }
        self.trae_cn_usage_enabled = enabled;
        self.refresh_sessions(cx);
    }

    pub(crate) fn navigate(&mut self, page: DashboardPage, cx: &mut Context<Self>) {
        if self.active_page != page {
            self.active_page = page;
            self.session_project_open = false;
            if page == DashboardPage::Limits {
                self.start_limit_refresh();
            }
            if page == DashboardPage::Sessions && self.snapshot.sessions.is_empty() {
                self.reload_snapshot(None);
            }
            cx.notify();
        }
    }

    pub(crate) fn set_overview_cards_width(&mut self, width: f32) -> bool {
        let previous = crate::views::dashboard::provider_card_columns(self.overview_cards_width);
        let next = crate::views::dashboard::provider_card_columns(width);
        self.overview_cards_width = width;
        previous != next
    }

    pub(crate) fn set_overview_period(&mut self, period: OverviewPeriod, cx: &mut Context<Self>) {
        if self.overview_period == period {
            return;
        }
        self.load_overview_period(period, self.overview_custom_range, cx);
    }

    pub(crate) fn set_overview_provider(
        &mut self,
        provider: OverviewProviderFilter,
        cx: &mut Context<Self>,
    ) {
        self.overview_provider = if self.overview_provider == provider {
            OverviewProviderFilter::None
        } else {
            provider
        };
        cx.notify();
    }

    fn set_overview_custom_range(
        &mut self,
        start: NaiveDate,
        end: NaiveDate,
        cx: &mut Context<Self>,
    ) {
        let range = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        self.overview_custom_range = range;
        if self.overview_period == OverviewPeriod::Custom {
            self.load_overview_period(OverviewPeriod::Custom, range, cx);
        }
    }

    fn load_overview_period(
        &mut self,
        period: OverviewPeriod,
        custom: (NaiveDate, NaiveDate),
        cx: &mut Context<Self>,
    ) {
        let repository = UsageRepository::new(self.collector.engine().database().clone());
        let (start, end) = period.bounds(Local::now(), custom);
        match OverviewRangeSnapshot::load(&repository, start, end) {
            Ok(snapshot) => {
                if matches!(
                    self.overview_provider,
                    OverviewProviderFilter::Provider(selected)
                        if !snapshot.providers.iter().any(|usage| usage.provider == selected)
                ) {
                    self.overview_provider = OverviewProviderFilter::None;
                }
                self.snapshot_generation = self.snapshot_generation.wrapping_add(1);
                self.overview_period = period;
                self.snapshot.overview_range = snapshot;
                cx.notify();
            }
            Err(error) => {
                self.snapshot
                    .warnings
                    .push(format!("overview query failed: {error}"));
                cx.notify();
            }
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
        cx.notify();
    }

    pub(crate) fn hook_action(
        &mut self,
        provider: llmeter_core::Provider,
        action: crate::views::settings::HookUiAction,
        cx: &mut Context<Self>,
    ) {
        let executable = std::env::current_exe().ok();
        let status = match (action, provider, executable.as_deref()) {
            (
                crate::views::settings::HookUiAction::Install,
                llmeter_core::Provider::Codex,
                Some(path),
            ) => llmeter_collector::hooks::install_codex_hook(path).ok(),
            (
                crate::views::settings::HookUiAction::Install,
                llmeter_core::Provider::Claude,
                Some(path),
            ) => llmeter_collector::hooks::install_claude_hook(path).ok(),
            (crate::views::settings::HookUiAction::Uninstall, llmeter_core::Provider::Codex, _) => {
                llmeter_collector::hooks::uninstall_codex_hook().ok()
            }
            (
                crate::views::settings::HookUiAction::Uninstall,
                llmeter_core::Provider::Claude,
                _,
            ) => llmeter_collector::hooks::uninstall_claude_hook().ok(),
            _ => None,
        };
        if let Some(status) = status {
            self.snapshot.warnings.push(status.detail);
        }
        cx.notify();
    }

    pub(crate) fn refresh_limits(&mut self, cx: &mut Context<Self>) {
        self.start_limit_refresh();
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
        let mut pending_reload = None;
        while let Some(event) = self.collector.try_recv() {
            match event {
                CollectorEvent::UsageChanged(result) => {
                    pending_reload = Some(Some((Utc::now(), result.warnings)));
                }
                CollectorEvent::PricingUpdated => {
                    pending_reload.get_or_insert(None);
                }
            }
            changed = true;
        }
        if let Some(sync) = pending_reload {
            self.reload_snapshot(sync);
        }
        while let Ok(update) = self.snapshot_receiver.try_recv() {
            if self.apply_snapshot(update) {
                changed = true;
            }
        }
        while let Ok(snapshot) = self.limit_receiver.try_recv() {
            self.limits = snapshot;
            self.limits_refreshing = false;
            changed = true;
        }
        if !cfg!(test)
            && self.active_page == DashboardPage::Limits
            && !self.limits_refreshing
            && self
                .limit_refresh_started_at
                .is_none_or(|started| started.elapsed() >= Duration::from_secs(5 * 60))
        {
            self.start_limit_refresh();
            changed = true;
        }
        changed
    }

    fn start_limit_refresh(&mut self) {
        if self.limits_refreshing {
            return;
        }
        self.limits_refreshing = true;
        self.limit_refresh_started_at = Some(Instant::now());
        let collector = self.limit_collector.clone();
        let sender = self.limit_sender.clone();
        if std::thread::Builder::new()
            .name("llmeter-limit-refresh".into())
            .spawn(move || {
                let _ = sender.send(collector.refresh());
            })
            .is_err()
        {
            self.limits_refreshing = false;
        }
    }

    fn reload_snapshot(&mut self, sync: Option<(chrono::DateTime<Utc>, Vec<String>)>) {
        let include_sessions = self.active_page == DashboardPage::Sessions;
        self.snapshot_generation = self.snapshot_generation.wrapping_add(1);
        let update_meta = (
            self.overview_period,
            self.overview_custom_range,
            self.snapshot_generation,
        );
        if cfg!(test) {
            if let Some(snapshot) = self.load_snapshot(include_sessions) {
                self.apply_snapshot(SnapshotUpdate {
                    snapshot: Some(snapshot),
                    load_error: None,
                    sync,
                    include_sessions,
                    period: update_meta.0,
                    custom_range: update_meta.1,
                    generation: update_meta.2,
                });
            }
            return;
        }
        let collector = self.collector.clone();
        let sender = self.snapshot_sender.clone();
        let (overview_start, overview_end) = self
            .overview_period
            .bounds(Local::now(), self.overview_custom_range);
        std::thread::Builder::new()
            .name("llmeter-snapshot".into())
            .spawn(move || {
                let repository = UsageRepository::new(collector.engine().database().clone());
                let update = match UiSnapshot::load(
                    &repository,
                    overview_start,
                    overview_end,
                    include_sessions,
                ) {
                    Ok(snapshot) => SnapshotUpdate {
                        snapshot: Some(snapshot.with_detections(collector.detect_all())),
                        load_error: None,
                        sync,
                        include_sessions,
                        period: update_meta.0,
                        custom_range: update_meta.1,
                        generation: update_meta.2,
                    },
                    Err(error) => {
                        tracing::warn!(error = %error, "snapshot query failed");
                        SnapshotUpdate {
                            snapshot: None,
                            load_error: Some(format!("{SNAPSHOT_LOAD_ERROR_PREFIX}: {error}")),
                            sync,
                            include_sessions,
                            period: update_meta.0,
                            custom_range: update_meta.1,
                            generation: update_meta.2,
                        }
                    }
                };
                let _ = sender.send(update);
            })
            .ok();
    }

    fn load_snapshot(&self, include_sessions: bool) -> Option<UiSnapshot> {
        let repository = UsageRepository::new(self.collector.engine().database().clone());
        let (overview_start, overview_end) = self
            .overview_period
            .bounds(Local::now(), self.overview_custom_range);
        let snapshot =
            UiSnapshot::load(&repository, overview_start, overview_end, include_sessions).ok()?;
        Some(snapshot.with_detections(self.collector.detect_all()))
    }

    fn apply_snapshot(&mut self, update: SnapshotUpdate) -> bool {
        if update.generation < self.applied_snapshot_generation {
            return false;
        }
        self.applied_snapshot_generation = update.generation;

        if let Some(error) = update.load_error {
            if let Some((timestamp, warnings)) = update.sync {
                self.snapshot.last_sync = Some(timestamp);
                for warning in warnings {
                    if !self
                        .snapshot
                        .warnings
                        .iter()
                        .any(|existing| existing == &warning)
                    {
                        self.snapshot.warnings.push(warning);
                    }
                }
            }
            if !self
                .snapshot
                .warnings
                .iter()
                .any(|warning| warning == &error)
            {
                self.snapshot.warnings.push(error);
            }
            return true;
        }

        let SnapshotUpdate {
            snapshot: Some(mut snapshot),
            sync,
            include_sessions,
            period,
            custom_range,
            ..
        } = update
        else {
            return false;
        };

        if let Some((timestamp, warnings)) = sync {
            snapshot = snapshot.with_sync(timestamp, warnings);
        } else {
            snapshot.last_sync = self.snapshot.last_sync;
            snapshot.warnings = self
                .snapshot
                .warnings
                .iter()
                .filter(|warning| !warning.starts_with(SNAPSHOT_LOAD_ERROR_PREFIX))
                .cloned()
                .collect();
        }

        if !include_sessions {
            snapshot.sessions = self.snapshot.sessions.clone();
        }

        let range_matches = period == self.overview_period
            && (period != OverviewPeriod::Custom || custom_range == self.overview_custom_range);
        if range_matches {
            if matches!(
                self.overview_provider,
                OverviewProviderFilter::Provider(selected)
                    if !snapshot
                        .overview_range
                        .providers
                        .iter()
                        .any(|usage| usage.provider == selected)
            ) {
                self.overview_provider = OverviewProviderFilter::None;
            }
        } else {
            snapshot.overview_range = self.snapshot.overview_range.clone();
        }
        self.snapshot = snapshot;
        true
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
    use super::{
        LLMeterView, LocalePreference, OverviewPeriod, OverviewProviderFilter, SnapshotUpdate,
    };
    use crate::views::dashboard::DashboardPage;
    use crate::views::sessions::SessionProviderFilter;
    use chrono::{Duration, Local, NaiveDate, TimeZone, Utc};
    use gpui::{Modifiers, ScrollDelta, ScrollWheelEvent, TestAppContext, TouchPhase, point, px};
    use gpui_component::ActiveTheme;
    use llmeter_collector::Collector;
    use llmeter_core::{LimitSource, LimitWindow, Provider, ProviderLimits};
    use llmeter_storage::{Database, ModelUsage, ProviderUsage, SessionSummary};

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

    #[test]
    fn overview_period_defaults_to_day() {
        assert_eq!(OverviewPeriod::default(), OverviewPeriod::Day);
    }

    #[test]
    fn overview_period_bounds_follow_local_calendar_periods() {
        let now = Local
            .with_ymd_and_hms(2026, 8, 20, 12, 30, 0)
            .single()
            .expect("valid local date");
        let custom = (
            NaiveDate::from_ymd_opt(2026, 7, 2).expect("valid start"),
            NaiveDate::from_ymd_opt(2026, 7, 5).expect("valid end"),
        );

        let (day_start, _) = OverviewPeriod::Day.bounds(now, custom);
        let (week_start, _) = OverviewPeriod::Week.bounds(now, custom);
        let (month_start, _) = OverviewPeriod::Month.bounds(now, custom);
        let (custom_start, custom_end) = OverviewPeriod::Custom.bounds(now, custom);

        assert_eq!(
            day_start.with_timezone(&Local).date_naive(),
            now.date_naive()
        );
        assert_eq!(
            week_start.with_timezone(&Local).date_naive(),
            NaiveDate::from_ymd_opt(2026, 8, 17).expect("valid week start")
        );
        assert_eq!(
            month_start.with_timezone(&Local).date_naive(),
            NaiveDate::from_ymd_opt(2026, 8, 1).expect("valid month start")
        );
        assert_eq!(custom_start.with_timezone(&Local).date_naive(), custom.0);
        assert_eq!(
            custom_end.with_timezone(&Local).date_naive(),
            NaiveDate::from_ymd_opt(2026, 7, 6).expect("exclusive day after custom end")
        );
    }

    #[gpui::test]
    fn overview_period_can_switch_from_default_day(cx: &mut TestAppContext) {
        let (view, cx) = setup_sessions_page(cx);
        view.update(cx, |view, cx| {
            assert_eq!(view.overview_period, OverviewPeriod::Day);
            view.set_overview_period(OverviewPeriod::Week, cx);
            assert_eq!(view.overview_period, OverviewPeriod::Week);
        });
    }

    #[gpui::test]
    fn stale_snapshot_does_not_clobber_overview_period(cx: &mut TestAppContext) {
        let (view, cx) = setup_sessions_page(cx);
        view.update(cx, |view, cx| {
            view.set_overview_period(OverviewPeriod::Week, cx);
            view.snapshot.overview_range.overview.total_tokens = 65_137_351;
            view.snapshot.today.total_tokens = 1;

            let mut stale = view.snapshot.clone();
            stale.overview_range.overview.total_tokens = 48_410_000;
            stale.today.total_tokens = 99;
            assert!(
                view.apply_snapshot(SnapshotUpdate {
                    snapshot: Some(stale),
                    load_error: None,
                    sync: None,
                    include_sessions: false,
                    period: OverviewPeriod::Day,
                    custom_range: view.overview_custom_range,
                    generation: view.snapshot_generation,
                }),
                "current generation should still apply non-range fields"
            );
            assert_eq!(view.overview_period, OverviewPeriod::Week);
            assert_eq!(
                view.snapshot.overview_range.overview.total_tokens,
                65_137_351
            );
            assert_eq!(view.snapshot.today.total_tokens, 99);

            let completed = view.snapshot_generation;
            view.snapshot_generation = view.snapshot_generation.wrapping_add(1);
            let mut completed_snapshot = view.snapshot.clone();
            completed_snapshot.today.total_tokens = 123;
            assert!(
                view.apply_snapshot(SnapshotUpdate {
                    snapshot: Some(completed_snapshot),
                    load_error: None,
                    sync: None,
                    include_sessions: false,
                    period: OverviewPeriod::Week,
                    custom_range: view.overview_custom_range,
                    generation: completed,
                }),
                "a finished load must apply even if a newer reload already started"
            );
            assert_eq!(view.snapshot.today.total_tokens, 123);
            assert_eq!(
                view.snapshot.overview_range.overview.total_tokens,
                65_137_351
            );

            let mut older = view.snapshot.clone();
            older.overview_range.overview.total_tokens = 1;
            older.today.total_tokens = 2;
            assert!(
                !view.apply_snapshot(SnapshotUpdate {
                    snapshot: Some(older),
                    load_error: None,
                    sync: None,
                    include_sessions: false,
                    period: OverviewPeriod::Week,
                    custom_range: view.overview_custom_range,
                    generation: completed.wrapping_sub(1),
                }),
                "older snapshot generation must be ignored"
            );
            assert_eq!(
                view.snapshot.overview_range.overview.total_tokens,
                65_137_351
            );
            assert_eq!(view.snapshot.today.total_tokens, 123);

            let synced_at = Utc::now();
            assert!(view.apply_snapshot(SnapshotUpdate {
                snapshot: None,
                load_error: Some("snapshot query failed: locked".into()),
                sync: Some((synced_at, vec!["collector warning".into()])),
                include_sessions: false,
                period: OverviewPeriod::Week,
                custom_range: view.overview_custom_range,
                generation: view.snapshot_generation,
            }));
            assert_eq!(view.snapshot.last_sync, Some(synced_at));
            assert!(
                view.snapshot
                    .warnings
                    .iter()
                    .any(|warning| warning.contains("snapshot query failed"))
            );
            assert!(
                view.snapshot
                    .warnings
                    .iter()
                    .any(|warning| warning == "collector warning")
            );
            assert_eq!(
                view.snapshot.overview_range.overview.total_tokens,
                65_137_351
            );

            let mut current = view.snapshot.clone();
            current.overview_range.overview.total_tokens = 70_000_000;
            assert!(view.apply_snapshot(SnapshotUpdate {
                snapshot: Some(current),
                load_error: None,
                sync: None,
                include_sessions: false,
                period: OverviewPeriod::Week,
                custom_range: view.overview_custom_range,
                generation: view.snapshot_generation,
            }));
            assert_eq!(
                view.snapshot.overview_range.overview.total_tokens,
                70_000_000
            );
            assert_eq!(view.snapshot.last_sync, Some(synced_at));
            assert!(
                view.snapshot
                    .warnings
                    .iter()
                    .any(|warning| warning == "collector warning")
            );
            assert!(
                !view
                    .snapshot
                    .warnings
                    .iter()
                    .any(|warning| warning.contains("snapshot query failed"))
            );
        });
    }

    #[gpui::test]
    fn overview_custom_period_renders_date_range_picker(cx: &mut TestAppContext) {
        let (view, cx) = setup_sessions_page(cx);
        view.update(cx, |view, cx| {
            view.active_page = DashboardPage::Overview;
            view.set_overview_period(OverviewPeriod::Custom, cx);
        });
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
    }

    #[gpui::test]
    fn overview_provider_selection_renders_model_and_cache_details(cx: &mut TestAppContext) {
        let (view, cx) = setup_sessions_page(cx);
        view.update(cx, |view, cx| {
            view.active_page = DashboardPage::Overview;
            view.snapshot.overview_range.providers = vec![ProviderUsage {
                provider: Provider::Codex,
                total_tokens: 500,
                input_tokens: 400,
                output_tokens: 100,
                cached_input_tokens: 300,
                cache_creation_input_tokens: 0,
                estimated_cost_usd: Some(0.5),
                last_activity: None,
            }];
            view.snapshot.overview_range.models = vec![ModelUsage {
                provider: Provider::Codex,
                model: "gpt-5.6-sol".into(),
                total_tokens: 500,
                estimated_cost_usd: Some(0.5),
            }];
            assert_eq!(view.overview_provider, OverviewProviderFilter::None);
            view.set_overview_provider(OverviewProviderFilter::Provider(Provider::Codex), cx);
            view.set_overview_provider(OverviewProviderFilter::Provider(Provider::Codex), cx);
            assert_eq!(view.overview_provider, OverviewProviderFilter::None);
            view.set_overview_provider(OverviewProviderFilter::All, cx);
            view.set_overview_provider(OverviewProviderFilter::Provider(Provider::Codex), cx);
        });
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.overview_provider,
                OverviewProviderFilter::Provider(Provider::Codex)
            );
        });
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
    fn limits_page_renders_live_cached_and_disconnected_states(cx: &mut TestAppContext) {
        let (view, cx) = setup_sessions_page(cx);
        view.update(cx, |view, cx| {
            let now = Utc::now();
            view.limits.providers = vec![
                ProviderLimits {
                    provider: Provider::Claude,
                    configured: true,
                    plan: Some("Max".into()),
                    windows: vec![LimitWindow {
                        reset_at: Some(now + Duration::hours(2)),
                        ..LimitWindow::new("five_hour", 78.0)
                    }],
                    captured_at: now,
                    source: LimitSource::ProviderApi,
                    stale: false,
                    error: None,
                    last_error: None,
                },
                ProviderLimits {
                    provider: Provider::Codex,
                    configured: true,
                    plan: Some("Plus".into()),
                    windows: vec![LimitWindow::new("seven_day", 25.0)],
                    captured_at: now - Duration::minutes(5),
                    source: LimitSource::DiskCache,
                    stale: true,
                    error: None,
                    last_error: Some("offline".into()),
                },
                ProviderLimits::not_configured(Provider::Grok, now),
            ];
            view.navigate(DashboardPage::Limits, cx);
        });

        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
    }

    #[gpui::test]
    fn overview_renders_trend_with_data(cx: &mut TestAppContext) {
        let (view, cx) = setup_sessions_page(cx);
        view.update(cx, |view, _| {
            view.snapshot.overview_range.daily = (0..30)
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
