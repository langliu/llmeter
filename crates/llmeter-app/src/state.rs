use std::path::PathBuf;

use chrono::{DateTime, Duration, Local, NaiveDate, TimeZone, Utc};
use llmeter_core::ProviderDetection;
use llmeter_storage::{
    DailyModelUsage, DailyUsage, ModelUsage, Overview, ProjectUsage, ProviderUsage, RecentActivity,
    SessionSummary, UsageRepository,
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
        Ok(Self {
            overview: repository.get_overview(start, end)?,
            daily: repository.get_daily_usage(start, end)?,
            providers: repository.get_provider_usage(start, end)?,
            models: repository.get_model_usage(start, end)?,
        })
    }
}

impl UiSnapshot {
    pub fn load(
        repository: &UsageRepository,
        overview_start: DateTime<Utc>,
        overview_end: DateTime<Utc>,
    ) -> Result<Self, llmeter_storage::StorageError> {
        let now = Utc::now();
        let today_start = local_midnight(Local::now().date_naive());
        let seven_start = now - Duration::days(7);
        let thirty_start = now - Duration::days(30);
        Ok(Self {
            today: repository.get_today_usage(today_start, now + Duration::seconds(1))?,
            seven_days: repository.get_overview(seven_start, now + Duration::seconds(1))?,
            thirty_days: repository.get_overview(thirty_start, now + Duration::seconds(1))?,
            overview_range: OverviewRangeSnapshot::load(repository, overview_start, overview_end)?,
            // Keep enough history for the overview calendar while the trend remains a
            // compact 30-day view.
            heatmap_daily: repository
                .get_daily_usage(now - Duration::days(147), now + Duration::seconds(1))?,
            heatmap_models: repository
                .get_daily_model_usage(now - Duration::days(147), now + Duration::seconds(1))?,
            providers: repository.get_provider_usage(thirty_start, now + Duration::seconds(1))?,
            models: repository.get_model_usage(thirty_start, now + Duration::seconds(1))?,
            projects: repository.get_project_usage(thirty_start, now + Duration::seconds(1))?,
            recent: repository.get_recent_activity(8)?,
            sessions: repository.get_sessions()?,
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
