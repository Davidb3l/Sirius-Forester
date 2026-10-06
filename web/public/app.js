// Sirius Console frontend — vanilla, zero deps. Fetches JSON endpoints, renders
// the four views, and re-fetches the active view when SSE reports a ledger
// data_version bump (the Ametrite live-update pattern).

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];

const state = {
  view: "fleet",
  dataVersion: null,
  // Which fleet this tab watches (?ws=<alias>); resolved in init().
  ws: new URL(location.href).searchParams.get("ws"),
  fleets: [],
  es: null,
  // Bumped on every fleet switch: a response for the fleet you just left
  // must never render under the new one's name.
  gen: 0,
};

// The chosen fleet persists per browser, like Ametrite's workspace switcher.
const WS_KEY = "sirius-ws";
const storedWs = () => {
  try {
    return localStorage.getItem(WS_KEY);
  } catch {
    return null;
  }
};
const storeWs = (alias) => {
  try {
    localStorage.setItem(WS_KEY, alias);
  } catch {
    // private window / blocked storage: the URL still carries the choice
  }
};
/** Scope an API/SSE URL to the current fleet. */
const withWs = (url) =>
  state.ws
    ? `${url}${url.includes("?") ? "&" : "?"}ws=${encodeURIComponent(state.ws)}`
    : url;

// ---- small html helpers ----
const esc = (s) =>
  String(s ?? "").replace(
    /[&<>"']/g,
    (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[
        c
      ],
  );
const rel = (iso) => {
  if (!iso) return "—";
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return esc(iso);
  const s = Math.round((Date.now() - t) / 1000);
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.round(s / 60)}m ago`;
  if (s < 86400) return `${Math.round(s / 3600)}h ago`;
  return `${Math.round(s / 86400)}d ago`;
};
const dur = (ms) => {
  if (ms == null) return "—";
  if (ms < 1000) return `${ms}ms`;
  if (ms < 60000) return `${(ms / 1000).toFixed(1)}s`;
  return `${Math.floor(ms / 60000)}m${Math.round((ms % 60000) / 1000)}s`;
};

async function getJSON(url) {
  const gen = state.gen;
  const r = await fetch(withWs(url), { headers: { accept: "application/json" } });
  // Switched fleets while this was in flight: drop it (never resolves, so the
  // stale render simply stops; the new fleet's render is already underway).
  if (gen !== state.gen) return new Promise(() => {});
  if (!r.ok) throw new Error(`${url} → ${r.status}`);
  return r.json();
}

// ---- views ------------------------------------------------------------------

async function renderFleet() {
  const el = $("#view-fleet");
  let d;
  try {
    d = await getJSON("/api/fleet");
  } catch (e) {
    el.innerHTML = notice(`Could not load fleet: ${esc(e.message)}`);
    return;
  }
  $("#dataver").textContent = `v${d.dataVersion}`;
  setLedgerNote(d);
  if (!d.ledgerAvailable) {
    el.innerHTML = notice(
      "No ledger yet. Run <code>sirius init</code> to create <code>.sirius/sirius.db</code>.",
    );
    return;
  }
  if (!d.workers.length) {
    el.innerHTML =
      head("Fleet", "0 workers") +
      notice("No workers registered. Start one with <code>sirius run</code>.");
    return;
  }
  const cards = d.workers.map(workerCard).join("");
  el.innerHTML =
    head("Fleet", `${d.workers.length} worker${d.workers.length === 1 ? "" : "s"}`) +
    `<div class="fleet-grid">${cards}</div>`;
}

function workerCard(w) {
  const st = w.status || "idle";
  const treeMatch = /^([^/]+)\/(.+)$/.exec(w.id || "");
  const name = treeMatch
    ? `${esc(treeMatch[1])}/<span class="tree">${esc(treeMatch[2])}</span>`
    : esc(w.id);

  const issue = w.issueRef
    ? `<div class="wc-issue"><span class="ref">${esc(w.issueRef)}</span>` +
      (w.issueTitle
        ? `<span class="title">${esc(w.issueTitle)}</span>`
        : "") +
      `</div>`
    : `<div class="wc-empty">${st === "idle" ? "idle — no active issue" : "no active iteration"}</div>`;

  const entities = w.entities.length
    ? `<div class="wc-row"><span class="wc-label">holds</span><div class="chips">${w.entities
        .map((e) => `<span class="chip">${esc(e)}</span>`)
        .join("")}</div></div>`
    : "";

  const verdicts = w.verdicts.length
    ? `<div class="wc-row"><span class="wc-label">oracle</span><div class="chips">${w.verdicts
        .map((v) => `<span class="verdict v-${esc(v)}">${esc(v)}</span>`)
        .join("")}</div></div>`
    : "";

  const gate = w.gateResult
    ? `<span class="gate gate-${esc(w.gateResult)}">gate ${esc(w.gateResult)}</span>`
    : `<span class="gate gate-none">gate —</span>`;

  const receipt = w.receipt
    ? `<a class="receipt-flag ${w.receipt.twoWay ? "twoway" : "oneway"}" data-receipt="${w.receipt.id}">` +
      `receipt #${w.receipt.id} ${w.receipt.twoWay ? "✓✓ two-way" : "◐ partial"}</a>`
    : `<span class="faint mono">no receipt</span>`;

  return `<article class="worker-card st-${esc(st)}">
    <div class="wc-head">
      <span class="wc-name">${name}</span>
      <span class="badge ${esc(st)}">${esc(st)}</span>
    </div>
    ${issue}
    ${entities}
    ${verdicts}
    <div class="wc-row" style="margin-top:11px">
      ${gate}
      ${receipt}
    </div>
    <div class="wc-row"><span class="wc-label">seen</span><span class="faint mono">${rel(
      w.lastSeenAt,
    )}</span></div>
  </article>`;
}

async function renderHistory() {
  const el = $("#view-history");
  let d;
  try {
    d = await getJSON("/api/history");
  } catch (e) {
    el.innerHTML = notice(`Could not load history: ${esc(e.message)}`);
    return;
  }
  if (!d.ledgerAvailable) {
    el.innerHTML = notice("No ledger yet.");
    return;
  }
  const s = d.stats;
  const stat = (num, lbl, cls = "") =>
    `<div class="stat"><div class="num ${cls}">${num}</div><div class="lbl">${lbl}</div></div>`;

  const stats =
    stat(s.completed, "completed", "good") +
    stat(
      s.throughputPerHour != null ? s.throughputPerHour : "—",
      "throughput / hr",
    ) +
    stat(s.medianCycleMs != null ? dur(s.medianCycleMs) : "—", "median cycle") +
    stat(s.avgCycleMs != null ? dur(s.avgCycleMs) : "—", "avg cycle") +
    stat(
      s.gateEscapeAttempts,
      "gate escape attempts",
      s.gateEscapeAttempts ? "bad" : "",
    ) +
    stat(
      s.collisionNearMisses,
      "collision near-misses",
      s.collisionNearMisses ? "warn" : "",
    ) +
    stat(s.twoWayReceipts + "/" + s.receiptsFiled, "two-way receipts") +
    stat(s.released + s.deadends, "released / deadend", s.deadends ? "warn" : "") +
    stat(s.tokensTotal.toLocaleString(), "tokens");

  const rows = d.recent
    .map(
      (i) => `<tr>
      <td class="mono faint">${i.id}</td>
      <td class="mono">${esc(i.worker)}</td>
      <td class="ref-blue">${esc(i.issue || "—")}</td>
      <td>${outcomeBadge(i.outcome)}</td>
      <td>${i.gate ? `<span class="gate gate-${esc(i.gate)}">${esc(i.gate)}</span>` : '<span class="faint">—</span>'}</td>
      <td class="mono dim">${esc(i.entities.join(", ") || "—")}</td>
      <td class="mono faint">${dur(i.durationMs)}</td>
      <td class="mono faint">${rel(i.startedAt)}</td>
    </tr>`,
    )
    .join("");

  const policy = d.policyEvents.length
    ? `<div class="section-head" style="margin-top:26px"><h2>Policy events</h2><span class="count">${d.policyEvents.length}</span></div>
       <div class="table-wrap"><table><thead><tr><th>id</th><th>kind</th><th>iter</th><th>detail</th><th>when</th></tr></thead><tbody>${d.policyEvents
         .map(
           (p) => `<tr><td class="mono faint">${p.id}</td><td class="mono">${esc(
             p.kind,
           )}</td><td class="mono faint">${p.iterationId ?? "—"}</td><td class="mono dim">${esc(
             typeof p.detail === "string" ? p.detail : JSON.stringify(p.detail ?? {}),
           )}</td><td class="mono faint">${rel(p.createdAt)}</td></tr>`,
         )
         .join("")}</tbody></table></div>`
    : "";

  el.innerHTML =
    head("History", `${s.totalIterations} iterations`) +
    `<div class="stat-grid">${stats}</div>` +
    `<div class="section-head"><h2>Recent iterations</h2><span class="count">${d.recent.length}</span></div>` +
    `<div class="table-wrap"><table><thead><tr><th>#</th><th>worker</th><th>issue</th><th>outcome</th><th>gate</th><th>entities</th><th>dur</th><th>started</th></tr></thead><tbody>${
      rows || `<tr><td colspan="8" class="faint">no iterations yet</td></tr>`
    }</tbody></table></div>` +
    policy;
}

function outcomeBadge(o) {
  if (!o) return '<span class="faint">running</span>';
  const map = {
    completed: "good",
    released: "warn",
    deadend: "warn",
    gate_failed: "bad",
    error: "bad",
  };
  const cls = map[o] || "";
  return `<span class="gate gate-${cls === "good" ? "pass" : cls === "bad" ? "fail" : "skipped"}">${esc(o)}</span>`;
}

async function renderReceipts() {
  const el = $("#view-receipts");
  let d;
  try {
    d = await getJSON("/api/receipts");
  } catch (e) {
    el.innerHTML = notice(`Could not load receipts: ${esc(e.message)}`);
    return;
  }
  if (!d.ledgerAvailable) {
    el.innerHTML = notice("No ledger yet.");
    return;
  }
  const rows = d.receipts
    .map(
      (r) => `<tr class="clickable" data-receipt="${r.id}">
      <td class="mono faint">#${r.id}</td>
      <td><span class="badge ${r.kind === "decision" ? "working" : "idle"}">${esc(r.kind)}</span></td>
      <td class="ref-blue">${esc(r.ref)}</td>
      <td class="mono dim">${esc(r.symbols.slice(0, 4).join(", "))}${r.symbols.length > 4 ? ` +${r.symbols.length - 4}` : ""}</td>
      <td>${r.twoWay ? '<span class="twoway mono">✓✓ two-way</span>' : `<span class="oneway mono">${r.forwardOk ? "fwd" : ""}${r.forwardOk && r.reverseOk ? "+" : ""}${r.reverseOk ? "rev" : !r.forwardOk && !r.reverseOk ? "none" : ""}</span>`}</td>
      <td class="mono faint">${esc(r.worker || "—")}</td>
      <td class="mono faint">${rel(r.createdAt)}</td>
    </tr>`,
    )
    .join("");
  el.innerHTML =
    head("Receipts", `${d.receipts.length}`) +
    `<div class="table-wrap"><table><thead><tr><th>id</th><th>kind</th><th>ref</th><th>symbols</th><th>provenance</th><th>worker</th><th>filed</th></tr></thead><tbody>${
      rows || `<tr><td colspan="7" class="faint">no receipts filed yet</td></tr>`
    }</tbody></table></div>`;
}

async function renderConfig() {
  const el = $("#view-config");
  let d;
  try {
    d = await getJSON("/api/config");
  } catch (e) {
    el.innerHTML = notice(`Could not load config: ${esc(e.message)}`);
    return;
  }
  const rawKeys = d.raw ? new Set(Object.keys(d.raw)) : new Set();
  const fmt = (v) =>
    typeof v === "object" ? JSON.stringify(v) : String(v);
  const rows = Object.entries(d.config)
    .map(([k, v]) => {
      const fromFile = rawKeys.has(k);
      return `<dt>${esc(k)}</dt><dd><code>${esc(fmt(v))}</code>${
        fromFile ? "" : '<span class="def">default</span>'
      }</dd>`;
    })
    .join("");
  const srcPill = d.present
    ? `<span class="pill-src pill-file">from ${esc(d.path)}</span>`
    : `<span class="pill-src pill-default">no config file — committed defaults</span>`;
  const err = d.error
    ? `<div class="notice" style="margin-bottom:14px;color:var(--red)">config parse error: ${esc(d.error)} (showing defaults)</div>`
    : "";
  el.innerHTML =
    head("Config", ".sirius/config.json") +
    err +
    `<div class="config-card">${srcPill}
       <div class="config-note">read-only · edit the file, not the console (PRD §2.2)</div>
       <dl class="kv">${rows}</dl></div>`;
}

// ---- receipt drawer ---------------------------------------------------------

async function openReceipt(id) {
  const drawer = $("#drawer");
  const body = $("#drawer-body");
  drawer.hidden = false;
  body.innerHTML = `<div class="d-loading">loading receipt #${esc(id)}…</div>`;
  let d;
  try {
    d = await getJSON(`/api/receipt/${id}`);
  } catch (e) {
    body.innerHTML = notice(`Could not load receipt: ${esc(e.message)}`);
    return;
  }
  if (!d || !d.receipt) {
    body.innerHTML = notice("Receipt not found.");
    return;
  }
  const r = d.receipt;
  const why = d.why; // may be null if sirius binary absent
  const iters = d.iterations || [];

  const symbolList = r.symbols.length
    ? `<div class="chips">${r.symbols.map((s) => `<span class="chip">${esc(s)}</span>`).join("")}</div>`
    : '<span class="faint">no symbols</span>';

  const iterBlock = iters.length
    ? `<div class="d-block"><h4>Iterations that filed this</h4>${iters
        .map(
          (i) =>
            `<p><span class="mono">#${i.id}</span> · <span class="mono">${esc(i.worker)}</span> · issue <span class="ref-blue">${esc(i.issue || "—")}</span> · ${outcomeBadge(i.outcome)} · gate ${esc(i.gate || "—")}</p>`,
        )
        .join("")}</div>`
    : "";

  let whyBlock = "";
  if (why && why.error) {
    whyBlock = `<div class="d-block"><h4>sirius why</h4><p class="faint">${esc(why.error)}</p></div>`;
  } else if (why && r.kind === "issue" && why.decisions) {
    whyBlock = `<div class="d-block"><h4>sirius why ${esc(r.ref)}</h4>
      <p class="faint">decisions: ${esc((why.decisions || []).join(", ") || "none")}</p>
      <p class="faint">symbols: ${esc((why.symbols || []).join(", ") || "none")}</p></div>`;
  }

  const decisionBlock =
    r.kind === "decision"
      ? `<div class="d-block"><h4>Decision</h4><p>This receipt stamps decision <span class="ref-blue">${esc(r.ref)}</span> onto the symbols below.</p></div>`
      : `<div class="d-block"><h4>Issue</h4><p>Issue <span class="ref-blue">${esc(r.ref)}</span> — the symbols below were stamped into its activity.</p></div>`;

  body.innerHTML = `
    <h3>Receipt #${esc(r.id)}</h3>
    <div class="sub">${esc(r.kind)} · ${esc(r.ref)} · filed ${rel(r.createdAt)} ${r.worker ? "· " + esc(r.worker) : ""}</div>
    <div class="d-block"><h4>Provenance</h4>
      <p>forward (amt comment): <span class="${r.forwardOk ? "twoway" : "oneway"} mono">${r.forwardOk ? "✓ landed" : "✕ missing"}</span></p>
      <p>reverse (hayven remember): <span class="${r.reverseOk ? "twoway" : "oneway"} mono">${r.reverseOk ? "✓ landed" : "✕ missing"}</span></p>
    </div>
    ${decisionBlock}
    <div class="d-block"><h4>Symbols stamped (${r.symbols.length})</h4>${symbolList}</div>
    ${iterBlock}
    ${whyBlock}
  `;
}

function closeDrawer() {
  $("#drawer").hidden = true;
  $("#drawer-body").innerHTML = "";
}

// ---- chrome helpers ---------------------------------------------------------

function head(title, count) {
  return `<div class="section-head"><h2>${esc(title)}</h2><span class="count">${esc(count)}</span></div>`;
}
function notice(html) {
  return `<div class="notice">${html}</div>`;
}
function setLedgerNote(d) {
  const note = $("#ledger-note");
  if (!d.ledgerAvailable) {
    note.textContent = "ledger: not found";
    return;
  }
  const v = d.meta && d.meta.sirius_version ? ` · sirius ${d.meta.sirius_version}` : "";
  note.textContent = `ledger: schema v${(d.meta && d.meta.schema_version) || "?"}${v}`;
}

const RENDER = {
  fleet: renderFleet,
  history: renderHistory,
  receipts: renderReceipts,
  config: renderConfig,
};

async function renderActive() {
  await RENDER[state.view]();
}

function switchView(view) {
  state.view = view;
  $$(".tab").forEach((t) => t.classList.toggle("active", t.dataset.view === view));
  $$(".view").forEach((v) =>
    v.classList.toggle("active", v.id === `view-${view}`),
  );
  renderActive();
}

// ---- SSE live updates -------------------------------------------------------

function connectSSE() {
  const conn = $("#conn");
  const setConn = (cls, label) => {
    conn.className = `conn ${cls}`;
    $(".conn-label", conn).textContent = label;
  };
  if (state.es) state.es.close();
  const es = new EventSource(withWs("/events"));
  state.es = es;
  es.addEventListener("open", () => setConn("conn-live", "live"));
  es.addEventListener("version", (e) => {
    const v = Number(e.data);
    if (state.dataVersion === null) {
      state.dataVersion = v;
      return;
    }
    if (v !== state.dataVersion) {
      state.dataVersion = v;
      renderActive(); // ledger changed → re-render the visible view
    }
  });
  es.addEventListener("error", () => {
    setConn("conn-down", "reconnecting");
    // EventSource auto-reconnects; nothing else to do
  });
}

// ---- wiring -----------------------------------------------------------------

// ---- fleet switcher -----------------------------------------------------------

const currentFleet = () => state.fleets.find((f) => f.alias === state.ws);

/** Header label + tab title name the fleet being watched and the real port. */
function labelWorkspace() {
  const port = location.port || (location.protocol === "https:" ? "443" : "80");
  const f = currentFleet();
  $("#brand-sub").textContent = `fleet console · α CMa · :${port}`;
  $("#ws-current").textContent = f ? f.name : state.ws || "fleet";
  $("#ws-dot").classList.toggle("running", Boolean(f && f.running));
  $("#ws-button").title = f ? `${f.root} — switch fleet` : "Switch fleet";
  if (f) document.title = `${f.name} — Sirius Fleet Console`;
}

function fleetState(f) {
  if (f.running) return `running · ${f.working} working`;
  if (!f.ledgerAvailable) return "no ledger";
  return "idle";
}

function renderSwitcher({ menu: rebuildMenu = true } = {}) {
  if (!rebuildMenu) {
    labelWorkspace();
    return;
  }
  const menu = $("#ws-menu");
  const running = state.fleets.filter((f) => f.running);
  const idle = state.fleets.filter((f) => !f.running);
  const item = (f) => `
    <button class="ws-item" role="option" type="button" data-ws="${esc(f.alias)}"
      aria-selected="${f.alias === state.ws}" title="${esc(f.root)}">
      <span class="ws-dot${f.running ? " running" : ""}"></span>
      <span class="ws-name">${esc(f.name)}</span>
      <span class="ws-state${f.running ? " running" : ""}">${esc(fleetState(f))}</span>
    </button>`;
  menu.innerHTML =
    (running.length
      ? `<div class="ws-menu-heading">Running · ${running.length}</div>${running.map(item).join("")}`
      : "") +
    (idle.length
      ? `<div class="ws-menu-heading">Other fleets · ${idle.length}</div>${idle.map(item).join("")}`
      : "");
  labelWorkspace();
}

/** Refresh the fleet list; on first load, settle which fleet to show. */
async function loadFleets(first = false, { menu = true } = {}) {
  let d;
  try {
    const r = await fetch("/api/workspaces", { headers: { accept: "application/json" } });
    if (!r.ok) throw new Error(String(r.status));
    d = await r.json();
  } catch {
    if (first) labelWorkspace(); // an older server: single-workspace mode
    return;
  }
  state.fleets = d.workspaces || [];
  if (first) {
    const known = (a) => a && state.fleets.some((f) => f.alias === a);
    // URL choice, else the remembered one, else a RUNNING fleet, else the
    // repo the console was launched in.
    const firstRunning = state.fleets.find((f) => f.running);
    const home = state.fleets.find((f) => f.alias === d.default);
    const firstWithLedger = state.fleets.find((f) => f.ledgerAvailable);
    state.ws = known(state.ws)
      ? state.ws
      : known(storedWs())
        ? storedWs()
        : firstRunning
          ? firstRunning.alias
          : home && !home.ledgerAvailable && firstWithLedger
            ? firstWithLedger.alias
            : d.default;
    syncUrl();
  }
  // Never rebuild an OPEN menu from a timed refresh: it would steal focus
  // and drop a click that straddles the re-render.
  renderSwitcher({ menu: menu && $("#ws-menu").hidden });
}

function syncUrl() {
  const url = new URL(location.href);
  if (state.ws) url.searchParams.set("ws", state.ws);
  history.replaceState(null, "", url);
}

function selectFleet(alias) {
  closeMenu(true);
  if (alias === state.ws) return;
  state.gen++;
  closeDrawer(); // a receipt from the fleet you left is not this fleet's
  state.ws = alias;
  storeWs(alias);
  syncUrl();
  state.dataVersion = null;
  renderSwitcher();
  connectSSE();
  renderActive();
}

function openMenu() {
  renderSwitcher(); // build from what we know, then show it
  $("#ws-menu").hidden = false;
  $("#ws-button").setAttribute("aria-expanded", "true");
}
function closeMenu(refocus = false) {
  const wasOpen = !$("#ws-menu").hidden;
  $("#ws-menu").hidden = true;
  $("#ws-button").setAttribute("aria-expanded", "false");
  if (wasOpen && refocus) $("#ws-button").focus();
}

async function init() {
  $("#ws-button").addEventListener("click", (e) => {
    e.stopPropagation();
    if ($("#ws-menu").hidden) openMenu();
    else closeMenu();
  });
  $("#ws-menu").addEventListener("click", (e) => {
    const b = e.target.closest("[data-ws]");
    if (b) selectFleet(b.dataset.ws);
  });
  document.addEventListener("click", (e) => {
    if (!e.target.closest("#ws-switch")) closeMenu();
  });
  await loadFleets(true);
  // Running dots stay current (an open menu is left alone until it closes).
  setInterval(() => loadFleets(false, { menu: true }), 15_000);
  $$(".tab").forEach((t) =>
    t.addEventListener("click", () => switchView(t.dataset.view)),
  );
  document.addEventListener("click", (e) => {
    const flag = e.target.closest("[data-receipt]");
    if (flag) {
      e.preventDefault();
      openReceipt(flag.dataset.receipt);
      return;
    }
    if (e.target.closest("[data-close]")) closeDrawer();
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      closeDrawer();
      closeMenu(true);
    }
  });
  switchView("fleet");
  connectSSE();
}

init();
