# Design and Limitations

## Pipeline

Fixture replay is the deterministic input boundary for tests. The live boundary uses grammers 0.10.0 directly: persistent `SqliteSession`, `SenderPool`, `Client`, interactive user login, `iter_dialogs`, cached `PeerRef` resolution, and `iter_messages`. No message-send API is exposed.

Messages are normalized into `Message`, then written in one SQLite transaction. The event key is `stream/message_id`; upserts make retries safe. Cursors advance monotonically in the same transaction, and the committed changed-event list is then appended to the raw archive. Raw JSONL is partitioned by configured-timezone observation date. Markdown is a projection partitioned by the message's original configured-timezone date, retained across edits.

Rendering sorts by message ID, emits stable anchors and same-day or cross-day reply links, tracks partition hashes, and writes through a unique same-directory temporary file. Attachment links are calculated relative to the actual configured roots and output depth. Existing identical bytes are left untouched. Startup removes stale temporary files from output, archive, and work roots.

## Safety

Configuration creates and validates four independent roots. Generated relative paths reject absolute paths and parent components, and parent canonicalization prevents escaping a root. Attachment names from a live adapter must be validated with the same helper before download.

## Live boundary

`grammers-client = 0.10.0` is pinned in `Cargo.toml`. The live adapter requires API ID/hash, a persisted session, interactive login code/password handling, and network access. It reads message metadata, reply/edit fields, media metadata, and downloads media through grammers' read-only download API. No message-send API is called.

## Known limitations

- The configured IANA timezone is used for archive and Markdown partitioning; Telegram timestamps remain UTC in storage.
- `watch` periodically reconciles with a bounded interval; Telegram push-update streaming is not used.
- Unsupported media types are represented by no file attachment. Downloadable photos/documents/stickers are written below `attachment_root/<stream-slug>/<message-id>/`; failures are retained in the message payload and do not claim a local file.
- Historical edits are covered by fixture replay; live sync intentionally starts after its stored cursor and does not rescan all history.
