//! Desktop notifications for reminders.

use super::*;

/// Identifies Kairo to Windows, so reminders show up under its name and not as "Windows PowerShell".
#[cfg(windows)]
pub(crate) const APP_ID: &str = "de.kairo.app";

/// Registers the app id with a display name and icon (for the current user; harmless to repeat).
#[cfg(windows)]
pub(crate) fn register_app_id() {
    use winreg::{RegKey, enums::HKEY_CURRENT_USER};
    let Ok((key, _)) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(format!(r"Software\Classes\AppUserModelId\{APP_ID}"))
    else {
        return;
    };
    let _ = key.set_value("DisplayName", &"Kairo");
    if let Some(dir) = dirs::data_dir().map(|d| d.join("kairo")) {
        let icon = dir.join("icon.png");
        let written = std::fs::create_dir_all(&dir).is_ok()
            && (icon.exists() || std::fs::write(&icon, include_bytes!("../assets/icon.png")).is_ok());
        if written {
            let _ = key.set_value("IconUri", &icon.to_string_lossy().to_string());
        }
    }
}

impl CalendarApp {
    /// Shows a desktop notification for every reminder that has become due and isn't over yet.
    pub(crate) fn check_reminders(&mut self, cx: &mut Context<Self>) {
        let now = crate::model::now();
        let today = now.date();
        let mut due: Vec<(String, Event)> = Vec::new();
        for event in self.store.events.iter().filter(|e| !e.draft && e.remind.is_some()) {
            let minutes = i64::from(event.remind.unwrap_or(0));
            for day in (0..=2).map(|d| today + Duration::days(d)) {
                for (occurrence, _) in event.starting_on(day) {
                    let remembered = |r: &(String, NaiveDateTime)| r.0 == event.id && r.1 == occurrence.start;
                    if occurrence.is_all_day()
                        || now < occurrence.start - Duration::minutes(minutes)
                        || now >= occurrence.end
                        || self.store.reminded.iter().any(remembered)
                        || due.iter().any(|(id, o)| *id == event.id && o.start == occurrence.start)
                    {
                        continue;
                    }
                    due.push((event.id.clone(), occurrence));
                }
            }
        }
        if due.is_empty() {
            return;
        }
        for (id, occurrence) in due {
            self.store.reminded.push((id, occurrence.start));
            let when = if occurrence.start.date() == today {
                occurrence.start.format("%H:%M").to_string()
            } else {
                occurrence.start.format("%d.%m. %H:%M").to_string()
            };
            let body = if occurrence.location.is_empty() { when } else { format!("{when} · {}", occurrence.location) };
            let title = occurrence.title;
            cx.background_executor()
                .spawn(async move {
                    let mut toast = notify_rust::Notification::new();
                    toast.appname("Kairo").summary(&title).body(&body);
                    #[cfg(windows)]
                    toast.app_id(APP_ID);
                    let _ = toast.show();
                })
                .detach();
        }
        // Old entries can't come due again.
        let cutoff = now - Duration::days(2);
        self.store.reminded.retain(|(_, start)| *start > cutoff);
        self.store.save(&self.session);
    }

}
