// GET /admin: a static page that reads /v1/admin/stats. Behind Cloudflare Access (coordinator
// README, "Cloudflare Access": Access protects this page only) the browser's Access session cookie
// (CF_Authorization) is enough for its API calls too: the page asks once without a token and, if
// the coordinator answers, hides the token box. Every call sends `x-farm-admin: 1` (the CSRF check
// for cookie-authenticated requests). Otherwise it falls back to the admin token, kept for this
// browser tab only (sessionStorage). It shows nothing without one of the two.

export const ADMIN_PAGE = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>zero-farm admin</title>
<style>
  :root { --bg: #fff; --fg: #1a1a1a; --muted: #666; --line: #ddd; --bad: #b00020; --ok: #1b7a2f; --warn: #a15c00; --head: #f4f4f4; }
  @media (prefers-color-scheme: dark) {
    :root { --bg: #141414; --fg: #e8e8e8; --muted: #999; --line: #333; --bad: #ff6b6b; --ok: #6bd47f; --warn: #ffb547; --head: #1f1f1f; }
  }
  body { background: var(--bg); color: var(--fg); font: 14px/1.4 system-ui, sans-serif; margin: 0; padding: 16px; }
  h1 { font-size: 18px; margin: 0 0 12px; }
  h2 { font-size: 15px; margin: 24px 0 8px; }
  .bar { display: flex; flex-wrap: wrap; gap: 8px; align-items: center; }
  input { font: inherit; padding: 4px 6px; background: var(--bg); color: var(--fg); border: 1px solid var(--line); min-width: 220px; }
  button { font: inherit; padding: 4px 10px; }
  .wrap { overflow-x: auto; }
  table { border-collapse: collapse; width: 100%; font-variant-numeric: tabular-nums; }
  th, td { border-bottom: 1px solid var(--line); padding: 4px 8px; text-align: left; white-space: nowrap; }
  th { background: var(--head); font-weight: 600; }
  td.n { text-align: right; }
  td.wrapok { white-space: normal; min-width: 220px; }
  .muted { color: var(--muted); }
  .bad { color: var(--bad); }
  .ok { color: var(--ok); }
  .warn { color: var(--warn); }
  #err { color: var(--bad); margin: 8px 0; }
  .tiles { display: flex; flex-wrap: wrap; gap: 16px; }
  .tile { border: 1px solid var(--line); padding: 8px 12px; min-width: 140px; }
  .tile b { display: block; font-size: 20px; }
  .tile small { display: block; color: var(--muted); }
  code { font-size: 12px; }
  [hidden] { display: none !important; }
</style>
</head>
<body>
<h1>zero-farm</h1>
<div class="bar">
  <span id="tokenbox" hidden>
    <input id="token" type="password" placeholder="admin token" autocomplete="off">
    <button id="save">Load</button>
  </span>
  <span id="who" class="muted"></span>
  <label><input id="auto" type="checkbox" checked style="min-width:0"> refresh every 30 s</label>
  <span id="when" class="muted"></span>
</div>
<div id="err"></div>
<div id="out"></div>
<script>
const $ = (id) => document.getElementById(id);
const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
const num = (x) => (x === null || x === undefined ? "" : Number(x).toLocaleString());
const hrs = (s) => (s ? (s / 3600).toFixed(1) : "0");
function ago(t, now) {
  if (!t) return "never";
  const d = now - t;
  if (d < 90) return d + " s ago";
  if (d < 5400) return Math.round(d / 60) + " min ago";
  if (d < 172800) return Math.round(d / 3600) + " h ago";
  return Math.round(d / 86400) + " d ago";
}
function when(t) { return t ? new Date(t * 1000).toLocaleString() : ""; }
function table(cols, rows) {
  if (!rows.length) return '<p class="muted">none</p>';
  const h = cols.map((c) => "<th>" + esc(c[0]) + "</th>").join("");
  const b = rows.map((r) => "<tr>" + cols.map((c) => {
    const v = c[1](r);
    return '<td class="' + (c[2] || "") + '">' + v + "</td>";
  }).join("") + "</tr>").join("");
  return '<div class="wrap"><table><thead><tr>' + h + "</tr></thead><tbody>" + b + "</tbody></table></div>";
}

// Auth: a Cloudflare Access login (no token needed) or the admin token, this tab only.
let viaAccess = false;
function getToken() { try { return sessionStorage.getItem("farm_admin_token") || ""; } catch { return ""; } }
function setToken(t) { try { sessionStorage.setItem("farm_admin_token", t); } catch {} }
try { localStorage.removeItem("farm_admin_token"); } catch {} // older pages kept it for good
function authHeaders() {
  const t = $("token").value.trim();
  return t ? { "x-farm-admin": "1", authorization: "Bearer " + t } : { "x-farm-admin": "1" };
}
function showTokenBox(msg) {
  viaAccess = false;
  $("tokenbox").hidden = false;
  $("who").textContent = "";
  $("err").textContent = msg || "";
}

async function load() {
  const token = $("token").value.trim();
  if (token) setToken(token);
  let s;
  try {
    // redirect "manual": an expired Access session answers with a redirect to the login page.
    const r = await fetch("/v1/admin/stats", { headers: authHeaders(), redirect: "manual", credentials: "same-origin" });
    if (r.type === "opaqueredirect" || r.status === 0) {
      showTokenBox("Your Cloudflare Access login has expired: reload the page to sign in again (or use the admin token).");
      return;
    }
    if (r.status === 401 || r.status === 403) {
      showTokenBox(token ? "stats: HTTP " + r.status + " (wrong admin token?)" : "Sign in through Cloudflare Access, or enter the admin token.");
      return;
    }
    if (!r.ok) { $("err").textContent = "stats: HTTP " + r.status + " " + (await r.text()).slice(0, 200); return; }
    s = await r.json();
  } catch (e) { $("err").textContent = "stats: " + e; return; }
  $("err").textContent = "";
  viaAccess = !!(s.auth && s.auth.via === "access");
  $("tokenbox").hidden = viaAccess;
  $("who").textContent = viaAccess ? "signed in through Cloudflare Access" + (s.auth.email || s.auth.common_name ? " as " + (s.auth.email || s.auth.common_name) : "") : "";
  $("when").textContent = "as of " + new Date(s.t * 1000).toLocaleTimeString();
  render(s);
}

function render(s) {
  const now = s.t;
  const online = s.machines.filter((m) => !m.revoked && m.last_seen && now - m.last_seen < 600).length;
  const working = s.machines.filter((m) => m.active_leases > 0).length;
  const own = (s.contributors || []).filter((c) => c.origin === "own");
  const ownOn = own.filter((c) => c.status !== "offline").length;
  const T = s.totals || { farm: {}, own: {}, all: {} };
  const tw = (o, k) => (o && o[k] ? num(o[k].h1) + " / " + num(o[k].h24) : "0 / 0");
  const tri = (k) => '<b>' + tw(T.all, k) + "</b><small>farm " + tw(T.farm, k) + " &middot; own " + tw(T.own, k) + "</small>";
  let h = '<div class="tiles">';
  h += '<div class="tile">farm<b class="' + (s.paused ? "warn" : "ok") + '">' + (s.paused ? "paused" : "running") + "</b></div>";
  h += '<div class="tile">farm PCs online / working<b>' + online + " / " + working + "</b><small>own machines online: " + ownOn + " of " + own.length + "</small></div>";
  h += '<div class="tile">self-play games 1 h / 24 h' + tri("selfplay") + "</div>";
  h += '<div class="tile">eval games 1 h / 24 h' + tri("eval") + "</div>";
  h += '<div class="tile">fuzz seeds 1 h / 24 h' + tri("fuzz") + "</div>";
  const f = s.failures.last_day;
  h += '<div class="tile">24 h: crashes / fuzz failures<b class="' + (f.crashes ? "bad" : "") + '">' + f.crashes + " / " + f.fuzz_failures + "</b></div>";
  h += '<div class="tile">24 h: leases expired / given back<b>' + f.expired + " / " + f.given_back + "</b></div>";
  h += "</div>";

  // Contributors: the farm's PCs and the owner's own machines (pushed by a desktop reporter).
  const origin = (c) => (c.origin === "farm" ? "farm" : "own: " + ({ local: "local", ssh: "SSH", vast: "Vast", other: "other" }[c.kind] || esc(c.kind)));
  const stat = (c) => '<span class="' + ({ working: "ok", online: "ok", offline: "muted", paused: "warn", revoked: "bad" }[c.status] || "") + '">' + esc(c.status) + "</span>";
  const w2 = (x) => num(x.h1) + " / " + num(x.h24);
  h += "<h2>Contributors</h2>" + table([
    ["name", (c) => esc(c.name) + (c.origin === "farm" ? '<br><code class="muted">' + esc(c.id) + "</code>" : "")],
    ["origin", (c) => origin(c)], ["status", (c) => stat(c) + ' <span class="muted">' + ago(c.last_seen, now) + "</span>"],
    ["hardware", (c) => esc(c.hardware || "")], ["working on", (c) => esc((c.working_on || []).join(", "))],
    ["self-play 1 h / 24 h", (c) => w2(c.selfplay), "n"], ["eval 1 h / 24 h", (c) => w2(c.eval), "n"],
    ["fuzz seeds 1 h / 24 h", (c) => w2(c.fuzz), "n"], ["busy h 24 h", (c) => (c.busy_h_24h === null ? "" : Number(c.busy_h_24h).toFixed(1)), "n"],
  ], s.contributors || []);

  h += "<h2>Per run and campaign</h2>" + table([
    ["scope", (x) => esc(x.scope)], ["farm self-play 1 h / 24 h", (x) => w2(x.farm.selfplay), "n"],
    ["farm eval 1 h / 24 h", (x) => w2(x.farm.eval), "n"], ["farm fuzz seeds 1 h / 24 h", (x) => w2(x.farm.fuzz), "n"],
    ["own machines on it", (x) => esc((x.own || []).join(", "))],
  ], s.by_scope || []);

  h += "<h2>Runs</h2>" + table([
    ["run", (r) => esc(r.run)], ["state", (r) => '<span class="' + (r.state === "active" ? "ok" : "warn") + '">' + r.state + "</span>"],
    ["priority", (r) => r.priority, "n"], ["gen", (r) => r.current_gen ?? "-", "n"],
    ["quota", (r) => (r.selfplay ? num(r.selfplay.quota) : "-"), "n"], ["cap", (r) => (r.selfplay ? num(r.selfplay.cap) : "-"), "n"],
    ["accepted", (r) => (r.selfplay ? num(r.selfplay.accepted) : "-"), "n"],
    ["in flight", (r) => (r.selfplay ? num(r.selfplay.leased) + " leased + " + num(r.selfplay.pending) + " unacked" : "-")],
    ["eval", (r) => (r.eval ? esc(r.eval.batch) + ": " + num(r.eval.done) + " / " + num(r.eval.games) + " done" : "-")],
    ["definition", (r) => defCell(r.definition)],
    ["active leases", (r) => r.active_leases, "n"], ["pool", (r) => num(r.pool), "n"], ["next gid", (r) => num(r.next_gid), "n"],
  ], s.runs);

  // Eval batches (PROTOCOL 5.2): progress only; the coordinator never reads game rows, so the
  // score is the desktop's to compute.
  const pct = (b) => (b.games ? Math.floor((100 * b.done) / b.games) : 0);
  const bar = (b) => '<span style="display:inline-block;width:80px;height:8px;border:1px solid var(--line);vertical-align:middle">' +
    '<span style="display:block;height:100%;width:' + pct(b) + '%;background:var(--ok)"></span></span> ' + pct(b) + "%";
  const bstate = (b) => (b.done >= b.games ? '<span class="ok">complete</span>' : b.replaced_at ? '<span class="muted">replaced</span>' : "running");
  h += "<h2>Eval batches</h2>" + table([
    ["run", (b) => esc(b.run)], ["batch", (b) => esc(b.batch)], ["networks", (b) => esc((b.checkpoints || []).join(" v ") || "bots")],
    ["paired", (b) => (b.paired ? "yes" : "no")], ["progress", (b) => bar(b)],
    ["games done", (b) => num(b.done) + " / " + num(b.games), "n"], ["in leases", (b) => num(b.leased), "n"],
    ["unissued", (b) => num(b.unissued), "n"], ["started", (b) => ago(b.created_at, now)], ["state", (b) => bstate(b)],
  ], s.eval_batches || []);

  h += "<h2>Fuzz campaigns</h2>" + table([
    ["campaign", (x) => esc(x.campaign)], ["mode", (x) => esc(x.mode)], ["state", (x) => x.state], ["priority", (x) => x.priority, "n"],
    ["cursor", (x) => num(x.cursor), "n"], ["seeds done", (x) => num(x.seeds_done), "n"], ["pool", (x) => num(x.pool), "n"],
    ["active leases", (x) => x.active_leases, "n"], ["failures", (x) => '<span class="' + (x.failures ? "bad" : "") + '">' + x.failures + "</span>", "n"],
    ["sites", (x) => x.sites, "n"], ["definition", (x) => defCell(x.definition)],
  ], s.campaigns);

  // Signed definitions (PROTOCOL 7): what each run's and campaign's current definition allows.
  const defs = [...s.runs.map((r) => ({ kind: "run", id: r.run, d: r.definition })), ...s.campaigns.map((x) => ({ kind: "campaign", id: x.campaign, d: x.definition }))];
  h += "<h2>Signed definitions</h2>" + table([
    ["run / campaign", (x) => esc(x.kind) + " " + esc(x.id)], ["version", (x) => (x.d && x.d.version !== null ? "v" + x.d.version : '<span class="warn">none</span>'), "n"],
    ["signed", (x) => (x.d && x.d.created ? when(x.d.created) : "")], ["key", (x) => "<code>" + esc((x.d && x.d.key_id) || "") + "</code>"],
    ["expires", (x) => (x.d && x.d.expires ? '<span class="' + (x.d.expires <= now ? "bad" : "") + '">' + when(x.d.expires) + "</span>" : '<span class="muted">never</span>')],
    ["jobs", (x) => esc(((x.d && x.d.jobs) || []).join(", "))], ["gens", (x) => esc(((x.d && x.d.gens) || []).join(", "))],
    ["builds", (x) => ((x.d && x.d.builds) || []).map((b) => "<code>" + esc(String(b).slice(0, 8)) + "</code>").join(" ") +
      (x.d && x.d.runtime ? ' <span class="muted">runtime</span> <code>' + esc(String(x.d.runtime).slice(0, 8)) + "</code>" : "")],
    ["can't lease because", (x) => '<span class="' + (x.d && x.d.version === null ? "warn" : "bad") + '">' + esc(((x.d && x.d.problems) || []).join("; ")) + "</span>", "wrapok"],
  ], defs);

  h += "<h2>Machines</h2>" + table([
    ["name", (m) => esc(m.name) + '<br><code class="muted">' + esc(m.machine_id) + "</code>"],
    ["class", (m) => esc(m.class)], ["agent", (m) => esc(m.agent_version) + ' <span class="muted">' + esc(m.channel) + "</span>" +
      (s.limits && m.agent_version && cmpVer(m.agent_version, s.limits.definition_agent) < 0 ? ' <span class="warn" title="older than the first agent that verifies definitions">legacy</span>' : "")],
    ["last seen", (m) => (m.revoked ? '<span class="bad">revoked</span>' : ago(m.last_seen, now)) + (m.paused ? ' <span class="warn">paused</span>' : "")],
    ["working on", (m) => esc(m.working_on || "")],
    ["caps", (m) => (m.caps ? m.caps.threads + " thr, " + Math.round(m.caps.ram_mb / 1024) + " GB" + (m.caps.gpu ? ", gpu" : "") : "")],
    ["games 24 h", (m) => num(m.games_24h), "n"], ["seeds 24 h", (m) => num(m.seeds_24h), "n"],
    ["busy h", (m) => hrs(m.busy_s_24h), "n"], ["paused h", (m) => hrs(m.paused_s_24h), "n"],
    ["idle h/day", (m) => (m.idle_h_day === null ? '<span class="muted">-</span>' : Number(m.idle_h_day).toFixed(1) +
      (m.on_h_day === null ? "" : ' <span class="muted">of ' + Number(m.on_h_day).toFixed(1) + " on</span>")), "n"],
    ["yields", (m) => num(m.yields_24h), "n"], ["max yield ms", (m) => num(m.yield_ms_max), "n"],
    ["starved h", (m) => '<span class="' + (m.starved_s_24h >= 600 ? "warn" : "") + '">' + hrs(m.starved_s_24h) + "</span>", "n"],
    ["given back", (m) => m.given_back_24h, "n"], ["expired", (m) => '<span class="' + (m.expired_24h ? "warn" : "") + '">' + m.expired_24h + "</span>", "n"],
    ["refused", (m) => '<span class="' + (m.refused_24h ? "bad" : "") + '" title="leases the agent refused (definition checks)">' + (m.refused_24h || 0) + "</span>", "n"],
    ["crashes", (m) => '<span class="' + (m.crashes_24h ? "bad" : "") + '">' + m.crashes_24h + "</span>", "n"],
    // Builds Windows refused to start on this PC (Smart App Control etc.): not offered to it until then.
    ["blocked builds", (m) => (m.blocked || []).map((b) => '<span class="bad" title="' + esc(b.message || "") + '"><code>' +
      esc(String(b.build_id).slice(0, 8)) + "</code> os error " + esc(b.os_error ?? "?") + "</span> " +
      '<span class="muted">until ' + when(b.until) + "</span>").join("<br>")],
  ], s.machines);

  // Every build Windows refused to start somewhere (crash kind "blocked"), until its block lifts.
  h += "<h2>Blocked builds</h2>" + table([
    ["machine", (b) => esc(b.name || b.machine_id)], ["build", (b) => "<code>" + esc(b.build_id) + "</code>"],
    ["os error", (b) => esc(b.os_error ?? "?"), "n"], ["reported by agent", (b) => esc(b.agent_version || "")],
    ["since", (b) => ago(b.t, now)], ["until", (b) => when(b.until)], ["message", (b) => esc((b.message || "").slice(0, 160))],
  ], s.blocks || []);

  h += "<h2>Rates by hardware class</h2>" + table([
    ["class", (r) => esc(r.class)], ["kind", (r) => r.kind], ["scope", (r) => esc(r.scope)],
    ["per hour", (r) => num(Math.round(r.rate * 3600)), "n"], ["machines", (r) => r.machines, "n"],
  ], s.classes);

  h += "<h2>Agent releases</h2>" + table([
    ["channel", (r) => esc(r.channel)], ["version", (r) => esc(r.version)], ["min agent", (r) => esc(r.min_agent)],
    ["released", (r) => ago(r.t, now)], ["installer", (r) => (r.github_url ? '<a href="' + esc(r.github_url) + '">GitHub</a>' : '<span class="muted">none</span>')],
    ["build", (r) => "<code>" + esc(r.build_id) + "</code>"],
  ], s.releases || []);

  h += "<h2>Diagnostics uploads</h2>" + table([
    ["when", (x) => ago(x.t, now)], ["machine", (x) => esc(x.name || x.machine_id)], ["what", (x) => esc(x.what)],
    ["from", (x) => (x.user ? '<span class="warn">friend (Send diagnostics)</span>' : "requested")],
    ["size", (x) => (x.size / 1048576).toFixed(1) + " MB", "n"],
    ["", (x) => '<button data-key="' + esc(x.key) + '">download</button>'],
  ], s.diag_uploads || []);

  h += "<h2>Recent crashes</h2>" + table([
    ["when", (x) => ago(x.t, now)], ["machine", (x) => esc(x.name || x.machine_id)], ["kind", (x) => esc(x.kind)],
    ["build", (x) => '<code>' + esc(x.build_id || "") + "</code>"], ["message", (x) => esc((x.message || "").slice(0, 160))],
  ], s.failures.crashes);

  h += "<h2>New fuzz failure sites</h2>" + table([
    ["when", (x) => ago(x.t, now)], ["campaign", (x) => esc(x.campaign)], ["seed", (x) => x.seed, "n"],
    ["site", (x) => "<code>" + esc(x.site) + "</code>"], ["panic", (x) => esc((x.panic || "").slice(0, 160))],
  ], s.failures.fuzz_sites);

  // The coordinator's limits (wrangler.jsonc vars; README "Limits").
  const L = s.limits || {};
  h += "<h2>Limits</h2>" + table([["limit", (x) => esc(x[0])], ["value", (x) => esc(x[1])]], [
    ["active leases per machine", num(L.max_active_leases)],
    ["result chunks per lease", num(L.max_lease_chunks)],
    ["result bytes per lease", L.max_lease_bytes ? (L.max_lease_bytes / 1048576).toLocaleString() + " MiB" : ""],
    ["results taken after a lease closed", L.late_results_s ? (L.late_results_s / 3600) + " h" : ""],
    ["crash reports kept per machine per day", num(L.max_crashes_per_day)],
    ["crash reports kept for", L.crash_retention_days + " days"],
    ["diagnostics kept for", L.diag_retention_days + " days"],
    ["first agent that verifies definitions", L.definition_agent || ""],
    ["data keys (DATA_PUBKEYS)", num(L.data_keys)],
    ["installers only from", L.github_repo ? "github.com/" + L.github_repo : ""],
    ["Cloudflare Access logins", L.access ? "accepted" : "not configured"],
  ]);
  $("out").innerHTML = h;
}
function defCell(d) {
  if (!d) return "";
  if (d.version === null) return '<span class="warn">none (legacy)</span>';
  return "v" + d.version + (d.problems && d.problems.length ? ' <span class="bad" title="' + esc(d.problems.join("; ")) + '">blocked</span>' : ' <span class="ok">ok</span>');
}
function cmpVer(a, b) {
  const p = (v) => String(v).split("-")[0].split(".").map(Number);
  const x = p(a), y = p(b);
  for (let i = 0; i < 3; i++) if ((x[i] || 0) !== (y[i] || 0)) return (x[i] || 0) < (y[i] || 0) ? -1 : 1;
  return 0;
}
// Diagnostics uploads are behind admin auth too: fetch, then save as a file.
$("out").addEventListener("click", async (e) => {
  const key = e.target && e.target.getAttribute && e.target.getAttribute("data-key");
  if (!key) return;
  try {
    const r = await fetch("/v1/admin/artifact/" + key, { headers: authHeaders(), credentials: "same-origin" });
    if (!r.ok) { $("err").textContent = "download: HTTP " + r.status; return; }
    const a = document.createElement("a");
    a.href = URL.createObjectURL(await r.blob());
    a.download = key.split("/").slice(1).join("-") + ".txt";
    document.body.appendChild(a);
    a.click();
    a.remove();
  } catch (err) { $("err").textContent = "download: " + err; }
});
$("token").value = getToken();
$("save").addEventListener("click", load);
$("token").addEventListener("keydown", (e) => { if (e.key === "Enter") load(); });
setInterval(() => { if ($("auto").checked && (viaAccess || $("token").value)) load(); }, 30000);
load(); // with Cloudflare Access in front, this works without a token
</script>
</body>
</html>
`;
