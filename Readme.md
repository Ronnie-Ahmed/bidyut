# Bidyut (বিদ্যুৎ)

> A crowdsourced power-outage tracker for Bangladesh, written in Rust.

When the power goes out, people want two answers: *is it just my house, or the whole area?* and *when will it come back?* Bidyut collects reports from many people, decides from independent evidence whether an area is really out, keeps a history of past outages, and alerts subscribers through a Telegram bot (in Bangla and English) or an HTTP API.

**Status:** the backend and the Telegram bot are implemented and tested locally. The project has not been deployed to a server yet. See [Known limitations and roadmap](#known-limitations-and-roadmap).

## Contents

- [How it works](#how-it-works)
- [Telegram bot](#telegram-bot)
- [HTTP API](#http-api)
- [Quick start](#quick-start)
- [Configuration](#configuration)
- [Testing](#testing)
- [Architecture](#architecture)
- [Security and privacy](#security-and-privacy)
- [Observability](#observability)
- [Deployment](#deployment)
- [Known limitations and roadmap](#known-limitations-and-roadmap)
- [Contributing](#contributing)
- [License](#license)

## How it works

1. **Report.** A user reports `outage` or `restored` from a location.
2. **Grid cell.** The location becomes a grid cell of 0.01° (about 1.1 km): `cell = (floor(lat × 100), floor(lng × 100))`. No address data is needed. Locations outside Bangladesh's bounding box (lat 20.5 to 26.7, lng 88.0 to 92.7) are rejected.
3. **Status engine.** A cell changes state only when enough independent evidence exists:
   - An `up` cell goes **down** when **3 distinct users** have reported an outage within the last **10 minutes**.
   - A `down` cell goes **up** when **2 distinct users** report `restored`.
   - Each user counts once, using their latest report. Repeating a report adds nothing.
   - Only reports **newer than the last status change** count, so old evidence can never reopen a restored area.
4. **Alerts.** When a cell changes state, one notification row per subscriber is written in the *same transaction* (transactional outbox). A background worker delivers them.
5. **History.** Every outage is stored as an event with a start, an end and how it was closed (`reports` or `timeout`).

A cell that goes down and never receives a `restored` report is closed automatically after **2 hours** without any new outage report. The end time is the last outage report seen, so downtime is never invented, and subscribers receive an "expired" alert (presumed over).

## Telegram bot

The bot is the main way for people to use Bidyut. Messages are in Bangla with an English translation.

1. `/start` shows a welcome message and a **📍 Share location** button.
2. After a location is shared, the bot shows the current status of that roughly 1 km area, a map link, and four buttons: **🔴 power out**, **🟢 power back**, **📊 status** and **🔔 alerts**.
3. Subscribers get a message whenever their area goes down, comes back, or is presumed over.
4. `/subscriptions` lists a user's alerts, each with a ❌ button to remove it.
5. `/stop` removes all of a user's alerts and the link to their chat.

How the bot treats identity and privacy:

- A Telegram account is one voter. The internal id is an HMAC of the Telegram user id with the server secret, so it cannot be computed without that secret.
- A shared location is reduced to its grid cell immediately. Buttons carry only the cell numbers. Exact coordinates are never stored or logged.
- The bot only talks in private chats. It uses long polling, so it needs no public URL.
- If a user blocks the bot, their alerts are removed automatically.

## HTTP API

All responses are JSON. Errors have the shape `{"error": "..."}`. Endpoints marked 🔑 need the header `Authorization: Bearer <device_token>`.

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/livez` | | Liveness: the process is up. Never touches dependencies. |
| GET | `/readyz` | | Readiness: `{"database": bool, "redis": bool}`. Returns 503 only if the database is unreachable. |
| POST | `/devices` | | Issues a signed device token. |
| POST | `/reports` | 🔑 | Records an outage or restored report. |
| GET | `/status?lat=&lng=` | | Current status of the cell containing the location. |
| GET | `/history?lat=&lng=&limit=` | | Past outages, newest first (`limit` default 20, max 100). |
| GET | `/stats?lat=&lng=&days=` | | Outage count, total and average downtime, and outages by hour of day in Asia/Dhaka time (`days` default 30, max 365). |
| POST | `/subscriptions` | 🔑 | Subscribe the device to a cell (max 5 per device). |
| GET | `/subscriptions` | 🔑 | List the device's subscriptions. |
| DELETE | `/subscriptions/{id}` | 🔑 | Unsubscribe. Pending alerts for it are cancelled. |

A cell nobody has reported from is not an error: `/status` returns `known: false` and `status: "up"`.

Devices registered through the HTTP API have no delivery channel, so they can read data and report, but only Telegram users receive alerts.

### Example

```bash
# 1. get a device token
curl -s -X POST http://localhost:3000/devices
# {"device_token":"<uuid>.<hex signature>"}

TOKEN="paste-the-token-here"

# 2. report an outage in Dhaka
curl -s -X POST http://localhost:3000/reports \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"kind":"outage","lat":23.8103,"lng":90.4125}'
# {"report_id":1,"cell_id":1,"lat_idx":2381,"lng_idx":9041,"cell_status":"up"}

# 3. check the area
curl -s "http://localhost:3000/status?lat=23.8103&lng=90.4125"
```

The cell flips to `down` once three different devices have reported.

## Quick start

**Requirements:** Rust (stable, edition 2024), Docker with Compose, and [`sqlx-cli`](https://crates.io/crates/sqlx-cli).

```bash
git clone https://github.com/<your-username>/bidyut.git
cd bidyut

# 1. start Postgres and Redis
docker compose up -d

# 2. configure
cp .env.example .env
# edit .env: set DEVICE_TOKEN_SECRET to the output of:  openssl rand -hex 32

# 3. create the schema
cargo install sqlx-cli --no-default-features --features rustls,postgres
sqlx migrate run

# 4. run
cargo run
```

Check it with `curl -s localhost:3000/readyz`.

To enable the Telegram bot, create a bot with [@BotFather](https://t.me/BotFather) and put its token in `.env` as `TELEGRAM_BOT_TOKEN`. Leave it blank to run without the bot. Use a separate bot for development and for production: Telegram allows only one process to poll per bot token.

> If port 5432 or 6379 is already used on your machine, change the left side of the port mappings in `docker-compose.yml` (for example `5433:5432`) and update `DATABASE_URL` and `REDIS_URL` in `.env` to match.

Alternatively, set `RUN_MIGRATIONS=true` and the app applies its embedded migrations at startup.

## Configuration

Everything is configured through environment variables (a `.env` file is loaded in development). The app validates all of them at startup and reports every problem at once, without ever printing a secret.

| Variable | Default | Description |
|---|---|---|
| `DATABASE_URL` | required | `postgres://user:pass@host:port/db` |
| `REDIS_URL` | required | `redis://[:password@]host:port` |
| `DEVICE_TOKEN_SECRET` | required | Key used to sign device tokens and derive Telegram device ids. At least 32 characters. Changing it invalidates every issued token. |
| `TELEGRAM_BOT_TOKEN` | unset | Enables the Telegram bot. Blank or unset disables it. |
| `TRUST_PROXY` | `false` | Set to `true` **only** behind a trusted reverse proxy that sets `X-Forwarded-For`. The last entry of that header is used as the client address. |
| `BIND_ADDR` | `0.0.0.0:3000` | API listen address. |
| `METRICS_ADDR` | `127.0.0.1:9090` | Metrics listen address. Never exposed on the public port. |
| `LOG_FORMAT` | `pretty` | `pretty` or `json`. |
| `RUST_LOG` | `info` | Log filter. |
| `DB_MAX_CONNECTIONS` | `10` | Database pool size (1 to 100). |
| `REQUEST_TIMEOUT_SECS` | `10` | Hard limit per request (1 to 60). |
| `MAX_BODY_BYTES` | `16384` | Maximum request body size (1024 to 1048576). |
| `RUN_MIGRATIONS` | `false` | Apply embedded migrations at startup. |

`.env` is git-ignored. Never commit real secrets. `.env.example` documents every setting.

## Testing

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

- **Unit tests** cover grid math, the vote decision function, device tokens, configuration validation, retry backoff, IPv6 rate-limit keys, Telegram identity derivation, button-callback parsing and Dhaka time formatting.
- **Integration tests** (`src/integration_tests.rs`) use `#[sqlx::test]`. Each test gets its own freshly migrated, throwaway database from the Postgres server in `DATABASE_URL`, so the database user needs permission to create databases. They cover vote counting, the 10-minute window, the stale-evidence rule, 10 simultaneous reports against one cell, outage expiry, exactly-once alert delivery with concurrent workers, retry and backoff, recovery of a crashed worker's claim, data pruning, and the Telegram actions (voting, subscribing, the subscription limit, `/stop`, and delivery rules).

CI runs formatting, clippy with warnings as errors, and the full test suite against a Postgres service container.

## Architecture

```
Telegram users ──(long polling)──┐
                                 v
phones / curl ──HTTPS──> Caddy ──> Axum API + bot + background workers ──> PostgreSQL
                                                                      └──> Redis (rate limits)
```

```
src/
  main.rs            startup, router, graceful shutdown
  config.rs          validated environment configuration
  state.rs           shared application state
  error.rs           error type -> JSON responses
  grid.rs            coordinates -> grid cell, Bangladesh bounds
  device.rs          signed device tokens, POST /devices
  ratelimit.rs       Redis fixed-window rate limiting, client IP extraction
  reports.rs         POST /reports (record_report is the core, free of HTTP)
  status.rs          the vote rules and status transitions
  read.rs            /status, /history, /stats
  subscriptions.rs   subscribe, list, unsubscribe
  telegram.rs        minimal Telegram Bot API client
  bot.rs             bot handlers, texts (Bangla and English), alert delivery
  worker.rs          alert delivery, stale-outage expiry, data pruning
  health.rs          /livez, /readyz
  layers.rs          JSON errors, request timeout, request metrics
  observability.rs   logging and Prometheus metrics
  shutdown.rs        SIGINT / SIGTERM handling
  integration_tests.rs
migrations/          SQL schema
deploy/              production compose file, Caddyfile, deployment scripts
.github/workflows/   CI, dependency audit, image build
```

Design notes:

- **Correctness under concurrency.** Reports for the same cell are serialized by the cell's row lock, and a partial unique index guarantees at most one open outage per cell. `evaluate_cell` must be called while holding that lock.
- **Alert delivery is at-least-once.** The worker claims batches with `FOR UPDATE SKIP LOCKED` and a 60-second lease, so several instances can run safely and a crashed worker's claim becomes available again. Failures retry with exponential backoff (30 s, 60 s, 120 s, ... capped at 30 minutes) for up to 5 attempts, then are marked `failed`. A permanent failure never blocks other alerts.
- **One core, two front ends.** The API and the bot call the same functions (`record_report`, `add_subscription`), so both obey the same rules and limits.
- **Graceful shutdown.** On SIGINT or SIGTERM the server stops accepting connections, finishes in-flight requests, stops the workers and the bot, and closes the database pool.

## Security and privacy

- **No exact locations are stored.** Reports keep only the grid cell. Subscriptions keep only the cell. Request logs keep the path, never the query string. The bot never logs updates.
- **Data retention.** An hourly job deletes reports older than 48 hours and finished notifications older than 7 days. Pending notifications are never deleted.
- **User control.** `/stop` in the bot deletes a user's subscriptions and the link to their chat.
- **Signed device tokens.** `POST /devices` issues `<uuid>.<HMAC-SHA256 signature>`, verified in constant time, so clients cannot invent identities. Telegram users get an id derived from their Telegram account instead.
- **Rate limits** (Redis): token issuance per IP, reports per device (shared by the API and the bot) and per IP, subscriptions per device, and a flood limit per Telegram user. IPv6 addresses are limited per /64. If Redis is unavailable the limiter fails open and logs an error, so the service keeps working.
- **Client address handling.** Forwarded headers are ignored unless `TRUST_PROXY=true`.
- **Secrets stay secret.** Configuration errors never echo values. The Telegram token never appears in logs, because error messages are stripped of request URLs.
- **Hardened container.** The image runs as a non-root user. In the production compose file the app has a read-only filesystem and no Linux capabilities, and Postgres and Redis sit on an internal network with no published ports.
- **CI hygiene.** Minimal workflow permissions, no persisted credentials, `--locked` builds, a weekly `cargo audit`, and Dependabot for crates, actions and base images.

Limits of this design: one IP address can represent many users behind carrier-grade NAT, which is why per-IP limits are generous. Anyone with several Telegram accounts (or many IP addresses, for the HTTP API) can cast several votes. Treat the vote threshold and rate limits as friction, not proof of identity.

## Observability

- **Logs:** structured JSON with `LOG_FORMAT=json`, each request tagged with an `x-request-id` that is also returned to the caller.
- **Health:** `/livez` for liveness and `/readyz` for readiness.
- **Metrics:** Prometheus format on `METRICS_ADDR` at `/metrics` (a separate, internal port). Includes `http_requests_total` and `http_request_duration_seconds` (labelled by route pattern, not raw URL), `reports_total`, `rate_limited_total`, `rate_limiter_failures_total`, `request_timeouts_total`, `outages_started_total`, `outages_restored_total`, `outages_expired_total`, `notifications_sent_total`, `notifications_failed_total`, `reports_pruned_total`, `notifications_pruned_total` and `telegram_updates_total`.

## Deployment

The deployment setup is prepared but has not been run on a real server yet. The target is a single small VPS in a nearby region (for example Singapore or Mumbai), running Docker Compose with four containers: the app, PostgreSQL, Redis and Caddy (automatic HTTPS). Only Caddy publishes ports (80 and 443).

1. **Image.** `Dockerfile` is a multi-stage build producing a small non-root image. The `Image` workflow builds it on pull requests and publishes it to GitHub Container Registry on pushes to `main` and on `v*` tags, tagged with the full commit SHA. The runtime stage needs CA certificates for the bot's outgoing HTTPS calls.
2. **Server.** `deploy/server-setup.sh` prepares a fresh Ubuntu server: a non-root `deploy` user with your SSH key, a firewall (SSH, 80, 443), automatic security updates, Docker, and SSH password and root login disabled after you confirm that a second login works.
3. **Release.** Copy `deploy/docker-compose.prod.yml`, `deploy/Caddyfile` and `deploy/deploy.sh` to the server, then:

   ```bash
   ./deploy.sh init your-domain.example      # once: generates random secrets in .env.prod
   ./deploy.sh deploy <full-commit-sha>      # pull, start, wait for health, smoke test
   ./deploy.sh rollback                      # redeploy the previous version
   ```

   `deploy.sh` checks that the app is healthy, that `https://<domain>/readyz` answers, and that only Caddy publishes ports.
4. **Bot token.** Use a separate production bot, and pass `TELEGRAM_BOT_TOKEN` to the app through `.env.prod`.

Migrations only move forward. A rollback is safe only while the older code works with the current database schema.

## Known limitations and roadmap

- **Not deployed yet.** There is no public instance. Backups (scheduled `pg_dump` copied off the server, plus a tested restore) are planned for the deployment step.
- **Telegram only.** Alerts reach Telegram users. There is no web or mobile client, and devices registered through the HTTP API cannot receive alerts.
- **Single bot instance.** The bot uses long polling, which allows one polling process per bot token. Running several instances would need webhooks.
- **Identity is lightweight.** HTTP tokens are free to obtain (rate-limited per IP) and cannot be revoked individually. Telegram accounts are a stronger signal but not a perfect one.
- **Grid cells, not administrative areas.** Cells do not map to wards or upazilas yet. The bounding box also includes small parts of neighbouring countries.
- **Fixed-window rate limits** allow a burst of up to twice the limit across a window boundary.
- **Subscription fan-out** inserts one row per subscriber inside the status-change transaction. This is fine for thousands of subscribers per cell, but very large cells would need fan-out moved into the worker.
- **Bangla text** was drafted by a non-native speaker and should be reviewed by native readers. All texts live in one module (`src/bot.rs`).

## Contributing

Issues and pull requests are welcome. Before opening a pull request, run formatting, clippy and the tests as shown in [Testing](#testing). CI runs the same checks and a pull request needs them to pass.

## License

Released under the MIT License. See the `LICENSE` file.