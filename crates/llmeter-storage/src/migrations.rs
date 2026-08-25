use rusqlite::Connection;

use crate::StorageError;

pub fn run(connection: &Connection) -> Result<(), StorageError> {
    connection.execute_batch(
        r#"
        PRAGMA foreign_keys = ON;
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;

        CREATE TABLE IF NOT EXISTS usage_events (
            id TEXT PRIMARY KEY,
            provider TEXT NOT NULL,
            model TEXT,
            session_id TEXT,
            project_path TEXT,
            project_name TEXT,
            timestamp INTEGER NOT NULL,
            input_tokens INTEGER NOT NULL DEFAULT 0,
            cached_input_tokens INTEGER NOT NULL DEFAULT 0,
            cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
            output_tokens INTEGER NOT NULL DEFAULT 0,
            reasoning_tokens INTEGER NOT NULL DEFAULT 0,
            total_tokens INTEGER NOT NULL DEFAULT 0,
            reported_cost_usd REAL,
            estimated_cost_usd REAL,
            source_file TEXT,
            source_event_id TEXT,
            snapshot_scope TEXT,
            created_at INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_usage_events_timestamp ON usage_events(timestamp);
        CREATE INDEX IF NOT EXISTS idx_usage_events_provider ON usage_events(provider);
        CREATE INDEX IF NOT EXISTS idx_usage_events_model ON usage_events(model);
        CREATE INDEX IF NOT EXISTS idx_usage_events_project_path ON usage_events(project_path);
        CREATE INDEX IF NOT EXISTS idx_usage_events_session_id ON usage_events(session_id);
        CREATE INDEX IF NOT EXISTS idx_usage_events_source_file ON usage_events(source_file);

        CREATE TABLE IF NOT EXISTS file_cursors (
            path TEXT PRIMARY KEY,
            provider TEXT NOT NULL,
            file_identity TEXT,
            byte_offset INTEGER NOT NULL DEFAULT 0,
            file_size INTEGER NOT NULL DEFAULT 0,
            modified_at INTEGER,
            parser_version INTEGER NOT NULL,
            last_event_hash TEXT,
            last_cumulative_json TEXT,
            source_metadata_json TEXT,
            updated_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS app_settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS limit_snapshots (
            provider TEXT PRIMARY KEY,
            payload_json TEXT NOT NULL,
            captured_at INTEGER NOT NULL
        );
        "#,
    )?;

    if !has_column(connection, "file_cursors", "source_metadata_json")? {
        connection.execute(
            "ALTER TABLE file_cursors ADD COLUMN source_metadata_json TEXT",
            [],
        )?;
    }
    if !has_column(connection, "usage_events", "reported_cost_usd")? {
        connection.execute(
            "ALTER TABLE usage_events ADD COLUMN reported_cost_usd REAL",
            [],
        )?;
    }
    if !has_column(connection, "usage_events", "snapshot_scope")? {
        connection.execute(
            "ALTER TABLE usage_events ADD COLUMN snapshot_scope TEXT",
            [],
        )?;
    }
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version < 8 {
        // Remote snapshots are account-scoped. Keep legacy uniqueness for
        // unscoped events while allowing one official ID per signed-in account.
        connection.execute(
            "DROP INDEX IF EXISTS idx_usage_events_provider_source_event",
            [],
        )?;
        connection.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_usage_events_provider_source_event_legacy
             ON usage_events(provider, source_event_id)
             WHERE source_event_id IS NOT NULL AND snapshot_scope IS NULL",
            [],
        )?;
        connection.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_usage_events_provider_scope_source_event
             ON usage_events(provider, snapshot_scope, source_event_id)
             WHERE snapshot_scope IS NOT NULL AND source_event_id IS NOT NULL",
            [],
        )?;
        reattribute_omp_sessions(connection)?;
    }
    if version < 9 {
        connection.execute(
            "CREATE INDEX IF NOT EXISTS idx_usage_events_provider_timestamp
             ON usage_events(provider, timestamp)",
            [],
        )?;
        connection.execute(
            "CREATE INDEX IF NOT EXISTS idx_usage_events_provider_source_file
             ON usage_events(provider, source_file)",
            [],
        )?;
    }
    connection.pragma_update(None, "user_version", 9)?;
    Ok(())
}

fn has_column(connection: &Connection, table: &str, expected: &str) -> Result<bool, StorageError> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
    Ok(columns
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .any(|column| column == expected))
}

fn reattribute_omp_sessions(connection: &Connection) -> Result<(), StorageError> {
    let transaction = connection.unchecked_transaction()?;
    transaction.execute(
        "DELETE FROM usage_events
         WHERE id IN (
             SELECT pi.id
             FROM usage_events AS pi
             INNER JOIN usage_events AS keep
               ON keep.provider = 'omp'
              AND keep.source_event_id = pi.source_event_id
              AND (
                    (keep.snapshot_scope IS NULL AND pi.snapshot_scope IS NULL)
                    OR keep.snapshot_scope = pi.snapshot_scope
                  )
             WHERE pi.provider = 'pi'
               AND pi.source_event_id IS NOT NULL
               AND replace(pi.source_file, '\\', '/') LIKE '%/.omp/%'
         )",
        [],
    )?;
    transaction.execute(
        "UPDATE usage_events
         SET provider = 'omp'
         WHERE provider = 'pi'
           AND replace(source_file, '\\', '/') LIKE '%/.omp/%'",
        [],
    )?;
    transaction.execute(
        "UPDATE file_cursors
         SET provider = 'omp'
         WHERE provider = 'pi'
           AND replace(path, '\\', '/') LIKE '%/.omp/%'",
        [],
    )?;
    transaction.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_reported_cost_column_without_losing_existing_usage() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE usage_events (
                    id TEXT PRIMARY KEY,
                    provider TEXT NOT NULL,
                    model TEXT,
                    session_id TEXT,
                    project_path TEXT,
                    project_name TEXT,
                    timestamp INTEGER NOT NULL,
                    input_tokens INTEGER NOT NULL DEFAULT 0,
                    cached_input_tokens INTEGER NOT NULL DEFAULT 0,
                    cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
                    output_tokens INTEGER NOT NULL DEFAULT 0,
                    reasoning_tokens INTEGER NOT NULL DEFAULT 0,
                    total_tokens INTEGER NOT NULL DEFAULT 0,
                    estimated_cost_usd REAL,
                    source_file TEXT,
                    source_event_id TEXT,
                    created_at INTEGER NOT NULL
                );
                INSERT INTO usage_events (
                    id, provider, timestamp, total_tokens, created_at
                ) VALUES ('existing', 'grok', 1, 42, 1);",
            )
            .unwrap();

        run(&connection).unwrap();

        assert!(has_column(&connection, "usage_events", "reported_cost_usd").unwrap());
        assert!(has_column(&connection, "usage_events", "snapshot_scope").unwrap());
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM usage_events", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            9
        );
    }

    #[test]
    fn does_not_delete_omp_usage_on_later_opens() {
        let connection = Connection::open_in_memory().unwrap();
        run(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO usage_events (
                    id, provider, timestamp, total_tokens, source_file, created_at
                ) VALUES ('omp-1', 'omp', 1, 99, '/Users/me/.omp/agent/sessions/a.jsonl', 1)",
                [],
            )
            .unwrap();

        run(&connection).unwrap();

        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM usage_events WHERE provider = 'omp'",
                    [],
                    |row| { row.get::<_, i64>(0) }
                )
                .unwrap(),
            1
        );
    }

    #[test]
    fn reattributes_pi_omp_paths_without_deleting_rows() {
        let connection = Connection::open_in_memory().unwrap();
        run(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO usage_events (
                    id, provider, timestamp, total_tokens, source_file, created_at
                ) VALUES ('pi-omp', 'pi', 1, 11, '/Users/me/.omp/agent/sessions/a.jsonl', 1)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO file_cursors (
                    path, provider, byte_offset, file_size, parser_version, updated_at
                ) VALUES ('/Users/me/.omp/agent/sessions/a.jsonl', 'pi', 0, 0, 1, 1)",
                [],
            )
            .unwrap();
        connection.pragma_update(None, "user_version", 0).unwrap();

        run(&connection).unwrap();

        assert_eq!(
            connection
                .query_row(
                    "SELECT provider, total_tokens FROM usage_events WHERE id = 'pi-omp'",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .unwrap(),
            ("omp".into(), 11)
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT provider FROM file_cursors WHERE path LIKE '%a.jsonl'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "omp"
        );
    }

    #[test]
    fn reattribute_keeps_existing_omp_on_source_event_conflict() {
        let connection = Connection::open_in_memory().unwrap();
        run(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO usage_events (
                    id, provider, timestamp, total_tokens, source_file, source_event_id, created_at
                ) VALUES ('omp-keep', 'omp', 1, 7, '/Users/me/.omp/agent/sessions/a.jsonl', 'evt-1', 1)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO usage_events (
                    id, provider, timestamp, total_tokens, source_file, source_event_id, created_at
                ) VALUES ('pi-dup', 'pi', 1, 11, '/Users/me/.omp/agent/sessions/a.jsonl', 'evt-1', 1)",
                [],
            )
            .unwrap();
        connection.pragma_update(None, "user_version", 0).unwrap();

        run(&connection).unwrap();

        let rows = connection
            .prepare("SELECT id, provider, total_tokens FROM usage_events ORDER BY id")
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(rows, vec![("omp-keep".into(), "omp".into(), 7)]);
    }

    #[test]
    fn creates_composite_read_indexes() {
        let connection = Connection::open_in_memory().unwrap();
        run(&connection).unwrap();
        let names = connection
            .prepare("SELECT name FROM sqlite_master WHERE type = 'index' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            names
                .iter()
                .any(|name| name == "idx_usage_events_provider_timestamp")
        );
        assert!(
            names
                .iter()
                .any(|name| name == "idx_usage_events_provider_source_file")
        );
    }
}
