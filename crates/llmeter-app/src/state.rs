use std::path::PathBuf;

use chrono::{DateTime, Duration, Local, NaiveDate, TimeZone, Utc};
use llmeter_core::ProviderDetection;
use llmeter_storage::{
    DailyModelUsage, DailyUsage, DashboardQuery, ModelUsage, Overview, ProjectUsage, ProviderUsage,
    RecentActivity, SessionLoad, SessionSummary, UsageRepository,
};

#[derive(Clone, Debug)]
pub struct UiSnapshot {
    pub today: Overview,
    pub seven_days: Overview,
    pub thirty_days: Overview,
    pub overview_range: OverviewRangeSnapshot,
    pub heatmap_daily: Vec<DailyUsage>,
    pub heatmap_models: Vec<DailyModelUsage>,
    pub providers: Vec<ProviderUsage>,
    pub models: Vec<ModelUsage>,
    pub projects: Vec<ProjectUsage>,
    pub recent: Vec<RecentActivity>,
    pub sessions: Vec<SessionSummary>,
    pub session_count: u64,
    pub detections: Vec<ProviderDetection>,
    pub database_path: PathBuf,
    pub last_sync: Option<DateTime<Utc>>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct OverviewRangeSnapshot {
    pub overview: Overview,
    pub daily: Vec<DailyUsage>,
    pub providers: Vec<ProviderUsage>,
    pub models: Vec<ModelUsage>,
}

impl OverviewRangeSnapshot {
    pub fn load(
        repository: &UsageRepository,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Self, llmeter_storage::StorageError> {
        let (overview, daily, providers, models) = repository.load_overview_range(start, end)?;
        Ok(Self {
            overview,
            daily,
            providers,
            models,
        })
    }
}

impl UiSnapshot {
    pub fn load(
        repository: &UsageRepository,
        overview_start: DateTime<Utc>,
        overview_end: DateTime<Utc>,
        session_load: SessionLoad,
    ) -> Result<Self, llmeter_storage::StorageError> {
        let now = Utc::now();
        let today_start = local_midnight(Local::now().date_naive());
        let seven_start = now - Duration::days(7);
        let thirty_start = now - Duration::days(30);
        let now_end = now + Duration::seconds(1);
        let data = repository.load_dashboard(DashboardQuery {
            today_start,
            seven_start,
            thirty_start,
            heatmap_start: now - Duration::days(147),
            overview_start,
            overview_end,
            now_end,
            session_load,
        })?;
        Ok(Self {
            today: data.today,
            seven_days: data.seven_days,
            thirty_days: data.thirty_days,
            overview_range: OverviewRangeSnapshot {
                overview: data.overview,
                daily: data.overview_daily,
                providers: data.overview_providers,
                models: data.overview_models,
            },
            heatmap_daily: data.heatmap_daily,
            heatmap_models: data.heatmap_models,
            providers: data.providers,
            models: data.models,
            projects: data.projects,
            recent: data.recent,
            session_count: data.session_count,
            sessions: data.sessions,
            detections: Vec::new(),
            database_path: repository.database().path().to_path_buf(),
            last_sync: None,
            warnings: Vec::new(),
        })
    }

    pub fn with_detections(mut self, detections: Vec<ProviderDetection>) -> Self {
        self.detections = detections;
        self
    }

    pub fn with_sync(mut self, timestamp: DateTime<Utc>, warnings: Vec<String>) -> Self {
        self.last_sync = Some(timestamp);
        self.warnings = warnings;
        self
    }
}

pub(crate) fn local_midnight(date: NaiveDate) -> DateTime<Utc> {
    Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap_or_default())
        .single()
        .unwrap_or_else(Local::now)
        .with_timezone(&Utc)
}
