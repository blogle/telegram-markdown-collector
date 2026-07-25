# Local Credential Smoke Runbook

The offline pipeline does not require Telegram credentials. Live authentication
does, and has not been validated in this environment because
`TELEGRAM_API_ID` and `TELEGRAM_API_HASH` are absent.

1. Copy `.env.local.example` to `.env.local` and fill the API ID, API hash, and phone number. Keep the file untracked.
2. Load it into the current shell: `set -a; . ./.env.local; set +a`.
3. Run authentication with `nix develop --command cargo run -- --config config.example.yaml auth`.
4. Complete the Telegram login code and optional 2FA prompts. The session is stored at the configured `session_path`.
5. Enumerate usable stream IDs with `nix develop --command cargo run -- --config config.example.yaml chats --json`; copy the numeric `id` and `title` into configuration.
6. Run a bounded live sync with `nix develop --command cargo run -- --config config.example.yaml sync`.
7. Inspect `status --json` and `verify --json`; `hash_mismatches` must be zero.

The exact first live command after the environment is present is:

```sh
set -a; . ./.env.local; set +a; nix develop --command cargo run -- --config config.example.yaml auth
```

Attachment bytes are intentionally reported as unsupported metadata rather than
silently claiming a download.
