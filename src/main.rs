use anyhow::{bail, Context, Result};
use chrono::TimeZone;
use chrono_tz::Tz;
use clap::{Parser, Subcommand};
use grammers_client::media::Media;
use grammers_client::sender::SenderPool;
use grammers_client::{Client, SignInError};
use grammers_session::{storages::SqliteSession, types::PeerId, Session};
use std::{
    env, fs,
    io::{self, BufRead, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use telegram_collector::*;

#[derive(Parser)]
#[command(author, version, about)]
struct Cli {
    #[arg(short, long, default_value = "config.yaml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Auth,
    Chats {
        #[arg(long)]
        json: bool,
    },
    Backfill,
    Sync,
    Watch,
    Render,
    Verify {
        #[arg(long)]
        json: bool,
    },
    Status {
        #[arg(long)]
        json: bool,
    },
    Replay {
        fixture: PathBuf,
    },
}

async fn connect(config: &Config) -> Result<(Client, Arc<SqliteSession>)> {
    let api_id: i32 = env::var(&config.telegram.api_id_env)
        .context("Telegram API ID environment variable")?
        .parse()
        .context("parse Telegram API ID")?;
    let session = Arc::new(
        SqliteSession::open(&config.telegram.session_path)
            .await
            .context("open Telegram session")?,
    );
    let pool = SenderPool::new(Arc::clone(&session), api_id);
    let client = Client::new(pool.handle);
    // SenderPool handles only enqueue requests; its runner must continuously
    // drive the MTProto connections or every request is immediately dropped.
    tokio::spawn(pool.runner.run());
    Ok((client, session))
}
async fn authenticate(client: &Client, config: &Config) -> Result<()> {
    if client.is_authorized().await? {
        return Ok(());
    }
    let hash = env::var(&config.telegram.api_hash_env)
        .context("Telegram API hash environment variable")?;
    let phone = env::var(&config.telegram.phone_env).or_else(|_| prompt("Telegram phone"))?;
    let token = client.request_login_code(&phone, &hash).await?;
    let code = env::var(&config.telegram.code_env).or_else(|_| prompt("Telegram login code"))?;
    match client.sign_in(&token, &code).await {
        Ok(_) => Ok(()),
        Err(SignInError::PasswordRequired(token)) => {
            let password = env::var(&config.telegram.password_env)
                .or_else(|_| prompt("Telegram 2FA password"))?;
            client
                .check_password(token, password)
                .await
                .map(|_| ())
                .map_err(Into::into)
        }
        Err(SignInError::SignUpRequired) => {
            bail!("Telegram account requires sign-up; collector will not create accounts")
        }
        Err(error) => Err(error.into()),
    }
}
fn prompt(label: &str) -> Result<String> {
    eprint!("{label}: ");
    io::stderr().flush()?;
    let mut input = String::new();
    io::stdin().lock().read_line(&mut input)?;
    let value = input.trim().to_string();
    if value.is_empty() {
        bail!("{label} must not be empty")
    }
    Ok(value)
}
async fn fetch_stream(
    client: &Client,
    session: &SqliteSession,
    config: &Config,
    stream: &StreamConfig,
    from: Option<chrono::DateTime<chrono::Utc>>,
    cursor: i64,
) -> Result<Vec<Message>> {
    let peer_id = PeerId::from_bot_api_dialog_id(stream.chat_id).context("invalid chat_id")?;
    let peer = session
        .peer_ref(peer_id)
        .await?
        .unwrap_or_else(|| peer_id.to_ambient_ref());
    let mut iter = client.iter_messages(peer).reverse(true).offset_id(
        cursor
            .try_into()
            .context("cursor exceeds Telegram ID range")?,
    );
    let mut messages = Vec::new();
    while let Some(message) = iter.next().await? {
        if from.is_some_and(|date| message.date() < date) {
            continue;
        }
        let sender_name = message
            .sender()
            .and_then(|peer| peer.name().map(str::to_owned));
        let mut normalized = Message {
            chat_id: stream.chat_id,
            message_id: i64::from(message.id()),
            date: message.date(),
            edit_date: message.edit_date(),
            sender_id: message.sender_id().and_then(|id| id.bare_id()),
            sender_name,
            sender_username: message
                .sender()
                .and_then(|peer| peer.username().map(str::to_owned)),
            entities: Vec::new(),
            service: None,
            text: message.text().to_owned(),
            reply_to: message.reply_to_message_id().map(i64::from),
            attachments: Vec::new(),
            deleted: false,
        };
        if let Some((name, mime, size)) = attachment_metadata(&message) {
            let mut attachment = Attachment {
                name,
                mime,
                size,
                sha256: None,
                download_error: None,
            };
            if let Err(error) = download_attachment(&message, config, stream, &mut attachment).await
            {
                attachment.download_error = Some(format!("{error:#}"));
            }
            normalized.attachments.push(attachment);
        }
        messages.push(normalized);
    }
    messages.sort_by_key(|message| message.message_id);
    Ok(messages)
}

fn attachment_metadata(
    message: &grammers_client::message::Message,
) -> Option<(String, Option<String>, Option<u64>)> {
    match message.media()? {
        Media::Photo(photo) => Some((
            format!("photo-{}.jpg", photo.id()),
            Some("image/jpeg".into()),
            photo.size().map(|size| size as u64),
        )),
        Media::Document(document) => Some((
            document
                .name()
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("document-{}", document.id())),
            document.mime_type().map(str::to_owned),
            document.size().map(|size| size as u64),
        )),
        Media::Sticker(sticker) => Some((
            format!("sticker-{}", sticker.document.id()),
            sticker.document.mime_type().map(str::to_owned),
            sticker.document.size().map(|size| size as u64),
        )),
        _ => None,
    }
}

async fn download_attachment(
    message: &grammers_client::message::Message,
    config: &Config,
    stream: &StreamConfig,
    attachment: &mut Attachment,
) -> Result<()> {
    let target = attachment_path(config, stream, i64::from(message.id()), &attachment.name)?;
    if target.exists() {
        attachment.sha256 = Some(hash_file(&target)?);
        return Ok(());
    }
    let temp = target.with_file_name(format!(
        ".{}.download.tmp.{}",
        target
            .file_name()
            .context("attachment has no filename")?
            .to_string_lossy(),
        std::process::id()
    ));
    let result = message.download_media(&temp).await;
    match result {
        Ok(true) => {
            fs::rename(&temp, &target)?;
            attachment.sha256 = Some(hash_file(&target)?);
            Ok(())
        }
        Ok(false) => {
            let _ = fs::remove_file(&temp);
            bail!("Telegram message has no downloadable media")
        }
        Err(error) => {
            let _ = fs::remove_file(&temp);
            Err(error.into())
        }
    }
}
async fn reconcile(
    client: &Client,
    session: &SqliteSession,
    store: &mut Store,
    since: bool,
) -> Result<usize> {
    authenticate(client, &store.config).await?;
    let observed = chrono::Utc::now();
    let mut total = 0;
    for stream in store.config.streams.clone() {
        let cursor = if since {
            0
        } else {
            store.cursor(&stream.slug)?
        };
        let from = if since {
            let tz: Tz = store.config.timezone().parse()?;
            stream
                .backfill
                .from
                .map(|date| {
                    tz.from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
                        .single()
                        .context("ambiguous backfill timezone")
                        .map(|date| date.with_timezone(&chrono::Utc))
                })
                .transpose()?
        } else {
            None
        };
        let result = async {
            let messages =
                fetch_stream(client, session, &store.config, &stream, from, cursor).await?;
            let changed = store.ingest(&stream, &messages, observed)?;
            flush_archive(store)?;
            Ok::<usize, anyhow::Error>(changed.len())
        }
        .await;
        let changed = match result {
            Ok(changed) => {
                store.set_failure(&stream.slug, None)?;
                changed
            }
            Err(error) => {
                store.set_failure(&stream.slug, Some(&format!("{error:#}")))?;
                return Err(error);
            }
        };
        total += changed;
    }
    if let Err(error) = render(store) {
        for stream in &store.config.streams {
            store.set_failure(&stream.slug, Some(&format!("{error:#}")))?;
        }
        return Err(error);
    }
    Ok(total)
}
async fn health_server(
    listen: String,
    ready: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    while !stop.load(Ordering::Relaxed) {
        let (mut socket, _) = listener.accept().await?;
        let mut buf = [0_u8; 256];
        let n = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await?;
        let request = String::from_utf8_lossy(&buf[..n]).into_owned();
        let path = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("/");
        let ok = matches!(path, "/healthz") || (path == "/readyz" && ready.load(Ordering::Relaxed));
        let (status, body) = if ok {
            ("200 OK", "OK")
        } else {
            ("503 Service Unavailable", "NOT READY")
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await?;
    }
    Ok(())
}

// grammers-session's local libsql backend configures SQLite for single-threaded
// use. Keep all Telegram and collector SQLite access on one runtime thread.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load(&cli.config).context("load configuration")?;
    let uses_telegram = matches!(
        &cli.command,
        Command::Auth | Command::Chats { .. } | Command::Backfill | Command::Sync | Command::Watch
    );
    if uses_telegram {
        // libsql must select its threading mode before rusqlite initializes the
        // process-wide SQLite library used by the collector state database.
        let session = SqliteSession::open(&config.telegram.session_path)
            .await
            .context("initialize Telegram session storage")?;
        drop(session);
    }
    let mut store = Store::open(config.clone())?;
    match cli.command {
        Command::Auth => {
            let (client, _) = connect(&config).await?;
            authenticate(&client, &config).await?;
            println!("authenticated");
        }
        Command::Chats { json } => {
            let (client, _) = connect(&config).await?;
            authenticate(&client, &config).await?;
            let mut dialogs = client.iter_dialogs();
            let mut rows = Vec::new();
            while let Some(dialog) = dialogs.next().await? {
                let peer = dialog.peer();
                let id = peer
                    .id()
                    .bot_api_dialog_id()
                    .context("dialog has no Bot API ID")?;
                let title = peer.name().unwrap_or("");
                rows.push(serde_json::json!({"id": id, "title": title, "name": title}));
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                for row in rows {
                    println!("{}", row);
                }
            }
        }
        Command::Backfill => {
            let (client, session) = connect(&config).await?;
            println!(
                "ingested {} messages",
                reconcile(&client, &session, &mut store, true).await?
            );
        }
        Command::Sync => {
            let (client, session) = connect(&config).await?;
            println!(
                "ingested {} messages",
                reconcile(&client, &session, &mut store, false).await?
            );
        }
        Command::Replay { fixture } => {
            let source = FixtureSource::from_file(&fixture)?;
            let messages = source.messages();
            let observed = messages
                .iter()
                .map(|m| m.date)
                .max()
                .unwrap_or_else(chrono::Utc::now);
            for stream in &config.streams {
                let selected: Vec<_> = messages
                    .iter()
                    .filter(|message| message.chat_id == stream.chat_id)
                    .cloned()
                    .collect();
                store.ingest(stream, &selected, observed)?;
                flush_archive(&store)?;
            }
            render(&store)?;
        }
        Command::Render => {
            println!("rendered {} documents", render(&store)?);
        }
        Command::Verify { json } => {
            let statuses = store.statuses()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&statuses)?);
            } else {
                for status in &statuses {
                    println!(
                        "{}: {} events, cursor {}, coverage {}, hash mismatches {}, edited {}, deleted {}, attachments {}, failed downloads {}, pending outbox {}, earliest {:?}, latest {:?}, missing ranges {}",
                        status.slug,
                        status.events,
                        status.cursor,
                        status.coverage,
                        status.hash_mismatches,
                        status.edited_count,
                        status.deleted_count,
                        status.attachment_count,
                        status.failed_attachment_downloads,
                        status.pending_archive_outbox,
                        status.earliest_date,
                        status.latest_date,
                        status.missing_ranges.len()
                    );
                }
            }
            if let Some(error) = verification_error(&statuses) {
                bail!("{error}");
            }
        }
        Command::Status { json } => {
            let statuses = store.statuses()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&statuses)?);
            } else {
                for status in statuses {
                    println!(
                        "{}: {} events, cursor {}, coverage {}, hash mismatches {}, edited {}, deleted {}, attachments {}, failed downloads {}, pending outbox {}, earliest {:?}, latest {:?}, missing ranges {}, failure {:?}",
                        status.slug,
                        status.events,
                        status.cursor,
                        status.coverage,
                        status.hash_mismatches,
                        status.edited_count,
                        status.deleted_count,
                        status.attachment_count,
                        status.failed_attachment_downloads,
                        status.pending_archive_outbox,
                        status.earliest_date,
                        status.latest_date,
                        status.missing_ranges.len(),
                        status.failure
                    );
                }
            }
        }
        Command::Watch => {
            let (client, session) = connect(&config).await?;
            let listen = config.health.listen.clone();
            let ready = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let health = tokio::spawn(health_server(listen, Arc::clone(&ready), Arc::clone(&stop)));
            let mut failure_delay = Duration::from_secs(2);
            loop {
                tokio::select! {
                    result = reconcile(&client, &session, &mut store, false) => {
                        let delay = match result {
                            Ok(_) => { ready.store(true, Ordering::Relaxed); failure_delay = Duration::from_secs(2); Duration::from_secs(15) },
                            Err(error) => { eprintln!("watch reconcile failed: {error:#}"); ready.store(false, Ordering::Relaxed); let delay = failure_delay; failure_delay = (failure_delay * 2).min(Duration::from_secs(60)); delay },
                        };
                        tokio::time::sleep(delay).await;
                    },
                    _ = tokio::signal::ctrl_c() => { stop.store(true, Ordering::Relaxed); break; }
                }
            }
            client.disconnect();
            health.abort();
        }
    }
    Ok(())
}
