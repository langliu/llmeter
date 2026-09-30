//! Shared read-only SQLite access for provider databases and transcript
//! stores. Every provider's local database is foreign state that LLMeter must
//! never mutate, so all connections here open read-only.

use std::path::Path;

use anyhow::Result;
use rusqlite::Connection;

/// Opens an existing database file with every write capability disabled.
pub(crate) fn open_read_only(path: &Path) -> Result<Connection> {
    Ok(Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?)
}

/// Quotes a table or column name so it can be interpolated into SQL.
pub(crate) fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

/// Whether the database contains a user table with this exact name.
pub(crate) fn table_exists(connection: &Connection, table: &str) -> bool {
    connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get::<_, i64>(0),
        )
        .is_ok()
}

/// Column names of `table`; empty when the table does not exist.
pub(crate) fn table_columns(connection: &Connection, table: &str) -> Result<Vec<String>> {
    let pragma = format!("PRAGMA table_info({})", quote_identifier(table));
    let mut statement = connection.prepare(&pragma)?;
    statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// Whether `table` exists and carries every column in `required`. Providers
/// only add columns over time, so a superset schema stays supported while a
/// missing table or column marks the installed version as unsupported.
pub(crate) fn table_has_columns(
    connection: &Connection,
    table: &str,
    required: &[&str],
) -> Result<bool> {
    if !table_exists(connection, table) {
        return Ok(false);
    }
    let columns = table_columns(connection, table)?;
    Ok(required
        .iter()
        .all(|required| columns.iter().any(|column| column == required)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT);
                 CREATE TABLE 'odd ''name' (value INTEGER);",
            )
            .unwrap();
        connection
    }

    #[test]
    fn table_has_columns_requires_the_table_and_every_column() {
        let connection = connection();
        assert!(table_exists(&connection, "session"));
        assert!(!table_exists(&connection, "missing"));

        assert!(table_has_columns(&connection, "session", &["id", "directory"]).unwrap());
        assert!(!table_has_columns(&connection, "session", &["id", "cwd"]).unwrap());
        assert!(!table_has_columns(&connection, "missing", &["id"]).unwrap());
    }

    #[test]
    fn quote_identifier_round_trips_odd_table_names() {
        let connection = connection();
        let columns = table_columns(&connection, "odd 'name").unwrap();
        assert_eq!(columns, vec!["value".to_string()]);
        assert!(table_columns(&connection, "missing").unwrap().is_empty());
    }
}
