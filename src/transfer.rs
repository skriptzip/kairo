//! Importing calendar files (`.pst`, `.ics`) and exporting calendars.

use super::*;

impl CalendarApp {
    /// Writes a calendar to an `.ics` file the user picks.
    pub(crate) fn export_calendar(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(calendar) = self.calendar(id) else { return };
        let name = calendar.name.clone();
        let events: Vec<Event> = self.store.events.iter().filter(|e| e.calendar == id && !e.draft).cloned().collect();
        let file: String = name.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
        let dir = dirs::download_dir().or_else(dirs::document_dir).unwrap_or_default();
        let picked = cx.prompt_for_new_path(&dir, Some(&format!("{file}.ics")));
        cx.spawn(async move |this, cx| {
            let Some(path) = picked.await.ok().and_then(|r| r.ok()).flatten() else { return };
            let text = ics::export(&name, &events.iter().collect::<Vec<_>>());
            let result = cx.background_executor().spawn(async move { std::fs::write(&path, text).map(|_| path) }).await;
            this.update(cx, |this, cx| {
                this.import_note = Some(match result {
                    Ok(path) => format!("{} Termine exportiert nach {}", events.len(), path.display()),
                    Err(e) => format!("Export fehlgeschlagen: {e}"),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Asks for a calendar file (`.pst` or `.ics`) and imports its appointments as new calendars.
    pub(crate) fn import_pst(&mut self, cx: &mut Context<Self>) {
        if self.importing {
            return;
        }
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Kalenderdatei importieren (.pst oder .ics)".into()),
        });
        cx.spawn(async move |this, cx| {
            let Some(path) = picked.await.ok().and_then(|r| r.ok()).flatten().and_then(|p| p.into_iter().next())
            else {
                return;
            };
            this.update(cx, |this, cx| {
                this.importing = true;
                this.import_note = Some("Importiere …".into());
                cx.notify();
            })
            .ok();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let is_ics = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("ics"));
                    if is_ics { ics::read(&path) } else { pst_import::read(&path) }
                })
                .await;
            this.update(cx, |this, cx| this.finish_import(result, cx)).ok();
        })
        .detach();
    }

    pub(crate) fn finish_import(&mut self, result: Result<pst_import::Imported, String>, cx: &mut Context<Self>) {
        self.importing = false;
        let imported = match result {
            Ok(imported) => imported,
            Err(message) => {
                self.import_note = Some(message);
                cx.notify();
                return;
            }
        };

        // Several calendars: let the user pick which ones to take.
        if imported.calendars.len() > 1 {
            let chosen = imported.calendars.iter().map(|c| !c.events.is_empty()).collect();
            self.pending_import = Some(PendingImport { imported, chosen });
            self.import_note = None;
            cx.notify();
            return;
        }
        let chosen = vec![true; imported.calendars.len()];
        self.apply_import(imported, chosen, cx);
    }

    /// Adds the chosen calendars of an import as new calendars.
    pub(crate) fn apply_import(&mut self, imported: pst_import::Imported, chosen: Vec<bool>, cx: &mut Context<Self>) {
        let (mut calendars, mut events) = (0, 0);
        let picked = imported.calendars.into_iter().zip(chosen).filter(|(_, on)| *on).map(|(c, _)| c);
        for source in picked {
            let id = new_id();
            let name: String = format!("{} (Import)", source.name.trim()).chars().take(100).collect();
            self.store.calendars.push(CalendarInfo {
                id: id.clone(),
                name,
                color: PALETTE[self.store.calendars.len() % PALETTE.len()].to_string(),
                rev: 0,
                dirty: true,
                visible: true,
                owner: String::new(),
                role: "owner".into(),
                draft: false,
            });
            calendars += 1;
            for event in source.events {
                self.store.events.push(Event {
                    id: new_id(),
                    calendar: id.clone(),
                    title: event.title,
                    location: event.location,
                    description: event.description,
                    start: event.start,
                    end: event.end,
                    tz: event.tz,
                    status: event.status,
                    repeat: event.repeat,
                    repeat_until: event.repeat_until,
                    repeat_interval: event.repeat_interval,
                    repeat_days: event.repeat_days,
                    repeat_nth: event.repeat_nth,
                    exceptions: event.exceptions,
                    remind: event.remind,
                    attendees: event.attendees,
                    color: event.color,
                    private: event.private,
                    categories: event.categories,
                    rev: 0,
                    dirty: true,
                    draft: false,
                });
                events += 1;
            }
        }

        let mut note = match events {
            0 => "Die Datei enthält keine Kalendertermine.".to_string(),
            _ => format!("{events} Termine in {calendars} Kalender(n) importiert."),
        };
        if imported.skipped > 0 {
            note += &format!(" {} Einträge übersprungen.", imported.skipped);
        }
        if imported.simplified > 0 {
            note += &format!(" {} Serien mit unbekannter Regel als Einzeltermin übernommen.", imported.simplified);
        }
        self.import_note = Some(note);
        if events > 0 {
            self.mark_changed(cx);
        }
        cx.notify();
    }

    /// The dialog that asks which calendars of a file to import.
    pub(crate) fn render_import_choice(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pending = self.pending_import.as_ref()?;
        let muted = cx.theme().muted_foreground;
        let rows = pending.imported.calendars.iter().enumerate().map(|(i, calendar)| {
            let count = calendar.events.len();
            Checkbox::new(("import-calendar", i))
                .label(format!("{} ({} {})", calendar.name, count, if count == 1 { "Termin" } else { "Termine" }))
                .checked(pending.chosen[i])
                .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                    if let Some(p) = this.pending_import.as_mut() {
                        p.chosen[i] = *checked;
                    }
                    cx.notify();
                }))
        });
        let any = pending.chosen.iter().any(|on| *on);
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::black().opacity(0.4))
                .occlude()
                .child(
                    div()
                        .w(px(420.))
                        .max_h(px(520.))
                        .flex()
                        .flex_col()
                        .gap_3()
                        .p_4()
                        .rounded_lg()
                        .border_1()
                        .border_color(cx.theme().border)
                        .bg(cx.theme().background)
                        .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child("Kalender importieren"))
                        .child(div().text_sm().text_color(muted).child("Welche Kalender der Datei sollen übernommen werden?"))
                        .child(div().id("import-list").flex().flex_col().gap_2().overflow_y_scroll().children(rows))
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap_2()
                                .child(Button::new("import-cancel").label("Abbrechen").on_click(cx.listener(|this, _, _, cx| {
                                    this.pending_import = None;
                                    cx.notify();
                                })))
                                .child(
                                    Button::new("import-ok")
                                        .primary()
                                        .label("Importieren")
                                        .disabled(!any)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            if let Some(p) = this.pending_import.take() {
                                                this.apply_import(p.imported, p.chosen, cx);
                                            }
                                        })),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}

/// A file read for import, waiting for the user to pick calendars.
pub(crate) struct PendingImport {
    imported: pst_import::Imported,
    chosen: Vec<bool>,
}
