// The public onboarding page (GET /) and the installer download (GET /download), for friends
// who got an invite code, and for an installer too old for the farm (enroll answers 426 with a
// link here: an unenrolled agent can't update itself). Installers live on GitHub Releases
// (rev 4): /download redirects to the newest one; the coordinator serves no installer bytes.

import type { Ctx } from "./ctx";
import { CHANNELS } from "./friend";
import { cmpSemver, notFound, oneOf } from "./util";

export const SOURCE_URL = "https://github.com/greggg230/zero-farm";

export interface Installer {
  channel: string;
  version: string;
  github_url: string;
  t: number;
}

// The newest release of `channel` that names an installer on GitHub, unless that installer is
// older than the channel's min_agent (enroll would refuse it with 426).
async function installerOf(db: D1Database, channel: string): Promise<Installer | null> {
  const [withInstaller, latest] = await db.batch<any>([
    db
      .prepare(
        `SELECT channel, version, github_url, t FROM agent_releases
         WHERE channel = ? AND github_url IS NOT NULL ORDER BY id DESC LIMIT 1`,
      )
      .bind(channel),
    db.prepare(`SELECT min_agent FROM agent_releases WHERE channel = ? ORDER BY id DESC LIMIT 1`).bind(channel),
  ]);
  const r = withInstaller.results[0];
  if (!r) return null;
  const min = latest.results[0]?.min_agent as string | undefined;
  if (min && cmpSemver(r.version, min) < 0) return null;
  return { channel: r.channel, version: r.version, github_url: r.github_url, t: r.t };
}

// `?channel=` picks one; without it: stable, else canary (said on the page and in x-farm-channel).
export async function findInstaller(db: D1Database, channel: string | null): Promise<Installer | null> {
  const order = channel ? [channel] : ["stable", "canary"];
  for (const ch of order) {
    const i = await installerOf(db, ch);
    if (i) return i;
  }
  return null;
}

function channelParam(c: Ctx): string | null {
  const v = c.url.searchParams.get("channel");
  return v === null ? null : oneOf(v, "channel", CHANNELS);
}

// GET|HEAD /download[?channel=stable|canary]: 302 to the installer on GitHub Releases.
export async function download(c: Ctx): Promise<Response> {
  const inst = await findInstaller(c.env.DB, channelParam(c));
  if (!inst) throw notFound("no installer has been released yet");
  return new Response(null, {
    status: 302,
    headers: {
      location: inst.github_url,
      "cache-control": "no-store",
      "x-farm-version": inst.version,
      "x-farm-channel": inst.channel,
    },
  });
}

const esc = (s: string) => s.replace(/[&<>"']/g, (ch) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[ch]!);

// GET /: what Zero Farm is, what it does to your PC, the download button.
export async function home(c: Ctx): Promise<Response> {
  const inst = await findInstaller(c.env.DB, null);
  let button: string;
  if (inst) {
    const test = inst.channel === "stable" ? "" : ` <span class="tag">test build</span>`;
    button =
      `<a class="button" href="/download">Download Zero Farm ${esc(inst.version)}</a>` +
      `<p class="small">Windows 10 or 11 &middot; ZeroFarmSetup.exe from GitHub Releases${test}</p>`;
    if (inst.channel !== "stable") {
      button += `<p class="small">There is no regular release yet, so this is the newest test build. It updates itself to regular releases later.</p>`;
    }
  } else {
    button = `<span class="button off">No download yet</span><p class="small">The first release isn't out yet. Check back soon.</p>`;
  }
  const page = HOME.replace("{{button}}", button).replaceAll("{{source}}", SOURCE_URL);
  return new Response(page, { headers: { "content-type": "text/html; charset=utf-8", "cache-control": "no-store" } });
}

const HOME = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Zero Farm</title>
<meta name="description" content="Lend your PC's idle time to Gregg's game-playing AIs.">
<style>
  :root { --bg: #fafaf7; --fg: #1d1d1b; --muted: #5d5d58; --card: #ffffff; --line: #e3e2dc; --accent: #2e7d32; --accent-fg: #fff; --tag: #fff3d6; --tag-fg: #6b4a00; }
  @media (prefers-color-scheme: dark) {
    :root { --bg: #121211; --fg: #ecebe6; --muted: #a3a29b; --card: #1b1b19; --line: #2f2f2b; --accent: #5cb860; --accent-fg: #0d1a0e; --tag: #3a2f12; --tag-fg: #ffd98a; }
  }
  * { box-sizing: border-box; }
  body { margin: 0; background: var(--bg); color: var(--fg); font: 17px/1.55 system-ui, -apple-system, "Segoe UI", sans-serif; }
  main { max-width: 680px; margin: 0 auto; padding: 40px 16px 64px; }
  header { display: flex; align-items: center; gap: 14px; margin-bottom: 8px; }
  .logo { width: 52px; height: 52px; flex: none; }
  h1 { font-size: 34px; line-height: 1.1; margin: 0; letter-spacing: -0.01em; }
  .lede { font-size: 20px; color: var(--muted); margin: 6px 0 28px; }
  h2 { font-size: 19px; margin: 36px 0 10px; }
  .get { background: var(--card); border: 1px solid var(--line); border-radius: 14px; padding: 22px; }
  .button { display: inline-block; background: var(--accent); color: var(--accent-fg); text-decoration: none; font-weight: 600; font-size: 18px; padding: 13px 22px; border-radius: 10px; }
  .button:hover { filter: brightness(1.08); }
  .button.off { background: var(--line); color: var(--muted); }
  .small { font-size: 14px; color: var(--muted); margin: 10px 0 0; }
  .invite { margin: 16px 0 0; font-weight: 600; }
  .tag { background: var(--tag); color: var(--tag-fg); border-radius: 6px; padding: 1px 7px; font-weight: 600; }
  ul, ol { padding-left: 22px; margin: 0; }
  li { margin: 6px 0; }
  footer { margin-top: 44px; font-size: 14px; color: var(--muted); }
  a { color: var(--accent); }
</style>
</head>
<body>
<main>
<header>
  <svg class="logo" viewBox="0 0 64 64" aria-hidden="true">
    <circle cx="32" cy="32" r="32" fill="#2e7d32"/>
    <path d="M19.2 19.2 H44.8 L19.2 44.8 H44.8" fill="none" stroke="#fff" stroke-width="9" stroke-linecap="round" stroke-linejoin="round"/>
  </svg>
  <h1>Zero Farm</h1>
</header>
<p class="lede">Lend your PC's idle time to Gregg's game-playing AIs.</p>

<section class="get">
  {{button}}
  <p class="invite">You need an invite code from Gregg to join.</p>
</section>

<h2>What it is</h2>
<p>Zero Farm is a small Windows app. While you're away from your PC, it plays practice games for
AlphaZero-style AIs that Gregg trains (MTG Zero and friends) and sends the results back. Those
games are what the AIs learn from, so every idle hour helps.</p>

<h2>What it does to your PC</h2>
<ul>
  <li><b>Only works when you're away</b>: after 5 minutes without mouse or keyboard input. Never
    during fullscreen games or presentations, on battery, or when memory is low.</li>
  <li><b>Stops when you come back</b>, within 2 seconds of touching the mouse or keyboard.</li>
  <li>Runs at the lowest priority, so anything you run comes first.</li>
  <li><b>No admin rights.</b> It installs into your own user account; Windows won't ask for
    permission.</li>
  <li>Uses at most 3 GB of disk and never looks at your files. It reports only hardware facts
    (processor, memory, graphics card) and how fast the games run.</li>
  <li>Updates itself quietly. Pause it or quit it any time from its round "Z" icon next to the clock.</li>
  <li><b>To remove it</b>: Settings &rsaquo; Apps &rsaquo; Installed apps (Apps &amp; features on
    Windows 10) &rsaquo; Zero Farm &rsaquo; Uninstall. That removes everything.</li>
</ul>

<h2>Installing</h2>
<ol>
  <li>Run ZeroFarmSetup.exe. Windows may say "Windows protected your PC" because the installer
    isn't code-signed yet: click <b>More info</b>, then <b>Run anyway</b>.</li>
  <li>Enter your invite code and a name for your PC.</li>
  <li>The "Z" icon appears near the clock. That's all: it starts with Windows from now on.</li>
</ol>

<h2>Open source</h2>
<p>Everything this app does on your PC is public: <a href="{{source}}">Source code and all releases on
GitHub</a>. Each release lists how to check that your download is the signed original.</p>

<footer>Zero Farm is a hobby project between friends. Questions? Ask Gregg.</footer>
</main>
</body>
</html>
`;
