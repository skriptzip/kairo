"use strict";

const invoke = (cmd, args) => window.__TAURI__.core.invoke(cmd, args);
const $ = (id) => document.getElementById(id);

const WEEKDAYS = ["So", "Mo", "Di", "Mi", "Do", "Fr", "Sa"];
const WEEKDAYS_LONG = ["Sonntag", "Montag", "Dienstag", "Mittwoch", "Donnerstag", "Freitag", "Samstag"];
const MONTHS = ["Januar", "Februar", "März", "April", "Mai", "Juni", "Juli", "August", "September", "Oktober", "November", "Dezember"];
const SYNC_INTERVAL_MS = 60_000;
const AGENDA_DAYS = 14;

const state = {
  view: "agenda",
  anchor: new Date(),
  selected: null, // ISO day picked in the month view
  items: [],
  calendars: [],
  user: null,
  register: false,
  syncing: false,
  offline: false,
};

// ---------------------------------------------------------------------------------------
// Dates (local wall-clock days, as ISO strings `YYYY-MM-DD`)
// ---------------------------------------------------------------------------------------

const pad = (n) => String(n).padStart(2, "0");
const iso = (d) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
const fromIso = (s) => new Date(+s.slice(0, 4), +s.slice(5, 7) - 1, +s.slice(8, 10));
const addDays = (d, n) => new Date(d.getFullYear(), d.getMonth(), d.getDate() + n);
const mondayOf = (d) => addDays(d, -((d.getDay() + 6) % 7));
const todayIso = () => iso(new Date());
const timeOf = (stamp) => stamp.slice(11, 16);
const dayLabel = (s) => {
  const d = fromIso(s);
  return `${WEEKDAYS_LONG[d.getDay()]}, ${d.getDate()}. ${MONTHS[d.getMonth()]}`;
};

// ---------------------------------------------------------------------------------------
// DOM helper: text always goes through textContent, never innerHTML
// ---------------------------------------------------------------------------------------

function h(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (key === "class") node.className = value;
    else if (key === "style") Object.assign(node.style, value);
    else if (key.startsWith("on")) node.addEventListener(key.slice(2), value);
    else node.setAttribute(key, value);
  }
  for (const child of children.flat(Infinity)) {
    if (child != null) node.append(child);
  }
  return node;
}

// ---------------------------------------------------------------------------------------
// Screens
// ---------------------------------------------------------------------------------------

function show(screen) {
  $("login").hidden = screen !== "login";
  $("main").hidden = screen !== "main";
}

async function showLogin(message = "") {
  show("login");
  const last = await invoke("last_login").catch(() => null);
  if (last) {
    $("l-server").value = last.server;
    $("l-email").value = last.email;
  }
  $("l-password").value = "";
  $("l-error").textContent = message;
}

function setRegister(on) {
  state.register = on;
  $("l-invite-row").hidden = !on;
  $("l-submit").textContent = on ? "Registrieren" : "Anmelden";
  $("l-toggle").textContent = on ? "Schon ein Konto? Anmelden" : "Noch kein Konto? Registrieren";
  $("l-password").autocomplete = on ? "new-password" : "current-password";
}

$("l-toggle").addEventListener("click", () => setRegister(!state.register));

$("login-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  $("l-error").textContent = "";
  $("l-submit").disabled = true;
  try {
    state.user = await invoke("login", {
      server: $("l-server").value,
      email: $("l-email").value,
      password: $("l-password").value,
      register: state.register,
      inviteCode: $("l-invite").value,
    });
    $("l-password").value = "";
    await enterMain();
  } catch (error) {
    $("l-error").textContent = String(error);
  } finally {
    $("l-submit").disabled = false;
  }
});

async function enterMain() {
  show("main");
  $("m-account").textContent = `${state.user.email}\n${state.user.server}`;
  state.anchor = new Date();
  state.selected = todayIso();
  state.calendars = await invoke("calendars");
  await refresh();
  syncNow();
}

// ---------------------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------------------

function visibleRange() {
  if (state.view === "agenda") {
    const from = new Date(state.anchor.getFullYear(), state.anchor.getMonth(), state.anchor.getDate());
    return [from, addDays(from, AGENDA_DAYS - 1)];
  }
  const first = new Date(state.anchor.getFullYear(), state.anchor.getMonth(), 1);
  const start = mondayOf(first);
  return [start, addDays(start, 41)];
}

async function refresh() {
  const [from, to] = visibleRange();
  try {
    state.items = await invoke("items", { from: iso(from), to: iso(to) });
  } catch (error) {
    state.items = [];
    setStatus(String(error));
  }
  render();
}

function setStatus(text) {
  $("status").hidden = !text;
  $("status").textContent = text || "";
}

async function syncNow() {
  if (state.syncing || !state.user) return;
  state.syncing = true;
  setStatus("Synchronisiere …");
  try {
    const result = await invoke("sync");
    if (result.state === "unauthorized") {
      state.user = null;
      await invoke("logout").catch(() => {});
      await showLogin("Deine Sitzung ist abgelaufen. Bitte neu anmelden.");
      return;
    }
    state.offline = result.state === "offline";
    setStatus(
      result.state === "offline" ? "Offline – Änderungen werden später übertragen." :
      result.state === "error" ? result.message : ""
    );
    state.calendars = await invoke("calendars");
    await refresh();
  } catch (error) {
    setStatus(String(error));
  } finally {
    state.syncing = false;
  }
}

// ---------------------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------------------

function render() {
  for (const tab of document.querySelectorAll("#tabs button")) {
    tab.classList.toggle("active", tab.dataset.view === state.view);
  }
  const a = state.anchor;
  $("title").textContent =
    state.view === "month" ? `${MONTHS[a.getMonth()]} ${a.getFullYear()}` : `${a.getDate()}. ${MONTHS[a.getMonth()]} ${a.getFullYear()}`;
  const content = $("content");
  content.replaceChildren(state.view === "month" ? monthView() : agendaView());
}

function eventRow(item) {
  const time = item.all_day ? "ganztägig" : timeOf(item.start);
  return h(
    "button",
    { class: "event", onclick: () => openDetail(item) },
    h("span", { class: "bar", style: { background: item.color } }),
    h("span", { class: "time" }, time),
    h("span", { class: "text" }, h("div", { class: "name" }, item.title || "(Ohne Titel)"), item.location ? h("div", { class: "where" }, item.location) : null)
  );
}

function dayBlock(day, items) {
  return [h("div", { class: "day-head" + (day === todayIso() ? " today" : "") }, dayLabel(day)), ...items.map(eventRow)];
}

function agendaView() {
  const byDay = new Map();
  for (const item of state.items) {
    if (!byDay.has(item.day)) byDay.set(item.day, []);
    byDay.get(item.day).push(item);
  }
  if (byDay.size === 0) return h("p", { class: "empty" }, "Keine Termine in diesem Zeitraum.");
  return h("div", {}, [...byDay.entries()].map(([day, items]) => dayBlock(day, items)));
}

function monthView() {
  const [start] = visibleRange();
  const month = state.anchor.getMonth();
  const today = todayIso();
  const byDay = new Map();
  for (const item of state.items) {
    if (!byDay.has(item.day)) byDay.set(item.day, []);
    byDay.get(item.day).push(item);
  }
  const head = h("div", { class: "weekdays" }, [1, 2, 3, 4, 5, 6, 0].map((i) => h("div", {}, WEEKDAYS[i])));
  const cells = [];
  for (let i = 0; i < 42; i++) {
    const day = addDays(start, i);
    const key = iso(day);
    const items = byDay.get(key) || [];
    const colors = [...new Set(items.map((x) => x.color))].slice(0, 3);
    cells.push(
      h(
        "button",
        {
          class: "cell" + (day.getMonth() !== month ? " other" : "") + (key === today ? " today" : "") + (key === state.selected ? " selected" : ""),
          onclick: () => {
            state.selected = key;
            render();
          },
        },
        h("span", { class: "num" }, String(day.getDate())),
        h("span", { class: "dots" }, colors.map((c) => h("span", { class: "dot", style: { background: c } })))
      )
    );
  }
  const chosen = state.selected;
  const list = state.items.filter((x) => x.day === chosen);
  return h(
    "div",
    {},
    head,
    h("div", { class: "grid" }, cells),
    chosen ? dayBlock(chosen, list) : null,
    chosen && list.length === 0 ? h("p", { class: "empty" }, "Keine Termine an diesem Tag.") : null
  );
}

// ---------------------------------------------------------------------------------------
// Navigation
// ---------------------------------------------------------------------------------------

function step(direction) {
  const a = state.anchor;
  state.anchor = state.view === "month" ? new Date(a.getFullYear(), a.getMonth() + direction, 1) : addDays(a, 7 * direction);
  if (state.view === "month") state.selected = null;
  return refresh();
}

function goToday() {
  state.anchor = new Date();
  state.selected = todayIso();
  return refresh();
}

$("prev").addEventListener("click", () => step(-1));
$("next").addEventListener("click", () => step(1));
$("title").addEventListener("click", goToday);
for (const tab of document.querySelectorAll("#tabs button")) {
  tab.addEventListener("click", () => {
    state.view = tab.dataset.view;
    if (state.view === "month") state.selected = state.selected || todayIso();
    refresh();
  });
}

// Swipe left/right to move through time.
let touchStart = null;
$("content").addEventListener("touchstart", (e) => { touchStart = e.touches[0]; }, { passive: true });
$("content").addEventListener("touchend", (e) => {
  if (!touchStart) return;
  const dx = e.changedTouches[0].clientX - touchStart.clientX;
  const dy = e.changedTouches[0].clientY - touchStart.clientY;
  touchStart = null;
  if (Math.abs(dx) > 80 && Math.abs(dx) > Math.abs(dy) * 2) step(dx < 0 ? 1 : -1);
});

// ---------------------------------------------------------------------------------------
// Menu
// ---------------------------------------------------------------------------------------

$("menu-btn").addEventListener("click", (e) => {
  e.stopPropagation();
  $("menu").hidden = !$("menu").hidden;
});
document.addEventListener("click", () => { $("menu").hidden = true; });
$("m-sync").addEventListener("click", syncNow);
$("m-today").addEventListener("click", goToday);
$("m-logout").addEventListener("click", async () => {
  await invoke("logout").catch(() => {});
  state.user = null;
  state.items = [];
  await showLogin();
});

// ---------------------------------------------------------------------------------------
// Sheet: details and editor
// ---------------------------------------------------------------------------------------

function openSheet(part) {
  $("sheet").hidden = false;
  $("detail").hidden = part !== "detail";
  $("edit").hidden = part !== "edit";
}

function closeSheet() {
  $("sheet").hidden = true;
}

$("sheet").addEventListener("click", (e) => { if (e.target === $("sheet")) closeSheet(); });
$("e-cancel").addEventListener("click", closeSheet);

function whenText(item) {
  const startDay = item.start.slice(0, 10);
  const endDay = item.end.slice(0, 10);
  if (item.all_day) {
    return startDay === endDay ? `${dayLabel(startDay)} · ganztägig` : `${dayLabel(startDay)} – ${dayLabel(endDay)}`;
  }
  return startDay === endDay
    ? `${dayLabel(startDay)}, ${timeOf(item.start)} – ${timeOf(item.end)}`
    : `${dayLabel(startDay)} ${timeOf(item.start)} – ${dayLabel(endDay)} ${timeOf(item.end)}`;
}

function openDetail(item) {
  const calendar = state.calendars.find((c) => c.id === item.calendar);
  const detail = $("detail");
  const actions = h("div", { class: "actions" });
  const render = () => {
    actions.replaceChildren(
      ...(item.can_edit
        ? [h("button", { class: "danger", onclick: askDelete }, "Löschen"), h("button", { onclick: () => openEdit(item) }, "Bearbeiten")]
        : []),
      h("button", { class: "primary", onclick: closeSheet }, "Schließen")
    );
  };
  const remove = async (origin) => {
    try {
      await invoke("delete_event", { id: item.id, origin });
      closeSheet();
      await refresh();
      syncNow();
    } catch (error) {
      setStatus(String(error));
    }
  };
  const askDelete = () => {
    if (!item.series) return remove(null);
    actions.replaceChildren(
      h("button", { onclick: render }, "Abbrechen"),
      h("button", { class: "danger", onclick: () => remove(null) }, "Ganze Serie"),
      h("button", { class: "primary", onclick: () => remove(item.origin) }, "Nur diesen")
    );
  };
  detail.replaceChildren(
    h("h2", {}, item.title || "(Ohne Titel)"),
    h("p", { class: "meta" }, h("strong", {}, whenText(item))),
    item.series ? h("p", { class: "meta" }, "Serientermin") : null,
    item.location ? h("p", { class: "meta" }, `Ort: ${item.location}`) : null,
    calendar ? h("p", { class: "meta" }, `Kalender: ${calendar.name}`) : null,
    item.description ? h("p", { class: "meta" }, item.description) : null,
    actions
  );
  render();
  openSheet("detail");
}

function updateAllDay() {
  const allDay = $("e-allday").checked;
  $("e-stime-row").hidden = allDay;
  $("e-etime-row").hidden = allDay;
  $("e-stime").required = !allDay;
  $("e-etime").required = !allDay;
}
$("e-allday").addEventListener("change", updateAllDay);

// Keep the end from falling before the start while editing.
$("e-sdate").addEventListener("change", () => {
  if ($("e-edate").value < $("e-sdate").value) $("e-edate").value = $("e-sdate").value;
});

let editingId = null;

function openEdit(item) {
  const editable = state.calendars.filter((c) => c.can_edit);
  if (editable.length === 0) {
    setStatus("Es gibt noch keinen Kalender zum Bearbeiten. Bitte synchronisieren.");
    return;
  }
  editingId = item ? item.id : null;
  $("e-heading").textContent = item ? "Termin bearbeiten" : "Neuer Termin";
  $("e-error").textContent = "";
  $("e-calendar").replaceChildren(...editable.map((c) => h("option", { value: c.id }, c.name)));

  const now = new Date();
  const day = state.view === "month" && state.selected ? state.selected : todayIso();
  const startHour = Math.min(now.getHours() + 1, 22);
  const defaults = {
    title: "", location: "", description: "", all_day: false,
    start: `${day}T${pad(startHour)}:00`, end: `${day}T${pad(startHour + 1)}:00`,
    calendar: editable[0].id, series: false,
  };
  const v = item || defaults;
  $("e-title").value = v.title;
  $("e-location").value = v.location;
  $("e-description").value = v.description;
  $("e-allday").checked = v.all_day;
  $("e-calendar").value = v.calendar;
  $("e-sdate").value = v.start.slice(0, 10);
  $("e-stime").value = timeOf(v.start);
  // An all-day event ends on its last day (the stored end may be the midnight after it).
  let endDay = v.end.slice(0, 10);
  if (v.all_day && timeOf(v.end) === "00:00" && endDay > v.start.slice(0, 10)) endDay = iso(addDays(fromIso(endDay), -1));
  $("e-edate").value = endDay;
  $("e-etime").value = timeOf(v.end);
  $("e-repeat").value = "";
  $("e-repeat-row").hidden = !!item;
  $("e-note").hidden = !v.series;
  for (const id of ["e-allday", "e-calendar", "e-sdate", "e-stime", "e-edate", "e-etime"]) $(id).disabled = !!v.series;
  updateAllDay();
  openSheet("edit");
}

$("edit").addEventListener("submit", async (event) => {
  event.preventDefault();
  $("e-error").textContent = "";
  try {
    await invoke("save_event", {
      input: {
        id: editingId,
        calendar: $("e-calendar").value,
        title: $("e-title").value,
        location: $("e-location").value,
        description: $("e-description").value,
        allDay: $("e-allday").checked,
        startDate: $("e-sdate").value,
        startTime: $("e-stime").value || "00:00",
        endDate: $("e-edate").value,
        endTime: $("e-etime").value || "00:00",
        repeat: $("e-repeat").value,
      },
    });
    closeSheet();
    await refresh();
    syncNow();
  } catch (error) {
    $("e-error").textContent = String(error);
  }
});

$("add").addEventListener("click", () => openEdit(null));

// Android back button / browser back: close the sheet first.
window.addEventListener("keydown", (e) => { if (e.key === "Escape") closeSheet(); });

// ---------------------------------------------------------------------------------------
// Start
// ---------------------------------------------------------------------------------------

setInterval(() => { if (!document.hidden) syncNow(); }, SYNC_INTERVAL_MS);
document.addEventListener("visibilitychange", () => { if (!document.hidden) syncNow(); });

(async function start() {
  try {
    state.user = await invoke("restore");
  } catch (_) {
    state.user = null;
  }
  if (state.user) await enterMain();
  else await showLogin();
})();
