use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
    time::UNIX_EPOCH,
};

use llmeter_core::FileCursor;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedLine {
    pub byte_start: u64,
    pub byte_end: u64,
    pub raw: Vec<u8>,
}

#[derive(Debug)]
pub struct IncrementalRead {
    pub lines: Vec<ParsedLine>,
    pub next_offset: u64,
    pub file_identity: Option<String>,
    pub file_size: u64,
    pub modified_at: Option<i64>,
    pub reset: bool,
}

#[derive(Default)]
pub struct IncrementalJsonlReader;

impl IncrementalJsonlReader {
    pub fn read(path: &Path, cursor: &FileCursor) -> io::Result<IncrementalRead> {
        let metadata = std::fs::metadata(path)?;
        let file_size = metadata.len();
        let file_identity = metadata_identity(&metadata);
        let identity_changed = cursor
            .file_identity
            .as_ref()
            .zip(file_identity.as_ref())
            .is_some_and(|(previous, current)| previous != current);
        let truncated = file_size < cursor.byte_offset;
        let reset = identity_changed || truncated;
        let offset = if reset {
            0
        } else {
            cursor.byte_offset.min(file_size)
        };

        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;

        let mut lines = Vec::new();
        let mut line_start = 0usize;
        for (index, byte) in bytes.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            let mut raw = bytes[line_start..index].to_vec();
            if raw.last() == Some(&b'\r') {
                raw.pop();
            }
            let byte_start = offset + line_start as u64;
            let byte_end = offset + index as u64 + 1;
            lines.push(ParsedLine {
                byte_start,
                byte_end,
                raw,
            });
            line_start = index + 1;
        }

        // A tail without a newline is deliberately held back. This covers an
        // incomplete JSON object and avoids advancing a cursor past a line
        // which the producer may still be writing.
        let next_offset = offset + line_start as u64;
        let modified_at = metadata_modified_at(&metadata);

        Ok(IncrementalRead {
            lines,
            next_offset,
            file_identity,
            file_size,
            modified_at,
            reset,
        })
    }

    pub fn is_unchanged(path: &Path, cursor: &FileCursor) -> io::Result<bool> {
        let metadata = std::fs::metadata(path)?;
        let identity = metadata_identity(&metadata);
        Ok(cursor.file_identity == identity
            && cursor.file_size == metadata.len()
            && cursor.modified_at == metadata_modified_at(&metadata))
    }
}

pub fn metadata_identity(metadata: &std::fs::Metadata) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(format!("{}:{}", metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        Some(format!("{}:{:?}", metadata.len(), metadata.modified().ok()))
    }
}

pub fn metadata_modified_at(metadata: &std::fs::Metadata) -> Option<i64> {
    metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .and_then(|value| i64::try_from(value.as_millis()).ok())
}

#[cfg(test)]
mod tests {
    use std::{fs::OpenOptions, io::Write, path::PathBuf};

    use llmeter_core::{FileCursor, Provider};

    use super::IncrementalJsonlReader;

    fn temp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("llmeter-jsonl-{label}-{}", std::process::id()))
    }

    #[test]
    fn reads_only_complete_appended_lines() {
        let path = temp_path("append");
        let _ = std::fs::remove_file(&path);
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(b"{\"a\":1}\n").unwrap();
        let cursor = FileCursor::new(path.clone(), Provider::Codex, 1);
        let first = IncrementalJsonlReader::read(&path, &cursor).unwrap();
        assert_eq!(first.lines.len(), 1);
        assert_eq!(first.lines[0].byte_start, 0);
        assert_eq!(first.next_offset, first.file_size);

        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"b\":").unwrap();
        let mut second_cursor = cursor;
        second_cursor.file_identity = first.file_identity;
        second_cursor.byte_offset = first.next_offset;
        let second = IncrementalJsonlReader::read(&path, &second_cursor).unwrap();
        assert!(second.lines.is_empty());
        assert_eq!(second.next_offset, first.next_offset);

        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"2}\n").unwrap();
        let third = IncrementalJsonlReader::read(&path, &second_cursor).unwrap();
        assert_eq!(third.lines.len(), 1);
        assert_eq!(third.lines[0].raw, b"{\"b\":2}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn detects_truncate_and_file_replace() {
        let path = temp_path("reset");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, b"one\ntwo\n").unwrap();
        let cursor = FileCursor::new(path.clone(), Provider::Codex, 1);
        let first = IncrementalJsonlReader::read(&path, &cursor).unwrap();
        let mut stale = cursor;
        stale.byte_offset = 1000;
        stale.file_identity = first.file_identity;
        let reset = IncrementalJsonlReader::read(&path, &stale).unwrap();
        assert!(reset.reset);
        assert_eq!(reset.lines.len(), 2);

        let replacement = path.with_extension("replacement");
        std::fs::write(&replacement, b"new\n").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let replaced = IncrementalJsonlReader::read(&path, &first_cursor(&path)).unwrap();
        assert_eq!(replaced.lines.len(), 1);
        assert!(replaced.reset);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn fixture_tail_is_held_until_json_line_is_completed() {
        let path = temp_path("fixture-tail");
        let _ = std::fs::remove_file(&path);
        let mut fixture = include_bytes!("../../../../fixtures/codex/truncated.jsonl").to_vec();
        while fixture.last() == Some(&b'\n') {
            fixture.pop();
        }
        std::fs::write(&path, fixture).unwrap();
        let cursor = FileCursor::new(path.clone(), Provider::Codex, 1);
        let first = IncrementalJsonlReader::read(&path, &cursor).unwrap();
        assert_eq!(first.lines.len(), 1);
        assert!(first.next_offset < first.file_size);

        let mut resumed = cursor;
        resumed.file_identity = first.file_identity;
        resumed.byte_offset = first.next_offset;
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(
            br#"{"input_tokens":100,"total_tokens":100}}}}
"#,
        )
        .unwrap();
        let second = IncrementalJsonlReader::read(&path, &resumed).unwrap();
        assert_eq!(second.lines.len(), 1);
        assert_eq!(second.next_offset, second.file_size);
        let _ = std::fs::remove_file(path);
    }

    fn first_cursor(path: &std::path::Path) -> FileCursor {
        let mut cursor = FileCursor::new(path.to_path_buf(), Provider::Codex, 1);
        cursor.file_identity = Some("old:identity".into());
        cursor.byte_offset = 6;
        cursor
    }
}
