use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use chrono::{DateTime, Utc};
use llmeter_core::{FileCursor, Provider, TokenCounts, UsageEvent};
use rusqlite::{
    Connection, OptionalExtension, Transaction, params, params_from_iter, types::Value,
};
use thiserror::Error;

use crate::migrations;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("database mutex was poisoned")]
    MutexPoisoned,
    #[error("numeric value cannot be represented in SQLite")]
    NumericOverflow,
}

#[derive(Clone)]
pub struct Database {
    pub(crate) connection: Arc<Mutex<Connection>>,
    path: PathBuf,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InsertSummary {
    pub inserted: usize,
    pub tokens_added: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UpsertSummary {
    pub inserted: usize,
    pub updated: usize,
    pub tokens_added: u64,
}

impl Database {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(&path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        migrations::run(&connection)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            path,
        })
    }

    pub fn open_in_memory() -> Result<Self, StorageError> {
        let connection = Connection::open_in_memory()?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        migrations::run(&connection)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            path: PathBuf::from(":memory:"),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn lock(&self) -> Result<MutexGuard<'_, Connection>, StorageError> {
        self.connection
            .lock()
            .map_err(|_| StorageError::MutexPoisoned)
    }

    pub fn insert_usage_events(&self, events: &[UsageEvent]) -> Result<usize, StorageError> {
        Ok(self.insert_usage_events_with_summary(events)?.inserted)
    }

    pub fn insert_usage_events_with_summary(
        &self,
        events: &[UsageEvent],
    ) -> Result<InsertSummary, StorageError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let mut statement = transaction.prepare(
            "INSERT OR IGNORE INTO usage_events (
                id, provider, model, session_id, project_path, project_name, timestamp,
                input_tokens, cached_input_tokens, cache_creation_input_tokens, output_tokens,
                reasoning_tokens, total_tokens, reported_cost_usd, estimated_cost_usd,
                source_file, source_event_id, snapshot_scope, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
        )?;
        let mut summary = InsertSummary::default();
        for event in events {
            let inserted = statement.execute(params![
                event.id,
                event.provider.as_str(),
                event.model,
                event.session_id,
                event
                    .project_path
                    .as_ref()
                    .map(|value| value.to_string_lossy().to_string()),
                event.project_name,
                event.timestamp.timestamp(),
                to_sqlite_i64(event.input_tokens)?,
                to_sqlite_i64(event.cached_input_tokens)?,
                to_sqlite_i64(event.cache_creation_input_tokens)?,
                to_sqlite_i64(event.output_tokens)?,
                to_sqlite_i64(event.reasoning_tokens)?,
                to_sqlite_i64(event.total_tokens)?,
                event.reported_cost_usd,
                event.estimated_cost_usd,
                event
                    .source_file
                    .as_ref()
                    .map(|value| value.to_string_lossy().to_string()),
                event.source_event_id,
                event.snapshot_scope,
                chrono::Utc::now().timestamp(),
            ])?;
            if inserted > 0 {
                summary.inserted += inserted;
                summary.tokens_added = summary.tokens_added.saturating_add(event.total_tokens);
            }
        }
        drop(statement);
        transaction.commit()?;
        Ok(summary)
    }

    pub fn upsert_usage_events_with_summary(
        &self,
        events: &[UsageEvent],
    ) -> Result<UpsertSummary, StorageError> {
        if events.is_empty() {
            return Ok(UpsertSummary::default());
        }
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let summary = upsert_usage_events(&transaction, events)?;
        transaction.commit()?;
        Ok(summary)
    }

    pub fn replace_usage_events_for_source(
        &self,
        provider: Provider,
        source: &Path,
        since: Option<DateTime<Utc>>,
        events: &[UsageEvent],
    ) -> Result<UpsertSummary, StorageError> {
        self.replace_usage_events(provider, Some(source), since, None, events)
    }

    pub fn replace_usage_events_for_provider(
        &self,
        provider: Provider,
        since: Option<DateTime<Utc>>,
        events: &[UsageEvent],
    ) -> Result<UpsertSummary, StorageError> {
        self.replace_usage_events(provider, None, since, None, events)
    }

    pub fn replace_usage_events_for_provider_scoped(
        &self,
        provider: Provider,
        since: Option<DateTime<Utc>>,
        snapshot_scope: Option<&str>,
        events: &[UsageEvent],
    ) -> Result<UpsertSummary, StorageError> {
        self.replace_usage_events(provider, None, since, snapshot_scope, events)
    }

    fn replace_usage_events(
        &self,
        provider: Provider,
        source: Option<&Path>,
        since: Option<DateTime<Utc>>,
        snapshot_scope: Option<&str>,
        events: &[UsageEvent],
    ) -> Result<UpsertSummary, StorageError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let source = source.map(|value| value.to_string_lossy().to_string());
        if source.is_none()
            && let Some(snapshot_scope) = snapshot_scope
        {
            // Claim unscoped rows for this account before the window delete.
            transaction.execute(
                "UPDATE usage_events
                 SET snapshot_scope = ?2
                 WHERE provider = ?1 AND snapshot_scope IS NULL",
                params![provider.as_str(), snapshot_scope],
            )?;
        }
        let (existing_ids, previous_tokens) = {
            let mut statement = transaction.prepare(
                "SELECT id, total_tokens FROM usage_events
                WHERE provider = ?1
                   AND (?2 IS NULL OR source_file = ?2)
                   AND (?3 IS NULL OR timestamp >= ?3)
                   AND (?4 IS NULL OR snapshot_scope = ?4)",
            )?;
            let rows = statement.query_map(
                params![
                    provider.as_str(),
                    source,
                    since.map(|value| value.timestamp()),
                    snapshot_scope,
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )?;
            let mut database_ids = HashSet::new();
            let mut event_ids = HashSet::new();
            let mut tokens = 0_u64;
            for row in rows {
                let (id, total) = row?;
                database_ids.insert(id.clone());
                event_ids.insert(id);
                tokens = tokens.saturating_add(from_sqlite_u64(total));
            }
            drop(statement);

            let identities = ExistingEventIndex::load(&transaction, events)?;
            for event in events {
                if let Some((database_id, total)) = identities.get(event) {
                    event_ids.insert(event.id.clone());
                    if database_ids.insert(database_id) {
                        tokens = tokens.saturating_add(from_sqlite_u64(total));
                    }
                }
            }
            (event_ids, tokens)
        };
        transaction.execute(
            "DELETE FROM usage_events
             WHERE provider = ?1
               AND (?2 IS NULL OR source_file = ?2)
               AND (?3 IS NULL OR timestamp >= ?3)
               AND (?4 IS NULL OR snapshot_scope = ?4)",
            params![
                provider.as_str(),
                source,
                since.map(|value| value.timestamp()),
                snapshot_scope,
            ],
        )?;
        upsert_usage_events(&transaction, events)?;
        let inserted = events
            .iter()
            .filter(|event| !existing_ids.contains(&event.id))
            .count();
        let current_tokens = events.iter().fold(0_u64, |total, event| {
            total.saturating_add(event.total_tokens)
        });
        let summary = UpsertSummary {
            inserted,
            updated: events.len().saturating_sub(inserted),
            tokens_added: current_tokens.saturating_sub(previous_tokens),
        };
        transaction.commit()?;
        Ok(summary)
    }

    pub fn get_cursor(&self, path: &Path) -> Result<Option<FileCursor>, StorageError> {
        let connection = self.lock()?;
        connection
            .query_row(
                "SELECT path, provider, file_identity, byte_offset, file_size, modified_at,
                        parser_version, last_event_hash, last_cumulative_json,
                        source_metadata_json, updated_at
                 FROM file_cursors WHERE path = ?1",
                params![path.to_string_lossy().to_string()],
                |row| {
                    let provider_text: String = row.get(1)?;
                    let provider = provider_text.parse::<Provider>().map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            1,
                            rusqlite::types::Type::Text,
                            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
                        )
                    })?;
                    let cumulative_json: Option<String> = row.get(8)?;
                    let last_cumulative = cumulative_json
                        .as_deref()
                        .map(serde_json::from_str)
                        .transpose()
                        .map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                8,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?;
                    let source_metadata_json: Option<String> = row.get(9)?;
                    let source_metadata = source_metadata_json
                        .as_deref()
                        .map(serde_json::from_str)
                        .transpose()
                        .map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                9,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?
                        .unwrap_or_default();
                    Ok(FileCursor {
                        path: PathBuf::from(row.get::<_, String>(0)?),
                        provider,
                        file_identity: row.get(2)?,
                        byte_offset: from_sqlite_u64(row.get(3)?),
                        file_size: from_sqlite_u64(row.get(4)?),
                        modified_at: row.get(5)?,
                        parser_version: row.get::<_, i64>(6)?.max(0) as u32,
                        last_event_hash: row.get(7)?,
                        last_cumulative,
                        source_metadata,
                        updated_at: row.get(10)?,
                    })
                },
            )
            .optional()
            .map_err(StorageError::from)
    }

    pub fn upsert_cursor(&self, cursor: &FileCursor) -> Result<(), StorageError> {
        let connection = self.lock()?;
        let cumulative_json = cursor
            .last_cumulative
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let source_metadata_json = serde_json::to_string(&cursor.source_metadata)?;
        connection.execute(
            "INSERT INTO file_cursors (
                path, provider, file_identity, byte_offset, file_size, modified_at,
                parser_version, last_event_hash, last_cumulative_json, source_metadata_json,
                updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            ON CONFLICT(path) DO UPDATE SET
                provider = excluded.provider,
                file_identity = excluded.file_identity,
                byte_offset = excluded.byte_offset,
                file_size = excluded.file_size,
                modified_at = excluded.modified_at,
                parser_version = excluded.parser_version,
                last_event_hash = excluded.last_event_hash,
                last_cumulative_json = excluded.last_cumulative_json,
                source_metadata_json = excluded.source_metadata_json,
                updated_at = excluded.updated_at",
            params![
                cursor.path.to_string_lossy().to_string(),
                cursor.provider.as_str(),
                cursor.file_identity,
                to_sqlite_i64(cursor.byte_offset)?,
                to_sqlite_i64(cursor.file_size)?,
                cursor.modified_at,
                i64::from(cursor.parser_version),
                cursor.last_event_hash,
                cumulative_json,
                source_metadata_json,
                cursor.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>, StorageError> {
        let connection = self.lock()?;
        connection
            .query_row(
                "SELECT value FROM app_settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(StorageError::from)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), StorageError> {
        let connection = self.lock()?;
        connection.execute(
            "INSERT INTO app_settings(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn delete_usage_for_source(
        &self,
        path: &Path,
        provider: Provider,
    ) -> Result<usize, StorageError> {
        let connection = self.lock()?;
        connection
            .execute(
                "DELETE FROM usage_events WHERE provider = ?1 AND source_file = ?2",
                params![provider.as_str(), path.to_string_lossy().to_string()],
            )
            .map_err(StorageError::from)
    }

    pub fn clear_usage_and_cursors(&self) -> Result<(), StorageError> {
        let connection = self.lock()?;
        connection.execute_batch("DELETE FROM usage_events; DELETE FROM file_cursors;")?;
        Ok(())
    }

    pub fn clear_usage_and_cursors_for_providers(
        &self,
        providers: &[Provider],
    ) -> Result<(), StorageError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        for provider in providers {
            transaction.execute(
                "DELETE FROM usage_events WHERE provider = ?1",
                params![provider.as_str()],
            )?;
            transaction.execute(
                "DELETE FROM file_cursors WHERE provider = ?1",
                params![provider.as_str()],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn list_usage_for_pricing(&self) -> Result<Vec<UsagePricingInput>, StorageError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT id, provider, model, input_tokens, cached_input_tokens,
                    cache_creation_input_tokens, output_tokens, reasoning_tokens,
                    estimated_cost_usd
             FROM usage_events",
        )?;
        let rows = statement.query_map([], |row| {
            let provider_text: String = row.get(1)?;
            let provider = provider_text.parse::<Provider>().map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
                )
            })?;
            Ok(UsagePricingInput {
                id: row.get(0)?,
                provider,
                model: row.get(2)?,
                counts: TokenCounts {
                    input_tokens: from_sqlite_u64(row.get(3)?),
                    cached_input_tokens: from_sqlite_u64(row.get(4)?),
                    cache_creation_input_tokens: from_sqlite_u64(row.get(5)?),
                    output_tokens: from_sqlite_u64(row.get(6)?),
                    reasoning_tokens: from_sqlite_u64(row.get(7)?),
                    total_tokens: 0,
                },
                estimated_cost_usd: row.get(8)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }

    pub fn update_estimated_costs(
        &self,
        updates: &[(String, Option<f64>)],
    ) -> Result<usize, StorageError> {
        if updates.is_empty() {
            return Ok(0);
        }
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        {
            let mut statement = transaction
                .prepare("UPDATE usage_events SET estimated_cost_usd = ?2 WHERE id = ?1")?;
            for (id, cost) in updates {
                statement.execute(params![id, cost])?;
            }
        }
        transaction.commit()?;
        Ok(updates.len())
    }
}

const IDENTITY_QUERY_CHUNK: usize = 400;

struct ExistingEventIndex {
    by_id: HashMap<String, (String, i64)>,
    by_source: Vec<(String, String, Option<String>, String, i64)>,
}

impl ExistingEventIndex {
    fn load(transaction: &Transaction<'_>, events: &[UsageEvent]) -> Result<Self, StorageError> {
        let mut index = Self {
            by_id: HashMap::new(),
            by_source: Vec::new(),
        };
        if events.is_empty() {
            return Ok(index);
        }
        let ids = events
            .iter()
            .map(|event| event.id.clone())
            .collect::<Vec<_>>();
        let source_ids = events
            .iter()
            .filter_map(|event| event.source_event_id.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        for chunk in ids.chunks(IDENTITY_QUERY_CHUNK) {
            index.extend_from_query(transaction, "id", chunk)?;
        }
        for chunk in source_ids.chunks(IDENTITY_QUERY_CHUNK) {
            index.extend_from_query(transaction, "source_event_id", chunk)?;
        }
        Ok(index)
    }

    fn extend_from_query(
        &mut self,
        transaction: &Transaction<'_>,
        column: &str,
        values: &[String],
    ) -> Result<(), StorageError> {
        if values.is_empty() {
            return Ok(());
        }
        let sql = format!(
            "SELECT id, provider, source_event_id, snapshot_scope, total_tokens
             FROM usage_events WHERE {column} IN ({})",
            placeholders(values.len()),
        );
        let values = values.iter().cloned().map(Value::Text).collect::<Vec<_>>();
        let mut statement = transaction.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(values), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        for row in rows {
            let (id, provider, source_event_id, snapshot_scope, total) = row?;
            self.by_id.insert(id.clone(), (id.clone(), total));
            if let Some(source_event_id) = source_event_id {
                self.by_source
                    .push((provider, source_event_id, snapshot_scope, id, total));
            }
        }
        Ok(())
    }

    fn get(&self, event: &UsageEvent) -> Option<(String, i64)> {
        if let Some(existing) = self.by_id.get(&event.id) {
            return Some(existing.clone());
        }
        let source_event_id = event.source_event_id.as_deref()?;
        let provider = event.provider.as_str();
        let mut null_scope = None;
        for (candidate_provider, candidate_source, scope, id, tokens) in &self.by_source {
            if candidate_provider != provider || candidate_source != source_event_id {
                continue;
            }
            if scope.as_deref() == event.snapshot_scope.as_deref() {
                return Some((id.clone(), *tokens));
            }
            if scope.is_none() {
                null_scope = Some((id.clone(), *tokens));
            }
        }
        null_scope
    }
}

fn placeholders(count: usize) -> String {
    std::iter::repeat_n("?", count)
        .collect::<Vec<_>>()
        .join(",")
}

fn upsert_usage_events(
    transaction: &Transaction<'_>,
    events: &[UsageEvent],
) -> Result<UpsertSummary, StorageError> {
    let existing = ExistingEventIndex::load(transaction, events)?;
    let mut statement = transaction.prepare(
        "INSERT INTO usage_events (
            id, provider, model, session_id, project_path, project_name, timestamp,
            input_tokens, cached_input_tokens, cache_creation_input_tokens, output_tokens,
            reasoning_tokens, total_tokens, reported_cost_usd, estimated_cost_usd,
            source_file, source_event_id, snapshot_scope, created_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
        ON CONFLICT DO UPDATE SET
            id = excluded.id,
            provider = excluded.provider,
            model = excluded.model,
            session_id = excluded.session_id,
            project_path = excluded.project_path,
            project_name = excluded.project_name,
            timestamp = excluded.timestamp,
            input_tokens = excluded.input_tokens,
            cached_input_tokens = excluded.cached_input_tokens,
            cache_creation_input_tokens = excluded.cache_creation_input_tokens,
            output_tokens = excluded.output_tokens,
            reasoning_tokens = excluded.reasoning_tokens,
            total_tokens = excluded.total_tokens,
            reported_cost_usd = excluded.reported_cost_usd,
            estimated_cost_usd = excluded.estimated_cost_usd,
            source_file = excluded.source_file,
            source_event_id = excluded.source_event_id,
            snapshot_scope = excluded.snapshot_scope",
    )?;
    let mut summary = UpsertSummary::default();
    for event in events {
        let existing_id = existing.get(event).map(|(id, _)| id);
        // Rekey NULL-scope official IDs so the scoped unique index updates in place.
        if let Some(existing_id) = existing_id.as_deref()
            && existing_id != event.id
        {
            transaction.execute(
                "UPDATE usage_events SET id = ?1, snapshot_scope = ?2 WHERE id = ?3",
                params![event.id, event.snapshot_scope, existing_id],
            )?;
        }
        statement.execute(params![
            event.id,
            event.provider.as_str(),
            event.model,
            event.session_id,
            event
                .project_path
                .as_ref()
                .map(|value| value.to_string_lossy().to_string()),
            event.project_name,
            event.timestamp.timestamp(),
            to_sqlite_i64(event.input_tokens)?,
            to_sqlite_i64(event.cached_input_tokens)?,
            to_sqlite_i64(event.cache_creation_input_tokens)?,
            to_sqlite_i64(event.output_tokens)?,
            to_sqlite_i64(event.reasoning_tokens)?,
            to_sqlite_i64(event.total_tokens)?,
            event.reported_cost_usd,
            event.estimated_cost_usd,
            event
                .source_file
                .as_ref()
                .map(|value| value.to_string_lossy().to_string()),
            event.source_event_id,
            event.snapshot_scope,
            chrono::Utc::now().timestamp(),
        ])?;
        if existing_id.is_some() {
            summary.updated += 1;
        } else {
            summary.inserted += 1;
            summary.tokens_added = summary.tokens_added.saturating_add(event.total_tokens);
        }
    }
    Ok(summary)
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsagePricingInput {
    pub id: String,
    pub provider: Provider,
    pub model: Option<String>,
    pub counts: TokenCounts,
    pub estimated_cost_usd: Option<f64>,
}

fn to_sqlite_i64(value: u64) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::NumericOverflow)
}

pub(crate) fn from_sqlite_u64(value: i64) -> u64 {
    value.max(0) as u64
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use chrono::Utc;
    use llmeter_core::{Provider, UsageEvent, UsageSnapshot};

    use super::*;
    use crate::{DashboardQuery, SessionQuery, UsageRepository};

    fn event(id: &str, source_event_id: Option<&str>, total: u64) -> UsageEvent {
        UsageEvent {
            id: id.into(),
            provider: Provider::Codex,
            model: Some("gpt-5.4".into()),
            session_id: Some("session".into()),
            project_path: Some(PathBuf::from("/tmp/project")),
            project_name: Some("project".into()),
            timestamp: Utc::now(),
            input_tokens: total,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: 0,
            total_tokens: total,
            reported_cost_usd: None,
            estimated_cost_usd: Some(0.1),
            source_file: Some(PathBuf::from("/tmp/session.jsonl")),
            source_event_id: source_event_id.map(str::to_string),
            snapshot_scope: None,
        }
    }

    fn overview(database: &Database) -> crate::Overview {
        UsageRepository::new(database.clone())
            .get_overview(
                chrono::DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
                Utc::now() + chrono::Duration::days(1),
            )
            .unwrap()
    }

    #[test]
    fn duplicate_event_id_and_source_id_do_not_double_count() {
        let database = Database::open_in_memory().unwrap();
        let first = event("one", Some("official-1"), 100);
        let duplicate_id = event("one", Some("official-1"), 100);
        let duplicate_source = event("different-id", Some("official-1"), 100);
        let summary = database
            .insert_usage_events_with_summary(&[first, duplicate_id, duplicate_source])
            .unwrap();
        assert_eq!(summary.inserted, 1);
        assert_eq!(summary.tokens_added, 100);

        let repository = UsageRepository::new(database);
        let start = chrono::DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        let overview = repository
            .get_overview(start, Utc::now() + chrono::Duration::seconds(1))
            .unwrap();
        assert_eq!(overview.total_tokens, 100);
        assert_eq!(overview.event_count, 1);
    }

    #[test]
    fn upsert_rekeys_an_existing_official_event_after_its_source_path_changes() {
        let database = Database::open_in_memory().unwrap();
        let mut original = event("legacy-path-id", Some("official-1"), 100);
        original.source_file = Some(PathBuf::from("/old/snapshot.json"));
        database
            .upsert_usage_events_with_summary(&[original])
            .unwrap();

        let mut moved = event("stable-official-id", Some("official-1"), 120);
        moved.source_file = Some(PathBuf::from("/new/snapshot.json"));
        let summary = database.upsert_usage_events_with_summary(&[moved]).unwrap();

        assert_eq!(summary.inserted, 0);
        assert_eq!(summary.updated, 1);
        assert_eq!(summary.tokens_added, 0);
        let usage = database.list_usage_for_pricing().unwrap();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].id, "stable-official-id");
        let overview = UsageRepository::new(database)
            .get_overview(
                chrono::DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
                Utc::now() + chrono::Duration::seconds(1),
            )
            .unwrap();
        assert_eq!(overview.event_count, 1);
        assert_eq!(overview.total_tokens, 120);
    }

    #[test]
    fn provider_scoped_clear_preserves_remote_usage_and_cursors() {
        let database = Database::open_in_memory().unwrap();
        let local_path = PathBuf::from("/tmp/local.jsonl");
        let remote_path = PathBuf::from("/tmp/remote.json");
        let mut local = event("local", Some("local"), 10);
        local.source_file = Some(local_path.clone());
        let mut remote = event("remote", Some("remote"), 20);
        remote.provider = Provider::Trae;
        remote.source_file = Some(remote_path.clone());
        database.insert_usage_events(&[local, remote]).unwrap();
        database
            .upsert_cursor(&FileCursor::new(local_path.clone(), Provider::Codex, 1))
            .unwrap();
        database
            .upsert_cursor(&FileCursor::new(remote_path.clone(), Provider::Trae, 1))
            .unwrap();

        database
            .clear_usage_and_cursors_for_providers(&[Provider::Codex])
            .unwrap();

        assert!(database.get_cursor(&local_path).unwrap().is_none());
        assert!(database.get_cursor(&remote_path).unwrap().is_some());
        let overview = UsageRepository::new(database)
            .get_overview(
                chrono::DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
                Utc::now() + chrono::Duration::seconds(1),
            )
            .unwrap();
        assert_eq!(overview.event_count, 1);
        assert_eq!(overview.total_tokens, 20);
    }

    #[test]
    fn snapshot_replacement_is_authoritative_for_the_entire_source() {
        let database = Database::open_in_memory().unwrap();
        let now = Utc::now();
        let mut old = event("old", Some("old"), 10);
        old.timestamp = now - chrono::Duration::days(10);
        let mut covered = event("covered", Some("covered"), 20);
        covered.timestamp = now - chrono::Duration::days(2);
        database.insert_usage_events(&[old, covered]).unwrap();

        let mut replacement = event("replacement", Some("replacement"), 30);
        replacement.timestamp = now - chrono::Duration::days(3);
        let first = database
            .replace_usage_events_for_source(
                Provider::Codex,
                Path::new("/tmp/session.jsonl"),
                None,
                std::slice::from_ref(&replacement),
            )
            .unwrap();
        assert_eq!(first.inserted, 1);
        assert_eq!(first.tokens_added, 0);
        let second = database
            .replace_usage_events_for_source(
                Provider::Codex,
                Path::new("/tmp/session.jsonl"),
                None,
                std::slice::from_ref(&replacement),
            )
            .unwrap();
        assert_eq!(second.inserted, 0);
        assert_eq!(second.updated, 1);
        assert_eq!(second.tokens_added, 0);

        let overview = UsageRepository::new(database.clone())
            .get_overview(
                chrono::DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
                now + chrono::Duration::seconds(1),
            )
            .unwrap();
        assert_eq!(overview.event_count, 1);
        assert_eq!(overview.total_tokens, 30);

        database
            .replace_usage_events_for_source(
                Provider::Codex,
                Path::new("/tmp/session.jsonl"),
                None,
                &[],
            )
            .unwrap();
        let overview = UsageRepository::new(database)
            .get_overview(
                chrono::DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
                now + chrono::Duration::seconds(1),
            )
            .unwrap();
        assert_eq!(overview.event_count, 0);
        assert_eq!(overview.total_tokens, 0);
    }

    #[test]
    fn empty_snapshot_window_clears_covered_rows_and_preserves_older_history() {
        let database = Database::open_in_memory().unwrap();
        let now = Utc::now();
        let mut old = event("old", Some("old"), 10);
        old.timestamp = now - chrono::Duration::days(10);
        let mut covered = event("covered", Some("covered"), 20);
        covered.timestamp = now - chrono::Duration::days(2);
        database.insert_usage_events(&[old, covered]).unwrap();

        database
            .replace_usage_events_for_source(
                Provider::Codex,
                Path::new("/tmp/session.jsonl"),
                Some(now - chrono::Duration::days(3)),
                &[],
            )
            .unwrap();

        let overview = UsageRepository::new(database)
            .get_overview(
                chrono::DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
                now + chrono::Duration::seconds(1),
            )
            .unwrap();
        assert_eq!(overview.event_count, 1);
        assert_eq!(overview.total_tokens, 10);
    }

    #[test]
    fn provider_snapshot_window_cleans_covered_rows_from_old_source_paths() {
        let database = Database::open_in_memory().unwrap();
        let now = Utc::now();
        let mut historical = event("historical", Some("historical"), 10);
        historical.provider = Provider::Trae;
        historical.source_file = Some(PathBuf::from("/old/trae/storage.json"));
        historical.timestamp = now - chrono::Duration::days(40);
        let mut stale = event("stale", Some("stale"), 20);
        stale.provider = Provider::Trae;
        stale.source_file = Some(PathBuf::from("/old/trae/storage.json"));
        stale.timestamp = now - chrono::Duration::days(2);
        let mut unrelated = event("unrelated", Some("unrelated"), 30);
        unrelated.provider = Provider::Cursor;
        unrelated.source_file = Some(PathBuf::from("/cursor/state.vscdb"));
        unrelated.timestamp = now - chrono::Duration::days(2);
        database
            .insert_usage_events(&[historical, stale, unrelated])
            .unwrap();

        database
            .replace_usage_events_for_provider(
                Provider::Trae,
                Some(now - chrono::Duration::days(30)),
                &[],
            )
            .unwrap();

        let overview = UsageRepository::new(database)
            .get_overview(
                chrono::DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
                now + chrono::Duration::seconds(1),
            )
            .unwrap();
        assert_eq!(overview.event_count, 2);
        assert_eq!(overview.total_tokens, 40);
    }

    #[test]
    fn scoped_remote_snapshots_keep_accounts_separate() {
        let database = Database::open_in_memory().unwrap();
        let mut account_a = event("cursor-a", Some("session-1"), 10);
        account_a.provider = Provider::Cursor;
        account_a.snapshot_scope = Some("account-a".into());
        let mut account_b = event("cursor-b", Some("session-1"), 20);
        account_b.provider = Provider::Cursor;
        account_b.snapshot_scope = Some("account-b".into());

        database
            .replace_usage_events_for_provider_scoped(
                Provider::Cursor,
                None,
                Some("account-a"),
                std::slice::from_ref(&account_a),
            )
            .unwrap();
        database
            .replace_usage_events_for_provider_scoped(
                Provider::Cursor,
                None,
                Some("account-b"),
                std::slice::from_ref(&account_b),
            )
            .unwrap();
        assert_eq!(overview(&database).total_tokens, 30);

        account_b.total_tokens = 3;
        account_b.input_tokens = 3;
        database
            .replace_usage_events_for_provider_scoped(
                Provider::Cursor,
                None,
                Some("account-b"),
                std::slice::from_ref(&account_b),
            )
            .unwrap();
        assert_eq!(overview(&database).total_tokens, 13);

        database
            .replace_usage_events_for_provider_scoped(
                Provider::Cursor,
                None,
                Some("account-a"),
                &[],
            )
            .unwrap();
        assert_eq!(overview(&database).total_tokens, 3);
    }

    #[test]
    fn reported_cost_takes_priority_in_usage_aggregates() {
        let database = Database::open_in_memory().unwrap();
        let mut usage = event("reported", Some("reported-1"), 100);
        usage.cached_input_tokens = 60;
        usage.cache_creation_input_tokens = 7;
        usage.reported_cost_usd = Some(0.25);
        usage.estimated_cost_usd = Some(9.0);
        database.insert_usage_events(&[usage]).unwrap();

        let repository = UsageRepository::new(database);
        let start = chrono::DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        let end = Utc::now() + chrono::Duration::seconds(1);
        assert_eq!(
            repository
                .get_overview(start, end)
                .unwrap()
                .estimated_cost_usd,
            Some(0.25)
        );
        assert_eq!(
            repository.get_daily_usage(start, end).unwrap()[0].estimated_cost_usd,
            Some(0.25)
        );
        let provider_usage = &repository.get_provider_usage(start, end).unwrap()[0];
        assert_eq!(provider_usage.cached_input_tokens, 60);
        assert_eq!(provider_usage.cache_creation_input_tokens, 7);
        assert_eq!(provider_usage.estimated_cost_usd, Some(0.25));
        assert_eq!(
            repository.get_model_usage(start, end).unwrap()[0].estimated_cost_usd,
            Some(0.25)
        );
        assert_eq!(
            repository.get_project_usage(start, end).unwrap()[0].estimated_cost_usd,
            Some(0.25)
        );
        assert_eq!(
            repository.get_sessions().unwrap()[0].estimated_cost_usd,
            Some(0.25)
        );
    }

    #[test]
    fn cursor_round_trip_preserves_identity_offset_cumulative_state_and_metadata() {
        let database = Database::open_in_memory().unwrap();
        let mut cursor = FileCursor::new(PathBuf::from("/tmp/session.jsonl"), Provider::Codex, 7);
        cursor.file_identity = Some("dev:ino".into());
        cursor.byte_offset = 42;
        cursor.file_size = 50;
        cursor.last_cumulative = Some(UsageSnapshot {
            input_tokens: 40,
            total_tokens: 42,
            ..Default::default()
        });
        cursor.source_metadata.model = Some("gpt-5.6-sol".into());
        cursor.source_metadata.project_path = Some(PathBuf::from("/tmp/project"));
        database.upsert_cursor(&cursor).unwrap();
        assert_eq!(database.get_cursor(&cursor.path).unwrap(), Some(cursor));
    }

    #[test]
    fn sessions_are_aggregated_from_usage_events() {
        let database = Database::open_in_memory().unwrap();
        let started = Utc::now();
        let mut first = event("one", Some("a"), 100);
        first.timestamp = started;
        first.session_id =
            Some("rollout-2026-08-14T15-01-51-019fff13-bd73-7c71-a844-4dbe59993141".into());
        first.project_name = Some("fe-ai-nexus".into());
        let mut second = event("two", Some("b"), 50);
        second.timestamp = started + chrono::Duration::minutes(12);
        second.session_id = first.session_id.clone();
        second.project_name = Some("fe-ai-nexus".into());
        database
            .insert_usage_events_with_summary(&[first, second])
            .unwrap();

        let sessions = UsageRepository::new(database).get_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title(), "fe-ai-nexus");
        assert_eq!(sessions[0].turn_count, 2);
        assert_eq!(sessions[0].total_tokens, 150);
        assert_eq!(
            sessions[0].resume_command().as_deref(),
            Some("codex resume 019fff13-bd73-7c71-a844-4dbe59993141")
        );
    }

    #[test]
    fn project_usage_merges_same_name_across_worktree_paths() {
        let database = Database::open_in_memory().unwrap();
        let started = Utc::now() - chrono::Duration::hours(1);
        let mut main = event("main", Some("a"), 100);
        main.timestamp = started;
        main.project_name = Some("workstation-web".into());
        main.project_path = Some(PathBuf::from(
            "/Users/liulang/WebstormProjects/workstation-web",
        ));
        let mut worktree = event("worktree", Some("b"), 50);
        worktree.timestamp = started + chrono::Duration::minutes(5);
        worktree.project_name = Some("workstation-web".into());
        worktree.project_path = Some(PathBuf::from(
            "/Users/liulang/WebstormProjects/worktrees/workstation-web/loyal-yak/workstation-web",
        ));
        let mut other = event("other", Some("c"), 25);
        other.timestamp = started + chrono::Duration::minutes(6);
        other.project_name = Some("llmeter".into());
        other.project_path = Some(PathBuf::from("/Users/liulang/lang-projects/llmeter"));
        database
            .insert_usage_events_with_summary(&[main, worktree, other])
            .unwrap();

        let start = chrono::DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        let projects = UsageRepository::new(database)
            .get_project_usage(start, Utc::now() + chrono::Duration::seconds(1))
            .unwrap();
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].project_name, "workstation-web");
        assert_eq!(projects[0].total_tokens, 150);
        assert_eq!(projects[1].project_name, "llmeter");
        assert_eq!(projects[1].total_tokens, 25);
    }

    #[test]
    fn dashboard_load_matches_individual_queries() {
        let database = Database::open_in_memory().unwrap();
        let now = Utc::now();
        let mut today = event("today", Some("t"), 10);
        today.timestamp = now;
        let mut week = event("week", Some("w"), 20);
        week.timestamp = now - chrono::Duration::days(3);
        week.provider = Provider::Claude;
        let mut month = event("month", Some("m"), 30);
        month.timestamp = now - chrono::Duration::days(20);
        month.provider = Provider::Pi;
        database.insert_usage_events(&[today, week, month]).unwrap();

        let repository = UsageRepository::new(database);
        let end = now + chrono::Duration::seconds(1);
        let today_start = now - chrono::Duration::hours(1);
        let seven_start = now - chrono::Duration::days(7);
        let thirty_start = now - chrono::Duration::days(30);
        let loaded = repository
            .load_dashboard(DashboardQuery {
                today_start,
                seven_start,
                thirty_start,
                heatmap_start: now - chrono::Duration::days(147),
                overview_start: thirty_start,
                overview_end: end,
                now_end: end,
                sessions: Some(SessionQuery::default()),
            })
            .unwrap();

        assert_eq!(
            loaded.today.total_tokens,
            repository
                .get_overview(today_start, end)
                .unwrap()
                .total_tokens
        );
        assert_eq!(
            loaded.seven_days.total_tokens,
            repository
                .get_overview(seven_start, end)
                .unwrap()
                .total_tokens
        );
        assert_eq!(
            loaded.thirty_days.total_tokens,
            repository
                .get_overview(thirty_start, end)
                .unwrap()
                .total_tokens
        );
        assert_eq!(loaded.session_count, 3);
        assert_eq!(loaded.sessions.len(), 3);
        assert_eq!(loaded.providers.len(), 3);
    }

    #[test]
    fn sessions_can_be_filtered_by_provider_and_end_time() {
        let database = Database::open_in_memory().unwrap();
        let now = Utc::now();
        let mut recent_codex = event("recent-codex", Some("a"), 10);
        recent_codex.timestamp = now;
        recent_codex.session_id = Some("codex-new".into());
        let mut old_codex = event("old-codex", Some("b"), 20);
        old_codex.timestamp = now - chrono::Duration::days(40);
        old_codex.session_id = Some("codex-old".into());
        let mut recent_claude = event("recent-claude", Some("c"), 30);
        recent_claude.provider = Provider::Claude;
        recent_claude.timestamp = now - chrono::Duration::days(2);
        recent_claude.session_id = Some("claude-new".into());
        database
            .insert_usage_events(&[recent_codex, old_codex, recent_claude])
            .unwrap();

        let repository = UsageRepository::new(database);
        assert_eq!(repository.get_sessions().unwrap().len(), 3);
        assert_eq!(
            repository
                .get_sessions_matching(SessionQuery {
                    provider: Some(Provider::Codex),
                    ended_after: None,
                })
                .unwrap()
                .len(),
            2
        );
        let recent = repository
            .get_sessions_matching(SessionQuery {
                provider: Some(Provider::Codex),
                ended_after: Some(now - chrono::Duration::days(7)),
            })
            .unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].session_id.as_deref(), Some("codex-new"));
        assert_eq!(
            repository
                .load_dashboard(DashboardQuery {
                    today_start: now - chrono::Duration::hours(1),
                    seven_start: now - chrono::Duration::days(7),
                    thirty_start: now - chrono::Duration::days(30),
                    heatmap_start: now - chrono::Duration::days(147),
                    overview_start: now - chrono::Duration::days(30),
                    overview_end: now + chrono::Duration::seconds(1),
                    now_end: now + chrono::Duration::seconds(1),
                    sessions: Some(SessionQuery {
                        provider: Some(Provider::Claude),
                        ended_after: None,
                    }),
                })
                .unwrap()
                .session_count,
            3,
            "overview conversation count stays unfiltered"
        );
    }
}
