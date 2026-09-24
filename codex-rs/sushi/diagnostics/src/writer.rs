use super::Record;
use serde::Serialize;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Seek;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use uuid::Uuid;

const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_RECORD_BYTES: usize = 4096;
static EMITTER: OnceLock<Option<Emitter>> = OnceLock::new();

#[derive(Clone)]
pub(super) struct Emitter {
    sender: mpsc::SyncSender<Record>,
    dropped: Arc<AtomicU64>,
}

impl Emitter {
    pub(super) fn emit(&self, record: Record) {
        if self.sender.try_send(record).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub(super) fn emitter() -> Option<&'static Emitter> {
    EMITTER
        .get_or_init(|| {
            if std::env::var("SUSHIGPT_TELEMETRY").as_deref() != Ok("1") {
                return None;
            }
            let home = std::env::var_os("CODEX_HOME")?;
            let home = Path::new(&home);
            if !home.is_absolute() || !home.is_dir() {
                return None;
            }
            match open(home) {
                Ok((emitter, _)) => Some(emitter),
                Err(_) => {
                    tracing::warn!(target: "sushigpt_telemetry", "local diagnostics unavailable");
                    None
                }
            }
        })
        .as_ref()
}

// Separate lock file permits readers on Windows. Never follow a telemetry-file symlink.
fn private_file(path: &Path) -> io::Result<File> {
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && !meta.is_file()
    {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;
        if file.metadata()?.nlink() != 1 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

pub(super) fn open(home: &Path) -> io::Result<(Emitter, std::thread::JoinHandle<()>)> {
    let lock = private_file(&home.join(".sushigpt-telemetry.lock"))?;
    lock.try_lock().map_err(io::Error::other)?;
    let file = private_file(&home.join("sushigpt-telemetry.jsonl"))?;
    file.set_len(0)?;
    let (sender, receiver) = mpsc::sync_channel(1024);
    let dropped = Arc::new(AtomicU64::new(0));
    let emitter = Emitter {
        sender,
        dropped: Arc::clone(&dropped),
    };
    let handle = std::thread::Builder::new().name("sushi-telemetry".into()).spawn(move || {
        let _lock = lock;
        let mut writer = Writer { file, process: Uuid::new_v4(), generation: Uuid::new_v4(), sequence: 0, bytes: 0 };
        for record in receiver {
            if writer.write(record, dropped.load(Ordering::Relaxed), MAX_FILE_BYTES).is_err() {
                tracing::warn!(target: "sushigpt_telemetry", "local diagnostics writer stopped");
                break;
            }
        }
    })?;
    Ok((emitter, handle))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Envelope<'a> {
    schema_version: u8,
    process_instance_id: Uuid,
    generation_id: Uuid,
    sequence: u64,
    dropped_records: u64,
    timestamp_ms: u128,
    record_id: Uuid,
    #[serde(flatten)]
    record: &'a Record,
}

struct Writer {
    file: File,
    process: Uuid,
    generation: Uuid,
    sequence: u64,
    bytes: u64,
}
impl Writer {
    fn write(&mut self, record: Record, dropped: u64, limit: u64) -> io::Result<()> {
        self.sequence += 1;
        // Reserve the maximum record size so the generation is correct in this record.
        if self.bytes + MAX_RECORD_BYTES as u64 > limit {
            self.file.set_len(0)?;
            self.file.rewind()?;
            self.bytes = 0;
            self.generation = Uuid::new_v4();
        }
        let mut bytes = serde_json::to_vec(&Envelope {
            schema_version: 1,
            process_instance_id: self.process,
            generation_id: self.generation,
            sequence: self.sequence,
            dropped_records: dropped,
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            record_id: Uuid::new_v4(),
            record: &record,
        })?;
        bytes.push(b'\n');
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(io::ErrorKind::InvalidData.into());
        }
        self.file.write_all(&bytes)?;
        self.bytes += bytes.len() as u64;
        Ok(())
    }
}

#[cfg(test)]
#[path = "writer_tests.rs"]
mod tests;
