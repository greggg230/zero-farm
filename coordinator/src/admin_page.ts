// GET /admin: a static page that reads /v1/admin/stats with the admin token (kept in this
// browser's localStorage). It shows nothing without that token; Cloudflare Access can also sit in
// front of /admin (coordinator README, deploy checklist step 9).

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
  .muted { color: var(--muted); }
  .bad { color: var(--bad); }
  .ok { color: var(--ok); }
  .warn { color: var(--warn); }
  #err { color: var(--bad); margin: 8px 0; }
  .tiles { display: flex; flex-wrap: wrap; gap: 16px; }
  .tile { border: 1px solid var(--line); padding: 8px 12px; min-width: 140px; }
  .tile b { display: block; font-size: 20px; }
  code { font-size: 12px; }
</style>
</head>
<body>
<h1>zero-farm</h1>
<div class="bar">
  <input id="token" type="password" placeholder="admin token" autocomplete="off">
  <button id="save">Load</button>
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
function table(cols, rows) {
  if (!rows.length) return '<p class="muted">none</p>';
  const h = cols.map((c) => "<th>" + esc(c[0]) + "</th>").join("");
  const b = rows.map((r) => "<tr>" + cols.map((c) => {
    const v = c[1](r);
    return '<td class="' + (c[2] || "") + '">' + v + "</td>";
  }).join("") + "</tr>").join("");
  return '<div class="wrap"><table><thead><tr>' + h + "</tr></thead><tbody>" + b + "</tbody></table></div>";
}
function getToken() { try { return localStorage.getItem("farm_admin_token") || ""; } catch { return ""; } }
function setToken(t) { try { localStorage.setItem("farm_admin_token", t); } catch {} }

async function load() {
  const token = $("token").value.trim();
  if (!token) { $("err").textContent = "Enter the admin token."; return; }
  setToken(token);
  let s;
  try {
    const r = await fetch("/v1/admin/stats", { headers: { authorization: "Bearer " + token } });
    if (!r.ok) { $("err").textContent = "stats: HTTP " + r.status + " " + (await r.text()).slice(0, 200); return; }
    s = await r.json();
  } catch (e) { $("err").textContent = "stats: " + e; return; }
  $("err").textContent = "";
  $("when").textContent = "as of " + new Date(s.t * 1000).toLocaleTimeString();
  const now = s.t;
  const online = s.machines.filter((m) => !m.revoked && m.last_seen && now - m.last_seen < 600).length;
  const working = s.machines.filter((m) => m.active_leases > 0).length;
  const thr = (k, f) => num((s.throughput[k] || {})[f] || 0);
  let h = '<div class="tiles">';
  h += '<div class="tile">farm<b class="' + (s.paused ? "warn" : "ok") + '">' + (s.paused ? "paused" : "running") + "</b></div>";
  h += '<div class="tile">machines online / working<b>' + online + " / " + working + "</b></div>";
  h += '<div class="tile">self-play games 1 h / 24 h<b>' + thr("selfplay", "last_hour") + " / " + thr("selfplay", "last_day") + "</b></div>";
  h += '<div class="tile">eval games 1 h / 24 h<b>' + thr("eval", "last_hour") + " / " + thr("eval", "last_day") + "</b></div>";
  h += '<div class="tile">fuzz seeds 1 h / 24 h<b>' + thr("fuzz", "last_hour") + " / " + thr("fuzz", "last_day") + "</b></div>";
  const f = s.failures.last_day;
  h += '<div class="tile">24 h: crashes / fuzz failures<b class="' + (f.crashes ? "bad" : "") + '">' + f.crashes + " / " + f.fuzz_failures + "</b></div>";
  h += '<div class="tile">24 h: leases expired / given back<b>' + f.expired + " / " + f.given_back + "</b></div>";
  h += "</div>";

  h += "<h2>Runs</h2>" + table([
    ["run", (r) => esc(r.run)], ["state", (r) => '<span class="' + (r.state === "active" ? "ok" : "warn") + '">' + r.state + "</span>"],
    ["priority", (r) => r.priority, "n"], ["gen", (r) => r.current_gen ?? "-", "n"],
    ["quota", (r) => (r.selfplay ? num(r.selfplay.quota) : "-"), "n"], ["cap", (r) => (r.selfplay ? num(r.selfplay.cap) : "-"), "n"],
    ["accepted", (r) => (r.selfplay ? num(r.selfplay.accepted) : "-"), "n"],
    ["in flight", (r) => (r.selfplay ? num(r.selfplay.leased) + " leased + " + num(r.selfplay.pending) + " unacked" : "-")],
    ["eval", (r) => (r.eval ? esc(r.eval.batch) + ": " + num(r.eval.done) + " / " + num(r.eval.games) + " done" : "-")],
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
    ["sites", (x) => x.sites, "n"],
  ], s.campaigns);

  h += "<h2>Machines</h2>" + table([
    ["name", (m) => esc(m.name) + '<br><code class="muted">' + esc(m.machine_id) + "</code>"],
    ["class", (m) => esc(m.class)], ["agent", (m) => esc(m.agent_version) + ' <span class="muted">' + esc(m.channel) + "</span>"],
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
    ["crashes", (m) => '<span class="' + (m.crashes_24h ? "bad" : "") + '">' + m.crashes_24h + "</span>", "n"],
    // Builds Windows refused to start on this PC (Smart App Control etc.): not offered to it until then.
    ["blocked builds", (m) => (m.blocked || []).map((b) => '<span class="bad" title="' + esc(b.message || "") + '"><code>' +
      esc(String(b.build_id).slice(0, 8)) + "</code> os error " + esc(b.os_error ?? "?") + "</span> " +
      '<span class="muted">until ' + new Date(b.until * 1000).toLocaleString() + "</span>").join("<br>")],
  ], s.machines);

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
    ["when", (x) => ago(x.t, now)], ["machine", (x) => esc(x.name || x.machine_id)], ["kind", (x) => x.kind],
    ["build", (x) => '<code>' + esc(x.build_id || "") + "</code>"], ["message", (x) => esc((x.message || "").slice(0, 160))],
  ], s.failures.crashes);

  h += "<h2>New fuzz failure sites</h2>" + table([
    ["when", (x) => ago(x.t, now)], ["campaign", (x) => esc(x.campaign)], ["seed", (x) => x.seed, "n"],
    ["site", (x) => "<code>" + esc(x.site) + "</code>"], ["panic", (x) => esc((x.panic || "").slice(0, 160))],
  ], s.failures.fuzz_sites);
  $("out").innerHTML = h;
}
// Diagnostics uploads are behind the admin token too: fetch, then save as a file.
$("out").addEventListener("click", async (e) => {
  const key = e.target && e.target.getAttribute && e.target.getAttribute("data-key");
  if (!key) return;
  try {
    const r = await fetch("/v1/admin/artifact/" + key, { headers: { authorization: "Bearer " + $("token").value.trim() } });
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
setInterval(() => { if ($("auto").checked && $("token").value) load(); }, 30000);
if ($("token").value) load();
</script>
</body>
</html>
`;
