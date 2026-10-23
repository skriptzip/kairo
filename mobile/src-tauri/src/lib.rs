//! Kairo for Android: a thin Tauri shell around `kairo-core`.
//!
//! The web UI (`../ui`) only shows data and sends edits; storage, sync and the recurrence
//! rules are the same Rust code the desktop app uses.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
};

use chrono::{Duration, NaiveDate, NaiveDateTime, NaiveTime};
use kairo_core::{
    api::{self, Session},
    model::{self, Event, PendingDelete, Store},
    platform::{self, SecretStore},
    sync::{self, Failure},
};
use serde::{Deserialize, Serialize};
use tauri::{Manager, State};

const MAX_DAYS: i64 = 400;

/// The signed-in account with its local cache.
struct Active {
    session: Session,
    store: Store,
}

#[derive(Default)]
struct App {
    active: Mutex<Option<Active>>,
}

impl App {
    fn lock(&self) -> MutexGuard<'_, Option<Active>> {
        self.active.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn with<T>(&self, f: impl FnOnce(&mut Active) -> Result<T, String>) -> Result<T, String> {
        match self.lock().as_mut() {
            Some(active) => f(active),
            None => Err("Nicht angemeldet.".into()),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------------------

/// Keeps the token in a file inside the app's private folder. On Android that folder is
/// sandboxed per app; moving this to the Android Keystore is a later hardening step.
struct FileSecrets {
    path: PathBuf,
}

impl FileSecrets {
    fn read(&self) -> HashMap<String, String> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn write(&self, secrets: &HashMap<String, String>) -> bool {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        serde_json::to_string(secrets).is_ok_and(|json| std::fs::write(&self.path, json).is_ok())
    }
}

impl SecretStore for FileSecrets {
    fn set(&self, account: &str, secret: &str) -> bool {
        let mut secrets = self.read();
        secrets.insert(account.to_string(), secret.to_string());
        self.write(&secrets)
    }

    fn get(&self, account: &str) -> Option<String> {
        self.read().remove(account)
    }

    fn delete(&self, account: &str) {
        let mut secrets = self.read();
        if secrets.remove(account).is_some() {
            self.write(&secrets);
        }
    }
}

// ---------------------------------------------------------------------------------------
// Data handed to the UI
// ---------------------------------------------------------------------------------------

#[derive(Serialize)]
struct UserInfo {
    email: String,
    server: String,
}

#[derive(Serialize)]
struct LastLogin {
    server: String,
    email: String,
}

#[derive(Serialize)]
struct CalendarDto {
    id: String,
    name: String,
    color: String,
    can_edit: bool,
}

/// One event on one day. `start` / `end` are `YYYY-MM-DDTHH:MM` in the display time zone.
#[derive(Serialize)]
struct Item {
    id: String,
    calendar: String,
    title: String,
    location: String,
    description: String,
    color: String,
    day: String,
    start: String,
    end: String,
    all_day: bool,
    /// The day this repetition regularly belongs to (to address a single repetition).
    origin: String,
    series: bool,
    can_edit: bool,
}

#[derive(Serialize)]
struct SyncStatus {
    /// `ok`, `offline`, `unauthorized` or `error`
    state: &'static str,
    message: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventInput {
    id: Option<String>,
    calendar: String,
    title: String,
    location: String,
    description: String,
    all_day: bool,
    start_date: String,
    start_time: String,
    end_date: String,
    end_time: String,
    /// Only used for new events: `daily`, `weekly`, `monthly`, `yearly` or empty.
    repeat: String,
}

fn parse_date(text: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(text, "%Y-%m-%d").map_err(|_| "Ungültiges Datum.".to_string())
}

fn parse_time(text: &str) -> Result<NaiveTime, String> {
    NaiveTime::parse_from_str(text, "%H:%M").map_err(|_| "Ungültige Uhrzeit.".to_string())
}

fn stamp(time: NaiveDateTime) -> String {
    time.format("%Y-%m-%dT%H:%M").to_string()
}

fn open(session: Session) -> Active {
    let store = Store::load(&session);
    Active { session, store }
}

// ---------------------------------------------------------------------------------------
// Commands: account
// ---------------------------------------------------------------------------------------

#[tauri::command]
fn last_login() -> Option<LastLogin> {
    api::last_login().map(|(server, email)| LastLogin { server, email })
}

/// The session remembered from the last run, if any (no network).
#[tauri::command]
fn restore(app: State<'_, App>) -> Option<UserInfo> {
    let session = api::load()?;
    let info = UserInfo { email: session.email.clone(), server: session.server.clone() };
    *app.lock() = Some(open(session));
    Some(info)
}

#[tauri::command]
async fn login(
    app: State<'_, App>,
    server: String,
    email: String,
    password: String,
    register: bool,
    invite_code: String,
) -> Result<UserInfo, String> {
    let session = tauri::async_runtime::spawn_blocking(move || {
        api::authenticate(&server, email.trim(), &password, register, invite_code.trim())
    })
    .await
    .map_err(|e| e.to_string())??;
    api::save(&session);
    let info = UserInfo { email: session.email.clone(), server: session.server.clone() };
    *app.lock() = Some(open(session));
    Ok(info)
}

#[tauri::command]
async fn logout(app: State<'_, App>) -> Result<(), String> {
    let Some(active) = app.lock().take() else { return Ok(()) };
    tauri::async_runtime::spawn_blocking(move || {
        api::logout(&active.session);
        api::forget_token(&active.session);
    })
    .await
    .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------------------
// Commands: data
// ---------------------------------------------------------------------------------------

/// Pushes local changes and pulls the server's. Repeats once if the server refused something.
#[tauri::command]
async fn sync(app: State<'_, App>) -> Result<SyncStatus, String> {
    for _ in 0..3 {
        let job = app.with(|a| Ok(a.store.job(&a.session)))?;
        let out = tauri::async_runtime::spawn_blocking(move || sync::run(job)).await.map_err(|e| e.to_string())?;
        let failure = out.failure.clone();
        let again = app.with(|a| {
            let again = a.store.apply(out);
            a.store.save(&a.session);
            Ok(again)
        })?;
        match failure {
            Some(Failure::Offline) => return Ok(SyncStatus { state: "offline", message: None }),
            Some(Failure::Unauthorized) => return Ok(SyncStatus { state: "unauthorized", message: None }),
            Some(Failure::Server(message)) => return Ok(SyncStatus { state: "error", message: Some(message) }),
            None if again => continue,
            None => break,
        }
    }
    Ok(SyncStatus { state: "ok", message: None })
}

#[tauri::command]
fn calendars(app: State<'_, App>) -> Result<Vec<CalendarDto>, String> {
    app.with(|a| {
        Ok(a.store
            .calendars
            .iter()
            .filter(|c| !c.draft)
            .map(|c| CalendarDto { id: c.id.clone(), name: c.name.clone(), color: c.color.clone(), can_edit: c.can_edit() })
            .collect())
    })
}

/// All events from `from` to `to` (inclusive, `YYYY-MM-DD`), one entry per event and day.
#[tauri::command]
fn items(app: State<'_, App>, from: String, to: String) -> Result<Vec<Item>, String> {
    let (from, to) = (parse_date(&from)?, parse_date(&to)?);
    if to < from || (to - from).num_days() > MAX_DAYS {
        return Err("Ungültiger Zeitraum.".into());
    }
    app.with(|a| {
        let mut out = Vec::new();
        let mut day = from;
        while day <= to {
            for event in a.store.events.iter().filter(|e| !e.draft) {
                let Some(calendar) = a.store.calendars.iter().find(|c| c.id == event.calendar && !c.draft && c.visible)
                else {
                    continue;
                };
                for (occurrence, origin) in event.occurrences_on(day) {
                    out.push(Item {
                        id: occurrence.id.clone(),
                        calendar: calendar.id.clone(),
                        color: occurrence.color.clone().unwrap_or_else(|| calendar.color.clone()),
                        all_day: occurrence.is_all_day(),
                        title: occurrence.title.clone(),
                        location: occurrence.location.clone(),
                        description: occurrence.description.clone(),
                        day: day.to_string(),
                        start: stamp(occurrence.start),
                        end: stamp(occurrence.end),
                        origin: origin.to_string(),
                        series: event.repeats(),
                        can_edit: calendar.can_edit(),
                    });
                }
            }
            day += Duration::days(1);
        }
        out.sort_by(|x, y| (&x.day, !x.all_day, &x.start, &x.title).cmp(&(&y.day, !y.all_day, &y.start, &y.title)));
        Ok(out)
    })
}

#[tauri::command]
fn save_event(app: State<'_, App>, input: EventInput) -> Result<(), String> {
    let title = input.title.trim().to_string();
    if title.is_empty() {
        return Err("Bitte einen Titel eingeben.".into());
    }
    let start_date = parse_date(&input.start_date)?;
    let end_date = parse_date(&input.end_date)?;
    let (start, end) = if input.all_day {
        (start_date.and_time(NaiveTime::MIN), end_date.and_hms_opt(23, 59, 0).unwrap_or_default())
    } else {
        (start_date.and_time(parse_time(&input.start_time)?), end_date.and_time(parse_time(&input.end_time)?))
    };
    if end <= start {
        return Err("Das Ende liegt nicht nach dem Beginn.".into());
    }
    if !["", "daily", "weekly", "monthly", "yearly"].contains(&input.repeat.as_str()) {
        return Err("Ungültige Wiederholung.".into());
    }

    app.with(|a| {
        if !a.store.calendars.iter().any(|c| c.id == input.calendar && c.can_edit()) {
            return Err("In diesem Kalender darfst du nichts ändern.".into());
        }
        match input.id.as_deref().and_then(|id| a.store.events.iter().position(|e| e.id == id)) {
            Some(i) => {
                let event = &mut a.store.events[i];
                event.title = title;
                event.location = input.location.trim().to_string();
                event.description = input.description.trim().to_string();
                // A series keeps its schedule here; only single events move.
                if !event.repeats() {
                    event.calendar = input.calendar;
                    event.start = start;
                    event.end = end;
                    event.tz = model::display_tz();
                }
                event.dirty = true;
            }
            None => a.store.events.push(Event {
                id: model::new_id(),
                calendar: input.calendar,
                title,
                location: input.location.trim().to_string(),
                description: input.description.trim().to_string(),
                start,
                end,
                tz: model::display_tz(),
                status: model::default_status(),
                repeat: input.repeat,
                repeat_until: None,
                repeat_interval: 1,
                repeat_days: 0,
                repeat_nth: 0,
                exceptions: Vec::new(),
                remind: None,
                attendees: Vec::new(),
                color: None,
                private: false,
                categories: Vec::new(),
                rev: 0,
                dirty: true,
                draft: false,
            }),
        }
        a.store.save(&a.session);
        Ok(())
    })
}

/// Deletes an event, or with `origin` a single repetition of a series.
#[tauri::command]
fn delete_event(app: State<'_, App>, id: String, origin: Option<String>) -> Result<(), String> {
    let origin = origin.as_deref().map(parse_date).transpose()?;
    app.with(|a| {
        let Some(i) = a.store.events.iter().position(|e| e.id == id) else { return Ok(()) };
        if !a.store.calendars.iter().any(|c| c.id == a.store.events[i].calendar && c.can_edit()) {
            return Err("In diesem Kalender darfst du nichts ändern.".into());
        }
        match origin {
            Some(origin) if a.store.events[i].repeats() => {
                a.store.events[i].delete_occurrence(origin);
                a.store.events[i].dirty = true;
            }
            _ => {
                let removed = a.store.events.remove(i);
                // Only events the server knows about need a deletion request.
                if removed.rev > 0 {
                    a.store.pending_deletes.push(PendingDelete { id: removed.id, rev: removed.rev });
                }
            }
        }
        a.store.save(&a.session);
        Ok(())
    })
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(App::default())
        .setup(|app| {
            let config = app.path().app_config_dir()?;
            let data = app.path().app_data_dir()?;
            let secrets = FileSecrets { path: data.join("secrets.json") };
            platform::configure(config, data, Arc::new(secrets));
            platform::set_device_name(if cfg!(target_os = "android") { "Kairo Android" } else { "Kairo Mobile (Test)" });
            model::load_display_tz();
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![last_login, restore, login, logout, sync, calendars, items, save_event, delete_event])
        .run(tauri::generate_context!())
        .expect("Kairo konnte nicht gestartet werden");
}
