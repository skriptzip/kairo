//! Printing: the shown period as a web page that opens in the browser's print dialog.

use super::*;

/// One line of the printout.
pub(crate) struct PrintLine {
    pub(crate) when: String,
    pub(crate) title: String,
    pub(crate) location: String,
    pub(crate) color: u32,
}

pub(crate) struct PrintDay {
    pub(crate) date: NaiveDate,
    /// Belongs to the month being shown (month printout only).
    pub(crate) in_month: bool,
    pub(crate) lines: Vec<PrintLine>,
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

const STYLE: &str = "
body { font-family: 'Segoe UI', Arial, sans-serif; margin: 24px; color: #222; }
h1 { font-size: 20px; margin: 0 0 16px; }
h2 { font-size: 14px; margin: 18px 0 4px; border-bottom: 1px solid #bbb; padding-bottom: 2px; }
.row { display: flex; gap: 12px; padding: 2px 0; font-size: 13px; break-inside: avoid; }
.when { width: 110px; flex: none; color: #555; }
.bar { width: 4px; flex: none; border-radius: 2px; }
.place { color: #666; }
table { border-collapse: collapse; width: 100%; table-layout: fixed; }
th { font-size: 12px; padding: 4px; border: 1px solid #bbb; background: #f2f2f2; }
td { vertical-align: top; border: 1px solid #bbb; height: 96px; padding: 3px; font-size: 11px; overflow: hidden; }
td.other { background: #f7f7f7; color: #999; }
.num { text-align: right; font-weight: 600; }
.ev { border-left: 3px solid; padding-left: 3px; margin: 1px 0; overflow: hidden; white-space: nowrap; text-overflow: ellipsis; }
@media print { body { margin: 0; } }
";

fn page(title: &str, body: &str) -> String {
    format!(
        "<!DOCTYPE html><html lang=\"de\"><head><meta charset=\"utf-8\"><title>{t}</title><style>{STYLE}</style></head>\
         <body><h1>{t}</h1>{body}<script>window.addEventListener('load', function () {{ window.print(); }});</script></body></html>",
        t = escape(title)
    )
}

/// Days with their events one below the other (day, week and list views). Days without events
/// are left out unless `keep_empty`.
pub(crate) fn html_list(title: &str, days: &[PrintDay], keep_empty: bool) -> String {
    let mut body = String::new();
    for day in days.iter().filter(|d| keep_empty || !d.lines.is_empty()) {
        body += &format!(
            "<h2>{}, {}. {} {}</h2>",
            WEEKDAYS[day.date.weekday().num_days_from_monday() as usize],
            day.date.day(),
            MONTHS[day.date.month0() as usize],
            day.date.year()
        );
        for line in &day.lines {
            body += &format!(
                "<div class=\"row\"><span class=\"when\">{}</span><span class=\"bar\" style=\"background:#{:06x}\"></span>\
                 <span><b>{}</b> <span class=\"place\">{}</span></span></div>",
                escape(&line.when),
                line.color,
                escape(&line.title),
                escape(&line.location)
            );
        }
        if day.lines.is_empty() {
            body += "<div class=\"row\"><span class=\"place\">–</span></div>";
        }
    }
    if body.is_empty() {
        body = "<p>Keine Termine.</p>".into();
    }
    page(title, &body)
}

/// Seven columns, one row per week.
pub(crate) fn html_month(title: &str, days: &[PrintDay]) -> String {
    let mut body = String::from("<table><tr>");
    for name in WEEKDAYS {
        body += &format!("<th>{}</th>", &name[..2]);
    }
    body += "</tr>";
    for week in days.chunks(7) {
        body += "<tr>";
        for day in week {
            body += &format!("<td class=\"{}\"><div class=\"num\">{}</div>", if day.in_month { "" } else { "other" }, day.date.day());
            for line in &day.lines {
                let time = if line.when.is_empty() { String::new() } else { format!("{} ", escape(&line.when)) };
                body += &format!("<div class=\"ev\" style=\"border-color:#{:06x}\">{time}{}</div>", line.color, escape(&line.title));
            }
            body += "</td>";
        }
        body += "</tr>";
    }
    body += "</table>";
    page(title, &body)
}

impl CalendarApp {
    /// Writes the shown period to a web page and opens it; the browser then asks to print.
    pub(crate) fn print_view(&mut self, cx: &mut Context<Self>) {
        let days: Vec<PrintDay> = self
            .visible_days()
            .into_iter()
            .map(|date| PrintDay {
                date,
                in_month: date.month() == self.date.month(),
                lines: self
                    .day_events(date)
                    .into_iter()
                    .map(|(ev, _)| PrintLine {
                        when: if ev.is_all_day() {
                            if self.mode == ViewMode::Month { String::new() } else { "ganztägig".into() }
                        } else if self.mode == ViewMode::Month {
                            ev.start.format("%H:%M").to_string()
                        } else {
                            format!("{}–{}", ev.start.format("%H:%M"), ev.end.format("%H:%M"))
                        },
                        color: self.event_color(&ev),
                        location: ev.location.clone(),
                        title: ev.title,
                    })
                    .collect(),
            })
            .collect();
        let title = format!("Kairo – {}", self.title());
        let html = match self.mode {
            ViewMode::Month => html_month(&title, &days),
            ViewMode::Agenda => html_list(&title, &days, false),
            _ => html_list(&title, &days, true),
        };
        let path = std::env::temp_dir().join("kairo-druck.html");
        match std::fs::write(&path, html) {
            Ok(()) => {
                cx.open_with_system(&path);
                self.import_note = Some("Druckansicht im Browser geöffnet.".into());
            }
            Err(e) => self.import_note = Some(format!("Drucken fehlgeschlagen: {e}")),
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{PrintDay, PrintLine, html_list, html_month};
    use chrono::NaiveDate;

    fn day(d: u32, lines: Vec<PrintLine>) -> PrintDay {
        PrintDay { date: NaiveDate::from_ymd_opt(2026, 10, d).unwrap(), in_month: true, lines }
    }

    fn line(title: &str) -> PrintLine {
        PrintLine { when: "09:00–10:00".into(), title: title.into(), location: "Raum <1>".into(), color: 0x0f6cbd }
    }

    #[test]
    fn list_escapes_text_and_skips_empty_days() {
        let html = html_list("Woche", &[day(5, vec![line("Planung & Rest")]), day(6, vec![])], false);
        assert!(html.contains("Planung &amp; Rest") && html.contains("Raum &lt;1&gt;"));
        assert!(html.contains("Montag, 5. Oktober 2026") && !html.contains("Dienstag"));
        assert!(html_list("Woche", &[day(6, vec![])], true).contains("Dienstag"));
        assert!(html_list("Woche", &[day(6, vec![])], false).contains("Keine Termine"));
    }

    #[test]
    fn month_has_a_row_per_week() {
        let days: Vec<PrintDay> = (1..=14).map(|d| day(d, if d == 5 { vec![line("A")] } else { vec![] })).collect();
        let html = html_month("Oktober", &days);
        assert_eq!(html.matches("<tr>").count(), 3); // header + 2 weeks
        assert!(html.contains("<b>") || html.contains("class=\"ev\""));
    }
}
