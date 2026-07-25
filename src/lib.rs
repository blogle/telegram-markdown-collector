use anyhow::{bail, Context, Result};
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use chrono_tz::Tz;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub timezone: String,
    pub telegram: TelegramConfig,
    pub state: StateConfig,
    pub output: OutputConfig,
    pub health: HealthConfig,
    pub streams: Vec<StreamConfig>,
}
#[derive(Debug, Clone, Deserialize)]
pub struct TelegramConfig {
    pub api_id_env: String,
    pub api_hash_env: String,
    pub session_path: PathBuf,
    #[serde(default = "default_phone_env")]
    pub phone_env: String,
    #[serde(default = "default_code_env")]
    pub code_env: String,
    #[serde(default = "default_password_env")]
    pub password_env: String,
}
#[derive(Debug, Clone, Deserialize)]
pub struct StateConfig {
    pub database: PathBuf,
    pub archive_root: PathBuf,
    pub work: PathBuf,
}
#[derive(Debug, Clone, Deserialize)]
pub struct OutputConfig {
    pub root: PathBuf,
    pub attachment_root: PathBuf,
}
#[derive(Debug, Clone, Deserialize)]
pub struct HealthConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
}
#[derive(Debug, Clone, Deserialize)]
pub struct StreamConfig {
    pub chat_id: i64,
    pub slug: String,
    pub title: String,
    pub output_subdirectory: PathBuf,
    pub backfill: BackfillConfig,
}
#[derive(Debug, Clone, Deserialize)]
pub struct BackfillConfig {
    pub from: Option<NaiveDate>,
}
fn default_phone_env() -> String {
    "TELEGRAM_PHONE".into()
}
fn default_code_env() -> String {
    "TELEGRAM_CODE".into()
}
fn default_password_env() -> String {
    "TELEGRAM_PASSWORD".into()
}
fn default_listen() -> String {
    "0.0.0.0:8080".into()
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
        let c: Self = serde_yaml::from_str(&text).context("parse YAML config")?;
        c.validate()?;
        Ok(c)
    }
    pub fn validate(&self) -> Result<()> {
        let _: Tz = self
            .timezone()
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid IANA timezone {}", self.timezone()))?;
        let mut slugs = std::collections::HashSet::new();
        let mut ids = std::collections::HashSet::new();
        let mut outputs: Vec<PathBuf> = Vec::new();
        for stream in &self.streams {
            if stream.slug.is_empty() || !slugs.insert(stream.slug.clone()) {
                bail!("duplicate or empty stream slug {}", stream.slug);
            }
            if stream.slug
                != Path::new(&stream.slug)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
                || clean_relative(Path::new(&stream.slug))?
                    .components()
                    .count()
                    != 1
            {
                bail!("unsafe stream slug {}", stream.slug);
            }
            if !ids.insert(stream.chat_id) {
                bail!("duplicate chat_id {}", stream.chat_id);
            }
            let output = clean_relative(&stream.output_subdirectory)?;
            for existing in &outputs {
                if existing == &output
                    || existing.starts_with(&output)
                    || output.starts_with(existing)
                {
                    bail!("duplicate or overlapping output_subdirectory");
                }
            }
            outputs.push(output);
            if stream.backfill.from.is_none() {
                bail!("stream {} requires backfill.from", stream.slug);
            }
        }
        for (name, path) in [
            ("session_path", &self.telegram.session_path),
            ("database", &self.state.database),
            ("archive_root", &self.state.archive_root),
            ("work", &self.state.work),
            ("output", &self.output.root),
            ("attachment_root", &self.output.attachment_root),
        ] {
            if path.as_os_str().is_empty() {
                bail!("{name} must not be empty");
            }
            if !path.is_absolute() {
                clean_relative(path)?;
            }
            let dir = if name == "session_path" || name == "database" {
                path.parent().unwrap_or(Path::new("."))
            } else {
                path
            };
            fs::create_dir_all(dir).with_context(|| format!("create {name} parent"))?;
            if dir.canonicalize()? == Path::new("/") {
                bail!("{name} may not be filesystem root");
            }
        }
        let configured_paths: Vec<_> = [
            &self.state.archive_root,
            &self.state.work,
            &self.output.root,
            &self.output.attachment_root,
        ]
        .iter()
        .map(|path| path.canonicalize())
        .collect::<std::result::Result<_, _>>()?;
        for (index, path) in configured_paths.iter().enumerate() {
            if configured_paths[index + 1..]
                .iter()
                .any(|other| other == path)
            {
                bail!("duplicate configured state/output path {}", path.display());
            }
        }
        Ok(())
    }
    pub fn timezone(&self) -> &str {
        &self.timezone
    }
    pub fn path(&self, root: &Path, relative: &Path) -> Result<PathBuf> {
        let rel = clean_relative(relative)?;
        let root = root.canonicalize()?;
        let candidate = root.join(rel);
        if let Some(parent) = candidate.parent() {
            fs::create_dir_all(parent)?;
        }
        let parent = candidate
            .parent()
            .context("path has no parent")?
            .canonicalize()?;
        if !parent.starts_with(&root) {
            bail!("path escapes configured root");
        }
        Ok(candidate)
    }
}
fn clean_relative(path: &Path) -> Result<PathBuf> {
    if path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        bail!("unsafe path {}", path.display());
    }
    Ok(path.to_path_buf())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Message {
    pub chat_id: i64,
    pub message_id: i64,
    pub date: DateTime<Utc>,
    pub edit_date: Option<DateTime<Utc>>,
    pub sender_id: Option<i64>,
    #[serde(default)]
    pub sender_name: Option<String>,
    #[serde(default)]
    pub sender_username: Option<String>,
    #[serde(default)]
    pub entities: Vec<MessageEntity>,
    #[serde(default)]
    pub service: Option<String>,
    pub text: String,
    pub reply_to: Option<i64>,
    pub attachments: Vec<Attachment>,
    pub deleted: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessageEntity {
    pub kind: String,
    pub offset: usize,
    pub length: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Attachment {
    pub name: String,
    pub mime: Option<String>,
    pub size: Option<u64>,
    pub sha256: Option<String>,
    #[serde(default)]
    pub download_error: Option<String>,
}
impl Message {
    pub fn anchor(&self, slug: &str) -> String {
        let _ = slug;
        format!("^telegram-{}-{}", self.chat_id, self.message_id)
    }
}
#[derive(Debug, Serialize)]
struct RawEnvelope<'a> {
    schema_version: u32,
    event_id: &'a str,
    event: &'a str,
    observed_at: DateTime<Utc>,
    message: &'a Message,
}
pub struct FixtureSource {
    messages: Vec<Message>,
}
impl FixtureSource {
    pub fn from_file(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)?;
        Self::from_text(&text)
    }
    pub fn from_text(text: &str) -> Result<Self> {
        Ok(Self {
            messages: text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(serde_json::from_str)
                .collect::<std::result::Result<_, _>>()?,
        })
    }
    pub fn messages(&self) -> Vec<Message> {
        self.messages.clone()
    }
}

pub struct Store {
    pub conn: Connection,
    pub config: Config,
}
#[derive(Debug, Clone)]
pub struct ChangedEvent {
    pub message: Message,
    pub observed_at: DateTime<Utc>,
    pub event_id: String,
    pub event_class: String,
}
#[derive(Debug, Serialize)]
pub struct MissingRange {
    pub start: i64,
    pub end: i64,
}

struct CoverageStats {
    earliest_date: Option<String>,
    latest_date: Option<String>,
    edited_count: i64,
    deleted_count: i64,
    attachment_count: i64,
    failed_attachment_downloads: i64,
    missing_ranges: Vec<MissingRange>,
}

#[derive(Debug, Serialize)]
pub struct StreamStatus {
    pub slug: String,
    pub events: i64,
    pub cursor: i64,
    pub last_observed_at: Option<String>,
    pub failure: Option<String>,
    pub coverage: i64,
    pub hash_mismatches: i64,
    pub earliest_date: Option<String>,
    pub latest_date: Option<String>,
    pub edited_count: i64,
    pub deleted_count: i64,
    pub attachment_count: i64,
    pub failed_attachment_downloads: i64,
    pub pending_archive_outbox: i64,
    pub missing_ranges: Vec<MissingRange>,
}
pub fn verification_error(statuses: &[StreamStatus]) -> Option<String> {
    statuses
        .iter()
        .find(|status| status.failure.is_some() || status.hash_mismatches != 0)
        .map(|status| {
            format!(
                "verification failed for {}: failure={:?}, hash_mismatches={}",
                status.slug, status.failure, status.hash_mismatches
            )
        })
}
impl Store {
    pub fn open(config: Config) -> Result<Self> {
        config.validate()?;
        let path = config.state.database.clone();
        let conn = Connection::open(path)?;
        let s = Self { conn, config };
        s.migrate()?;
        s.cleanup_temps()?;
        flush_archive(&s)?;
        Ok(s)
    }
    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE IF NOT EXISTS migrations(version INTEGER PRIMARY KEY); CREATE TABLE IF NOT EXISTS events(event_key TEXT PRIMARY KEY, stream_slug TEXT NOT NULL, chat_id INTEGER NOT NULL, message_id INTEGER NOT NULL, observed_at TEXT NOT NULL, original_date TEXT NOT NULL, payload TEXT NOT NULL, source_hash TEXT NOT NULL DEFAULT '', deleted INTEGER NOT NULL DEFAULT 0); CREATE TABLE IF NOT EXISTS cursors(stream_slug TEXT PRIMARY KEY, last_message_id INTEGER NOT NULL DEFAULT 0, updated_at TEXT NOT NULL DEFAULT ''); CREATE TABLE IF NOT EXISTS stream_status(slug TEXT PRIMARY KEY, failure TEXT, updated_at TEXT NOT NULL); CREATE TABLE IF NOT EXISTS rendered_partitions(stream_slug TEXT NOT NULL, day TEXT NOT NULL, content_hash TEXT NOT NULL, PRIMARY KEY(stream_slug, day)); CREATE TABLE IF NOT EXISTS archive_outbox(event_id TEXT PRIMARY KEY, stream_slug TEXT NOT NULL, observed_at TEXT NOT NULL, event_class TEXT NOT NULL, payload TEXT NOT NULL); CREATE INDEX IF NOT EXISTS events_stream_day ON events(stream_slug, original_date); CREATE INDEX IF NOT EXISTS archive_outbox_stream ON archive_outbox(stream_slug,event_id);")?;
        let has_column = |table: &str, column: &str| -> Result<bool> {
            let mut statement = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
            let names = statement
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(names.iter().any(|name| name == column))
        };
        if !has_column("events", "source_hash")? {
            self.conn.execute_batch(
                "ALTER TABLE events ADD COLUMN source_hash TEXT NOT NULL DEFAULT '';",
            )?;
        }
        if !has_column("cursors", "updated_at")? {
            self.conn.execute_batch(
                "ALTER TABLE cursors ADD COLUMN updated_at TEXT NOT NULL DEFAULT '';",
            )?;
        }
        self.conn.execute_batch(
            "INSERT OR IGNORE INTO migrations(version) VALUES(1); INSERT OR IGNORE INTO migrations(version) VALUES(2);",
        )?;
        Ok(())
    }
    fn cleanup_temps(&self) -> Result<()> {
        fn walk(dir: &Path) -> Result<()> {
            if !dir.exists() {
                return Ok(());
            }
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();
                if entry.file_type()?.is_dir() {
                    walk(&path)?;
                } else if path
                    .file_name()
                    .and_then(|x| x.to_str())
                    .is_some_and(|x| x.contains(".tmp."))
                {
                    let _ = fs::remove_file(path);
                }
            }
            Ok(())
        }
        fn owned_path(root: &Path, relative: &Path) -> Result<PathBuf> {
            let root = root.canonicalize()?;
            let candidate = root.join(clean_relative(relative)?);
            if candidate.exists() {
                let resolved = candidate.canonicalize()?;
                if !resolved.starts_with(&root) {
                    bail!("configured collector path escapes its root");
                }
                return Ok(resolved);
            }
            Ok(candidate)
        }
        walk(&self.config.state.work)?;
        walk(&self.config.state.archive_root)?;
        for stream in &self.config.streams {
            walk(&owned_path(
                &self.config.output.root,
                &stream.output_subdirectory,
            )?)?;
            walk(&owned_path(
                &self.config.output.attachment_root,
                Path::new(&stream.slug),
            )?)?;
        }
        Ok(())
    }
    pub fn ingest(
        &mut self,
        stream: &StreamConfig,
        messages: &[Message],
        observed_at: DateTime<Utc>,
    ) -> Result<Vec<ChangedEvent>> {
        let tx = self.conn.transaction()?;
        let mut changed = Vec::new();
        for message in messages {
            let key = format!("{}:{}", stream.slug, message.message_id);
            let payload = serde_json::to_string(message)?;
            let old: Option<(String, i32)> = tx
                .query_row(
                    "SELECT payload, deleted FROM events WHERE event_key=?",
                    [&key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if old
                .as_ref()
                .is_some_and(|(old_payload, _)| old_payload == &payload)
            {
                continue;
            }
            let source_hash = message_source_hash(message)?;
            let event_id = hash_bytes(
                format!(
                    "archive-v1\0{key}\0{source_hash}\0{}",
                    observed_at.to_rfc3339()
                )
                .as_bytes(),
            );
            let event_class = if message.deleted {
                "message_deleted"
            } else if old.is_none() {
                "message_created"
            } else {
                "message_edited"
            };
            let tz: Tz = self
                .config
                .timezone()
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid timezone"))?;
            let original_date = message.date.with_timezone(&tz).date_naive().to_string();
            tx.execute("INSERT INTO events(event_key,stream_slug,chat_id,message_id,observed_at,original_date,payload,source_hash,deleted) VALUES(?,?,?,?,?,?,?,?,?) ON CONFLICT(event_key) DO UPDATE SET observed_at=excluded.observed_at, payload=excluded.payload, source_hash=excluded.source_hash, deleted=excluded.deleted", params![key,stream.slug,message.chat_id,message.message_id,observed_at.to_rfc3339(),original_date,payload,source_hash,message.deleted as i32])?;
            tx.execute("INSERT INTO cursors(stream_slug,last_message_id,updated_at) VALUES(?,?,?) ON CONFLICT(stream_slug) DO UPDATE SET last_message_id=MAX(last_message_id,excluded.last_message_id),updated_at=excluded.updated_at", params![stream.slug,message.message_id,observed_at.to_rfc3339()])?;
            tx.execute("INSERT INTO archive_outbox(event_id,stream_slug,observed_at,event_class,payload) VALUES(?,?,?,?,?) ON CONFLICT(event_id) DO NOTHING", params![event_id,stream.slug,observed_at.to_rfc3339(),event_class,payload])?;
            changed.push(ChangedEvent {
                message: message.clone(),
                observed_at,
                event_id,
                event_class: event_class.into(),
            });
        }
        tx.commit()?;
        Ok(changed)
    }
    pub fn cursor(&self, slug: &str) -> Result<i64> {
        Ok(self
            .conn
            .query_row(
                "SELECT COALESCE(last_message_id, 0) FROM cursors WHERE stream_slug=?",
                [slug],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }
    fn messages(&self, slug: &str) -> Result<Vec<(Message, DateTime<Utc>, NaiveDate)>> {
        let mut st = self.conn.prepare("SELECT payload, observed_at, original_date FROM events WHERE stream_slug=? ORDER BY original_date,message_id")?;
        let rows = st.query_map([slug], |r| {
            let payload: String = r.get(0)?;
            let observed: String = r.get(1)?;
            let original: String = r.get(2)?;
            Ok((
                serde_json::from_str(&payload)
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?,
                observed,
                original,
            ))
        })?;
        rows.map(|row| {
            let (message, observed, original) = row?;
            Ok((message, observed.parse()?, original.parse()?))
        })
        .collect()
    }
    pub fn statuses(&self) -> Result<Vec<StreamStatus>> {
        self.config
            .streams
            .iter()
            .map(|stream| {
                let events: i64 = self.conn.query_row(
                    "SELECT COUNT(*) FROM events WHERE stream_slug=?",
                    [&stream.slug],
                    |r| r.get(0),
                )?;
                let cursor: i64 = self.conn.query_row(
                    "SELECT COALESCE(MAX(last_message_id),0) FROM cursors WHERE stream_slug=?",
                    [&stream.slug],
                    |r| r.get(0),
                )?;
                let last: Option<String> = self.conn.query_row(
                    "SELECT MAX(observed_at) FROM events WHERE stream_slug=?",
                    [&stream.slug],
                    |r| r.get(0),
                )?;
                let failure: Option<String> = self
                    .conn
                    .query_row(
                        "SELECT failure FROM stream_status WHERE slug=?",
                        [&stream.slug],
                        |r| r.get::<_, Option<String>>(0),
                    )
                    .optional()?
                    .flatten();
                let coverage: i64 = self.conn.query_row(
                    "SELECT COUNT(*) FROM rendered_partitions WHERE stream_slug=?",
                    [&stream.slug],
                    |r| r.get(0),
                )?;
                let pending_archive_outbox: i64 = self.conn.query_row(
                    "SELECT COUNT(*) FROM archive_outbox WHERE stream_slug=?",
                    [&stream.slug],
                    |r| r.get(0),
                )?;
                let mut hash_mismatches = 0;
                let mut partitions = self.conn.prepare(
                    "SELECT day, content_hash FROM rendered_partitions WHERE stream_slug=?",
                )?;
                for row in partitions.query_map([&stream.slug], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })? {
                    let (day, expected) = row?;
                    let rel = stream
                        .output_subdirectory
                        .join(format!("{}/{day}.md", &day[..4]));
                    let path = self.config.output.root.join(rel);
                    if !path.exists() || hash_file(&path)? != expected {
                        hash_mismatches += 1;
                    }
                }
                let stats = self.stream_coverage(stream)?;
                Ok(StreamStatus {
                    slug: stream.slug.clone(),
                    events,
                    cursor,
                    last_observed_at: last,
                    failure,
                    coverage,
                    hash_mismatches,
                    earliest_date: stats.earliest_date,
                    latest_date: stats.latest_date,
                    edited_count: stats.edited_count,
                    deleted_count: stats.deleted_count,
                    attachment_count: stats.attachment_count,
                    failed_attachment_downloads: stats.failed_attachment_downloads,
                    pending_archive_outbox,
                    missing_ranges: stats.missing_ranges,
                })
            })
            .collect()
    }
    fn stream_coverage(&self, stream: &StreamConfig) -> Result<CoverageStats> {
        let mut stmt = self.conn.prepare(
            "SELECT message_id, payload FROM events WHERE stream_slug=? ORDER BY message_id",
        )?;
        let rows = stmt.query_map([&stream.slug], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut edited_count = 0i64;
        let mut deleted_count = 0i64;
        let mut attachment_count = 0i64;
        let mut failed_attachment_downloads = 0i64;
        let mut earliest_date: Option<String> = None;
        let mut latest_date: Option<String> = None;
        for row in rows {
            let (_message_id, payload) = row?;
            let message: Message = serde_json::from_str(&payload)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            if message.deleted {
                deleted_count += 1;
            }
            if message.edit_date.is_some() {
                edited_count += 1;
            }
            attachment_count += message.attachments.len() as i64;
            failed_attachment_downloads += message
                .attachments
                .iter()
                .filter(|attachment| attachment.download_error.is_some())
                .count() as i64;
            let timestamp = message.date.to_rfc3339();
            earliest_date = Some(match earliest_date {
                Some(existing) if existing < timestamp => existing,
                _ => timestamp.clone(),
            });
            latest_date = Some(match latest_date {
                Some(existing) if existing > timestamp => existing,
                _ => timestamp,
            });
        }
        // Telegram message IDs are not guaranteed to be contiguous, so gaps
        // alone are not proof of missing source updates.
        let missing_ranges = Vec::new();
        Ok(CoverageStats {
            earliest_date,
            latest_date,
            edited_count,
            deleted_count,
            attachment_count,
            failed_attachment_downloads,
            missing_ranges,
        })
    }
    pub fn set_failure(&self, slug: &str, failure: Option<&str>) -> Result<()> {
        self.conn.execute("INSERT INTO stream_status(slug,failure,updated_at) VALUES(?,?,?) ON CONFLICT(slug) DO UPDATE SET failure=excluded.failure,updated_at=excluded.updated_at", params![slug,failure,Utc::now().to_rfc3339()])?;
        Ok(())
    }
}

pub fn flush_archive(store: &Store) -> Result<()> {
    let tz: Tz = store
        .config
        .timezone()
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid timezone"))?;
    let mut rows = store.conn.prepare(
        "SELECT event_id,stream_slug,observed_at,event_class,payload FROM archive_outbox ORDER BY rowid",
    )?;
    let pending = rows
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(rows);
    for (event_id, slug, observed, event_class, payload) in pending {
        let observed_at: DateTime<Utc> = observed.parse()?;
        let message: Message = serde_json::from_str(&payload)?;
        let local = observed_at.with_timezone(&tz);
        let day = local.format("%Y-%m-%d").to_string();
        let rel = PathBuf::from(&slug)
            .join(local.format("%Y/%m").to_string())
            .join(format!("{day}.jsonl"));
        let path = store.config.path(&store.config.state.archive_root, &rel)?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let existing = match fs::read(&path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error.into()),
        };
        if !existing.lines().any(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|value| value.get("event_id")?.as_str().map(str::to_owned))
                .as_deref()
                == Some(event_id.as_str())
        }) {
            writeln!(
                file,
                "{}",
                serde_json::to_string(&RawEnvelope {
                    schema_version: 1,
                    event_id: &event_id,
                    event: &event_class,
                    observed_at,
                    message: &message,
                })?
            )?;
        }
        file.sync_data()?;
        store
            .conn
            .execute("DELETE FROM archive_outbox WHERE event_id=?", [&event_id])?;
    }
    Ok(())
}
pub fn render(store: &Store) -> Result<usize> {
    let tz: Tz = store
        .config
        .timezone()
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid timezone"))?;
    let mut count = 0;
    for stream in &store.config.streams {
        let mut by_day =
            std::collections::BTreeMap::<NaiveDate, Vec<(Message, DateTime<Utc>)>>::new();
        for (message, observed, original_date) in store.messages(&stream.slug)? {
            by_day
                .entry(original_date)
                .or_default()
                .push((message, observed));
        }
        for (day, mut messages) in by_day {
            messages.sort_by_key(|(message, _)| message.message_id);
            let generated_at = messages
                .iter()
                .map(|(_, observed)| *observed)
                .max()
                .context("message observation missing")?;
            let first = messages
                .first()
                .map(|(m, _)| m.message_id)
                .unwrap_or_default();
            let last = messages
                .last()
                .map(|(m, _)| m.message_id)
                .unwrap_or_default();
            let mut body = format!("---\ntype: telegram-chat\nsource: telegram\nschema: telegram-collector/markdown/v1\nchat: {}\ntitle: {}\nstream: {}\ndate: {}\ntimezone: {}\nfirst: {}\nlast: {}\ncount: {}\ngenerated_at: {}\n---\n\n# {}\n\n", stream.chat_id, yaml_quote(&stream.title), yaml_quote(&stream.slug), day, store.config.timezone(), first, last, messages.len(), generated_at.to_rfc3339(), safe_text(&stream.title));
            for (message, _) in messages {
                body.push_str(&format!("{}\n", message.anchor(&stream.slug)));
                let sender = message
                    .sender_username
                    .as_deref()
                    .or(message.sender_name.as_deref())
                    .unwrap_or("unknown");
                body.push_str(&format!(
                    "**{}** · {}",
                    message.date.with_timezone(&tz).format("%H:%M:%S %Z"),
                    markdown_escape(sender)
                ));
                if message.deleted {
                    body.push_str(" · [tombstone]");
                } else if message.edit_date.is_some() {
                    body.push_str(" · [edited]");
                }
                body.push_str("\n\n");
                if let Some(reply) = message.reply_to {
                    let target_day = store
                        .conn
                        .query_row(
                            "SELECT original_date FROM events WHERE event_key=?",
                            params![format!("{}:{}", stream.slug, reply)],
                            |r| r.get::<_, String>(0),
                        )
                        .optional()?;
                    let link = match target_day {
                        Some(target) if target == day.to_string() => {
                            format!("#^telegram-{}-{}", stream.chat_id, reply)
                        }
                        Some(target) => format!(
                            "../{}/{}.md#^telegram-{}-{}",
                            &target[..4],
                            target,
                            stream.chat_id,
                            reply
                        ),
                        None => format!("#^telegram-{}-{}", stream.chat_id, reply),
                    };
                    body.push_str(&format!("> Reply to [#{}]({link})\n>\n", reply));
                }
                if !message.text.is_empty() && !message.deleted {
                    body.push_str(&markdown_escape(&message.text));
                    body.push_str("\n\n");
                }
                for attachment in message.attachments {
                    let safe_name = clean_filename(&attachment.name);
                    if let Some(error) = attachment.download_error {
                        body.push_str(&format!(
                            "Attachment: {} (download failed: {})\n\n",
                            markdown_escape(&safe_name),
                            markdown_escape(&error)
                        ));
                    } else {
                        let rel = attachment_link(
                            &store.config,
                            stream,
                            day,
                            message.message_id,
                            &safe_name,
                        )?;
                        body.push_str(&format!(
                            "Attachment: [{}]({rel})\n\n",
                            markdown_escape(&safe_name)
                        ));
                    }
                }
            }
            let rel = stream
                .output_subdirectory
                .join(format!("{}/{day}.md", day.year()));
            let path = store.config.path(&store.config.output.root, &rel)?;
            let hash = hash_bytes(body.as_bytes());
            let old: Option<String> = store
                .conn
                .query_row(
                    "SELECT content_hash FROM rendered_partitions WHERE stream_slug=? AND day=?",
                    params![stream.slug, day.to_string()],
                    |r| r.get(0),
                )
                .optional()?;
            let output_matches = path.exists() && hash_file(&path)? == hash;
            if old.as_deref() != Some(hash.as_str()) || !output_matches {
                atomic_replace(&path, body.as_bytes())?;
                store.conn.execute("INSERT INTO rendered_partitions(stream_slug,day,content_hash) VALUES(?,?,?) ON CONFLICT(stream_slug,day) DO UPDATE SET content_hash=excluded.content_hash", params![stream.slug, day.to_string(), hash])?;
                count += 1;
            }
        }
    }
    Ok(count)
}
fn yaml_quote(value: &str) -> String {
    serde_yaml::to_string(value)
        .unwrap_or_else(|_| "''".into())
        .trim()
        .to_string()
}
fn safe_text(value: &str) -> String {
    value.replace(['\u{0000}', '\r'], "")
}
fn markdown_escape(value: &str) -> String {
    safe_text(value)
        .replace('\\', "\\\\")
        .replace('*', "\\*")
        .replace('_', "\\_")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace('(', "\\(")
        .replace(')', "\\)")
        .replace('#', "\\#")
        .replace('+', "\\+")
        .replace('-', "\\-")
        .replace('!', "\\!")
        .replace('|', "\\|")
        .replace('<', "\\<")
        .replace('>', "\\>")
        .replace('`', "\\`")
}
fn clean_filename(value: &str) -> String {
    value
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("attachment")
        .replace(['\0', '\n', '\r'], "_")
}
pub fn attachment_path(
    config: &Config,
    stream: &StreamConfig,
    message_id: i64,
    name: &str,
) -> Result<PathBuf> {
    let relative = PathBuf::from(&stream.slug)
        .join(message_id.to_string())
        .join(clean_filename(name));
    config.path(&config.output.attachment_root, &relative)
}

pub fn attachment_link(
    config: &Config,
    stream: &StreamConfig,
    day: NaiveDate,
    message_id: i64,
    name: &str,
) -> Result<String> {
    let target = attachment_path(config, stream, message_id, name)?;
    let output = config.path(
        &config.output.root,
        &stream
            .output_subdirectory
            .join(format!("{}/{day}.md", day.year())),
    )?;
    relative_path(
        output.parent().context("rendered output has no parent")?,
        &target,
    )
}

fn relative_path(from: &Path, to: &Path) -> Result<String> {
    let from = from.canonicalize()?;
    let to = if to.exists() {
        to.canonicalize()?
    } else {
        to.parent()
            .context("attachment has no parent")?
            .canonicalize()?
            .join(to.file_name().context("attachment has no filename")?)
    };
    let from = from.components().collect::<Vec<_>>();
    let to = to.components().collect::<Vec<_>>();
    let common = from
        .iter()
        .zip(&to)
        .take_while(|(left, right)| left == right)
        .count();
    let mut result = PathBuf::new();
    for _ in common..from.len() {
        result.push("..");
    }
    for component in &to[common..] {
        result.push(component.as_os_str());
    }
    Ok(result.to_string_lossy().into_owned())
}
fn hash_bytes(bytes: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(bytes);
    format!("{:x}", hash.finalize())
}
fn message_source_hash(message: &Message) -> Result<String> {
    Ok(hash_bytes(serde_json::to_string(message)?.as_bytes()))
}
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
fn atomic_replace(target: &Path, bytes: &[u8]) -> Result<()> {
    if target.exists() && fs::read(target)? == bytes {
        return Ok(());
    }
    let parent = target.parent().context("target has no parent")?;
    fs::create_dir_all(parent)?;
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(
        ".{}.tmp.{}.{}",
        target.file_name().unwrap().to_string_lossy(),
        std::process::id(),
        nonce
    ));
    fs::write(&temp, bytes)?;
    fs::rename(&temp, target).with_context(|| format!("replace {}", target.display()))?;
    Ok(())
}
pub fn hash_file(path: &Path) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(fs::read(path)?);
    Ok(format!("{:x}", hash.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use tempfile::tempdir;
    fn config(dir: &Path) -> Config {
        Config {
            timezone: "UTC".into(),
            telegram: TelegramConfig {
                api_id_env: "ID".into(),
                api_hash_env: "HASH".into(),
                session_path: dir.join("session"),
                phone_env: "PHONE".into(),
                code_env: "CODE".into(),
                password_env: "PASS".into(),
            },
            state: StateConfig {
                database: dir.join("db.sqlite"),
                archive_root: dir.join("archive"),
                work: dir.join("work"),
            },
            output: OutputConfig {
                root: dir.join("out"),
                attachment_root: dir.join("att"),
            },
            health: HealthConfig {
                listen: "0.0.0.0:8080".into(),
            },
            streams: vec![StreamConfig {
                chat_id: 1,
                slug: "one".into(),
                title: "One".into(),
                output_subdirectory: "one".into(),
                backfill: BackfillConfig {
                    from: Some(NaiveDate::from_ymd_opt(2024, 1, 1).unwrap()),
                },
            }],
        }
    }
    fn message(id: i64, text: &str, edit: bool, deleted: bool) -> Message {
        Message {
            chat_id: 1,
            message_id: id,
            date: Utc.with_ymd_and_hms(2024, 1, 1, 12, 0, 0).unwrap(),
            edit_date: edit.then_some(Utc::now()),
            sender_id: Some(2),
            sender_name: Some("Ada".into()),
            sender_username: Some("ada".into()),
            entities: vec![],
            service: None,
            text: text.into(),
            reply_to: None,
            attachments: vec![],
            deleted,
        }
    }
    #[test]
    fn duplicate_and_edit_counts() {
        let d = tempdir().unwrap();
        let mut s = Store::open(config(d.path())).unwrap();
        let stream = s.config.streams[0].clone();
        assert_eq!(
            s.ingest(&stream, &[message(1, "a", false, false)], Utc::now())
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            s.ingest(&stream, &[message(1, "a", false, false)], Utc::now())
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            s.ingest(&stream, &[message(1, "b", true, false)], Utc::now())
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            s.ingest(&stream, &[message(1, "b", true, true)], Utc::now())
                .unwrap()
                .len(),
            1
        );
        assert_eq!(s.cursor(&stream.slug).unwrap(), 1);
    }
    #[test]
    fn render_returns_count_and_anchor() {
        let d = tempdir().unwrap();
        let mut s = Store::open(config(d.path())).unwrap();
        let stream = s.config.streams[0].clone();
        s.ingest(&stream, &[message(1, "hello", false, false)], Utc::now())
            .unwrap();
        assert_eq!(render(&s).unwrap(), 1);
        assert!(
            fs::read_to_string(d.path().join("out/one/2024/2024-01-01.md"))
                .unwrap()
                .contains("^telegram-1-1")
        );
    }
    #[test]
    fn duplicate_slug_rejected() {
        let d = tempdir().unwrap();
        let mut c = config(d.path());
        c.streams.push(c.streams[0].clone());
        assert!(c.validate().is_err());
    }

    #[test]
    fn overlapping_output_subdirectories_are_rejected() {
        let d = tempdir().unwrap();
        let mut c = config(d.path());
        let mut nested = c.streams[0].clone();
        nested.chat_id = 2;
        nested.slug = "two".into();
        nested.output_subdirectory = "one/nested".into();
        c.streams.push(nested);
        assert!(c.validate().is_err());
    }

    #[test]
    fn cleanup_only_removes_temps_below_owned_paths() {
        let d = tempdir().unwrap();
        let c = config(d.path());
        c.validate().unwrap();
        let unmanaged = c.output.root.join("unmanaged.tmp.keep");
        let owned_dir = c.output.root.join("one");
        let owned_temp = owned_dir.join("render.tmp.remove");
        let attachment_dir = c.output.attachment_root.join("one");
        let attachment_temp = attachment_dir.join("media.tmp.remove");
        fs::create_dir_all(&owned_dir).unwrap();
        fs::create_dir_all(&attachment_dir).unwrap();
        fs::write(&unmanaged, "keep").unwrap();
        fs::write(&owned_temp, "remove").unwrap();
        fs::write(&attachment_temp, "remove").unwrap();

        Store::open(c).unwrap();

        assert!(unmanaged.exists());
        assert!(!owned_temp.exists());
        assert!(!attachment_temp.exists());
    }

    #[test]
    fn timezone_boundary_controls_original_partition_and_raw_archive() {
        let d = tempdir().unwrap();
        let mut c = config(d.path());
        c.timezone = "America/New_York".into();
        let mut s = Store::open(c).unwrap();
        let stream = s.config.streams[0].clone();
        let m = Message {
            date: Utc.with_ymd_and_hms(2024, 1, 2, 4, 30, 0).unwrap(),
            ..message(1, "boundary", false, false)
        };
        let observed = Utc.with_ymd_and_hms(2024, 1, 2, 4, 31, 0).unwrap();
        s.ingest(&stream, &[m], observed).unwrap();
        flush_archive(&s).unwrap();
        assert_eq!(render(&s).unwrap(), 1);
        assert!(d.path().join("out/one/2024/2024-01-01.md").exists());
        assert!(d
            .path()
            .join("archive/one/2024/01/2024-01-01.jsonl")
            .exists());
        let raw =
            fs::read_to_string(d.path().join("archive/one/2024/01/2024-01-01.jsonl")).unwrap();
        assert!(raw.contains("\"schema_version\":1"));
        assert!(raw.contains("\"event\":\"message_created\""));
    }

    #[test]
    fn raw_archive_is_changed_event_only_and_restart_is_idempotent() {
        let d = tempdir().unwrap();
        let mut s = Store::open(config(d.path())).unwrap();
        let stream = s.config.streams[0].clone();
        let observed = Utc.with_ymd_and_hms(2024, 1, 1, 1, 0, 0).unwrap();
        s.ingest(&stream, &[message(1, "a", false, false)], observed)
            .unwrap();
        flush_archive(&s).unwrap();
        let path = d.path().join("archive/one/2024/01/2024-01-01.jsonl");
        let before = fs::read_to_string(&path).unwrap();
        s.ingest(&stream, &[message(1, "a", false, false)], observed)
            .unwrap();
        flush_archive(&s).unwrap();
        assert_eq!(before, fs::read_to_string(&path).unwrap());
        render(&s).unwrap();
        drop(s);
        let s = Store::open(config(d.path())).unwrap();
        assert_eq!(render(&s).unwrap(), 0);
    }

    #[test]
    fn pending_outbox_is_flushed_on_restart_without_duplicate() {
        let d = tempdir().unwrap();
        let observed = Utc.with_ymd_and_hms(2024, 1, 1, 1, 0, 0).unwrap();
        let mut s = Store::open(config(d.path())).unwrap();
        let stream = s.config.streams[0].clone();
        s.ingest(&stream, &[message(1, "pending", false, false)], observed)
            .unwrap();
        let (event_id, payload, event_class): (String, String, String) = s
            .conn
            .query_row(
                "SELECT event_id,payload,event_class FROM archive_outbox",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        let path = d.path().join("archive/one/2024/01/2024-01-01.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let pending_message: Message = serde_json::from_str(&payload).unwrap();
        fs::write(
            &path,
            serde_json::to_string(&RawEnvelope {
                schema_version: 1,
                event_id: &event_id,
                event: &event_class,
                observed_at: observed,
                message: &pending_message,
            })
            .unwrap()
                + "\n",
        )
        .unwrap();
        drop(s);

        let s = Store::open(config(d.path())).unwrap();
        assert_eq!(fs::read_to_string(path).unwrap().lines().count(), 1);
        assert_eq!(
            s.conn
                .query_row("SELECT COUNT(*) FROM archive_outbox", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn archive_event_classes_follow_state() {
        let d = tempdir().unwrap();
        let mut s = Store::open(config(d.path())).unwrap();
        let stream = s.config.streams[0].clone();
        let observed = Utc::now();
        let created = s
            .ingest(&stream, &[message(1, "a", false, false)], observed)
            .unwrap();
        assert_eq!(created[0].event_class, "message_created");
        flush_archive(&s).unwrap();
        let edited = s
            .ingest(&stream, &[message(1, "b", true, false)], observed)
            .unwrap();
        assert_eq!(edited[0].event_class, "message_edited");
        flush_archive(&s).unwrap();
        let deleted = s
            .ingest(&stream, &[message(1, "b", true, true)], observed)
            .unwrap();
        assert_eq!(deleted[0].event_class, "message_deleted");
    }

    #[test]
    fn migrates_legacy_event_and_cursor_columns() {
        let directory = tempdir().unwrap();
        let config = config(directory.path());
        let connection = Connection::open(&config.state.database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE events(event_key TEXT PRIMARY KEY, stream_slug TEXT NOT NULL, chat_id INTEGER NOT NULL, message_id INTEGER NOT NULL, observed_at TEXT NOT NULL, original_date TEXT NOT NULL, payload TEXT NOT NULL, deleted INTEGER NOT NULL DEFAULT 0);
                 CREATE TABLE cursors(stream_slug TEXT PRIMARY KEY, last_message_id INTEGER NOT NULL DEFAULT 0);",
            )
            .unwrap();
        drop(connection);

        let mut store = Store::open(config).unwrap();
        let stream = store.config.streams[0].clone();
        store
            .ingest(&stream, &[message(1, "migrated", false, false)], Utc::now())
            .unwrap();
        assert_eq!(store.cursor(&stream.slug).unwrap(), 1);
    }

    #[test]
    fn render_golden_reply_delete_unicode_and_attachment() {
        let d = tempdir().unwrap();
        let mut s = Store::open(config(d.path())).unwrap();
        let stream = s.config.streams[0].clone();
        let mut first = message(1, "plain *safe*\nélan", false, false);
        first.attachments.push(Attachment {
            name: "../photo.png".into(),
            mime: Some("image/png".into()),
            size: Some(3),
            sha256: None,
            download_error: Some("not downloaded".into()),
        });
        let mut reply = message(2, "gone", false, true);
        reply.date = Utc.with_ymd_and_hms(2024, 1, 2, 12, 0, 0).unwrap();
        reply.reply_to = Some(1);
        s.ingest(&stream, &[first, reply], Utc::now()).unwrap();
        render(&s).unwrap();
        let first_body = fs::read_to_string(d.path().join("out/one/2024/2024-01-01.md")).unwrap();
        let body = fs::read_to_string(d.path().join("out/one/2024/2024-01-02.md")).unwrap();
        assert!(first_body.contains("photo.png"));
        assert!(!first_body.contains("../photo.png"));
        assert!(body.contains("type: telegram-chat"));
        assert!(body.contains("^telegram-1-2"));
        assert!(body.contains("tombstone"));
        assert!(body.contains("../2024/2024-01-01.md#^telegram-1-1"));
    }

    #[test]
    fn attachment_link_uses_configured_roots_and_output_depth() {
        let d = tempdir().unwrap();
        let mut c = config(d.path());
        c.streams[0].output_subdirectory = "nested/deep/chat".into();
        let s = Store::open(c).unwrap();
        let stream = s.config.streams[0].clone();
        let link = attachment_link(
            &s.config,
            &stream,
            NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
            42,
            "photo.png",
        )
        .unwrap();
        assert_eq!(link, "../../../../../att/one/42/photo.png");
        let first = attachment_path(&s.config, &stream, 42, "../photo.png").unwrap();
        let second = attachment_path(&s.config, &stream, 43, "../photo.png").unwrap();
        assert!(first.starts_with(d.path().join("att")));
        assert_ne!(first, second);
    }

    #[test]
    fn fixture_matches_example_config_and_replay_is_end_to_end() {
        let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/messages.jsonl");
        let source = FixtureSource::from_file(&fixture_path).unwrap_or_else(|_| {
            FixtureSource::from_text(
                r#"{"chat_id":123456789,"message_id":1,"date":"2024-01-01T12:00:01Z","edit_date":null,"sender_id":7,"sender_name":"Fixture Sender","sender_username":"fixture","entities":[],"service":null,"text":"fixture hello","reply_to":null,"attachments":[],"deleted":false}
{"chat_id":123456789,"message_id":2,"date":"2024-01-01T12:03:00Z","edit_date":null,"sender_id":8,"sender_name":"Fixture Replier","sender_username":null,"entities":[],"service":null,"text":"fixture reply","reply_to":1,"attachments":[],"deleted":false}"#,
            )
            .unwrap()
        });
        let d = tempdir().unwrap();
        let mut c = config(d.path());
        c.streams[0].chat_id = 123_456_789;
        let mut s = Store::open(c).unwrap();
        let stream = s.config.streams[0].clone();
        let selected = source
            .messages()
            .into_iter()
            .filter(|message| message.chat_id == stream.chat_id)
            .collect::<Vec<_>>();
        s.ingest(&stream, &selected, Utc::now()).unwrap();
        flush_archive(&s).unwrap();
        assert_eq!(render(&s).unwrap(), 1);
        assert_eq!(s.cursor(&stream.slug).unwrap(), 2);
        assert!(d.path().join("out/one/2024/2024-01-01.md").exists());
    }

    #[test]
    fn no_op_render_preserves_mtime() {
        let d = tempdir().unwrap();
        let mut s = Store::open(config(d.path())).unwrap();
        let stream = s.config.streams[0].clone();
        s.ingest(&stream, &[message(1, "stable", false, false)], Utc::now())
            .unwrap();
        render(&s).unwrap();
        let path = d.path().join("out/one/2024/2024-01-01.md");
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert_eq!(render(&s).unwrap(), 0);
        assert_eq!(before, fs::metadata(path).unwrap().modified().unwrap());
    }

    #[test]
    fn verification_reports_failure_and_hash_mismatch() {
        let status = StreamStatus {
            slug: "one".into(),
            events: 1,
            cursor: 1,
            last_observed_at: None,
            failure: Some("download failed".into()),
            coverage: 1,
            hash_mismatches: 0,
            earliest_date: None,
            latest_date: None,
            edited_count: 0,
            deleted_count: 0,
            attachment_count: 0,
            failed_attachment_downloads: 0,
            pending_archive_outbox: 0,
            missing_ranges: Vec::new(),
        };
        assert!(verification_error(&[status]).is_some());
        let status = StreamStatus {
            slug: "one".into(),
            events: 1,
            cursor: 1,
            last_observed_at: None,
            failure: None,
            coverage: 1,
            hash_mismatches: 1,
            earliest_date: None,
            latest_date: None,
            edited_count: 0,
            deleted_count: 0,
            attachment_count: 0,
            failed_attachment_downloads: 0,
            pending_archive_outbox: 0,
            missing_ranges: Vec::new(),
        };
        assert!(verification_error(&[status]).is_some());
        assert!(verification_error(&[]).is_none());
    }

    #[test]
    fn unsafe_slug_and_duplicate_roots_are_rejected() {
        let d = tempdir().unwrap();
        let mut c = config(d.path());
        c.streams[0].slug = "../escape".into();
        assert!(c.validate().is_err());
        let mut c = config(d.path());
        c.output.attachment_root = c.output.root.clone();
        assert!(c.validate().is_err());
    }

    #[test]
    fn markdown_escapes_message_markup_and_status_reports_partition_hash() {
        let d = tempdir().unwrap();
        let mut s = Store::open(config(d.path())).unwrap();
        let stream = s.config.streams[0].clone();
        s.ingest(
            &stream,
            &[message(
                7,
                "[link](https://example.invalid) *raw*",
                false,
                false,
            )],
            Utc::now(),
        )
        .unwrap();
        render(&s).unwrap();
        let path = d.path().join("out/one/2024/2024-01-01.md");
        let body = fs::read_to_string(path).unwrap();
        assert!(body.contains(r"\[link\]\(https://example.invalid\) \*raw\*"));
        let status = s.statuses().unwrap();
        assert_eq!(status[0].coverage, 1);
        assert_eq!(status[0].hash_mismatches, 0);
    }

    #[test]
    fn status_reports_normalized_coverage_and_pending_archive() {
        let directory = tempdir().unwrap();
        let mut store = Store::open(config(directory.path())).unwrap();
        let stream = store.config.streams[0].clone();
        let mut edited = message(1, "edited", true, false);
        edited.attachments.push(Attachment {
            name: "ok.png".into(),
            mime: Some("image/png".into()),
            size: Some(1),
            sha256: Some("hash".into()),
            download_error: None,
        });
        let mut deleted = message(3, "", false, true);
        deleted.attachments.push(Attachment {
            name: "failed.bin".into(),
            mime: None,
            size: None,
            sha256: None,
            download_error: Some("unsupported".into()),
        });
        store
            .ingest(&stream, &[edited, deleted], Utc::now())
            .unwrap();
        store.set_failure(&stream.slug, None).unwrap();

        let statuses = store.statuses().unwrap();
        let status = &statuses[0];
        assert_eq!(status.events, 2);
        assert_eq!(status.edited_count, 1);
        assert_eq!(status.deleted_count, 1);
        assert_eq!(status.attachment_count, 2);
        assert_eq!(status.failed_attachment_downloads, 1);
        assert_eq!(status.pending_archive_outbox, 2);
        assert!(status.earliest_date.is_some());
        assert!(status.latest_date.is_some());
        assert!(status.missing_ranges.is_empty());
        assert!(status.failure.is_none());
    }
}
