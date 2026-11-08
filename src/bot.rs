//! Telegram bot: long-polling loop, message handlers and alert delivery.
//!
//! Identity: a Telegram user is a "device". The device id is an HMAC of the
//! Telegram user id, so one Telegram account is one voter, and the id cannot be
//! computed without the server secret.
//!
//! Privacy: exact locations are never stored or logged. A shared location is
//! reduced to its ~1 km grid cell immediately, and buttons carry only the cell
//! indices. Never log a whole `Update`: it contains the user's location.

use std::{sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    error::AppError,
    grid, ratelimit,
    reports::{self, ReportKind, record_report},
    state::AppState,
    status::{self, Status},
    subscriptions,
    telegram::{CallbackQuery, Message, SendOutcome, Telegram, Update},
};

type HmacSha256 = Hmac<Sha256>;

// ------------------------------------------------------------------ texts

mod txt {
    pub const WELCOME: &str = "স্বাগতম! 👋\n\
এই বট আপনার এলাকার লোডশেডিং ও বিদ্যুৎ বিভ্রাটের খবর জানায়। কয়েকজন আলাদা মানুষ রিপোর্ট করলে এলাকাটি “বিদ্যুৎ নেই” হিসেবে দেখানো হয়।\n\n\
📍 শুরু করতে নিচের বোতামে চাপ দিয়ে আপনার লোকেশন পাঠান।\n\n\
আপনার সঠিক লোকেশন জমা রাখা হয় না, শুধু প্রায় ১ কিমি এলাকার একটি ঘর ব্যবহার করা হয়।\n\n\
/subscriptions – আমার অ্যালার্ট\n\
/stop – সব অ্যালার্ট বন্ধ করে আমার তথ্য মুছুন\n\n\
—\n\
Welcome! 👋\n\
This bot tracks power outages in your area. When several different people report an outage, the area is shown as out.\n\n\
📍 To begin, tap the button below to share your location.\n\n\
Your exact location is not stored; only a grid cell of about 1 km is used.\n\n\
/subscriptions – my alerts\n\
/stop – stop all alerts and delete my data";

    pub const SEND_LOCATION: &str = "📍 আপনার লোকেশন পাঠান (নিচের বোতামে চাপুন)।\n\
Please share your location (tap the button below).";

    pub const OUTSIDE: &str = "দুঃখিত, এই লোকেশনটি বাংলাদেশের বাইরে।\n\
Sorry, that location is outside Bangladesh.";

    pub const AREA_HEADER: &str = "📍 আপনার এলাকা (প্রায় ১ কিমি) · Your area (about 1 km)";
    pub const WHAT_TO_DO: &str = "কী করতে চান? · What would you like to do?";

    pub const THANKS: &str = "🙏 ধন্যবাদ! আপনার রিপোর্ট নেওয়া হয়েছে।\n\
Thanks! Your report was recorded.";

    pub const TOO_MANY: &str = "⏳ অনেক বেশি অনুরোধ। কিছুক্ষণ পরে আবার চেষ্টা করুন।\n\
Too many requests. Please try again in a few minutes.";

    pub const ERROR: &str = "⚠️ কিছু একটা সমস্যা হয়েছে। একটু পরে আবার চেষ্টা করুন।\n\
Something went wrong. Please try again shortly.";

    pub const FOLLOWING: &str = "🔔 ঠিক আছে! এই এলাকায় বিদ্যুৎ গেলে বা এলে আপনাকে জানানো হবে।\n\
OK! You'll be alerted when power goes out or comes back in this area.";

    pub const NO_ALERTS: &str = "আপনার কোনো অ্যালার্ট চালু নেই। লোকেশন পাঠিয়ে চালু করুন।\n\
You have no alerts. Share a location to add one.";

    pub const ALERTS_HEADER: &str = "🔔 আপনার অ্যালার্ট (বন্ধ করতে ❌ চাপুন)\n\
Your alerts (tap ❌ to remove):";

    pub const REMOVED: &str = "✅ অ্যালার্ট বন্ধ করা হয়েছে।\n✅ Alert removed.";

    pub const ALREADY_REMOVED: &str = "এই অ্যালার্টটি আগেই বন্ধ করা হয়েছে।\n\
This alert was already removed.";

    pub const STOPPED: &str = "✅ সব অ্যালার্ট বন্ধ করা হয়েছে এবং আপনার তথ্য মুছে ফেলা হয়েছে। আবার শুরু করতে /start লিখুন।\n\
✅ All alerts stopped and your data deleted. Send /start to begin again.";
}

fn alert_text(kind: &str) -> &'static str {
    match kind {
        "down" => {
            "🔴 আপনার এলাকায় বিদ্যুৎ নেই বলে একাধিক রিপোর্ট এসেছে।\nPower outage reported in your area."
        }
        "restored" => {
            "🟢 আপনার এলাকায় বিদ্যুৎ ফিরে এসেছে বলে রিপোর্ট এসেছে।\nPower reported back in your area."
        }
        "expired" => {
            "⚪ অনেকক্ষণ কোনো নতুন রিপোর্ট আসেনি, তাই বিভ্রাট শেষ হয়েছে বলে ধরা হচ্ছে।\nNo recent reports: the outage is presumed over."
        }
        _ => "ℹ️ আপনার এলাকার অবস্থা বদলেছে।\nStatus update for your area.",
    }
}

fn need_more_outage() -> String {
    let n = status::DOWN_THRESHOLD;
    format!(
        "আরও প্রতিবেশীর রিপোর্ট লাগবে: এলাকাটি “বিদ্যুৎ নেই” দেখাতে {n} জন আলাদা মানুষকে জানাতে হবে।\n\
More neighbours need to report: {n} different people are needed to mark the area as out."
    )
}

fn need_more_restored() -> String {
    let n = status::UP_THRESHOLD;
    format!(
        "{n} জন আলাদা মানুষ জানালে বিভ্রাট শেষ হয়েছে বলে ধরা হবে।\n\
Once {n} different people report power is back, the outage is considered over."
    )
}

fn dhaka_time(t: DateTime<Utc>) -> String {
    let zone = chrono::FixedOffset::east_opt(6 * 3600).expect("valid offset");
    t.with_timezone(&zone).format("%d %b, %I:%M %p").to_string()
}

fn status_text(status: &str, since: Option<DateTime<Utc>>) -> String {
    if status == "down" {
        let when = since
            .map(|s| format!(" (since {})", dhaka_time(s)))
            .unwrap_or_default();
        format!("🔴 বিদ্যুৎ নেই বলে রিপোর্ট এসেছে · Outage reported{when}")
    } else {
        "🟢 এখন কোনো বিভ্রাটের রিপোর্ট নেই · No outage reported".to_string()
    }
}

// ------------------------------------------------------ identity and cells

pub fn device_for_telegram_user(secret: &[u8], telegram_user_id: i64) -> Uuid {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(b"telegram:");
    mac.update(&telegram_user_id.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    Uuid::from_bytes(bytes)
}

/// The middle of a grid cell: the only "location" the bot ever works with.
pub fn cell_center(cell: grid::CellIndex) -> (f64, f64) {
    (
        (cell.lat_idx as f64 + 0.5) / 100.0,
        (cell.lng_idx as f64 + 0.5) / 100.0,
    )
}

fn map_link(lat: f64, lng: f64) -> String {
    format!("https://maps.google.com/?q={lat:.4},{lng:.4}")
}

// -------------------------------------------------------- button callbacks

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Action {
    Outage,
    Restored,
    Status,
    Follow,
}

impl Action {
    fn code(self) -> &'static str {
        match self {
            Action::Outage => "o",
            Action::Restored => "r",
            Action::Status => "s",
            Action::Follow => "f",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Callback {
    OnCell(Action, grid::CellIndex),
    Unfollow(i64),
}

pub fn encode_cell_action(action: Action, cell: grid::CellIndex) -> String {
    format!("{}:{}:{}", action.code(), cell.lat_idx, cell.lng_idx)
}

pub fn parse_callback(data: &str) -> Option<Callback> {
    let mut parts = data.split(':');
    let code = parts.next()?;

    if code == "u" {
        let id: i64 = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        return Some(Callback::Unfollow(id));
    }

    let action = match code {
        "o" => Action::Outage,
        "r" => Action::Restored,
        "s" => Action::Status,
        "f" => Action::Follow,
        _ => return None,
    };
    let lat_idx: i32 = parts.next()?.parse().ok()?;
    let lng_idx: i32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(Callback::OnCell(
        action,
        grid::CellIndex { lat_idx, lng_idx },
    ))
}

fn command_of(text: Option<&str>) -> Option<String> {
    let first = text?.split_whitespace().next()?;
    let command = first.strip_prefix('/')?;
    let command = command.split('@').next().unwrap_or(command);
    Some(command.to_lowercase())
}

// ------------------------------------------------------------------ replies

pub struct Reply {
    pub text: String,
    pub keyboard: Option<Value>,
}

impl Reply {
    fn text(text: impl Into<String>) -> Reply {
        Reply {
            text: text.into(),
            keyboard: None,
        }
    }

    fn with_keyboard(text: impl Into<String>, keyboard: Value) -> Reply {
        Reply {
            text: text.into(),
            keyboard: Some(keyboard),
        }
    }
}

fn share_location_keyboard() -> Value {
    json!({
        "keyboard": [[{ "text": "📍 লোকেশন পাঠান · Share location", "request_location": true }]],
        "resize_keyboard": true,
        "one_time_keyboard": true
    })
}

fn area_keyboard(cell: grid::CellIndex) -> Value {
    json!({
        "inline_keyboard": [
            [
                { "text": "🔴 বিদ্যুৎ নেই", "callback_data": encode_cell_action(Action::Outage, cell) },
                { "text": "🟢 বিদ্যুৎ এসেছে", "callback_data": encode_cell_action(Action::Restored, cell) }
            ],
            [
                { "text": "📊 অবস্থা", "callback_data": encode_cell_action(Action::Status, cell) },
                { "text": "🔔 অ্যালার্ট", "callback_data": encode_cell_action(Action::Follow, cell) }
            ]
        ]
    })
}

// ------------------------------------------- actions (database only, tested)

async fn cell_status(
    pool: &PgPool,
    cell: grid::CellIndex,
) -> Result<(String, Option<DateTime<Utc>>), sqlx::Error> {
    let row: Option<(String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT status, status_since FROM cells WHERE lat_idx = $1 AND lng_idx = $2",
    )
    .bind(cell.lat_idx)
    .bind(cell.lng_idx)
    .fetch_optional(pool)
    .await?;

    Ok(match row {
        Some((status, since)) => (status, Some(since)),
        None => ("up".to_string(), None),
    })
}

pub async fn register_chat(
    pool: &PgPool,
    device_id: Uuid,
    chat_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO telegram_chats (device_id, chat_id) VALUES ($1, $2)
         ON CONFLICT (device_id) DO UPDATE SET chat_id = EXCLUDED.chat_id",
    )
    .bind(device_id)
    .bind(chat_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn forget_device(pool: &PgPool, device_id: Uuid) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM subscriptions WHERE device_id = $1")
        .bind(device_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM telegram_chats WHERE device_id = $1")
        .bind(device_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn area_reply(pool: &PgPool, cell: grid::CellIndex) -> Result<Reply, AppError> {
    let (status, since) = cell_status(pool, cell).await?;
    let (lat, lng) = cell_center(cell);
    Ok(Reply::with_keyboard(
        format!(
            "{}\n\n{}\n📍 {}\n\n{}",
            txt::AREA_HEADER,
            status_text(&status, since),
            map_link(lat, lng),
            txt::WHAT_TO_DO
        ),
        area_keyboard(cell),
    ))
}

pub async fn act_status(pool: &PgPool, cell: grid::CellIndex) -> Result<Reply, AppError> {
    let (status, since) = cell_status(pool, cell).await?;
    let (lat, lng) = cell_center(cell);
    Ok(Reply::with_keyboard(
        format!("📍 {}\n{}", map_link(lat, lng), status_text(&status, since)),
        area_keyboard(cell),
    ))
}

pub async fn act_report(
    pool: &PgPool,
    device_id: Uuid,
    kind: ReportKind,
    cell: grid::CellIndex,
) -> Result<Reply, AppError> {
    let (lat, lng) = cell_center(cell);
    let recorded = record_report(pool, device_id, kind, lat, lng).await?;
    let (status, since) = cell_status(pool, cell).await?;

    let mut body = format!("{}\n{}", txt::THANKS, status_text(&status, since));
    match (kind, recorded.status) {
        (ReportKind::Outage, Status::Up) => {
            body.push_str("\n\n");
            body.push_str(&need_more_outage());
        }
        (ReportKind::Restored, Status::Down) => {
            body.push_str("\n\n");
            body.push_str(&need_more_restored());
        }
        _ => {}
    }
    Ok(Reply::with_keyboard(body, area_keyboard(cell)))
}

pub async fn act_follow(
    pool: &PgPool,
    device_id: Uuid,
    cell: grid::CellIndex,
) -> Result<Reply, AppError> {
    let (_id, status) = subscriptions::add_subscription(pool, device_id, cell).await?;
    let (lat, lng) = cell_center(cell);
    Ok(Reply::text(format!(
        "{}\n\n{}\n📍 {}",
        txt::FOLLOWING,
        status_text(&status, None),
        map_link(lat, lng)
    )))
}

pub async fn act_list(pool: &PgPool, device_id: Uuid) -> Result<Reply, AppError> {
    let rows: Vec<(i64, i32, i32, String)> = sqlx::query_as(
        "SELECT s.id, c.lat_idx, c.lng_idx, c.status
         FROM subscriptions s
         JOIN cells c ON c.id = s.cell_id
         WHERE s.device_id = $1
         ORDER BY s.id",
    )
    .bind(device_id)
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok(Reply::text(txt::NO_ALERTS));
    }

    let mut lines = vec![txt::ALERTS_HEADER.to_string()];
    let mut buttons = Vec::new();
    for (n, (id, lat_idx, lng_idx, status)) in rows.iter().enumerate() {
        let (lat, lng) = cell_center(grid::CellIndex {
            lat_idx: *lat_idx,
            lng_idx: *lng_idx,
        });
        let icon = if status == "down" { "🔴" } else { "🟢" };
        lines.push(format!("{}. {icon} {}", n + 1, map_link(lat, lng)));
        buttons
            .push(json!({ "text": format!("❌ {}", n + 1), "callback_data": format!("u:{id}") }));
    }

    Ok(Reply::with_keyboard(
        lines.join("\n"),
        json!({ "inline_keyboard": [buttons] }),
    ))
}

pub async fn act_unfollow(pool: &PgPool, device_id: Uuid, sub_id: i64) -> Result<Reply, AppError> {
    // matching on device_id means nobody can remove someone else's alert
    let result = sqlx::query("DELETE FROM subscriptions WHERE id = $1 AND device_id = $2")
        .bind(sub_id)
        .bind(device_id)
        .execute(pool)
        .await?;

    Ok(Reply::text(if result.rows_affected() == 0 {
        txt::ALREADY_REMOVED
    } else {
        txt::REMOVED
    }))
}

pub async fn act_stop(pool: &PgPool, device_id: Uuid) -> Result<Reply, AppError> {
    forget_device(pool, device_id).await?;
    Ok(Reply::text(txt::STOPPED))
}

// ---------------------------------------------------------- alert delivery

/// Called by the notification worker. Devices without a Telegram chat (for
/// example ones registered through the HTTP API) have no channel: nothing to do.
pub async fn deliver_alert(
    pool: &PgPool,
    telegram: Option<&Telegram>,
    device_id: Uuid,
    lat_idx: i32,
    lng_idx: i32,
    kind: &str,
) -> Result<(), String> {
    let chat_id: Option<i64> =
        sqlx::query_scalar("SELECT chat_id FROM telegram_chats WHERE device_id = $1")
            .bind(device_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| e.to_string())?;

    let Some(chat_id) = chat_id else {
        tracing::info!(%device_id, kind, "no delivery channel for this device; alert skipped");
        return Ok(());
    };

    // A chat exists but the bot is off: fail, so the alert is retried and not lost.
    let Some(telegram) = telegram else {
        return Err("telegram bot is not configured".to_string());
    };

    let (lat, lng) = cell_center(grid::CellIndex { lat_idx, lng_idx });
    let text = format!("{}\n\n📍 {}", alert_text(kind), map_link(lat, lng));

    match telegram.send_message(chat_id, &text, None).await {
        SendOutcome::Sent => Ok(()),
        SendOutcome::Blocked => {
            tracing::info!(%device_id, "user blocked the bot; removing their alerts");
            forget_device(pool, device_id)
                .await
                .map_err(|e| e.to_string())?;
            Ok(())
        }
        SendOutcome::Failed(reason) => Err(reason),
    }
}

// ------------------------------------------------------------- update loop

async fn limited(app: &AppState, key: &str, limit: i64, window_secs: i64) -> bool {
    ratelimit::check(&app.redis, key, limit, window_secs)
        .await
        .is_err()
}

async fn send_reply(tg: &Telegram, chat_id: i64, reply: Result<Reply, AppError>) {
    let reply = match reply {
        Ok(reply) => reply,
        Err(AppError::Validation(msg)) => Reply::text(format!("⚠️ {msg}")),
        Err(AppError::Database(e)) => {
            tracing::error!("bot database error: {e}");
            Reply::text(txt::ERROR)
        }
        Err(_) => Reply::text(txt::ERROR),
    };

    match tg.send_message(chat_id, &reply.text, reply.keyboard).await {
        SendOutcome::Sent => {}
        SendOutcome::Blocked => tracing::info!("user blocked the bot"),
        SendOutcome::Failed(reason) => tracing::warn!("telegram send failed: {reason}"),
    }
}

async fn location_reply(pool: &PgPool, lat: f64, lng: f64) -> Result<Reply, AppError> {
    if !lat.is_finite() || !lng.is_finite() || !grid::in_bangladesh(lat, lng) {
        return Ok(Reply::text(txt::OUTSIDE));
    }
    // From here on only the grid cell exists; the exact point is dropped.
    area_reply(pool, grid::cell_for(lat, lng)).await
}

async fn handle_message(app: &AppState, tg: &Telegram, msg: Message) {
    let Message {
        chat,
        from,
        text,
        location,
    } = msg;

    // The bot only talks in private chats.
    if chat.kind != "private" {
        return;
    }
    let Some(from) = from else { return };
    let device_id = device_for_telegram_user(&app.device_secret, from.id);

    if limited(app, &format!("rl:tg:user:{}", from.id), 30, 60).await {
        return;
    }

    if let Err(e) = register_chat(&app.pool, device_id, chat.id).await {
        tracing::error!("could not register chat: {e}");
        return;
    }

    let reply = if let Some(loc) = location {
        location_reply(&app.pool, loc.latitude, loc.longitude).await
    } else {
        match command_of(text.as_deref()).as_deref() {
            Some("start" | "help") => Ok(Reply::with_keyboard(
                txt::WELCOME,
                share_location_keyboard(),
            )),
            Some("subscriptions") => act_list(&app.pool, device_id).await,
            Some("stop") => act_stop(&app.pool, device_id).await,
            _ => Ok(Reply::with_keyboard(
                txt::SEND_LOCATION,
                share_location_keyboard(),
            )),
        }
    };

    send_reply(tg, chat.id, reply).await;
}

async fn handle_callback(app: &AppState, tg: &Telegram, cb: CallbackQuery) {
    // Always acknowledge, so the tapped button stops showing its spinner.
    tg.answer_callback_query(&cb.id).await;

    let Some(message) = cb.message else { return };
    if message.chat.kind != "private" {
        return;
    }
    let chat_id = message.chat.id;
    let device_id = device_for_telegram_user(&app.device_secret, cb.from.id);

    if limited(app, &format!("rl:tg:user:{}", cb.from.id), 30, 60).await {
        return;
    }
    let Some(parsed) = cb.data.as_deref().and_then(parse_callback) else {
        return;
    };

    let reply = match parsed {
        Callback::Unfollow(id) => act_unfollow(&app.pool, device_id, id).await,
        Callback::OnCell(action, cell) => {
            let (lat, lng) = cell_center(cell);
            if !grid::in_bangladesh(lat, lng) {
                Ok(Reply::text(txt::OUTSIDE))
            } else {
                match action {
                    Action::Outage | Action::Restored => {
                        let key = format!("rl:reports:dev:{device_id}");
                        if limited(
                            app,
                            &key,
                            reports::REPORTS_PER_DEVICE,
                            reports::REPORT_WINDOW_SECS,
                        )
                        .await
                        {
                            Ok(Reply::text(txt::TOO_MANY))
                        } else {
                            let kind = if action == Action::Outage {
                                ReportKind::Outage
                            } else {
                                ReportKind::Restored
                            };
                            act_report(&app.pool, device_id, kind, cell).await
                        }
                    }
                    Action::Status => act_status(&app.pool, cell).await,
                    Action::Follow => {
                        let key = format!("rl:subs:dev:{device_id}");
                        if limited(
                            app,
                            &key,
                            subscriptions::SUBSCRIBE_PER_DEVICE_PER_HOUR,
                            3600,
                        )
                        .await
                        {
                            Ok(Reply::text(txt::TOO_MANY))
                        } else {
                            act_follow(&app.pool, device_id, cell).await
                        }
                    }
                }
            }
        }
    };

    send_reply(tg, chat_id, reply).await;
}

async fn handle_update(app: AppState, tg: Arc<Telegram>, update: Update) {
    if let Some(message) = update.message {
        handle_message(&app, &tg, message).await;
    } else if let Some(callback) = update.callback_query {
        handle_callback(&app, &tg, callback).await;
    }
}

pub async fn run(app: AppState, tg: Arc<Telegram>, shutdown: CancellationToken) {
    let mut offset: i64 = 0;
    let mut backoff = Duration::from_secs(1);

    loop {
        let polled = tokio::select! {
            _ = shutdown.cancelled() => break,
            result = tg.get_updates(offset, 30) => result,
        };

        match polled {
            Ok(updates) => {
                backoff = Duration::from_secs(1);
                for update in updates {
                    offset = offset.max(update.update_id + 1);
                    metrics::counter!("telegram_updates_total").increment(1);
                    // One task per update: a panic in a handler cannot kill the poller.
                    let task = tokio::spawn(handle_update(app.clone(), tg.clone(), update));
                    if let Err(e) = task.await {
                        tracing::error!("telegram update handler failed: {e}");
                    }
                }
            }
            Err(e) => {
                tracing::warn!("telegram polling failed: {e}");
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
    tracing::info!("telegram bot stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"test-secret-test-secret-test-secret";

    #[test]
    fn same_telegram_user_is_always_the_same_device() {
        assert_eq!(
            device_for_telegram_user(SECRET, 42),
            device_for_telegram_user(SECRET, 42)
        );
    }

    #[test]
    fn different_users_are_different_devices() {
        assert_ne!(
            device_for_telegram_user(SECRET, 42),
            device_for_telegram_user(SECRET, 43)
        );
    }

    #[test]
    fn device_ids_depend_on_the_secret() {
        assert_ne!(
            device_for_telegram_user(SECRET, 42),
            device_for_telegram_user(b"another-secret-another-secret-123", 42)
        );
    }

    #[test]
    fn callbacks_round_trip_and_fit_telegrams_limit() {
        let cell = grid::CellIndex {
            lat_idx: 2381,
            lng_idx: 9041,
        };
        for action in [
            Action::Outage,
            Action::Restored,
            Action::Status,
            Action::Follow,
        ] {
            let data = encode_cell_action(action, cell);
            assert!(data.len() <= 64);
            assert_eq!(parse_callback(&data), Some(Callback::OnCell(action, cell)));
        }
    }

    #[test]
    fn malformed_callbacks_are_rejected() {
        for bad in ["", "x:1:2", "o:1", "o:a:b", "o:1:2:3", "u", "u:x", "u:1:2"] {
            assert_eq!(parse_callback(bad), None, "{bad}");
        }
        assert_eq!(parse_callback("u:7"), Some(Callback::Unfollow(7)));
    }

    #[test]
    fn commands_are_parsed() {
        assert_eq!(command_of(Some("/start")), Some("start".to_string()));
        assert_eq!(
            command_of(Some("/Stop@MyBot now")),
            Some("stop".to_string())
        );
        assert_eq!(command_of(Some("hello")), None);
        assert_eq!(command_of(None), None);
    }

    #[test]
    fn cell_center_stays_inside_its_cell() {
        let cell = grid::cell_for(23.8103, 90.4125);
        let (lat, lng) = cell_center(cell);
        assert_eq!(grid::cell_for(lat, lng), cell);
    }

    #[test]
    fn times_are_shown_in_dhaka_time() {
        let t = DateTime::parse_from_rfc3339("2026-11-08T14:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(dhaka_time(t), "08 Nov, 08:30 PM");
    }
}
