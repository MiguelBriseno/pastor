// La PWA de pastor: habla con /api del mismo servidor. Sin framework ni
// build; `h` arma los nodos con textContent para no interpretar como HTML
// nada de lo que llega de los agentes.
"use strict";

const TASK_LABEL = { todo: "pendiente", in_progress: "en curso", review: "por revisar", done: "hecha" };
const KINDS = ["claude", "codex", "opencode"];
const KEYS = [
  ["Enter", "⏎"], ["Escape", "Esc"], ["Up", "↑"], ["Down", "↓"], ["Tab", "Tab"], ["BTab", "⇧Tab"],
  ["1", "1"], ["2", "2"], ["3", "3"], ["y", "y"], ["n", "n"], ["C-c", "^C"],
];

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
    else if (k in el && typeof v !== "string") el[k] = v;
    else el.setAttribute(k, v);
  }
  el.append(...kids.flat().filter((x) => x != null && x !== false));
  return el;
}

let toastTimer;
function toast(msg) {
  const t = document.getElementById("toast");
  t.textContent = msg;
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.hidden = true), 3500);
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

async function api(method, path, body) {
  const res = await fetch("api" + path, {
    method,
    headers: {
      Authorization: "Bearer " + store.get("pastor-token"),
      ...(body ? { "Content-Type": "application/json" } : {}),
    },
    body: body ? JSON.stringify(body) : undefined,
  });
  const data = await res.json().catch(() => ({}));
  if (res.status === 401) throw new Unauthorized(data.error || "token no válido");
  if (!res.ok) throw new Error(data.error || `error ${res.status}`);
  return data;
}

/** Corre `fn` y avisa si falla; sin token, vuelve a pedirlo. */
async function attempt(fn) {
  try {
    return await fn();
  } catch (e) {
    if (e instanceof Unauthorized) {
      // Pueden fallar dos consultas a la vez: solo la primera vuelve al token.
      if (!store.get("pastor-token")) return;
      store.set("pastor-token", "");
      toast(e.message);
      route();
    } else {
      // `fetch` rechaza con TypeError si no hay red, con otro texto en cada navegador.
      toast(e instanceof TypeError ? "sin conexión con pastor" : e.message);
    }
  }
}

function badge(status, label) {
  return h("span", { class: "badge " + status, text: label });
}

// ---------- navegación ----------

const main = document.getElementById("main");
/** Lo que hay que parar al salir de una pantalla (polling). */
let cleanup = [];

/** Llama a `fn` cada `ms` mientras la pantalla siga abierta. Devuelve una
 * función que adelanta la siguiente vuelta sin duplicar el ciclo. */
function every(ms, fn) {
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
  cleanup.push(() => {
    stopped = true;
    clearTimeout(timer);
    document.removeEventListener("visibilitychange", onVisible);
  });
  tick();
  return tick;
}

function route() {
  cleanup.forEach((f) => f());
  cleanup = [];
  main.replaceChildren();
  const hash = location.hash.slice(1) || "/agents";
  const tab = hash.startsWith("/tasks") ? "tasks" : "agents";
  document.querySelectorAll("nav a").forEach((a) => a.classList.toggle("on", a.dataset.tab === tab));
  document.querySelector("nav").hidden = !store.get("pastor-token");
  if (!store.get("pastor-token")) return tokenView();
  const agent = hash.match(/^\/agent\/(.+)$/);
  if (agent) {
    let id;
    try {
      id = decodeURIComponent(agent[1]);
    } catch {
      location.hash = "#/agents";
      return;
    }
    return agentView(id);
  }
  if (tab === "tasks") return tasksView();
  agentsView();
}

window.addEventListener("hashchange", route);

// ---------- token ----------

function tokenView() {
  const input = h("input", { type: "password", autocomplete: "off", placeholder: "token", required: true });
  main.append(
    h("h1", { text: "Conectar con pastor" }),
    h("form", {
      class: "card",
      onsubmit: once(async () => {
        store.set("pastor-token", input.value.trim());
        const ok = await attempt(() => api("GET", "/agents"));
        if (ok) route();
        else if (store.get("pastor-token")) store.set("pastor-token", "");
      }),
    },
      h("label", { text: "Token de pastor serve" }),
      input,
      h("p", { class: "muted", text: "En la máquina: cat ~/.local/share/pastor/serve-token. Se guarda solo en este dispositivo." }),
      h("div", { class: "row" }, h("button", { class: "primary", text: "Entrar" })),
    ),
  );
}

// ---------- agentes ----------

function agentsView() {
  const list = h("div", { class: "list" });
  const refresh = every(3000, () => attempt(async () => {
    const agents = await api("GET", "/agents");
    list.replaceChildren(
      ...(agents.length ? agents.map(agentCard) : [h("p", { class: "empty", text: "No hay agentes." })])
    );
  }));
  const open = h("button", { class: "primary", text: "Nuevo", onclick: () => { open.hidden = true; form.hidden = false; } });
  const form = newAgentForm(() => { form.hidden = true; open.hidden = false; refresh(); });
  form.hidden = true;
  main.append(h("div", { class: "row between" }, h("h1", { text: "Agentes" }), open), form, list);
}

function agentCard(a) {
  return h("a", { class: "card", href: "#/agent/" + encodeURIComponent(a.id) },
    h("div", { class: "row between" }, h("span", { class: "name", text: a.name }), badge(a.status, a.status_label)),
    h("div", { class: "meta", text: `${a.kind} · ${a.cwd}` }),
    a.task && h("div", { class: "meta", text: "tarea " + a.task }),
  );
}

function newAgentForm(done) {
  const kind = h("select", {}, KINDS.map((k) => h("option", { value: k, text: k })));
  const cwd = h("input", { required: true, placeholder: "/home/…/proyecto", value: store.get("pastor-cwd") });
  const name = h("input", { placeholder: "opcional" });
  const prompt = h("textarea", { rows: 2, placeholder: "opcional" });
  const form = h("form", {
    class: "card",
    onsubmit: once(async () => {
      const body = { kind: kind.value, cwd: cwd.value.trim(), name: name.value.trim() || undefined, prompt: prompt.value.trim() || undefined };
      const a = await attempt(() => api("POST", "/agents", body));
      if (!a) return;
      store.set("pastor-cwd", body.cwd);
      toast(`${a.name} creado`);
      name.value = prompt.value = "";
      done();
    }),
  },
    h("label", { text: "Tipo" }), kind,
    h("label", { text: "Directorio (ruta absoluta)" }), cwd,
    h("label", { text: "Nombre" }), name,
    h("label", { text: "Primer mensaje" }), prompt,
    h("div", { class: "row" },
      h("button", { type: "button", text: "Cancelar", onclick: done }),
      h("button", { class: "primary", text: "Crear" }),
    ),
  );
  return form;
}

function agentView(id) {
  const title = h("span", { class: "name", text: id });
  const status = h("span");
  const meta = h("div", { class: "meta muted" });
  const screen = h("pre", { class: "screen", text: "…" });
  const note = h("p", { class: "muted", hidden: true });
  const text = h("textarea", { rows: 2, placeholder: "Mensaje para el agente" });

  every(5000, () => attempt(async () => {
    const a = (await api("GET", "/agents")).find((x) => x.id === id);
    if (!a) {
      toast("ese agente ya no existe");
      location.hash = "#/agents";
      return;
    }
    title.textContent = a.name;
    status.replaceChildren(badge(a.status, a.status_label));
    meta.textContent = `${a.kind} · ${a.cwd}` + (a.task ? ` · tarea ${a.task}` : "");
  }));

  const pollScreen = every(1500, async () => {
    try {
      const { text: t } = await api("GET", `/agents/${encodeURIComponent(id)}/screen`);
      note.hidden = true;
      if (screen.textContent === t) return;
      // Solo baja si ya estaba abajo: si subiste a leer, no te mueve.
      const atBottom = screen.scrollHeight - screen.scrollTop - screen.clientHeight < 40;
      screen.textContent = t;
      if (atBottom) screen.scrollTop = screen.scrollHeight;
    } catch (e) {
      if (e instanceof Unauthorized) return attempt(() => { throw e; });
      // Debajo, sin tapar lo último que se vio: suele ser lo que había que leer.
      note.textContent = e instanceof TypeError ? "sin conexión con pastor" : e.message;
      note.hidden = false;
    }
  });

  // En fila: dos toques seguidos irían en paralelo y tmux podría recibirlos
  // al revés, o Escape y la siguiente tecla como una sola (Alt+tecla).
  let queue = Promise.resolve();
  const post = (path, body) => (queue = queue.then(() => attempt(async () => {
    await api("POST", `/agents/${encodeURIComponent(id)}${path}`, body);
    setTimeout(pollScreen, 300);
    return true;
  })));

  const keys = KEYS.map(([key, label]) => h("button", {
    type: "button",
    text: label,
    onclick: () => {
      if (key === "C-c" && !confirm("¿Mandar Ctrl-C? Puede terminar al agente.")) return;
      post("/keys", { keys: [key] });
    },
  }));

  main.append(
    h("div", { class: "agent-head" },
      h("div", { class: "row between" }, h("a", { href: "#/agents", text: "← Agentes" }), status),
      title, meta,
    ),
    screen,
    note,
    h("div", { class: "keys" }, keys),
    h("form", {
      class: "send",
      onsubmit: once(async () => {
        if (!text.value.trim()) return;
        if (await post("/send", { text: text.value })) text.value = "";
      }),
    }, text, h("button", { class: "primary", text: "Enviar" })),
    h("div", { class: "row", style: "margin-top:24px;justify-content:flex-end" },
      h("button", {
        class: "danger",
        text: "Terminar agente",
        onclick: async () => {
          if (!confirm("¿Terminar este agente? Se cierra su sesión de tmux.")) return;
          if (await attempt(() => api("DELETE", `/agents/${encodeURIComponent(id)}`))) location.hash = "#/agents";
        },
      }),
    ),
  );
  requestAnimationFrame(() => (screen.scrollTop = screen.scrollHeight));
}

// ---------- tareas ----------

function tasksView() {
  const list = h("div");
  const load = async () => {
    const [tasks, agents] = await Promise.all([api("GET", "/tasks"), api("GET", "/agents")]);
    const groups = Object.keys(TASK_LABEL).map((st) => [st, tasks.filter((t) => t.status === st)]).filter(([, ts]) => ts.length);
    list.replaceChildren(
      ...(groups.length
        ? groups.map(([st, ts]) => [h("h2", { text: TASK_LABEL[st] }), h("div", { class: "list" }, ts.map((t) => taskCard(t, agents, reload)))])
        : [h("p", { class: "empty", text: "No hay tareas." })]).flat()
    );
  };
  const reload = () => attempt(load);
  reload();

  const title = h("input", { required: true, placeholder: "Título" });
  const desc = h("textarea", { rows: 2, placeholder: "Descripción (opcional)" });
  main.append(
    h("h1", { text: "Tareas" }),
    h("form", {
      class: "card",
      onsubmit: once(async () => {
        if (await attempt(() => api("POST", "/tasks", { title: title.value.trim(), description: desc.value.trim() }))) {
          title.value = desc.value = "";
          reload();
        }
      }),
    }, title, h("div", { style: "height:8px" }), desc, h("div", { class: "row" }, h("button", { class: "primary", text: "Agregar" }))),
    list,
  );
}

function taskCard(t, agents, reload) {
  const card = h("div", { class: "card" },
    h("div", { class: "name", text: `${t.id} · ${t.title}` }),
    t.description && h("div", { class: "meta", text: t.description }),
    t.agent && h("div", { class: "meta", text: "agente " + (agents.find((a) => a.id === t.agent)?.name || t.agent) }),
  );
  const act = (method, path) => attempt(async () => { await api(method, `/tasks/${encodeURIComponent(t.id)}${path}`); reload(); });
  const actions = h("div", { class: "row wrap", style: "margin-top:8px" },
    t.status !== "done" && h("button", { text: "Asignar", onclick: () => { actions.hidden = true; card.append(assignForm(t, agents, reload, () => { actions.hidden = false; })); } }),
    t.status !== "done" && h("button", { text: "Hecha", onclick: () => act("POST", "/done") }),
    h("button", { class: "danger", text: "Borrar", onclick: () => confirm(`¿Borrar «${t.title}»?`) && act("DELETE", "") }),
  );
  card.append(actions);
  return card;
}

function assignForm(t, agents, reload, cancel) {
  const target = h("select", {},
    agents.map((a) => h("option", { value: "agent:" + a.id, text: `${a.name} (${a.status_label})` })),
    KINDS.map((k) => h("option", { value: "new:" + k, text: `Nuevo ${k}` })),
  );
  const cwd = h("input", { placeholder: "/home/…/proyecto", value: store.get("pastor-cwd") });
  const cwdWrap = h("div", {}, h("label", { text: "Directorio (ruta absoluta)" }), cwd);
  const sync = () => (cwdWrap.hidden = !target.value.startsWith("new:"));
  target.addEventListener("change", sync);
  sync();
  const form = h("form", {
    onsubmit: once(async () => {
      const [what, val] = target.value.split(/:(.*)/);
      const body = what === "agent" ? { agent: val } : { kind: val, cwd: cwd.value.trim() };
      if (await attempt(() => api("POST", `/tasks/${encodeURIComponent(t.id)}/assign`, body))) {
        if (body.cwd) store.set("pastor-cwd", body.cwd);
        toast("tarea asignada");
        reload();
      }
    }),
  },
    h("label", { text: "Asignar a" }), target, cwdWrap,
    h("div", { class: "row", style: "margin-top:12px;justify-content:flex-end" },
      h("button", { type: "button", text: "Cancelar", onclick: () => { form.remove(); cancel(); } }),
      h("button", { class: "primary", text: "Asignar" }),
    ),
  );
  return form;
}

// ---------- arranque ----------

if ("serviceWorker" in navigator) navigator.serviceWorker.register("sw.js").catch(() => {});
route();
