// La PWA de pastor: habla con la API del mismo servidor. Sin framework ni
// build; `h` arma los nodos con textContent para no interpretar como HTML
// nada de lo que llega de los agentes.
"use strict";

const KIND = {
  claude: { label: "Claude", color: "#ff7a45" },
  codex: { label: "Codex", color: "#00ff9c" },
  opencode: { label: "OpenCode", color: "#b46bff" },
};
const KINDS = Object.keys(KIND);
const AGENT_GROUPS = [["blocked", "Te esperan"], ["working", "Trabajando"], ["idle", "Inactivos"], ["dead", "Terminados"]];
const TASKS = [["todo", "Pendientes", "pendiente"], ["in_progress", "En curso", "en curso"], ["review", "Por revisar", "por revisar"], ["done", "Hechas", "hecha"]];
const TASK_LABEL = Object.fromEntries(TASKS.map(([s, , one]) => [s, one]));
const KEYS = [
  ["Escape", "Esc"], ["Enter", "⏎"], ["Up", "↑"], ["Down", "↓"], ["Left", "←"], ["Right", "→"],
  ["Tab", "Tab"], ["BTab", "⇧Tab"], ["Space", "Espacio"], ["1", "1"], ["2", "2"], ["3", "3"],
  ["y", "y"], ["n", "n"], ["C-c", "Ctrl-C"],
];

// Trazos de Lucide (ISC). Texto fijo nuestro: se puede meter con innerHTML.
const ICONS = {
  gauge: '<path d="m12 14 4-4"/><path d="M3.34 19a10 10 0 1 1 17.32 0"/>',
  bot: '<path d="M12 8V4H8"/><rect width="16" height="12" x="4" y="8" rx="2"/><path d="M2 14h2M20 14h2M15 13v2M9 13v2"/>',
  tasks: '<path d="m3 17 2 2 4-4"/><path d="m3 7 2 2 4-4"/><path d="M13 6h8M13 12h8M13 18h8"/>',
  plus: '<path d="M5 12h14M12 5v14"/>',
  back: '<path d="m15 18-6-6 6-6"/>',
  forward: '<path d="m9 18 6-6-6-6"/>',
  send: '<path d="m5 12 7-7 7 7M12 19V5"/>',
  down: '<path d="M12 5v14M19 12l-7 7-7-7"/>',
  more: '<circle cx="12" cy="5" r="1"/><circle cx="12" cy="12" r="1"/><circle cx="12" cy="19" r="1"/>',
  folder: '<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/>',
  wrap: '<path d="M3 6h18M3 12h15a3 3 0 1 1 0 6h-4"/><path d="m16 16-2 2 2 2"/><path d="M3 18h7"/>',
  type: '<path d="M4 7V4h16v3M9 20h6M12 4v16"/>',
  trash: '<path d="M3 6h18M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>',
  check: '<path d="M20 6 9 17l-5-5"/>',
  assign: '<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M19 8v6M22 11h-6"/>',
  key: '<circle cx="7.5" cy="15.5" r="5.5"/><path d="m21 2-9.6 9.6M15.5 7.5l3 3L22 7l-3-3"/>',
  x: '<path d="M18 6 6 18M6 6l12 12"/>',
  dashboard: '<rect width="7" height="9" x="3" y="3" rx="1"/><rect width="7" height="5" x="14" y="3" rx="1"/><rect width="7" height="9" x="14" y="12" rx="1"/><rect width="7" height="5" x="3" y="16" rx="1"/>',
  history: '<path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"/><path d="M3 3v5h5"/><path d="M12 7v5l4 2"/>',
  bell: '<path d="M10.268 21a2 2 0 0 0 3.464 0"/><path d="M3.262 15.326A1 1 0 0 0 4 17h16a1 1 0 0 0 .74-1.673C19.41 13.956 18 12.499 18 8A6 6 0 0 0 6 8c0 4.499-1.411 5.956-2.738 7.326"/>',
};

// ---------- utilidades ----------

/** localStorage puede no estar (modo privado): nada de esto es crítico. */
const store = {
  get(k) { try { return localStorage.getItem(k) || ""; } catch { return ""; } },
  set(k, v) { try { v ? localStorage.setItem(k, v) : localStorage.removeItem(k); } catch {} },
};

function h(tag, props = {}, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (v == null || v === false) continue;
    if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else if (k === "class") el.className = v;
    else if (k === "text") el.textContent = v;
    // Por CSSOM y no como atributo: la CSP del servidor bloquea los estilos inline.
    else if (k === "style") el.style.cssText = v;
    else if (k in el && typeof v !== "string") el[k] = v;
    else el.setAttribute(k, v);
  }
  el.append(...kids.flat().filter((x) => x != null && x !== false && x !== ""));
  return el;
}

function icon(name) {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("class", "icon");
  svg.setAttribute("aria-hidden", "true");
  svg.innerHTML = ICONS[name];
  return svg;
}

const toastEl = document.getElementById("toast");
let toastTimer;
/** En la capa superior (popover), para que se vea también sobre una hoja abierta. */
function toast(msg, error = false) {
  toastEl.textContent = msg;
  toastEl.classList.toggle("error", error);
  toastEl.hidden = false;
  try { toastEl.hidePopover(); toastEl.showPopover(); } catch {}
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    toastEl.hidden = true;
    try { toastEl.hidePopover(); } catch {}
  }, 3500);
}

class Unauthorized extends Error {}

/** Para `onsubmit`: ignora los envíos mientras el anterior no termine (un
 * doble toque mandaría el mensaje o crearía el agente dos veces). */
function once(fn) {
  return async (e) => {
    e.preventDefault();
    const form = e.currentTarget;
    if (form.dataset.busy) return;
    form.dataset.busy = "1";
    const buttons = form.querySelectorAll("button:not([type=button])");
    buttons.forEach((b) => (b.disabled = true));
    try {
      await fn();
    } finally {
      delete form.dataset.busy;
      buttons.forEach((b) => (b.disabled = false));
    }
  };
}

const API_TIMEOUT = 10000;
// Una tecla o un mensaje que esperan más que esto su turno se descartan.
const STALE_INPUT = 5000;

// Relativa: funciona aunque pastor se sirva bajo una subruta.
async function api(method, path, body) {
  const res = await fetch("api" + path, {
    method,
    headers: {
      Authorization: "Bearer " + store.get("pastor-token"),
      ...(body ? { "Content-Type": "application/json" } : {}),
    },
    body: body ? JSON.stringify(body) : undefined,
    // Sin límite, al perder la red una petición puede quedar colgada minutos.
    signal: AbortSignal.timeout?.(API_TIMEOUT),
  }).catch((e) => {
    // Cortada a medias, una orden pudo hacerse igual: repetirla sin mirar
    // lanzaría un segundo agente.
    if (e.name === "TimeoutError" && method !== "GET") {
      throw new Error("pastor no respondió a tiempo; revisa si se hizo antes de repetirlo");
    }
    throw e;
  });
  const data = await res.json().catch(() => ({}));
  if (res.status === 401) throw new Unauthorized(data.error || "token no válido");
  if (!res.ok) throw new Error(data.error || `error ${res.status}`);
  return data;
}

// `fetch` rechaza con TypeError si no hay red, con otro texto en cada navegador.
const errorText = (e) =>
  e instanceof TypeError ? "Sin conexión con pastor"
  : e.name === "TimeoutError" ? "pastor no responde"
  : e.message;

/** Corre `fn` y avisa si falla; sin token, vuelve a pedirlo. */
async function attempt(fn) {
  try {
    return await fn();
  } catch (e) {
    if (e instanceof Unauthorized) {
      // Pueden fallar dos consultas a la vez: solo la primera vuelve al token.
      if (!store.get("pastor-token")) return;
      store.set("pastor-token", "");
      toast(e.message, true);
      route();
    } else {
      toast(errorText(e), true);
    }
  }
}

function ago(iso) {
  const s = Math.max(0, (Date.now() - Date.parse(iso)) / 1000);
  if (Number.isNaN(s)) return "";
  if (s < 60) return "ahora";
  if (s < 3600) return `${Math.floor(s / 60)} min`;
  if (s < 86400) return `${Math.floor(s / 3600)} h`;
  return `${Math.floor(s / 86400)} d`;
}
function since(iso) {
  const a = ago(iso);
  return a && a !== "ahora" ? "hace " + a : a;
}
const shortPath = (p) => p.replace(/^\/home\/[^/]+(?=\/|$)/, "~");

function recentCwds() {
  let saved = [];
  try { saved = JSON.parse(store.get("pastor-cwds") || "[]"); } catch {}
  return [...new Set([...saved, ...(feed.agents || []).map((a) => a.cwd)])].slice(0, 8);
}
function rememberCwd(cwd) {
  store.set("pastor-cwd", cwd);
  store.set("pastor-cwds", JSON.stringify([cwd, ...recentCwds().filter((c) => c !== cwd)].slice(0, 8)));
}

// ---------- piezas ----------

const pill = (status, label) => h("span", { class: "pill " + status }, h("i"), label);

function avatar(kind) {
  const k = KIND[kind] || { label: kind || "?", color: "" };
  return h("span", { class: "avatar", style: k.color ? `--kind:${k.color}` : null, text: k.label[0] });
}

const skeleton = (n) => Array.from({ length: n }, () => h("div", { class: "skeleton" }));

const empty = (ic, title, text, action) =>
  h("div", { class: "empty" }, h("div", { class: "empty-icon" }, icon(ic)), h("h3", { text: title }), h("p", { text }), action);

/** Un campo con su etiqueta; `label` solo envuelve controles de texto (con
 * botones dentro, tocar la etiqueta pulsaría el primero). */
function field(label, control, hint) {
  const tag = /^(INPUT|TEXTAREA|SELECT)$/.test(control.tagName) ? "label" : "div";
  return h(tag, { class: "field" }, h("span", { class: "field-label", text: label }), control, hint && h("small", { text: hint }));
}

const row = (label, value) => [h("dt", { text: label }), h("dd", {}, value)];

const menuItem = (ic, label, onclick, cls = "") =>
  h("button", { type: "button", class: "menu-item " + cls, onclick }, icon(ic), h("span", { text: label }));

function kindPicker() {
  let kind = KINDS.includes(store.get("pastor-kind")) ? store.get("pastor-kind") : "claude";
  const el = h("div", { class: "segmented", role: "radiogroup", "aria-label": "Tipo de agente" });
  const draw = () => el.replaceChildren(...KINDS.map((k) => h("button", {
    type: "button", role: "radio", "aria-checked": String(k === kind), class: k === kind ? "on" : "",
    onclick: () => { kind = k; draw(); },
  }, avatar(k), KIND[k].label)));
  draw();
  return { el, get value() { return kind; } };
}

function cwdInput() {
  const id = "cwds-" + Math.random().toString(36).slice(2);
  const input = h("input", {
    required: true, placeholder: "/home/…/proyecto", value: store.get("pastor-cwd"), list: id,
    autocapitalize: "off", autocorrect: "off", spellcheck: "false", inputmode: "url",
  });
  return { input, el: h("div", {}, field("Directorio", input, "Ruta absoluta en la máquina"), h("datalist", { id }, recentCwds().map((c) => h("option", { value: c })))) };
}

// ---------- hojas ----------

const sheets = new Set();

function sheet(title, ...body) {
  const d = h("dialog", { class: "sheet" },
    h("div", { class: "sheet-inner" },
      h("div", { class: "sheet-grip" }),
      h("div", { class: "sheet-head" },
        h("h2", { text: title }),
        h("button", { type: "button", class: "icon-btn", "aria-label": "Cerrar", onclick: () => d.close() }, icon("x")),
      ),
      body,
    ),
  );
  // Fuera de `.sheet-inner` solo queda el fondo.
  d.addEventListener("click", (e) => { if (e.target === d) d.close(); });
  d.addEventListener("close", () => { sheets.delete(d); d.remove(); });
  document.body.append(d);
  sheets.add(d);
  d.showModal();
  return d;
}

function ask(title, text, okLabel, danger = false) {
  return new Promise((resolve) => {
    let answer = false;
    const d = sheet(title,
      h("p", { class: "sheet-text", text }),
      h("div", { class: "sheet-actions" },
        h("button", { type: "button", class: "btn", text: "Cancelar", onclick: () => d.close() }),
        h("button", { type: "button", class: "btn " + (danger ? "danger-solid" : "primary"), text: okLabel, onclick: () => { answer = true; d.close(); } }),
      ),
    );
    d.addEventListener("close", () => resolve(answer));
  });
}

// ---------- navegación ----------

const topEl = document.getElementById("top");
const main = document.getElementById("main");
const tabs = document.getElementById("tabs");
/** Lo que hay que parar al salir de una pantalla (polling). */
let cleanup = [];

/** Llama a `fn` cada `ms` mientras no se pare. `tick` adelanta la siguiente
 * vuelta sin duplicar el ciclo. */
function loop(ms, fn) {
  let stopped = false;
  let running = false;
  let again = false;
  let timer;
  const tick = async () => {
    clearTimeout(timer);
    if (stopped) return;
    if (running) { again = true; return; }
    running = true;
    try {
      if (!document.hidden) await fn();
    } finally {
      running = false;
    }
    if (stopped) return;
    if (again) { again = false; return tick(); }
    timer = setTimeout(tick, ms);
  };
  // Al volver a la app, que no espere a su siguiente vuelta.
  const onVisible = () => { if (!document.hidden) tick(); };
  document.addEventListener("visibilitychange", onVisible);
  tick();
  return {
    tick,
    stop() {
      stopped = true;
      clearTimeout(timer);
      document.removeEventListener("visibilitychange", onVisible);
    },
  };
}

/** Cambia la lista sin esperar a la siguiente consulta. Una consulta que ya
 * iba en camino traería la lista de antes: `version` hace que se descarte. */
function changeAgents(list) {
  feed.version++;
  setAgents(list);
}

/** Un `loop` que vive lo que la pantalla actual. */
function every(ms, fn) {
  const l = loop(ms, fn);
  cleanup.push(l.stop);
  return l.tick;
}

/** Los agentes se consultan en un solo lugar para todas las pantallas (y la
 * pestaña muestra cuántos te esperan aunque estés en Tareas). */
const feed = { agents: null, json: "", version: 0, subs: new Set() };

function watchAgents(fn) {
  feed.subs.add(fn);
  cleanup.push(() => feed.subs.delete(fn));
  if (feed.agents) fn(feed.agents);
}

function setAgents(list) {
  // Redibujar sin cambios cambia las tarjetas bajo el dedo y se pierde el toque.
  const json = JSON.stringify(list);
  if (json === feed.json) return;
  feed.json = json;
  feed.agents = list;
  const waiting = list.filter((a) => a.status === "blocked").length;
  const count = tabs.querySelector(".count");
  count.textContent = String(waiting);
  count.hidden = !waiting;
  document.title = waiting ? `(${waiting}) pastor` : "pastor";
  feed.subs.forEach((f) => f(list));
}

function setTop({ title, back, actions = [] }) {
  const titleEl = h("div", { class: "title", text: title });
  const subtitle = h("div", { class: "subtitle" });
  topEl.replaceChildren(
    back
      ? h("a", { class: "icon-btn", href: back, "aria-label": "Volver" }, icon("back"))
      : h("img", { class: "logo", src: "icon-192.png", alt: "" }),
    h("div", { class: "titles" }, titleEl, subtitle),
    ...actions,
  );
  return { title: titleEl, subtitle };
}

const TABS = [["agents", "Agentes", "bot"], ["tasks", "Tareas", "tasks"], ["dashboard", "Dashboard", "dashboard"], ["history", "Historial", "history"]];
tabs.append(...TABS.map(([tab, label, ic]) =>
  h("a", { href: "#/" + tab, "data-tab": tab },
    h("span", { class: "tab-icon" }, icon(ic), tab === "agents" && h("b", { class: "count", hidden: true })),
    h("span", { text: label }),
  )));

function route() {
  cleanup.forEach((f) => f());
  cleanup = [];
  sheets.forEach((d) => d.close());
  main.replaceChildren();
  main.className = "";
  const hash = location.hash.slice(1) || "/agents";
  const tab = TABS.find(([t]) => hash.startsWith("/" + t))?.[0] || "agents";
  tabs.querySelectorAll("a").forEach((a) => a.classList.toggle("on", a.dataset.tab === tab));
  if (!store.get("pastor-token")) {
    document.body.dataset.view = "token";
    return tokenView();
  }
  const agent = hash.match(/^\/agent\/(.+)$/);
  if (agent) {
    let id;
    try {
      id = decodeURIComponent(agent[1]);
    } catch {
      location.hash = "#/agents";
      return;
    }
    document.body.dataset.view = "agent";
    return agentView(id);
  }
  document.body.dataset.view = tab;
  if (tab === "tasks") return tasksView();
  if (tab === "dashboard") return dashboardView();
  if (tab === "history") return historyView();
  agentsView();
}

window.addEventListener("hashchange", route);

// ---------- token ----------

function tokenView() {
  const input = h("input", { type: "password", autocomplete: "current-password", placeholder: "Pega aquí el token", required: true });
  main.append(h("div", { class: "login" },
    h("img", { class: "login-logo", src: "icon-512.png", alt: "" }),
    h("h1", { text: "pastor" }),
    h("p", { text: "Tus agentes, desde el celular." }),
    h("form", {
      class: "form login-form",
      onsubmit: once(async () => {
        store.set("pastor-token", input.value.trim());
        const agents = await attempt(() => api("GET", "/agents"));
        if (!agents) {
          store.set("pastor-token", "");
          return;
        }
        setAgents(agents);
        route();
      }),
    },
      field("Token", input),
      h("button", { class: "btn primary block" }, icon("key"), "Conectar"),
    ),
    h("p", { class: "hint" }, "En la máquina: ", h("code", { text: "cat ~/.local/share/pastor/serve-token" })),
  ));
  input.focus();
}

// ---------- agentes ----------

function agentsView() {
  const { subtitle } = setTop({
    title: "Agentes",
    actions: [h("button", { class: "icon-btn accent", "aria-label": "Nuevo agente", onclick: newAgentSheet }, icon("plus"))],
  });
  const list = h("div", {}, h("div", { class: "cards" }, skeleton(3)));
  main.append(list);
  watchAgents((agents) => {
    const n = (...st) => agents.filter((a) => st.includes(a.status)).length;
    const parts = [n("blocked") && `${n("blocked")} te esperan`, n("working", "starting") && `${n("working", "starting")} trabajando`];
    subtitle.textContent = parts.filter(Boolean).join(" · ") || (agents.length ? "Todo en calma" : "");
    if (!agents.length) {
      list.replaceChildren(empty("bot", "No hay agentes", "Crea uno para ponerlo a trabajar.",
        h("button", { class: "btn primary", onclick: newAgentSheet }, icon("plus"), "Nuevo agente")));
      return;
    }
    const group = (s) => (s === "starting" ? "working" : s);
    list.replaceChildren(...AGENT_GROUPS.flatMap(([g, label]) => {
      const items = agents.filter((a) => group(a.status) === g);
      return items.length
        ? [h("section", { class: "group" },
            h("h2", { class: "group-title" }, label, h("span", { class: "group-count", text: String(items.length) })),
            h("div", { class: "cards" }, items.map(agentCard)))]
        : [];
    }));
  });
}

function agentCard(a) {
  return h("a", { class: "card agent " + a.status, href: "#/agent/" + encodeURIComponent(a.id) },
    avatar(a.kind),
    h("div", { class: "card-main" },
      h("div", { class: "card-title" }, h("span", { class: "name", text: a.name }), pill(a.status, a.status_label)),
      h("div", { class: "card-meta" },
        icon("folder"), h("span", { class: "ellipsis", text: shortPath(a.cwd) }),
        h("span", { class: "time", text: ago(a.created_at) }),
      ),
      a.task && h("div", { class: "card-meta" }, icon("tasks"), h("span", { class: "ellipsis", text: "Tarea " + a.task })),
    ),
    h("span", { class: "chev" }, icon("forward")),
  );
}

function newAgentSheet() {
  const kind = kindPicker();
  const cwd = cwdInput();
  const name = h("input", { placeholder: "Opcional", autocapitalize: "off", autocorrect: "off" });
  const prompt = h("textarea", { rows: 3, placeholder: "Opcional: lo primero que le pides" });
  const d = sheet("Nuevo agente", h("form", {
    class: "form",
    onsubmit: once(async () => {
      const body = { kind: kind.value, cwd: cwd.input.value.trim(), name: name.value.trim() || undefined, prompt: prompt.value.trim() || undefined };
      const a = await attempt(() => api("POST", "/agents", body));
      if (!a) return;
      rememberCwd(body.cwd);
      store.set("pastor-kind", body.kind);
      // Ya en la lista, para que su pantalla no lo dé por inexistente
      // antes de la siguiente consulta.
      changeAgents([...(feed.agents || []), { ...a, status: "starting", status_label: "iniciando" }]);
      d.close();
      toast(`${a.name} creado`);
      location.hash = "#/agent/" + encodeURIComponent(a.id);
    }),
  },
    field("Tipo", kind.el), cwd.el, field("Nombre", name), field("Primer mensaje", prompt),
    h("button", { class: "btn primary block" }, icon("plus"), "Crear agente"),
  ));
}

function termPrefs(el) {
  const size = Math.min(18, Math.max(9, Number(store.get("pastor-font")) || 12));
  el.style.fontSize = size + "px";
  // Por defecto ajusta: el agente ya dibuja a este ancho, y así una línea que
  // aún venga ancha (otra vista lo cambió) tampoco obliga a moverse de lado.
  el.classList.toggle("wrap", store.get("pastor-wrap") !== "0");
  return size;
}

/** Ancho de un carácter y alto de una línea con la letra de `el`; se mide
 * una vez por tipo de letra. */
const glyphs = new Map();
function glyph(el) {
  const font = getComputedStyle(el).font;
  if (!glyphs.has(font)) {
    const probe = h("span", { text: "M".repeat(50), style: "position:absolute;visibility:hidden;white-space:pre" });
    probe.style.font = font;
    el.after(probe);
    const { width, height } = probe.getBoundingClientRect();
    probe.remove();
    if (!width || !height) return null;
    glyphs.set(font, { w: width / 50, h: height });
  }
  return glyphs.get(font);
}

/** Columnas y filas que caben en `el` con su letra actual, o `null` si aún
 * no tiene tamaño. */
function termSize(el) {
  const g = glyph(el);
  if (!g) return null;
  const cs = getComputedStyle(el);
  const px = (p) => parseFloat(cs[p]) || 0;
  // Una columna de margen: si se redondea hacia arriba, el separador de
  // ancho completo de Claude pasaría a dos líneas.
  const cols = Math.floor((el.clientWidth - px("paddingLeft") - px("paddingRight")) / g.w) - 1;
  const rows = Math.floor((el.clientHeight - px("paddingTop") - px("paddingBottom")) / g.h);
  return cols > 0 && rows > 0 ? { cols, rows } : null;
}

function agentView(id) {
  let agent = null;
  const more = h("button", { class: "icon-btn", "aria-label": "Opciones", onclick: () => agentMenu() }, icon("more"));
  const top = setTop({ title: id, back: "#/agents", actions: [more] });
  const screen = h("pre", { class: "term loading", text: "Cargando pantalla…" });
  termPrefs(screen);
  const { atBottom, jump, sync } = scrollFollow(screen);
  const note = h("div", { class: "note", role: "alert", hidden: true });

  // Pantalla en vivo o conversación entera (solo de los agentes de Claude).
  const termWrap = h("div", { class: "term-wrap" }, screen, jump);
  const chat = conversationPane(id, () => agent);
  const views = { screen: termWrap, chat: chat.el };
  const switcher = h("div", { class: "view-switch", role: "tablist", "aria-label": "Vista" });
  const show = (v, remember = true) => {
    if (remember) store.set("pastor-agent-view", v);
    for (const [k, el] of Object.entries(views)) el.hidden = k !== v;
    screenVisible = v === "screen";
    if (screenVisible) pollScreen();
    chat.active(v === "chat");
    switcher.replaceChildren(...[["screen", "Pantalla"], ["chat", "Conversación"]].map(([k, label]) =>
      h("button", { type: "button", role: "tab", "aria-selected": String(k === v), class: k === v ? "on" : "", text: label, onclick: () => show(k) })));
  };

  let gone = false;
  let checking = false;
  cleanup.push(() => (gone = true));
  // La lista guardada puede ser de antes de que existiera el agente (recién
  // creado o asignado, o la app estuvo en segundo plano): se confirma con
  // el servidor antes de darlo por perdido.
  const confirmGone = async () => {
    if (checking) return;
    checking = true;
    const fresh = await attempt(() => api("GET", "/agents"));
    checking = false;
    if (gone || !fresh) return;
    if (fresh.some((x) => x.id === id)) return changeAgents(fresh);
    gone = true;
    toast("Ese agente ya no existe", true);
    location.hash = "#/agents";
  };
  watchAgents((agents) => {
    if (gone) return;
    const a = agents.find((x) => x.id === id);
    if (!a) return confirmGone();
    agent = a;
    top.title.textContent = a.name;
    top.subtitle.replaceChildren(pill(a.status, a.status_label), h("span", { class: "ellipsis", text: shortPath(a.cwd) }));
    if (a.kind !== "claude" && !switcher.hidden) {
      switcher.hidden = true;
      // Después de armar la vista: esto puede correr antes de que exista `pollScreen`.
      queueMicrotask(() => show("screen", false));
    }
  });

  let first = true;
  let sentSize = "";
  // Mientras no mirabas, la TUI u otro celular pudieron cambiar el tamaño:
  // al volver se manda de nuevo.
  const resend = () => { if (!document.hidden) sentSize = ""; };
  document.addEventListener("visibilitychange", resend);
  cleanup.push(() => document.removeEventListener("visibilitychange", resend));
  // Con la conversación a la vista, la pantalla no se pide.
  let screenVisible = true;
  const pollScreen = every(1500, async () => {
    if (!screenVisible) return;
    try {
      // El tamaño solo va cuando cambia (o al abrir): el agente redibuja a este
      // ancho y sus líneas caben sin moverse de lado. Mandarlo siempre haría
      // que dos pantallas abiertas en el mismo agente se lo quitaran sin parar.
      const size = termSize(screen);
      const key = size && `?cols=${size.cols}&rows=${size.rows}`;
      const query = key && key !== sentSize ? key : "";
      const { text } = await api("GET", `/agents/${encodeURIComponent(id)}/screen${query}`);
      if (query) sentSize = query;
      note.hidden = true;
      if (!first && screen.textContent === text) return;
      // Solo baja si ya estaba abajo: si subiste a leer, no te mueve.
      const stick = first || atBottom();
      screen.classList.remove("loading");
      screen.textContent = text;
      if (stick) screen.scrollTop = screen.scrollHeight;
      first = false;
      sync();
    } catch (e) {
      if (e instanceof Unauthorized) return attempt(() => { throw e; });
      // Debajo, sin tapar lo último que se vio: suele ser lo que había que leer.
      note.textContent = errorText(e);
      note.hidden = false;
    }
  });

  // Al girar el celular, abrirse el teclado o cambiar la letra cambia lo que
  // cabe: que el agente se ajuste ya, sin esperar a la siguiente vuelta.
  let resizeTimer;
  const resized = new ResizeObserver(() => {
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(pollScreen, 300);
  });
  resized.observe(screen);
  cleanup.push(() => { clearTimeout(resizeTimer); resized.disconnect(); });

  // En fila: dos toques seguidos irían en paralelo y tmux podría recibirlos
  // al revés, o Escape y la siguiente tecla como una sola (Alt+tecla).
  // Lo que esperó demasiado en la fila no se manda: al volver la red, un «y»
  // o un Enter viejos caerían en otra pantalla del agente.
  let queue = Promise.resolve();
  const post = (path, body) => {
    const queued = Date.now();
    return (queue = queue.then(() => {
      if (Date.now() - queued > STALE_INPUT) {
        toast("No se mandó: pastor tardó en responder", true);
        return false;
      }
      return attempt(async () => {
        await api("POST", `/agents/${encodeURIComponent(id)}${path}`, body);
        setTimeout(screenVisible ? pollScreen : chat.refresh, 300);
        return true;
      });
    }));
  };

  const keys = h("div", { class: "keys", role: "toolbar", "aria-label": "Teclas" }, KEYS.map(([key, label]) => h("button", {
    type: "button",
    class: key === "C-c" ? "key danger" : "key",
    text: label,
    // Que tocar una tecla no le quite el foco a la caja (cerraría el teclado).
    onpointerdown: (e) => e.preventDefault(),
    onclick: async () => {
      if (key === "C-c" && !(await ask("¿Mandar Ctrl-C?", "Puede cancelar lo que está haciendo o terminar al agente.", "Mandar Ctrl-C", true))) return;
      navigator.vibrate?.(8);
      post("/keys", { keys: [key] });
    },
  })));

  const draftKey = "pastor-draft-" + id;
  const text = h("textarea", { rows: 1, placeholder: "Escribe al agente…", "aria-label": "Mensaje", enterkeyhint: "enter" });
  text.value = store.get(draftKey);
  const grow = () => {
    text.style.height = "auto";
    text.style.height = Math.min(text.scrollHeight + 2, 150) + "px";
  };
  text.addEventListener("input", () => { store.set(draftKey, text.value); grow(); });
  const form = h("form", {
    class: "composer-row",
    onsubmit: once(async () => {
      const sent = text.value;
      if (!sent.trim()) return;
      if (await post("/send", { text: sent })) {
        // Lo que se escribió mientras se enviaba se queda.
        text.value = text.value.startsWith(sent) ? text.value.slice(sent.length).trimStart() : text.value;
        store.set(draftKey, text.value);
        grow();
      }
    }),
  }, text, h("button", { class: "send-btn", "aria-label": "Enviar" }, icon("send")));
  // Enter hace salto de línea (en el celular no hay Shift); Ctrl+Enter envía.
  text.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) { e.preventDefault(); form.requestSubmit(); }
  });

  function agentMenu() {
    const size = h("output", { text: String(termPrefs(screen)) });
    const step = (delta) => {
      store.set("pastor-font", String(Number(size.textContent) + delta));
      size.textContent = String(termPrefs(screen));
      // Con otra letra caben otras columnas: que el agente se ajuste ya.
      setTimeout(pollScreen, 300);
    };
    const wrapped = store.get("pastor-wrap") !== "0";
    const d = sheet(agent?.name || id,
      agent && h("dl", { class: "info" },
        row("Tipo", KIND[agent.kind]?.label || agent.kind),
        row("Estado", agent.status_label),
        row("Directorio", agent.cwd),
        agent.task && row("Tarea", agent.task),
        row("Creado", since(agent.created_at)),
      ),
      h("div", { class: "menu" },
        menuItem("wrap", wrapped ? "Sin ajustar líneas" : "Ajustar líneas al ancho", () => {
          store.set("pastor-wrap", wrapped ? "0" : "1");
          termPrefs(screen);
          d.close();
        }),
        h("div", { class: "menu-row" }, icon("type"), h("span", { text: "Tamaño de letra" }),
          h("div", { class: "stepper" },
            h("button", { type: "button", text: "A−", "aria-label": "Más chica", onclick: () => step(-1) }),
            size,
            h("button", { type: "button", text: "A+", "aria-label": "Más grande", onclick: () => step(1) }),
          ),
        ),
        menuItem("trash", "Terminar agente", async () => {
          d.close();
          const name = agent?.name || id;
          if (!(await ask(`¿Terminar ${name}?`, "Se cierra su sesión de tmux y lo que esté haciendo.", "Terminar", true))) return;
          if (!(await attempt(() => api("DELETE", `/agents/${encodeURIComponent(id)}`)))) return;
          gone = true;
          store.set(draftKey, "");
          changeAgents((feed.agents || []).filter((a) => a.id !== id));
          toast(`${name} terminado`);
          location.hash = "#/agents";
        }, "danger"),
      ),
    );
  }

  main.className = "agent-main";
  main.append(switcher, termWrap, chat.el, h("div", { class: "composer" }, note, keys, form));
  show(store.get("pastor-agent-view") === "chat" ? "chat" : "screen");
  requestAnimationFrame(grow);
}

/** Texto de un mensaje con lo básico de Markdown: bloques de código, `código`
 * y **negritas**. Todo con nodos de texto, nunca como HTML. */
function markdown(text) {
  const out = [];
  text.split(/^[ \t]*(?:```|~~~)[^\n]*\n?/m).forEach((part, i) => {
    if (i % 2) {
      out.push(h("pre", { class: "md-code", text: part.replace(/\n$/, "") }));
      return;
    }
    const p = h("div", { class: "md-text" });
    for (const piece of part.split(/(`[^`\n]+`|\*\*[^*\n]+\*\*)/)) {
      if (/^`[^`]+`$/.test(piece)) p.append(h("code", { text: piece.slice(1, -1) }));
      else if (/^\*\*[^*]+\*\*$/.test(piece)) p.append(h("strong", { text: piece.slice(2, -2) }));
      else if (piece) p.append(piece);
    }
    if (part.trim()) out.push(p);
  });
  return out;
}

/** Seguir el final de `el` al llegar contenido: el botón para bajar
 * aparece si subiste a leer. */
function scrollFollow(el) {
  const atBottom = () => el.scrollHeight - el.scrollTop - el.clientHeight < 50;
  const jump = h("button", { type: "button", class: "jump", hidden: true, "aria-label": "Ir al final", onclick: () => (el.scrollTop = el.scrollHeight) }, icon("down"));
  const sync = () => (jump.hidden = atBottom());
  el.addEventListener("scroll", sync, { passive: true });
  return { atBottom, jump, sync };
}

const hhmm = (iso) => {
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? "" : d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
};

/** Nodos de la conversación que se guardan, como mucho, a la vista. */
const MAX_NODES = 600;

/** La conversación entera del agente, de su transcripción de Claude Code: la
 * pantalla solo tiene lo visible. Llegan los últimos mensajes; los anteriores,
 * al pedirlos. Mientras está a la vista pide solo lo nuevo. */

function conversationPane(id, getAgent) {
  const list = h("div", { class: "chat" });
  const older = h("button", { type: "button", class: "btn block older", hidden: true, text: "Ver mensajes anteriores", onclick: () => loadOlder() });
  const scroller = h("div", { class: "chat-scroll" }, h("div", { class: "chat" }, older), list);
  const { atBottom, jump, sync } = scrollFollow(scroller);
  const el = h("div", { class: "chat-wrap" }, scroller, jump);
  let session = "";
  let first = 0;
  let total = 0;
  let visible = false;
  /** Si hay una petición de mensajes anteriores en camino. */
  let loading = false;

  const isTools = (node) => node?.classList?.contains("tools");

  /** Pone en el resumen de un grupo cuántas herramientas tiene y la última. */
  const summarize = (group) => {
    const tools = group.querySelector(".tools-list");
    const n = tools.childElementCount;
    group.querySelector("summary").textContent = `${n === 1 ? "1 herramienta" : `${n} herramientas`} · ${tools.lastElementChild.textContent}`;
  };

  /** Los nodos de `messages`, que empiezan en el mensaje `start`, con las
   * herramientas seguidas juntas en un grupo que se abre al tocarlo. `last`
   * es el nodo previo, por si el grupo continúa. Cada nodo nuevo guarda en
   * `data-from` su primer mensaje, para saber desde dónde pedir al recortar. */
  const render = (messages, start, last) => {
    const out = [];
    messages.forEach((m, i) => {
      if (m.role !== "tool") {
        out.push(h("div", { class: "msg " + m.role, "data-from": start + i },
          markdown(m.text),
          m.role === "user" && m.at && h("span", { class: "msg-time", text: hhmm(m.at) })));
        return;
      }
      let group = out.length ? out[out.length - 1] : last;
      if (!isTools(group)) {
        group = h("details", { class: "tools", "data-from": start + i }, h("summary"), h("div", { class: "tools-list" }));
        out.push(group);
      }
      group.querySelector(".tools-list").append(h("div", { class: "tool", text: m.text }));
      summarize(group);
    });
    return out;
  };

  /** Si la vista tiene más de `MAX_NODES` nodos, quita los de arriba: una
   * sesión larga con la vista abierta haría crecer la página sin fin. Solo
   * los que quedan más de una pantalla por encima de lo que se ve, y sin
   * mover lo que tienes delante: lo que estás leyendo nunca se quita. Lo
   * quitado se puede volver a pedir con "Ver mensajes anteriores". */
  const trim = () => {
    // Con una página anterior en camino, `first` tiene que seguir siendo
    // donde acaba esa página.
    if (loading) return;
    let extra = list.childElementCount - MAX_NODES;
    if (extra <= 0) return;
    const limit = scroller.getBoundingClientRect().top - scroller.clientHeight;
    // Antes de quitar nada: al encoger, el navegador ya acerca `scrollTop`.
    const [top, before] = [scroller.scrollTop, scroller.scrollHeight];
    let removed = false;
    while (extra-- > 0 && list.firstElementChild.getBoundingClientRect().bottom < limit) {
      list.firstElementChild.remove();
      removed = true;
    }
    if (!removed) return;
    scroller.scrollTop = top - (before - scroller.scrollHeight);
    first = Number(list.firstElementChild.dataset.from);
    older.hidden = false;
  };

  const placeholder = (title, text) => list.replaceChildren(empty("bot", title, text));
  const url = (q) => `/agents/${encodeURIComponent(id)}/conversation${q}`;

  const poll = async () => {
    if (!visible) return;
    const q = session ? `?session=${encodeURIComponent(session)}&after=${total}` : "";
    const data = await attempt(() => api("GET", url(q)));
    if (!data) return;
    if (!data.available) {
      const kind = getAgent()?.kind;
      placeholder("Sin conversación todavía", kind && kind !== "claude"
        ? "Por ahora solo se ve la conversación de los agentes de Claude."
        : "Aparece cuando el agente responda por primera vez.");
      session = "";
      older.hidden = true;
      return;
    }
    const fresh = data.session !== session;
    const stick = fresh || atBottom();
    // Otra sesión (`/clear`) o una transcripción reescrita: desde cero.
    if (fresh) {
      list.replaceChildren();
      session = data.session;
      first = data.from;
      older.hidden = first === 0;
    }
    total = data.to;
    if (data.messages.length) list.querySelector(":scope > .empty")?.remove();
    list.append(...render(data.messages, data.from, list.lastElementChild));
    if (!list.childElementCount) placeholder("Sin mensajes", "La conversación aún está vacía.");
    if (stick) scroller.scrollTop = scroller.scrollHeight;
    trim();
    sync();
    // Si quedaron mensajes nuevos por traer, que no esperen a la siguiente vuelta.
    if (data.to < data.total) setTimeout(tick, 0);
  };

  const loadOlder = async () => {
    older.disabled = true;
    loading = true;
    const data = await attempt(() => api("GET", url(`?session=${encodeURIComponent(session)}&before=${first}`)));
    loading = false;
    older.disabled = false;
    if (!data?.available || data.session !== session) return tick();
    // Arriba, sin mover lo que estás leyendo.
    const before = scroller.scrollHeight;
    const nodes = render(data.messages, data.from);
    // Si la página acaba en herramientas y lo de abajo empieza igual, un solo grupo.
    const below = list.firstElementChild;
    if (isTools(nodes[nodes.length - 1]) && isTools(below)) {
      const group = nodes[nodes.length - 1];
      group.querySelector(".tools-list").append(...below.querySelector(".tools-list").children);
      group.open ||= below.open;
      summarize(group);
      below.remove();
    }
    list.prepend(...nodes);
    scroller.scrollTop += scroller.scrollHeight - before;
    first = data.from;
    older.hidden = first === 0;
  };

  const tick = every(3000, poll);
  return {
    el,
    refresh: () => tick(),
    active(on) {
      visible = on;
      if (on) tick();
    },
  };
}

// ---------- tareas ----------

function tasksView() {
  let tasks = null;
  let filter = store.get("pastor-task-filter");
  let reload = () => {};
  const add = () => newTaskSheet(reload);
  setTop({
    title: "Tareas",
    actions: [h("button", { class: "icon-btn accent", "aria-label": "Nueva tarea", onclick: add }, icon("plus"))],
  });
  const chips = h("div", { class: "chips", role: "tablist" });
  const list = h("div", { class: "cards" }, skeleton(3));
  const agentName = (id) => (feed.agents || []).find((a) => a.id === id)?.name || id;

  const card = (t) => h("button", { type: "button", class: "card task", onclick: () => taskSheet(t, agentName, reload) },
    h("div", { class: "card-main" },
      h("span", { class: "name", text: t.title }),
      t.description && h("div", { class: "desc", text: t.description }),
      h("div", { class: "card-meta" },
        h("span", { class: "tag", text: t.id }),
        t.agent && [icon("bot"), h("span", { class: "ellipsis", text: agentName(t.agent) })],
        h("span", { class: "time", text: ago(t.created_at) }),
      ),
    ),
    h("span", { class: "chev" }, icon("forward")),
  );

  const draw = () => {
    if (!tasks) return;
    const counts = Object.fromEntries(TASKS.map(([s]) => [s, tasks.filter((t) => t.status === s).length]));
    if (!TASKS.some(([s]) => s === filter)) filter = TASKS.find(([s]) => s !== "done" && counts[s])?.[0] || "todo";
    chips.replaceChildren(...TASKS.map(([s, label]) => h("button", {
      type: "button", role: "tab", class: s === filter ? "chip on" : "chip", "aria-selected": String(s === filter),
      onclick: () => { filter = s; store.set("pastor-task-filter", s); draw(); },
    }, label, h("span", { class: "chip-count", text: String(counts[s]) }))));
    const shown = tasks.filter((t) => t.status === filter);
    list.replaceChildren(...(shown.length
      ? shown.map(card)
      : [tasks.length
          ? empty("tasks", "Nada por aquí", "No hay tareas en este estado.")
          : empty("tasks", "No hay tareas", "Anota lo que quieres que hagan tus agentes.",
              h("button", { class: "btn primary", onclick: add }, icon("plus"), "Nueva tarea"))]));
  };

  main.append(chips, list);
  reload = poll(10000, "/tasks", (list) => { tasks = list; draw(); });
  watchAgents(draw);
}

function newTaskSheet(reload) {
  const title = h("input", { required: true, placeholder: "Qué hay que hacer" });
  const desc = h("textarea", { rows: 4, placeholder: "Opcional: contexto, criterios, enlaces…" });
  const d = sheet("Nueva tarea", h("form", {
    class: "form",
    onsubmit: once(async () => {
      if (!(await attempt(() => api("POST", "/tasks", { title: title.value.trim(), description: desc.value.trim() })))) return;
      d.close();
      toast("Tarea agregada");
      reload();
    }),
  },
    field("Título", title), field("Descripción", desc),
    h("button", { class: "btn primary block" }, icon("plus"), "Agregar tarea"),
  ));
  title.focus();
}

function taskSheet(t, agentName, reload) {
  const path = `/tasks/${encodeURIComponent(t.id)}`;
  const d = sheet(t.title,
    t.description && h("p", { class: "sheet-text", text: t.description }),
    h("dl", { class: "info" },
      row("Estado", TASK_LABEL[t.status] || t.status),
      row("Id", t.id),
      t.agent && row("Agente", agentName(t.agent)),
      row("Creada", since(t.created_at)),
      /^https?:\/\//.test(t.url || "") && row("Enlace", h("a", { href: t.url, target: "_blank", rel: "noopener", text: t.url })),
    ),
    h("div", { class: "menu" },
      t.agent && menuItem("bot", "Abrir su agente", () => { location.hash = "#/agent/" + encodeURIComponent(t.agent); }),
      t.status !== "done" && menuItem("assign", t.agent ? "Asignar a otro agente" : "Asignar a un agente", () => { d.close(); assignSheet(t, reload); }),
      t.status !== "done" && menuItem("check", "Marcar como hecha", async () => {
        d.close();
        if (await attempt(() => api("POST", path + "/done"))) { toast("Tarea hecha"); reload(); }
      }),
      menuItem("trash", "Borrar", async () => {
        d.close();
        if (!(await ask("¿Borrar la tarea?", `«${t.title}» se borra para siempre.`, "Borrar", true))) return;
        if (await attempt(() => api("DELETE", path))) { toast("Tarea borrada"); reload(); }
      }, "danger"),
    ),
  );
}

function assignSheet(t, reload) {
  const agents = (feed.agents || []).filter((a) => a.status !== "dead");
  let choice = agents.length ? "agent:" + agents[0].id : "new";
  const kind = kindPicker();
  const cwd = cwdInput();
  const extra = h("div", { class: "form" }, field("Tipo", kind.el), cwd.el);
  const options = h("div", { class: "options", role: "radiogroup", "aria-label": "Agente" });
  const option = (value, ...kids) => h("button", {
    type: "button", role: "radio", "aria-checked": String(choice === value), class: choice === value ? "option on" : "option",
    onclick: () => { choice = value; draw(); },
  }, kids);
  const draw = () => {
    options.replaceChildren(
      ...agents.map((a) => option("agent:" + a.id,
        avatar(a.kind),
        h("span", { class: "opt-main" }, h("span", { class: "name", text: a.name }), h("small", { text: shortPath(a.cwd) })),
        pill(a.status, a.status_label))),
      option("new",
        h("span", { class: "avatar plus" }, icon("plus")),
        h("span", { class: "opt-main" }, h("span", { class: "name", text: "Agente nuevo" }), h("small", { text: "Lo crea y le pasa la tarea" }))),
    );
    extra.hidden = choice !== "new";
    cwd.input.required = choice === "new";
  };
  draw();
  const d = sheet("Asignar «" + t.title + "»", h("form", {
    class: "form",
    onsubmit: once(async () => {
      const body = choice === "new" ? { kind: kind.value, cwd: cwd.input.value.trim() } : { agent: choice.slice("agent:".length) };
      if (!(await attempt(() => api("POST", `/tasks/${encodeURIComponent(t.id)}/assign`, body)))) return;
      if (body.cwd) { rememberCwd(body.cwd); store.set("pastor-kind", body.kind); }
      d.close();
      toast("Tarea asignada");
      reload();
      agentsLoop.tick();
    }),
  }, options, extra, h("button", { class: "btn primary block" }, icon("assign"), "Asignar")));
}

// ---------- dashboard e historial ----------

/** Duración corta y legible, como `history::human`: "45s", "12m", "3h 20m", "2d 5h". */
function human(secs) {
  secs = Math.max(0, secs);
  const d = Math.floor(secs / 86400), hr = Math.floor(secs / 3600) % 24, m = Math.floor(secs / 60) % 60;
  return d ? `${d}d ${hr}h` : hr ? `${hr}h ${m}m` : m ? `${m}m` : `${secs}s`;
}

const stat = (label, value, color) =>
  h("div", { class: "stat", style: color && `--c: ${color}` }, h("b", { text: String(value) }), h("span", { text: label }));

function panel(ic, title, ...body) {
  return h("section", { class: "panel" }, h("h2", { class: "group-title" }, icon(ic), title), body);
}

/** Barras horizontales: `[etiqueta, valor, color]`. */
function hbars(rows) {
  const max = Math.max(1, ...rows.map(([, n]) => n));
  return h("div", { class: "hbars" }, rows.map(([label, n, color]) => h("div", { class: "hbar", style: `--c: ${color}` },
    h("span", { class: "hbar-label", text: label }),
    h("span", { class: "hbar-track" }, h("i", { style: `width: ${(n / max) * 100}%` })),
    h("b", { text: String(n) }),
  )));
}

/** Datos que se consultan cada `ms` y se redibujan solo si cambiaron.
 * Devuelve el `tick` de `every`, para consultar ya. */
function poll(ms, path, fn) {
  let json = "";
  return every(ms, () => attempt(async () => {
    const v = await api("GET", path);
    const j = JSON.stringify(v);
    if (j === json) return;
    json = j;
    fn(v);
  }));
}

/** Lo que queda de cada ventana de límite, una caja por suscripción. */
function limitsView(list) {
  const color = (left) => left < 10 ? "var(--magenta)" : left < 30 ? "var(--yellow)" : "var(--green)";
  return h("div", { class: "limits" }, list.map((u) => h("div", { class: "limit", style: `--k: ${KIND[u.kind].color}` },
    h("div", { class: "limit-title" }, h("b", { text: KIND[u.kind].label }), u.plan && h("span", { text: u.plan }),
      u.stale && h("small", { text: since(u.updated_at) })),
    u.error
      ? h("p", { class: "quiet", text: u.error })
      : u.windows.map((w) => {
        const left = w.left_pct;
        const secs = w.resets_at ? Math.round((Date.parse(w.resets_at) - Date.now()) / 1000) : 0;
        const reset = secs > 0 ? "se reinicia en " + human(secs) : "";
        return h("div", { class: "hbar limit-row", style: `--c: ${color(left)}` },
          h("span", { class: "hbar-label", text: w.name }),
          h("span", { class: "hbar-track" }, h("i", { style: `width: ${left}%` })),
          h("b", { text: `${Math.round(left)}%` }),
          reset && h("small", { text: reset }));
      }),
  )));
}

const TASK_COLOR = { todo: "var(--muted)", in_progress: "var(--cyan)", review: "var(--yellow)", done: "var(--green)" };

function dashboardView() {
  setTop({ title: "Dashboard" });
  let tasks = null, limits = null;
  const body = h("div", {}, h("div", { class: "cards" }, skeleton(3)));
  main.append(body);
  const draw = () => {
    const agents = feed.agents;
    if (!agents || !tasks) return;
    const n = (...st) => agents.filter((a) => st.includes(a.status)).length;
    const t = (st) => tasks.filter((x) => x.status === st).length;
    const attention = [
      ...agents.filter((a) => a.status === "blocked").map((a) =>
        h("a", { class: "card agent blocked", href: "#/agent/" + encodeURIComponent(a.id) },
          avatar(a.kind),
          h("div", { class: "card-main" }, h("span", { class: "name", text: a.name }), h("div", { class: "card-meta", text: "Está esperando tu respuesta" })),
          h("span", { class: "chev" }, icon("forward")))),
      ...tasks.filter((x) => x.status === "review").map((x) =>
        h("a", { class: "card task", href: "#/tasks", onclick: () => store.set("pastor-task-filter", "review") },
          h("div", { class: "card-main" }, h("span", { class: "name", text: x.title }),
            h("div", { class: "card-meta" }, h("span", { class: "tag", text: x.id }), h("span", { text: "Por revisar" }))),
          h("span", { class: "chev" }, icon("forward")))),
    ];
    body.replaceChildren(
      panel("bot", "Agentes", h("div", { class: "stats" },
        stat("agentes", agents.length),
        stat("trabajando", n("working", "starting"), "var(--working)"),
        stat("esperándote", n("blocked"), "var(--blocked)"),
        stat("inactivos", n("idle"), "var(--idle)"),
        stat("terminados", n("dead"), "var(--dead)"),
      )),
      panel("tasks", "Tareas", h("div", { class: "stats" },
        TASKS.map(([s, label]) => stat(label.toLowerCase(), t(s), TASK_COLOR[s])),
        stat("completado", `${tasks.length ? Math.floor((t("done") * 100) / tasks.length) : 0}%`, "var(--accent)"),
      )),
      limits && panel("gauge", "Uso restante", limitsView(limits)),
      panel("bell", "Necesita tu atención", attention.length
        ? h("div", { class: "cards" }, attention)
        : h("p", { class: "quiet", text: "Nada pendiente ✓" })),
      panel("bot", "Agentes por tipo", hbars(KINDS.map((k) => [KIND[k].label, agents.filter((a) => a.kind === k).length, KIND[k].color]))),
    );
  };
  poll(10000, "/tasks", (list) => { tasks = list; draw(); });
  poll(30000, "/limits", (list) => { limits = list; draw(); });
  watchAgents(draw);
}

function historyView() {
  setTop({ title: "Historial" });
  const body = h("div", {}, h("div", { class: "cards" }, skeleton(3)));
  main.append(body);
  poll(15000, "/stats", (s) => {
    // Días de calendario (ayer a las 23:00 es "hace 1 día"); con el reloj
    // desfasado podría salir negativo.
    const midnight = (d) => new Date(d.getFullYear(), d.getMonth(), d.getDate());
    const days = s.since ? Math.max(0, Math.round((midnight(new Date()) - midnight(new Date(s.since))) / 86400000)) : 0;
    const when = days === 0 ? "hoy" : days === 1 ? "hace 1 día" : `hace ${days} días`;
    const dash = (v) => (v == null ? "—" : human(v));
    const max = Math.max(1, ...s.daily.map((d) => d.events));
    body.replaceChildren(
      panel("history", s.since ? `Desde el ${new Date(s.since).toLocaleDateString("es")} (${when})` : "Totales", h("div", { class: "stats" },
        stat("sesiones", s.sessions),
        stat("tareas creadas", s.tasks_created),
        stat("tareas hechas", s.tasks_done, "var(--green)"),
        stat("mensajes enviados", s.messages, "var(--cyan)"),
        stat("turnos de Claude", s.turns, KIND.claude.color),
        stat("sesiones por día", (s.sessions / Math.max(1, days)).toFixed(1)),
      )),
      panel("history", "Tiempos", h("div", { class: "stats" },
        stat("duración media de sesión", dash(s.avg_session_secs)),
        stat("tiempo medio por tarea", dash(s.avg_task_secs)),
        stat("tiempo total de agentes", human(s.agent_secs)),
      )),
      panel("history", `Actividad de los últimos ${s.daily.length} días`,
        h("div", { class: "vbars", role: "img", "aria-label": "Eventos por día" }, s.daily.map((d) => h("div", { class: "vbar", title: `${d.day}: ${d.events}` },
          h("b", { text: d.events ? String(d.events) : "" }),
          h("span", { class: "vbar-track" }, h("i", { style: `height: ${(d.events / max) * 100}%` })),
          h("small", { text: d.day.slice(8) }),
        ))),
        h("p", { class: "quiet", text: "Sesiones, tareas, mensajes y turnos por día" }),
      ),
      panel("bot", "Sesiones por tipo", hbars(s.by_kind.map((k) => [KIND[k.kind]?.label || k.kind, k.sessions, KIND[k.kind]?.color || "var(--muted)"]))),
      panel("folder", "Proyectos con más sesiones", s.projects.length
        ? h("div", { class: "projects" }, s.projects.slice(0, 10).map((p) =>
            h("div", { class: "project" }, h("b", { text: String(p.sessions) }), h("span", { class: "ellipsis", text: shortPath(p.path) }))))
        : h("p", { class: "quiet", text: "Aún no hay sesiones registradas" })),
    );
  });
}

// ---------- arranque ----------

const agentsLoop = loop(4000, async () => {
  if (!store.get("pastor-token")) return;
  const version = feed.version;
  const list = await attempt(() => api("GET", "/agents"));
  if (!list || version !== feed.version) return;
  setAgents(list);
  pruneDrafts(list);
});

/** Los borradores de agentes que ya no existen. */
function pruneDrafts(list) {
  const ids = new Set(list.map((a) => "pastor-draft-" + a.id));
  try {
    Object.keys(localStorage)
      .filter((k) => k.startsWith("pastor-draft-") && !ids.has(k))
      .forEach((k) => localStorage.removeItem(k));
  } catch {}
}

if ("serviceWorker" in navigator) navigator.serviceWorker.register("sw.js").catch(() => {});
route();
