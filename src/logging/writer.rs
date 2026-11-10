use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};

const MAX_PENDING_BYTES: usize = 4 * 1024 * 1024;
const MAX_PENDING_RECORDS: usize = 1_024;
const BATCH_RECORDS: usize = 64;

#[derive(Default)]
struct Pending {
    bytes: usize,
    records: usize,
}

#[derive(Default)]
struct Budget {
    pending: Mutex<Pending>,
    available: Condvar,
}

impl Budget {
    fn reserve(self: &Arc<Self>, bytes: usize) -> Permit {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        // One oversized record is allowed only when the queue is empty. Logs
        // are never silently dropped when the bounded queue needs backpressure.
        while pending.records >= MAX_PENDING_RECORDS
            || (pending.records > 0 && pending.bytes.saturating_add(bytes) > MAX_PENDING_BYTES)
        {
            pending = self
                .available
                .wait(pending)
                .unwrap_or_else(|e| e.into_inner());
        }
        pending.bytes += bytes;
        pending.records += 1;
        Permit {
            budget: self.clone(),
            bytes,
        }
    }
}

struct Permit {
    budget: Arc<Budget>,
    bytes: usize,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut pending = self
            .budget
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        pending.bytes -= self.bytes;
        pending.records -= 1;
        self.budget.available.notify_all();
    }
}

struct Record {
    path: PathBuf,
    line: String,
    _permit: Permit,
}

enum Command {
    Record(Record),
    Flush(mpsc::Sender<()>),
}

struct BackgroundWriter {
    send: mpsc::SyncSender<Command>,
    budget: Arc<Budget>,
}

impl BackgroundWriter {
    fn start() -> io::Result<Self> {
        let (send, receive) = mpsc::sync_channel(MAX_PENDING_RECORDS);
        std::thread::Builder::new()
            .name("proxy-log-writer".into())
            .spawn(move || {
                let mut file = LogFile::new(super::MAX_LOG_BYTES);
                while let Ok(first) = receive.recv() {
                    file.begin_batch();
                    let mut command = first;
                    for index in 0..BATCH_RECORDS {
                        match command {
                            Command::Record(record) => {
                                let _ = file.write(&record.path, record.line);
                            }
                            Command::Flush(done) => {
                                let _ = file.flush();
                                let _ = done.send(());
                            }
                        }
                        if index + 1 == BATCH_RECORDS {
                            break;
                        }
                        match receive.try_recv() {
                            Ok(next) => command = next,
                            Err(_) => break,
                        }
                    }
                    let _ = file.flush();
                }
                let _ = file.flush();
            })?;
        Ok(Self {
            send,
            budget: Arc::new(Budget::default()),
        })
    }

    fn write(&self, path: PathBuf, line: String) -> io::Result<()> {
        let permit = self.budget.reserve(line.len().saturating_add(1));
        let command = Command::Record(Record {
            path,
            line,
            _permit: permit,
        });
        if let Err(mpsc::SendError(Command::Record(record))) = self.send.send(command) {
            return super::write_log_line_sync(&record.path, record.line);
        }
        Ok(())
    }

    fn flush(&self) {
        let (done, completed) = mpsc::channel();
        if self.send.send(Command::Flush(done)).is_ok() {
            let _ = completed.recv();
        }
    }
}

static WRITER: OnceLock<Option<BackgroundWriter>> = OnceLock::new();

pub(super) fn write(path: PathBuf, line: String) -> io::Result<()> {
    match WRITER.get_or_init(|| BackgroundWriter::start().ok()) {
        Some(writer) => writer.write(path, line),
        None => super::write_log_line_sync(&path, line),
    }
}

pub(super) fn flush() {
    if let Some(Some(writer)) = WRITER.get() {
        writer.flush();
    }
}

#[derive(PartialEq, Eq)]
struct Identity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(not(unix))]
    created: Option<std::time::SystemTime>,
}

impl Identity {
    fn of(metadata: &Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Self {
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(not(unix))]
            created: metadata.created().ok(),
        }
    }
}

struct OpenLog {
    path: PathBuf,
    output: BufWriter<File>,
    bytes: u64,
    identity: Identity,
}

struct LogFile {
    current: Option<OpenLog>,
    checked: bool,
    limit: u64,
}

impl LogFile {
    fn new(limit: u64) -> Self {
        Self {
            current: None,
            checked: false,
            limit,
        }
    }
    fn begin_batch(&mut self) {
        self.checked = false;
    }

    fn open(&mut self, path: &Path) -> io::Result<()> {
        self.close()?;
        if let Some(directory) = path.parent() {
            super::create_dir(directory, 0o700)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let metadata = file.metadata()?;
        self.current = Some(OpenLog {
            path: path.into(),
            output: BufWriter::with_capacity(64 * 1024, file),
            bytes: metadata.len(),
            identity: Identity::of(&metadata),
        });
        self.checked = true;
        Ok(())
    }

    fn write(&mut self, path: &Path, mut line: String) -> io::Result<()> {
        let mut reopen = self.current.as_ref().is_none_or(|file| file.path != path);
        if !reopen && !self.checked {
            match fs::metadata(path) {
                Ok(metadata) => {
                    let file = self.current.as_mut().expect("open log");
                    reopen = file.identity != Identity::of(&metadata);
                    if !reopen {
                        file.bytes = metadata.len();
                    }
                }
                Err(_) => reopen = true,
            }
            self.checked = true;
        }
        if reopen {
            self.open(path)?;
        }
        if self
            .current
            .as_ref()
            .is_some_and(|file| file.bytes > self.limit)
        {
            self.close()?;
            super::rotate_file(path)?;
            self.open(path)?;
        }
        let file = self.current.as_mut().expect("open log");
        // Keep the newline in the same write as its record, including records
        // larger than the buffer. Other CLI/server processes may append here.
        line.push('\n');
        file.output.write_all(line.as_bytes())?;
        file.bytes = file.bytes.saturating_add(line.len() as u64);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = &mut self.current {
            file.output.flush()?;
        }
        Ok(())
    }

    fn close(&mut self) -> io::Result<()> {
        if let Some(mut file) = self.current.take() {
            file.output.flush()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn background_flush_preserves_every_record_and_producer_order() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("proxy.log");
        let writer = Arc::new(BackgroundWriter::start().unwrap());
        std::thread::scope(|scope| {
            for producer in 0..8 {
                let writer = writer.clone();
                let path = path.clone();
                scope.spawn(move || {
                    for sequence in 0..1_000 {
                        writer
                            .write(
                                path.clone(),
                                serde_json::json!({"producer":producer,"sequence":sequence})
                                    .to_string(),
                            )
                            .unwrap();
                    }
                });
            }
        });
        writer.flush();
        let mut last = [None; 8];
        let mut count = 0;
        for line in fs::read_to_string(&path).unwrap().lines() {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            let producer = value["producer"].as_u64().unwrap() as usize;
            let sequence = value["sequence"].as_u64().unwrap();
            assert_eq!(sequence, last[producer].map_or(0, |previous| previous + 1));
            last[producer] = Some(sequence);
            count += 1;
        }
        assert_eq!(count, 8_000);
    }

    #[test]
    fn rotation_retains_existing_and_all_new_records() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("proxy.log");
        fs::write(&path, "existing\n").unwrap();
        let mut writer = LogFile::new(128);
        for index in 0..200 {
            writer.write(&path, format!("record-{index}")).unwrap();
        }
        writer.close().unwrap();
        let mut actual = HashSet::new();
        for entry in fs::read_dir(directory.path()).unwrap() {
            for line in fs::read_to_string(entry.unwrap().path()).unwrap().lines() {
                assert!(actual.insert(line.to_owned()), "duplicate record");
            }
        }
        assert_eq!(actual.len(), 201);
        assert!(actual.contains("existing"));
        for index in 0..200 {
            assert!(actual.contains(&format!("record-{index}")));
        }
    }

    #[test]
    fn path_changes_keep_records_in_their_original_files() {
        let directory = tempfile::TempDir::new().unwrap();
        let first = directory.path().join("first.log");
        let second = directory.path().join("second.log");
        let writer = BackgroundWriter::start().unwrap();
        writer.write(first.clone(), "first".into()).unwrap();
        writer.write(second.clone(), "second".into()).unwrap();
        writer.write(first.clone(), "third".into()).unwrap();
        writer.flush();
        assert_eq!(fs::read_to_string(first).unwrap(), "first\nthird\n");
        assert_eq!(fs::read_to_string(second).unwrap(), "second\n");
    }

    #[cfg(unix)]
    #[test]
    fn external_rotation_reopens_the_current_path() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("proxy.log");
        let old = directory.path().join("old.log");
        let mut writer = LogFile::new(1_024);
        writer.write(&path, "before".into()).unwrap();
        writer.flush().unwrap();
        fs::rename(&path, &old).unwrap();
        fs::write(&path, "other\n").unwrap();
        writer.begin_batch();
        writer.write(&path, "after".into()).unwrap();
        writer.flush().unwrap();
        assert_eq!(fs::read_to_string(old).unwrap(), "before\n");
        assert_eq!(fs::read_to_string(path).unwrap(), "other\nafter\n");
    }

    #[test]
    fn independent_writers_keep_large_records_and_newlines_together() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("proxy.log");
        let large = serde_json::json!({"text": "x".repeat(70_000)});
        let small = serde_json::json!({"text": "second"});
        let mut first = LogFile::new(1_000_000);
        let mut second = LogFile::new(1_000_000);
        first.write(&path, large.to_string()).unwrap();
        second.write(&path, small.to_string()).unwrap();
        second.flush().unwrap();
        first.flush().unwrap();
        let records: Vec<serde_json::Value> = fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records, [large, small]);
    }
}
