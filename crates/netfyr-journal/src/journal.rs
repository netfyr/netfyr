use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use chrono::Utc;
use flate2::write::GzEncoder;
use flate2::Compression;
use thiserror::Error;

use crate::entry::{JournalEntry, SequenceId};

// ── Error type ────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Invalid sequence number: {0}")]
    InvalidSequence(String),
}

pub type Result<T> = std::result::Result<T, JournalError>;

// ── Journal ───────────────────────────────────────────────────────────────────

const DEFAULT_MAX_ENTRIES: usize = 10_000;
const DEFAULT_MAX_SIZE: u64 = 50 * 1024 * 1024; // 50 MB
const DEFAULT_RETENTION_DAYS: u64 = 90;

pub struct Journal {
    dir: PathBuf,
    current_path: PathBuf,
    archive_dir: PathBuf,
    seq: SequenceId,
    entry_count: usize,
    max_entries: usize,
    max_size: u64,
    retention_days: u64,
}

impl Journal {
    /// Open or create the journal at the default directory.
    /// Reads NETFYR_JOURNAL_DIR env var, defaults to /var/lib/netfyr/journal/.
    pub fn open_default() -> Result<Self> {
        let dir = std::env::var("NETFYR_JOURNAL_DIR")
            .unwrap_or_else(|_| "/var/lib/netfyr/journal".to_string());
        Self::open(Path::new(&dir))
    }

    /// Open or create the journal at a specific directory.
    pub fn open(dir: &Path) -> Result<Self> {
        let archive_dir = dir.join("archive");
        std::fs::create_dir_all(&archive_dir)?;

        let current_path = dir.join("current.ndjson");

        let seq_file = Self::read_seq(dir)?;
        let last_line_seq = Self::read_last_seq(&current_path)?;
        let seq = seq_file.max(last_line_seq);

        let entry_count = Self::count_lines(&current_path)?;

        let max_entries = std::env::var("NETFYR_JOURNAL_MAX_ENTRIES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_ENTRIES);

        let max_size = std::env::var("NETFYR_JOURNAL_MAX_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_SIZE);

        let retention_days = std::env::var("NETFYR_JOURNAL_RETENTION_DAYS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_RETENTION_DAYS);

        let journal = Self {
            dir: dir.to_path_buf(),
            current_path,
            archive_dir,
            seq,
            entry_count,
            max_entries,
            max_size,
            retention_days,
        };

        let _ = journal.cleanup_archives(journal.retention_days);

        Ok(journal)
    }

    /// Append a journal entry. Assigns seq and timestamp, handles rotation.
    pub fn append(&mut self, mut entry: JournalEntry) -> Result<()> {
        // Open file in create/append mode
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.current_path)?;

        // Acquire advisory write lock (blocks until acquired)
        Self::lock_file_write(&file)?;

        // Assign sequence number; caller is responsible for setting timestamp.
        self.seq += 1;
        entry.seq = self.seq;

        // Serialize and write
        let json = serde_json::to_string(&entry)?;
        {
            let mut writer = std::io::BufWriter::new(&file);
            writer.write_all(json.as_bytes())?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }

        // Atomically persist the sequence number
        self.write_seq_atomic(self.seq)?;

        self.entry_count += 1;

        // Unlock (file descriptor closed when it goes out of scope, releasing lock)
        Self::unlock_file(&file)?;
        drop(file);

        // Check rotation thresholds
        let file_size = std::fs::metadata(&self.current_path)
            .map(|m| m.len())
            .unwrap_or(0);
        if self.entry_count >= self.max_entries || file_size >= self.max_size {
            self.rotate()?;
        }

        Ok(())
    }

    /// Read entries from the journal, most recent first (current.ndjson only).
    pub fn read_recent(&self, count: usize) -> Result<Vec<JournalEntry>> {
        let entries = self.read_current_entries()?;
        let start = entries.len().saturating_sub(count);
        let mut result: Vec<JournalEntry> = entries.into_iter().skip(start).collect();
        result.reverse();
        Ok(result)
    }

    /// Read a specific entry by sequence ID (current.ndjson only).
    pub fn read_entry(&self, seq: SequenceId) -> Result<Option<JournalEntry>> {
        let entries = self.read_current_entries()?;
        Ok(entries.into_iter().find(|e| e.seq == seq))
    }

    /// Get the latest state snapshot for a given entity name (current.ndjson only).
    pub fn latest_state_for(
        &self,
        entity_name: &str,
    ) -> Result<Option<crate::serializable::SerializableState>> {
        let entries = self.read_current_entries()?;
        for entry in entries.into_iter().rev() {
            if let Some(state) = entry
                .state_after
                .entities
                .into_iter()
                .find(|s| s.selector_name == entity_name)
            {
                return Ok(Some(state));
            }
        }
        Ok(None)
    }

    /// Rotate the current journal into the archive directory (gzip compressed).
    fn rotate(&mut self) -> Result<()> {
        let timestamp = Utc::now().format("%Y%m%dT%H%M%SZ");
        let archive_name = format!("journal-{}.ndjson.gz", timestamp);
        let archive_path = self.archive_dir.join(&archive_name);

        // Read current content
        let content = match std::fs::read(&self.current_path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
            Err(e) => return Err(JournalError::Io(e)),
        };

        // Write gzip-compressed archive
        let archive_file = std::fs::File::create(&archive_path)?;
        let mut encoder = GzEncoder::new(archive_file, Compression::default());
        encoder.write_all(&content)?;
        encoder.finish()?;

        // Recreate empty current.ndjson
        std::fs::File::create(&self.current_path)?;
        self.entry_count = 0;

        Ok(())
    }

    /// Remove archived journals older than the retention period.
    pub fn cleanup_archives(&self, retention_days: u64) -> Result<()> {
        let cutoff = Utc::now()
            - chrono::Duration::try_days(retention_days as i64).unwrap_or_default();

        let dir_entries = match std::fs::read_dir(&self.archive_dir) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(JournalError::Io(e)),
        };

        for entry in dir_entries {
            let entry = entry?;
            let path = entry.path();

            let fname = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };

            // Parse "journal-{timestamp}.ndjson.gz"
            if let Some(ts_part) = fname
                .strip_prefix("journal-")
                .and_then(|s| s.strip_suffix(".ndjson.gz"))
            {
                if let Ok(dt) =
                    chrono::NaiveDateTime::parse_from_str(ts_part, "%Y%m%dT%H%M%SZ")
                {
                    let dt_utc = dt.and_utc();
                    if dt_utc < cutoff {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
        }

        Ok(())
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn read_current_entries(&self) -> Result<Vec<JournalEntry>> {
        let content = match std::fs::read_to_string(&self.current_path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(JournalError::Io(e)),
        };

        let entries = content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str::<JournalEntry>(line).ok())
            .collect();

        Ok(entries)
    }

    fn lock_file_write(file: &std::fs::File) -> Result<()> {
        let fd = file.as_raw_fd();
        let mut fl = libc::flock {
            l_type: libc::F_WRLCK as libc::c_short,
            l_whence: libc::SEEK_SET as libc::c_short,
            l_start: 0,
            l_len: 0,
            l_pid: 0,
        };
        let ret = unsafe { libc::fcntl(fd, libc::F_SETLKW, &mut fl) };
        if ret == -1 {
            return Err(JournalError::Io(std::io::Error::last_os_error()));
        }
        Ok(())
    }

    fn unlock_file(file: &std::fs::File) -> Result<()> {
        let fd = file.as_raw_fd();
        let mut fl = libc::flock {
            l_type: libc::F_UNLCK as libc::c_short,
            l_whence: libc::SEEK_SET as libc::c_short,
            l_start: 0,
            l_len: 0,
            l_pid: 0,
        };
        let ret = unsafe { libc::fcntl(fd, libc::F_SETLK, &mut fl) };
        if ret == -1 {
            return Err(JournalError::Io(std::io::Error::last_os_error()));
        }
        Ok(())
    }

    fn write_seq_atomic(&self, seq: SequenceId) -> Result<()> {
        let tmp_path = self.dir.join(".seq.tmp");
        std::fs::write(&tmp_path, seq.to_string())?;
        std::fs::rename(&tmp_path, self.dir.join(".seq"))?;
        Ok(())
    }

    fn read_seq(dir: &Path) -> Result<SequenceId> {
        let path = dir.join(".seq");
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let trimmed = content.trim();
                trimmed.parse::<SequenceId>().map_err(|_| {
                    JournalError::InvalidSequence(trimmed.to_string())
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(JournalError::Io(e)),
        }
    }

    fn read_last_seq(path: &Path) -> Result<SequenceId> {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(JournalError::Io(e)),
        };

        let max_seq = content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter_map(|v| v.get("seq").and_then(|s| s.as_u64()))
            .max()
            .unwrap_or(0);

        Ok(max_seq)
    }

    fn count_lines(path: &Path) -> Result<usize> {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(JournalError::Io(e)),
        };
        Ok(content.lines().filter(|line| !line.trim().is_empty()).count())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{ApplyOutcome, JournalEntry, Trigger};
    use crate::serializable::{SerializableDiff, SerializableState, SerializableStateSet};
    use std::sync::Mutex;

    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    fn temp_dir() -> PathBuf {
        let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir()
            .join(format!("netfyr-journal-test-{}-{}", std::process::id(), id));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_entry() -> JournalEntry {
        JournalEntry {
            seq: 0,
            timestamp: Utc::now(),
            trigger: Trigger::PolicyApply { source: "test".to_string() },
            active_policies: vec![],
            diff: SerializableDiff { operations: vec![] },
            state_after: SerializableStateSet { entities: vec![] },
            outcome: ApplyOutcome::Applied { succeeded: 1, failed: 0, skipped: 0 },
        }
    }

    fn count_entries_in_gz(path: &Path) -> usize {
        use flate2::read::GzDecoder;
        use std::io::Read;
        let file = std::fs::File::open(path).unwrap();
        let mut decoder = GzDecoder::new(file);
        let mut content = String::new();
        decoder.read_to_string(&mut content).unwrap();
        content.lines().filter(|l| !l.trim().is_empty()).count()
    }

    fn list_archives(dir: &Path) -> Vec<std::fs::DirEntry> {
        std::fs::read_dir(dir.join("archive"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".ndjson.gz"))
            .collect()
    }

    /// AC: read_recent(5) returns all 5 entries in reverse chronological order (most recent first).
    #[test]
    fn test_append_and_read_recent_returns_entries_in_reverse_chronological_order() {
        let dir = temp_dir();
        let mut journal = Journal::open(&dir).unwrap();

        for _ in 0..5 {
            journal.append(make_entry()).unwrap();
        }

        let entries = journal.read_recent(5).unwrap();
        assert_eq!(entries.len(), 5, "read_recent(5) should return exactly 5 entries");

        for i in 0..entries.len() - 1 {
            assert!(
                entries[i].seq > entries[i + 1].seq,
                "entries should be in reverse order: entries[{}].seq={} > entries[{}].seq={}",
                i,
                entries[i].seq,
                i + 1,
                entries[i + 1].seq
            );
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    /// AC: each entry has a unique, monotonically increasing seq number (1-based).
    #[test]
    fn test_entries_have_unique_monotonically_increasing_seq_numbers() {
        let dir = temp_dir();
        let mut journal = Journal::open(&dir).unwrap();

        for _ in 0..5 {
            journal.append(make_entry()).unwrap();
        }

        let entries = journal.read_recent(5).unwrap();
        let mut seqs: Vec<u64> = entries.iter().map(|e| e.seq).collect();
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(seqs.len(), 5, "all 5 seq numbers must be unique");
        assert_eq!(seqs, vec![1, 2, 3, 4, 5], "seq numbers must be 1-based and contiguous");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// AC: sequence numbers persist across restarts — after 3 entries the next entry gets seq 4.
    #[test]
    fn test_sequence_numbers_persist_across_restarts() {
        let dir = temp_dir();

        {
            let mut journal = Journal::open(&dir).unwrap();
            for _ in 0..3 {
                journal.append(make_entry()).unwrap();
            }
            let entries = journal.read_recent(10).unwrap();
            assert_eq!(entries[0].seq, 3, "last entry before restart should have seq=3");
        }

        {
            let mut journal = Journal::open(&dir).unwrap();
            journal.append(make_entry()).unwrap();
            let entries = journal.read_recent(10).unwrap();
            assert_eq!(entries[0].seq, 4, "first entry after restart should have seq=4");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    /// AC: rotation triggers at entry count threshold — after max+1 entries, archive has max entries
    /// and current.ndjson has 1 entry.
    #[test]
    fn test_rotation_triggers_at_entry_count_threshold() {
        let dir = temp_dir();
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        // Safety: protected by ENV_MUTEX; set_var is unsafe in Rust ≥1.81.
        unsafe { std::env::set_var("NETFYR_JOURNAL_MAX_ENTRIES", "100") };
        let mut journal = Journal::open(&dir).unwrap();
        unsafe { std::env::remove_var("NETFYR_JOURNAL_MAX_ENTRIES") };

        for _ in 0..101 {
            journal.append(make_entry()).unwrap();
        }

        let archives = list_archives(&dir);
        assert_eq!(
            archives.len(),
            1,
            "exactly 1 archive should exist after 101 entries with max_entries=100"
        );

        let archived_count = count_entries_in_gz(&archives[0].path());
        assert_eq!(archived_count, 100, "archive should contain 100 entries");

        let current = std::fs::read_to_string(dir.join("current.ndjson")).unwrap_or_default();
        let current_count = current.lines().filter(|l| !l.trim().is_empty()).count();
        assert_eq!(current_count, 1, "current.ndjson should have exactly 1 entry (the 101st)");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// AC: rotation triggers at file size threshold — archive appears after current.ndjson exceeds limit.
    #[test]
    fn test_rotation_triggers_at_file_size_threshold() {
        let dir = temp_dir();
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        // Safety: protected by ENV_MUTEX; set_var is unsafe in Rust ≥1.81.
        unsafe { std::env::set_var("NETFYR_JOURNAL_MAX_SIZE", "1024") };
        let mut journal = Journal::open(&dir).unwrap();
        unsafe { std::env::remove_var("NETFYR_JOURNAL_MAX_SIZE") };

        // Each entry is ~200+ bytes; 20 entries will exceed 1024 bytes and trigger rotation.
        for _ in 0..20 {
            journal.append(make_entry()).unwrap();
        }

        let archives = list_archives(&dir);
        assert!(
            !archives.is_empty(),
            "at least 1 archive should exist after size-triggered rotation (1024 byte limit)"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// AC: cleanup_archives deletes archives older than retention_days and keeps recent ones.
    #[test]
    fn test_archive_cleanup_deletes_old_archives_keeps_recent() {
        let dir = temp_dir();
        let archive_dir = dir.join("archive");
        std::fs::create_dir_all(&archive_dir).unwrap();

        let now = Utc::now();
        let old_dt = now - chrono::Duration::try_days(100).unwrap();
        let recent_dt = now - chrono::Duration::try_days(10).unwrap();

        let old_name = format!("journal-{}.ndjson.gz", old_dt.format("%Y%m%dT%H%M%SZ"));
        let recent_name = format!("journal-{}.ndjson.gz", recent_dt.format("%Y%m%dT%H%M%SZ"));

        std::fs::write(archive_dir.join(&old_name), b"").unwrap();
        std::fs::write(archive_dir.join(&recent_name), b"").unwrap();

        // Journal::open() calls cleanup_archives with default 90-day retention.
        let _journal = Journal::open(&dir).unwrap();

        assert!(
            !archive_dir.join(&old_name).exists(),
            "100-day-old archive should be deleted (retention=90 days)"
        );
        assert!(
            archive_dir.join(&recent_name).exists(),
            "10-day-old archive should be kept (retention=90 days)"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// AC: read_entry(5) returns the entry with seq 5 from a journal with entries 1..=10.
    #[test]
    fn test_read_entry_by_sequence_id_returns_correct_entry() {
        let dir = temp_dir();
        let mut journal = Journal::open(&dir).unwrap();

        for _ in 0..10 {
            journal.append(make_entry()).unwrap();
        }

        let entry = journal.read_entry(5).unwrap();
        assert!(entry.is_some(), "read_entry(5) should return Some");
        assert_eq!(entry.unwrap().seq, 5, "returned entry should have seq=5");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// AC: read_entry returns None for a seq that does not exist.
    #[test]
    fn test_read_entry_returns_none_for_nonexistent_seq() {
        let dir = temp_dir();
        let mut journal = Journal::open(&dir).unwrap();
        journal.append(make_entry()).unwrap();

        let entry = journal.read_entry(999).unwrap();
        assert!(entry.is_none(), "read_entry for nonexistent seq should return None");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// AC: journal directory is configurable via NETFYR_JOURNAL_DIR env var.
    #[test]
    fn test_journal_dir_configurable_via_env_var() {
        let dir = temp_dir();
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        // Safety: protected by ENV_MUTEX; set_var is unsafe in Rust ≥1.81.
        unsafe { std::env::set_var("NETFYR_JOURNAL_DIR", dir.to_str().unwrap()) };
        let result = Journal::open_default();
        unsafe { std::env::remove_var("NETFYR_JOURNAL_DIR") };

        assert!(result.is_ok(), "Journal::open_default() should succeed when NETFYR_JOURNAL_DIR is set");
        assert!(
            dir.join("archive").exists(),
            "archive subdir should be created in the configured NETFYR_JOURNAL_DIR"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// AC: latest_state_for returns the most recent snapshot for the named entity.
    #[test]
    fn test_latest_state_for_returns_most_recent_state_snapshot() {
        let dir = temp_dir();
        let mut journal = Journal::open(&dir).unwrap();

        let mut entry1 = make_entry();
        entry1.state_after = SerializableStateSet {
            entities: vec![SerializableState {
                entity_type: "ethernet".to_string(),
                selector_name: "eth0".to_string(),
                fields: serde_json::json!({ "mtu": 1500u64 }),
            }],
        };
        journal.append(entry1).unwrap();

        let mut entry2 = make_entry();
        entry2.state_after = SerializableStateSet {
            entities: vec![SerializableState {
                entity_type: "ethernet".to_string(),
                selector_name: "eth0".to_string(),
                fields: serde_json::json!({ "mtu": 9000u64 }),
            }],
        };
        journal.append(entry2).unwrap();

        let state = journal.latest_state_for("eth0").unwrap();
        assert!(state.is_some(), "should find a state snapshot for eth0");
        let state = state.unwrap();
        assert_eq!(
            state.fields["mtu"],
            serde_json::json!(9000u64),
            "latest_state_for should return the most recent snapshot (mtu=9000)"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// read_recent with count larger than total entries returns all entries.
    #[test]
    fn test_read_recent_with_count_larger_than_total_returns_all() {
        let dir = temp_dir();
        let mut journal = Journal::open(&dir).unwrap();

        for _ in 0..3 {
            journal.append(make_entry()).unwrap();
        }

        let entries = journal.read_recent(100).unwrap();
        assert_eq!(entries.len(), 3, "read_recent(100) on 3-entry journal should return 3");

        std::fs::remove_dir_all(&dir).ok();
    }
}
