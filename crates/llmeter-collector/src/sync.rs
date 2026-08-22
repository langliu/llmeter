use std::{collections::HashSet, path::Path, sync::Arc, time::Instant};

use anyhow::Result;
use chrono::Utc;
use llmeter_core::{
    CumulativeUsageTracker, FileCursor, Provider, ProviderStatus, SourceFile, SourceFormat,
    SyncResult, TokenCounts, UsageEvent, estimate_cost_usd,
};
use llmeter_storage::{Database, InsertSummary};
use tracing::{debug, warn};

use crate::{
    parsers::jsonl::IncrementalJsonlReader,
    providers::{ParsedUsage, ProviderAdapter, SnapshotPolicy, default_adapters},
};

#[derive(Clone, Debug)]
pub struct SyncOptions {
    pub providers: Option<HashSet<Provider>>,
    pub include_local: bool,
    pub include_remote_snapshots: bool,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            providers: None,
            include_local: true,
            include_remote_snapshots: true,
        }
    }
}

impl SyncOptions {
    pub fn only(provider: Provider) -> Self {
        Self {
            providers: Some(HashSet::from([provider])),
            include_local: true,
            include_remote_snapshots: true,
        }
    }

    pub fn local_changes() -> Self {
        Self {
            providers: None,
            include_local: true,
            include_remote_snapshots: false,
        }
    }

    pub fn remote_snapshots() -> Self {
        Self {
            providers: None,
            include_local: false,
            include_remote_snapshots: true,
        }
    }
}

#[derive(Clone)]
pub struct SyncEngine {
    database: Database,
    adapters: Arc<Vec<Box<dyn ProviderAdapter>>>,
}

impl SyncEngine {
    pub fn new(database: Database) -> Self {
        let adapters = default_adapters(&database);
        Self::with_adapters(database, adapters)
    }

    pub fn with_adapters(database: Database, adapters: Vec<Box<dyn ProviderAdapter>>) -> Self {
        Self {
            database,
            adapters: Arc::new(adapters),
        }
    }

    pub fn database(&self) -> &Database {
        &self.database
    }

    pub(crate) fn clear_rebuildable_usage(&self) -> Result<()> {
        let providers = self
            .adapters
            .iter()
            .filter(|adapter| !adapter.uses_remote_snapshot())
            .map(|adapter| adapter.provider())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        self.database
            .clear_usage_and_cursors_for_providers(&providers)?;
        Ok(())
    }

    pub fn sync_all(&self) -> Result<SyncResult> {
        self.sync(SyncOptions::default())
    }

    pub fn detect_all(&self) -> Vec<llmeter_core::ProviderDetection> {
        self.adapters
            .iter()
            .map(|adapter| {
                adapter
                    .detect()
                    .unwrap_or_else(|error| llmeter_core::ProviderDetection {
                        provider: adapter.provider(),
                        status: ProviderStatus::UnsupportedVersion,
                        roots: Vec::new(),
                        detail: Some(format!("detection failed: {error:#}")),
                    })
            })
            .collect()
    }

    pub fn sync(&self, options: SyncOptions) -> Result<SyncResult> {
        let started = Instant::now();
        let mut result = SyncResult::default();
        for adapter in self.adapters.iter() {
            if adapter.uses_remote_snapshot() {
                if !options.include_remote_snapshots {
                    continue;
                }
            } else if !options.include_local {
                continue;
            }
            if options
                .providers
                .as_ref()
                .is_some_and(|providers| !providers.contains(&adapter.provider()))
            {
                continue;
            }
            match self.sync_provider(adapter.as_ref(), &mut result) {
                Ok(()) => {}
                Err(error) => {
                    result.warnings.push(format!(
                        "{} provider sync failed: {error:#}",
                        adapter.provider()
                    ));
                    warn!(provider = %adapter.provider(), error = %error, "provider sync failed");
                }
            }
        }
        result.duration_ms = started.elapsed().as_millis();
        Ok(result)
    }

    fn sync_provider(&self, adapter: &dyn ProviderAdapter, result: &mut SyncResult) -> Result<()> {
        let detection = adapter.detect()?;
        if detection.status == ProviderStatus::UnsupportedVersion {
            result.warnings.push(format!(
                "{} data detected but this storage version is not supported: {}",
                adapter.provider(),
                detection.detail.unwrap_or_else(|| "unknown schema".into())
            ));
            return Ok(());
        }
        let sources = adapter.discover_sources()?;
        for source in sources {
            match source.format {
                SourceFormat::Jsonl => self.sync_source(adapter, &source, result)?,
                SourceFormat::Sqlite => {
                    let parsed = adapter.parse_sqlite(&source)?;
                    self.sync_batch_source(
                        adapter,
                        &source,
                        parsed,
                        SnapshotPolicy::Upsert,
                        None,
                        result,
                    )?;
                }
                SourceFormat::Snapshot => {
                    let snapshot = adapter.parse_snapshot(&source)?;
                    self.sync_batch_source(
                        adapter,
                        &source,
                        snapshot.usages,
                        snapshot.policy,
                        snapshot.scope.as_deref(),
                        result,
                    )?;
                }
            }
        }
        Ok(())
    }

    fn sync_batch_source(
        &self,
        adapter: &dyn ProviderAdapter,
        source: &SourceFile,
        parsed: Vec<ParsedUsage>,
        policy: SnapshotPolicy,
        snapshot_scope: Option<&str>,
        result: &mut SyncResult,
    ) -> Result<()> {
        result.files_scanned += 1;
        result.events_seen += parsed.len();
        let existing_cursor = self.database.get_cursor(&source.path)?;
        let parser_changed = existing_cursor
            .as_ref()
            .is_some_and(|cursor| cursor.parser_version != adapter.parser_version());
        let events = parsed
            .iter()
            .map(|parsed| self.to_usage_event(source, parsed, parsed.counts, 0, snapshot_scope))
            .collect::<Vec<_>>();
        let summary = match policy {
            SnapshotPolicy::Upsert if parser_changed => self
                .database
                .replace_usage_events_for_source(source.provider, &source.path, None, &events)?,
            SnapshotPolicy::Upsert => self.database.upsert_usage_events_with_summary(&events)?,
            SnapshotPolicy::ReplaceAll => self.database.replace_usage_events_for_provider_scoped(
                source.provider,
                None,
                snapshot_scope,
                &events,
            )?,
            SnapshotPolicy::ReplaceSince(since) => {
                self.database.replace_usage_events_for_provider_scoped(
                    source.provider,
                    Some(since),
                    snapshot_scope,
                    &events,
                )?
            }
        };
        result.events_inserted += summary.inserted;
        result.tokens_added = result.tokens_added.saturating_add(summary.tokens_added);

        let metadata = std::fs::metadata(&source.path)?;
        let mut cursor = existing_cursor
            .filter(|_| !parser_changed)
            .unwrap_or_else(|| {
                FileCursor::new(
                    source.path.clone(),
                    source.provider,
                    adapter.parser_version(),
                )
            });
        let identity_kind = match source.format {
            SourceFormat::Sqlite => "sqlite",
            SourceFormat::Snapshot => "snapshot",
            SourceFormat::Jsonl => "file",
        };
        cursor.file_identity = Some(format!("{identity_kind}:{}", metadata.len()));
        cursor.byte_offset = metadata.len();
        cursor.file_size = metadata.len();
        cursor.modified_at = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|duration| i64::try_from(duration.as_secs()).ok());
        cursor.parser_version = adapter.parser_version();
        cursor.last_event_hash = events.last().map(|event| event.id.clone());
        cursor.updated_at = Utc::now().timestamp();
        self.database.upsert_cursor(&cursor)?;
        Ok(())
    }

    fn sync_source(
        &self,
        adapter: &dyn ProviderAdapter,
        source: &SourceFile,
        result: &mut SyncResult,
    ) -> Result<()> {
        result.files_scanned += 1;
        let existing = self.database.get_cursor(&source.path)?;
        let mut cursor = existing.unwrap_or_else(|| {
            FileCursor::new(
                source.path.clone(),
                source.provider,
                adapter.parser_version(),
            )
        });
        if cursor.parser_version != adapter.parser_version() {
            self.database
                .delete_usage_for_source(&source.path, source.provider)?;
            cursor = FileCursor::new(
                source.path.clone(),
                source.provider,
                adapter.parser_version(),
            );
        }

        let read = match IncrementalJsonlReader::read(&source.path, &cursor) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                debug!(path = %source.path.display(), "source disappeared during sync");
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };

        if read.reset {
            cursor.byte_offset = 0;
            cursor.last_cumulative = None;
        }
        let key = source.path.to_string_lossy().to_string();
        let mut cumulative_tracker = CumulativeUsageTracker::default();
        if let Some(snapshot) = cursor.last_cumulative {
            cumulative_tracker.seed(key.clone(), snapshot);
        }

        let mut events = Vec::new();
        for line in &read.lines {
            let parsed = match adapter
                .update_source_metadata(source, &line.raw, &mut cursor.source_metadata)
                .and_then(|()| adapter.parse_line(source, &line.raw))
            {
                Ok(parsed) => parsed,
                Err(error) => {
                    // We advance past malformed/non-usage records as handled
                    // input, while never logging the raw session line.
                    result.warnings.push(format!(
                        "{} {} at byte {}: {error}",
                        adapter.provider(),
                        source.path.display(),
                        line.byte_start
                    ));
                    continue;
                }
            };
            let Some(mut parsed) = parsed else {
                continue;
            };
            parsed.model = parsed
                .model
                .or_else(|| cursor.source_metadata.model.clone());
            parsed.session_id = parsed
                .session_id
                .or_else(|| cursor.source_metadata.session_id.clone());
            parsed.project_path = parsed
                .project_path
                .or_else(|| cursor.source_metadata.project_path.clone());
            parsed.project_name = parsed
                .project_name
                .or_else(|| cursor.source_metadata.project_name.clone());
            result.events_seen += 1;

            let counts = if let Some(snapshot) = parsed.cumulative_snapshot {
                cursor.last_cumulative = Some(snapshot);
                let delta = cumulative_tracker.observe(key.clone(), snapshot);
                if delta.duplicate {
                    TokenCounts::default()
                } else {
                    delta.counts
                }
            } else {
                parsed.counts
            };
            if counts.is_zero() {
                continue;
            }
            events.push(self.to_usage_event(source, &parsed, counts, line.byte_start, None));
            cursor.last_event_hash = events.last().map(|event| event.id.clone());
        }

        let InsertSummary {
            inserted,
            tokens_added,
        } = self.database.insert_usage_events_with_summary(&events)?;
        result.events_inserted += inserted;
        result.tokens_added = result.tokens_added.saturating_add(tokens_added);

        cursor.byte_offset = read.next_offset;
        cursor.file_identity = read.file_identity;
        cursor.file_size = read.file_size;
        cursor.modified_at = read.modified_at;
        cursor.updated_at = Utc::now().timestamp();
        self.database.upsert_cursor(&cursor)?;
        Ok(())
    }

    fn to_usage_event(
        &self,
        source: &SourceFile,
        parsed: &ParsedUsage,
        counts: TokenCounts,
        byte_start: u64,
        snapshot_scope: Option<&str>,
    ) -> UsageEvent {
        let session_id = parsed
            .session_id
            .clone()
            .or_else(|| source.session_id.clone());
        let project_path = parsed
            .project_path
            .clone()
            .or_else(|| source.project_path.clone());
        let project_name = parsed
            .project_name
            .clone()
            .or_else(|| source.project_name.clone())
            .or_else(|| {
                project_path
                    .as_deref()
                    .and_then(Path::file_name)
                    .map(|value| value.to_string_lossy().to_string())
            });
        let id = match source.format {
            SourceFormat::Sqlite => sqlite_event_id(source, parsed, session_id.as_deref()),
            SourceFormat::Snapshot if parsed.source_event_id.is_some() => {
                snapshot_event_id(source, parsed, snapshot_scope)
            }
            SourceFormat::Snapshot | SourceFormat::Jsonl => event_id(
                source.provider,
                &source.path,
                session_id.as_deref(),
                parsed,
                counts,
                byte_start,
            ),
        };
        UsageEvent {
            id,
            provider: source.provider,
            model: parsed.model.clone(),
            session_id,
            project_path,
            project_name,
            timestamp: parsed.timestamp,
            input_tokens: counts.input_tokens,
            cached_input_tokens: counts.cached_input_tokens,
            cache_creation_input_tokens: counts.cache_creation_input_tokens,
            output_tokens: counts.output_tokens,
            reasoning_tokens: counts.reasoning_tokens,
            total_tokens: counts.total_tokens,
            reported_cost_usd: parsed.reported_cost_usd,
            estimated_cost_usd: estimate_cost_usd(source.provider, parsed.model.as_deref(), counts),
            source_file: Some(source.path.clone()),
            source_event_id: parsed.source_event_id.clone(),
            snapshot_scope: snapshot_scope.map(str::to_string),
        }
    }
}

fn snapshot_event_id(
    source: &SourceFile,
    parsed: &ParsedUsage,
    snapshot_scope: Option<&str>,
) -> String {
    let value = format!(
        "v3|snapshot|{}|{}|{}",
        source.provider,
        snapshot_scope.unwrap_or_default(),
        parsed.source_event_id.as_deref().unwrap_or_default(),
    );
    blake3::hash(value.as_bytes()).to_hex().to_string()
}

fn sqlite_event_id(source: &SourceFile, parsed: &ParsedUsage, session_id: Option<&str>) -> String {
    let value = format!(
        "v1|sqlite|{}|{}|{}",
        source.provider,
        source.path.to_string_lossy(),
        parsed
            .source_event_id
            .as_deref()
            .or(session_id)
            .unwrap_or_default()
    );
    blake3::hash(value.as_bytes()).to_hex().to_string()
}

fn event_id(
    provider: Provider,
    path: &Path,
    session_id: Option<&str>,
    parsed: &ParsedUsage,
    counts: TokenCounts,
    byte_start: u64,
) -> String {
    let mut value = format!(
        "v1|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        provider,
        session_id.unwrap_or_default(),
        parsed.timestamp.timestamp_millis(),
        parsed.model.as_deref().unwrap_or_default(),
        counts.input_tokens,
        counts.cached_input_tokens,
        counts.cache_creation_input_tokens,
        counts.output_tokens,
        counts.reasoning_tokens,
        counts.total_tokens,
        byte_start,
    );
    // Pi and oh-my-pi can expose the same canonical session through two
    // directory roots. Keep the fallback ID path-independent for that
    // provider; official IDs still win through the database unique index.
    if provider != Provider::Pi {
        value.push('|');
        value.push_str(&path.to_string_lossy());
    }
    if let Some(source_event_id) = parsed.source_event_id.as_deref() {
        value.push('|');
        value.push_str(source_event_id);
    }
    blake3::hash(value.as_bytes()).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, OpenOptions},
        io::Write,
        path::PathBuf,
    };

    use chrono::{Duration, Utc};
    use llmeter_storage::UsageRepository;
    use rusqlite::Connection;

    use super::*;
    use crate::providers::{CodexAdapter, OpenCodeAdapter};

    struct SnapshotTestAdapter {
        path: PathBuf,
        provider: Provider,
        parser_version: u32,
        policy: SnapshotPolicy,
        remote: bool,
    }

    impl ProviderAdapter for SnapshotTestAdapter {
        fn provider(&self) -> Provider {
            self.provider
        }

        fn parser_version(&self) -> u32 {
            self.parser_version
        }

        fn detect(&self) -> Result<llmeter_core::ProviderDetection> {
            Ok(llmeter_core::ProviderDetection {
                provider: self.provider,
                status: ProviderStatus::DataFound,
                roots: vec![self.path.clone()],
                detail: None,
            })
        }

        fn discover_sources(&self) -> Result<Vec<SourceFile>> {
            Ok(vec![SourceFile {
                path: self.path.clone(),
                provider: self.provider,
                format: SourceFormat::Snapshot,
                session_id: None,
                project_path: None,
                project_name: None,
            }])
        }

        fn parse_line(&self, _source: &SourceFile, _line: &[u8]) -> Result<Option<ParsedUsage>> {
            Ok(None)
        }

        fn parse_snapshot(&self, source: &SourceFile) -> Result<crate::providers::ParsedSnapshot> {
            let value = fs::read_to_string(&source.path)?;
            let value = value.trim();
            if value.is_empty() {
                return Ok(crate::providers::ParsedSnapshot {
                    usages: Vec::new(),
                    policy: self.policy.clone(),
                    scope: None,
                });
            }
            let total_tokens = value.parse::<u64>()?;
            Ok(crate::providers::ParsedSnapshot {
                usages: vec![ParsedUsage {
                    counts: TokenCounts {
                        input_tokens: total_tokens,
                        total_tokens,
                        ..Default::default()
                    },
                    cumulative_snapshot: None,
                    timestamp: Utc::now(),
                    model: Some("test-model".into()),
                    session_id: Some("stable-session".into()),
                    project_path: None,
                    project_name: None,
                    source_event_id: Some("stable-source-event".into()),
                    reported_cost_usd: None,
                }],
                policy: self.policy.clone(),
                scope: None,
            })
        }

        fn uses_remote_snapshot(&self) -> bool {
            self.remote
        }
    }

    fn test_home(label: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("llmeter-sync-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join(".codex").join("sessions")).unwrap();
        path
    }

    fn overview(database: &Database) -> llmeter_storage::Overview {
        UsageRepository::new(database.clone())
            .get_overview(
                chrono::DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
                Utc::now() + Duration::days(1),
            )
            .unwrap()
    }

    fn stored_usage(
        id: &str,
        provider: Provider,
        source: PathBuf,
        source_event_id: &str,
        timestamp: chrono::DateTime<Utc>,
        total_tokens: u64,
    ) -> UsageEvent {
        UsageEvent {
            id: id.into(),
            provider,
            model: Some("test-model".into()),
            session_id: Some("stable-session".into()),
            project_path: None,
            project_name: None,
            timestamp,
            input_tokens: total_tokens,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: 0,
            total_tokens,
            reported_cost_usd: None,
            estimated_cost_usd: None,
            source_file: Some(source),
            source_event_id: Some(source_event_id.into()),
            snapshot_scope: None,
        }
    }

    #[test]
    fn snapshot_updates_keep_a_stable_id_and_empty_snapshots_clear_old_rows() {
        let home = test_home("snapshot-replacement");
        let source_path = home.join("snapshot.txt");
        fs::write(&source_path, "10").unwrap();
        let database = Database::open_in_memory().unwrap();
        let engine = SyncEngine::with_adapters(
            database.clone(),
            vec![Box::new(SnapshotTestAdapter {
                path: source_path.clone(),
                provider: Provider::Trae,
                parser_version: 1,
                policy: SnapshotPolicy::ReplaceAll,
                remote: false,
            })],
        );

        let first = engine.sync_all().unwrap();
        assert_eq!(first.events_inserted, 1);
        assert_eq!(overview(&database).total_tokens, 10);

        fs::write(&source_path, "20").unwrap();
        let updated = engine.sync_all().unwrap();
        assert!(updated.warnings.is_empty());
        assert_eq!(updated.events_inserted, 0);
        assert_eq!(overview(&database).event_count, 1);
        assert_eq!(overview(&database).total_tokens, 20);

        fs::write(&source_path, "").unwrap();
        engine.sync_all().unwrap();
        assert_eq!(overview(&database).event_count, 0);
        assert_eq!(overview(&database).total_tokens, 0);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn local_change_sync_skips_remote_snapshot_adapters() {
        let home = test_home("remote-snapshot-filter");
        let source_path = home.join("snapshot.txt");
        fs::write(&source_path, "10").unwrap();
        let database = Database::open_in_memory().unwrap();
        let engine = SyncEngine::with_adapters(
            database.clone(),
            vec![Box::new(SnapshotTestAdapter {
                path: source_path,
                provider: Provider::Trae,
                parser_version: 1,
                policy: SnapshotPolicy::ReplaceAll,
                remote: true,
            })],
        );

        let local = engine.sync(SyncOptions::local_changes()).unwrap();
        assert_eq!(local.files_scanned, 0);
        assert_eq!(overview(&database).event_count, 0);

        let periodic = engine.sync_all().unwrap();
        assert_eq!(periodic.files_scanned, 1);
        assert_eq!(overview(&database).total_tokens, 10);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn remote_snapshot_sync_skips_local_adapters() {
        let home = test_home("remote-only-filter");
        let local_path = home.join("local.txt");
        let remote_path = home.join("remote.txt");
        fs::write(&local_path, "10").unwrap();
        fs::write(&remote_path, "20").unwrap();
        let database = Database::open_in_memory().unwrap();
        let engine = SyncEngine::with_adapters(
            database.clone(),
            vec![
                Box::new(SnapshotTestAdapter {
                    path: local_path,
                    provider: Provider::Codex,
                    parser_version: 1,
                    policy: SnapshotPolicy::ReplaceAll,
                    remote: false,
                }),
                Box::new(SnapshotTestAdapter {
                    path: remote_path,
                    provider: Provider::Trae,
                    parser_version: 1,
                    policy: SnapshotPolicy::ReplaceAll,
                    remote: true,
                }),
            ],
        );

        let remote = engine.sync(SyncOptions::remote_snapshots()).unwrap();
        assert_eq!(remote.files_scanned, 1);
        assert_eq!(overview(&database).total_tokens, 20);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn rebuild_clear_preserves_remote_provider_history() {
        let database = Database::open_in_memory().unwrap();
        let local_path = PathBuf::from("/tmp/local-snapshot.json");
        let remote_path = PathBuf::from("/tmp/remote-snapshot.json");
        database
            .insert_usage_events(&[
                stored_usage(
                    "local",
                    Provider::Codex,
                    local_path.clone(),
                    "local",
                    Utc::now(),
                    10,
                ),
                stored_usage(
                    "remote",
                    Provider::Trae,
                    remote_path.clone(),
                    "remote",
                    Utc::now() - Duration::days(60),
                    20,
                ),
            ])
            .unwrap();
        database
            .upsert_cursor(&FileCursor::new(local_path.clone(), Provider::Codex, 1))
            .unwrap();
        database
            .upsert_cursor(&FileCursor::new(remote_path.clone(), Provider::Trae, 1))
            .unwrap();
        let engine = SyncEngine::with_adapters(
            database.clone(),
            vec![
                Box::new(SnapshotTestAdapter {
                    path: local_path.clone(),
                    provider: Provider::Codex,
                    parser_version: 1,
                    policy: SnapshotPolicy::ReplaceAll,
                    remote: false,
                }),
                Box::new(SnapshotTestAdapter {
                    path: remote_path.clone(),
                    provider: Provider::Trae,
                    parser_version: 1,
                    policy: SnapshotPolicy::ReplaceSince(Utc::now() - Duration::days(30)),
                    remote: true,
                }),
            ],
        );

        engine.clear_rebuildable_usage().unwrap();

        assert!(database.get_cursor(&local_path).unwrap().is_none());
        assert!(database.get_cursor(&remote_path).unwrap().is_some());
        assert_eq!(overview(&database).event_count, 1);
        assert_eq!(overview(&database).total_tokens, 20);
    }

    #[test]
    fn snapshot_parser_upgrade_preserves_history_outside_the_remote_window() {
        let home = test_home("snapshot-parser-upgrade");
        let source_path = home.join("snapshot.txt");
        fs::write(&source_path, "30").unwrap();
        let now = Utc::now();
        let database = Database::open_in_memory().unwrap();
        database
            .insert_usage_events(&[
                stored_usage(
                    "historical",
                    Provider::Trae,
                    source_path.clone(),
                    "historical",
                    now - Duration::days(60),
                    10,
                ),
                stored_usage(
                    "legacy-current",
                    Provider::Trae,
                    source_path.clone(),
                    "stable-source-event",
                    now - Duration::days(2),
                    20,
                ),
            ])
            .unwrap();
        database
            .upsert_cursor(&FileCursor::new(source_path.clone(), Provider::Trae, 1))
            .unwrap();
        let engine = SyncEngine::with_adapters(
            database.clone(),
            vec![Box::new(SnapshotTestAdapter {
                path: source_path.clone(),
                provider: Provider::Trae,
                parser_version: 2,
                policy: SnapshotPolicy::ReplaceSince(now - Duration::days(30)),
                remote: true,
            })],
        );

        let result = engine.sync_all().unwrap();

        assert!(result.warnings.is_empty());
        assert_eq!(overview(&database).event_count, 2);
        assert_eq!(overview(&database).total_tokens, 40);
        assert_eq!(
            database
                .get_cursor(&source_path)
                .unwrap()
                .unwrap()
                .parser_version,
            2
        );
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn snapshot_source_path_change_rekeys_the_official_event() {
        let home = test_home("snapshot-path-change");
        let old_path = home.join("old-snapshot.json");
        let new_path = home.join("new-snapshot.json");
        fs::write(&new_path, "25").unwrap();
        let database = Database::open_in_memory().unwrap();
        database
            .insert_usage_events(&[
                stored_usage(
                    "legacy-path-dependent-id",
                    Provider::Trae,
                    old_path.clone(),
                    "stable-source-event",
                    Utc::now(),
                    10,
                ),
                stored_usage(
                    "stale-old-path-event",
                    Provider::Trae,
                    old_path.clone(),
                    "removed-source-event",
                    Utc::now(),
                    99,
                ),
            ])
            .unwrap();
        let engine = SyncEngine::with_adapters(
            database.clone(),
            vec![Box::new(SnapshotTestAdapter {
                path: new_path.clone(),
                provider: Provider::Trae,
                parser_version: 1,
                policy: SnapshotPolicy::ReplaceAll,
                remote: true,
            })],
        );

        let result = engine.sync_all().unwrap();

        assert!(result.warnings.is_empty());
        assert_eq!(result.events_inserted, 0);
        assert_eq!(overview(&database).event_count, 1);
        assert_eq!(overview(&database).total_tokens, 25);
        let usages = database.list_usage_for_pricing().unwrap();
        assert_eq!(usages.len(), 1);
        let parsed = ParsedUsage {
            counts: TokenCounts::default(),
            cumulative_snapshot: None,
            timestamp: Utc::now(),
            model: None,
            session_id: None,
            project_path: None,
            project_name: None,
            source_event_id: Some("stable-source-event".into()),
            reported_cost_usd: None,
        };
        let old_source = SourceFile {
            path: old_path,
            provider: Provider::Trae,
            format: SourceFormat::Snapshot,
            session_id: None,
            project_path: None,
            project_name: None,
        };
        let new_source = SourceFile {
            path: new_path,
            ..old_source.clone()
        };
        assert_eq!(
            snapshot_event_id(&old_source, &parsed, None),
            snapshot_event_id(&new_source, &parsed, None)
        );
        assert_eq!(usages[0].id, snapshot_event_id(&new_source, &parsed, None));
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn codex_incremental_sync_is_idempotent_and_resumes_after_append() {
        let home = test_home("incremental");
        let source_path = home.join(".codex").join("sessions").join("session.jsonl");
        fs::write(
            &source_path,
            include_str!("../../../fixtures/codex/cumulative.jsonl"),
        )
        .unwrap();
        let database = Database::open_in_memory().unwrap();
        let engine = SyncEngine::with_adapters(
            database.clone(),
            vec![Box::new(CodexAdapter::with_home(home.clone()))],
        );

        let first = engine.sync_all().unwrap();
        assert_eq!(first.events_inserted, 3);
        assert_eq!(first.tokens_added, 180);
        assert_eq!(overview(&database).total_tokens, 180);

        let duplicate = engine.sync_all().unwrap();
        assert_eq!(duplicate.events_inserted, 0);
        assert_eq!(duplicate.tokens_added, 0);
        assert_eq!(overview(&database).total_tokens, 180);

        let mut file = OpenOptions::new().append(true).open(&source_path).unwrap();
        file.write_all(br#"{"type":"event_msg","timestamp":"2026-08-14T00:03:00Z","payload":{"model":"gpt-5.4","info":{"total_token_usage":{"input_tokens":200,"total_tokens":200},"event_id":"codex-cum-4"}}}
"#)
        .unwrap();
        let appended = engine.sync_all().unwrap();
        assert_eq!(appended.events_inserted, 1);
        assert_eq!(appended.tokens_added, 20);
        assert_eq!(overview(&database).total_tokens, 200);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn codex_associates_turn_context_model_and_rebuilds_old_parser_data() {
        let home = test_home("codex-model-context");
        let source_path = home.join(".codex").join("sessions").join("context.jsonl");
        fs::write(
            &source_path,
            concat!(
                "{\"type\":\"session_meta\",\"timestamp\":\"2026-08-14T00:00:00Z\",\"payload\":{\"id\":\"session-context\",\"cwd\":\"/tmp/project\"}}\n",
                "{\"type\":\"turn_context\",\"timestamp\":\"2026-08-14T00:00:01Z\",\"payload\":{\"model\":\"gpt-5.6-sol\",\"cwd\":\"/tmp/project\"}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-08-14T00:00:02Z\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":10,\"output_tokens\":3,\"total_tokens\":13}}}}\n",
            ),
        )
        .unwrap();

        let database = Database::open_in_memory().unwrap();
        database
            .insert_usage_events(&[UsageEvent {
                id: "old-parser-event".into(),
                provider: Provider::Codex,
                model: None,
                session_id: Some("session-context".into()),
                project_path: Some(PathBuf::from("/tmp/project")),
                project_name: Some("project".into()),
                timestamp: Utc::now(),
                input_tokens: 999,
                cached_input_tokens: 0,
                cache_creation_input_tokens: 0,
                output_tokens: 0,
                reasoning_tokens: 0,
                total_tokens: 999,
                reported_cost_usd: None,
                estimated_cost_usd: None,
                source_file: Some(source_path.clone()),
                source_event_id: None,
                snapshot_scope: None,
            }])
            .unwrap();
        database
            .upsert_cursor(&FileCursor::new(source_path.clone(), Provider::Codex, 1))
            .unwrap();

        let engine = SyncEngine::with_adapters(
            database.clone(),
            vec![Box::new(CodexAdapter::with_home(home.clone()))],
        );
        let result = engine.sync_all().unwrap();
        assert_eq!(result.events_inserted, 1);
        assert_eq!(overview(&database).total_tokens, 13);

        let recent = UsageRepository::new(database.clone())
            .get_recent_activity(1)
            .unwrap();
        assert_eq!(recent[0].model.as_deref(), Some("gpt-5.6-sol"));

        let cursor = database.get_cursor(&source_path).unwrap().unwrap();
        assert_eq!(cursor.source_metadata.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(
            cursor.source_metadata.project_path.as_deref(),
            Some(std::path::Path::new("/tmp/project"))
        );
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn codex_duplicate_snapshot_fixture_does_not_double_count() {
        let home = test_home("duplicate");
        let source_path = home.join(".codex").join("sessions").join("duplicate.jsonl");
        fs::write(
            &source_path,
            include_str!("../../../fixtures/codex/duplicate.jsonl"),
        )
        .unwrap();
        let database = Database::open_in_memory().unwrap();
        let engine = SyncEngine::with_adapters(
            database.clone(),
            vec![Box::new(CodexAdapter::with_home(home.clone()))],
        );
        let result = engine.sync_all().unwrap();
        assert_eq!(result.tokens_added, 180);
        assert_eq!(overview(&database).total_tokens, 180);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn codex_reset_fixture_starts_a_new_counter_period() {
        let home = test_home("reset");
        let source_path = home.join(".codex").join("sessions").join("reset.jsonl");
        fs::write(
            &source_path,
            include_str!("../../../fixtures/codex/reset.jsonl"),
        )
        .unwrap();
        let database = Database::open_in_memory().unwrap();
        let engine = SyncEngine::with_adapters(
            database.clone(),
            vec![Box::new(CodexAdapter::with_home(home.clone()))],
        );
        let result = engine.sync_all().unwrap();
        assert_eq!(result.events_inserted, 3);
        assert_eq!(result.tokens_added, 170);
        assert_eq!(overview(&database).total_tokens, 170);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn opencode_sqlite_snapshot_upserts_without_double_counting() {
        let home = test_home("opencode-sqlite");
        let root = home.join(".local").join("share").join("opencode");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("opencode.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session_v2 (
                    id TEXT PRIMARY KEY,
                    directory TEXT NOT NULL,
                    model TEXT,
                    tokens_input INTEGER NOT NULL DEFAULT 0,
                    tokens_output INTEGER NOT NULL DEFAULT 0,
                    tokens_reasoning INTEGER NOT NULL DEFAULT 0,
                    tokens_cache_read INTEGER NOT NULL DEFAULT 0,
                    tokens_cache_write INTEGER NOT NULL DEFAULT 0,
                    time_created INTEGER NOT NULL,
                    time_updated INTEGER NOT NULL
                );
                INSERT INTO session_v2 VALUES
                    ('s1', '/tmp/project', 'gpt-5.4', 10, 3, 1, 5, 2,
                     1786700000000, 1786700001000);",
            )
            .unwrap();
        drop(connection);

        let database = Database::open_in_memory().unwrap();
        let engine = SyncEngine::with_adapters(
            database.clone(),
            vec![Box::new(OpenCodeAdapter::with_home(home.clone()))],
        );
        let first = engine.sync_all().unwrap();
        assert_eq!(first.events_inserted, 1);
        assert_eq!(overview(&database).total_tokens, 21);

        let duplicate = engine.sync_all().unwrap();
        assert_eq!(duplicate.events_inserted, 0);
        assert_eq!(duplicate.tokens_added, 0);
        assert_eq!(overview(&database).total_tokens, 21);

        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "UPDATE session_v2
                 SET tokens_input = 20, time_updated = 1786700002000
                 WHERE id = 's1'",
                [],
            )
            .unwrap();
        drop(connection);
        let updated = engine.sync_all().unwrap();
        assert_eq!(updated.events_inserted, 0);
        assert_eq!(overview(&database).total_tokens, 31);
        let _ = fs::remove_dir_all(home);
    }
}
