# Telegram Collector

An offline-first, stable-Rust Telegram-to-Markdown collector. `replay` is a deterministic credential-free ingestion path:

```sh
nix develop --command cargo run -- --config config.example.yaml replay fixtures/messages.jsonl
nix develop --command cargo run -- --config config.example.yaml verify --json
```

The flake provides the pinned Rust toolchain, native C linker/toolchain, and
SQLite development dependencies. It uses Crane's two-stage dependency build,
so ordinary source changes reuse the expensive Cargo dependency artifacts.
Build the package or OCI image with:

```sh
nix build .#default
nix build .#ociImage
```

The state database is SQLite with transactional event/cursor writes and an archive outbox populated in the same transaction. The outbox is flushed after ingestion and at startup before collection; raw JSONL is append-only under `archive/<slug>/YYYY/MM/YYYY-MM-DD.jsonl`, partitioned by configured-timezone `observed_at`, and contains normalized `schema_version`/`event_id`/`event` envelopes only for durable changes. Archive event IDs make a retry after a crash between file append and outbox deletion duplicate-safe; appended data is synced before deletion. Event classes are `message_created`, `message_edited`, and `message_deleted`. Markdown is grouped by the message's original configured-timezone date under `<output_subdirectory>/YYYY/YYYY-MM-DD.md`, uses stable `^telegram-<chat-id>-<message-id>` anchors, cross-day reply links, sender metadata, edits, tombstones, and relative attachment links derived from the configured output and attachment roots. Live media metadata is collected read-only and downloadable media is written below `attachment_root/<stream-slug>/<message-id>/<stable-file-name>`; failed downloads remain explicit in normalized state. Rendering hashes partitions, uses unique same-directory temporary files, and preserves bytes and mtime for no-op renders. Startup removes stale temporary files only beneath configured collector-owned stream, attachment, archive, and work paths.

Commands are `auth`, `chats`, `backfill`, `sync`, `watch`, `render`, `verify`, `status`, and `replay`. Live commands use grammers 0.10.0 user authentication, persistent `SqliteSession`, dialog enumeration, cached-peer resolution, and history iterators. `watch` binds configured health endpoints, reconciles continuously with bounded backoff, and handles Ctrl-C.

`status` and `verify` report each stream's earliest/latest message, current
message and cursor counts, edited/deleted messages, attachment and failed
download counts, rendered partitions and hash mismatches, pending archive
outbox work, last observation, and recoverable failure. `missing_ranges` remains
empty unless a future Telegram adapter can prove a source gap; numeric message
ID gaps alone are not treated as missing messages.

## Live limitation

The crate pins `grammers-client = 0.10.0` and `grammers-session = 0.10.0`. Runtime authentication and Telegram network calls require real API ID/hash, phone, login code, optional 2FA password, and network access; those are the only untested paths here. No message-send APIs are called.

## Design and limitations

Configuration validates IANA timezones, duplicate roots/stream paths, and traversal-safe generated paths. Event upserts and cursors make replay idempotent; raw records are emitted after a successful transaction and only for changed events. Per-stream failures are persisted and `verify` exits nonzero for recorded failures or rendered hash mismatches. The collector never sends Telegram messages.

## Quality

Run `bash scripts/check-boundaries.sh`, `bash scripts/test-isolated.sh`, and `nix build .#default`. The isolated test copies only this repository's tracked or unignored files to a temporary directory before running formatting, Clippy, and all tests.
