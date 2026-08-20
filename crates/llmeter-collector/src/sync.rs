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
    providers::{ParsedUsage, ProviderAdapter, default_adapters},
};

#[derive(Clone, Debug, Default)]
pub struct SyncOptions {
    pub providers: Option<HashSet<Provider>>,
}

impl SyncOptions {
    pub fn only(provider: Provider) -> Self {
        Self {
            providers: Some(HashSet::from([provider])),
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
        Self::with_adapters(database, default_adapters())
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
            if source.format == SourceFormat::Sqlite {
                self.sync_sqlite_source(adapter, &source, result)?;
            } else {
                self.sync_source(adapter, &source, result)?;
            }
        }
        Ok(())
    }

    fn sync_sqlite_source(
        &self,
        adapter: &dyn ProviderAdapter,
        source: &SourceFile,
        result: &mut SyncResult,
    ) -> Result<()> {
        result.files_scanned += 1;
        let parsed = adapter.parse_sqlite(source)?;
        result.events_seen += parsed.len();
        let events = parsed
            .iter()
            .map(|parsed| self.to_usage_event(source, parsed, parsed.counts, 0))
            .collect::<Vec<_>>();
        let summary = self.database.upsert_usage_events_with_summary(&events)?;
        result.events_inserted += summary.inserted;
        result.tokens_added = result.tokens_added.saturating_add(summary.tokens_added);

        let metadata = std::fs::metadata(&source.path)?;
        let mut cursor = self.database.get_cursor(&source.path)?.unwrap_or_else(|| {
            FileCursor::new(
                source.path.clone(),
                source.provider,
                adapter.parser_version(),
            )
        });
        cursor.file_identity = Some(format!("sqlite:{}", metadata.len()));
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
            events.push(self.to_usage_event(source, &parsed, counts, line.byte_start));
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
        let id = if source.format == SourceFormat::Sqlite {
            sqlite_event_id(source, parsed, session_id.as_deref())
        } else {
            event_id(
                source.provider,
                &source.path,
                session_id.as_deref(),
                parsed,
                counts,
                byte_start,
            )
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
            estimated_cost_usd: parsed
                .reported_cost_usd
                .or_else(|| estimate_cost_usd(source.provider, parsed.model.as_deref(), counts)),
            source_file: Some(source.path.clone()),
            source_event_id: parsed.source_event_id.clone(),
        }
    }
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
                estimated_cost_usd: None,
                source_file: Some(source_path.clone()),
                source_event_id: None,
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
