#!/usr/bin/env node
// Refuse private details in the repository's tracked files (README "Contributing").
//
//     node tools/privacy-check.mjs [--root DIR] [PATH ...]
//
// The repository is public. This scans every tracked file (`git ls-files`; or only the PATHs given) for:
// * the committed GENERIC patterns below: Tailscale (CGNAT 100.64/10) addresses and MagicDNS names, farm machine ids
//   (`m_` + 26 base32 characters), home-directory paths that name a user (`C:/Users/<name>`, `/c/Users/<name>`,
//   `/Users/<name>`; placeholders such as `<user>` pass), Windows' and macOS's default PC host names (`DESKTOP-` + 7
//   characters, `<Name>s-MacBook-Pro`), and ssh targets written as `user@<IP>`. A line carrying the marker
//   `privacy-check: allow`, or a whole file with `privacy-check: allow-file` in its first 5 lines, is spared these
//   generic rules (synthetic data that must look real, e.g. the redactor and its tests; never for real names);
// * the machine's LOCAL patterns: one regex per line (case-insensitive, `#` comments) in the untracked file
//   ~/.config/zero/privacy_patterns.txt ($ZERO_PRIVACY_PATTERNS for another): the real names of hosts, farm machines
//   (friends' PCs) and accounts. They are never committed (that would publish them);
//   tools/privacy_patterns.example.txt shows the format.
// Binary files are skipped. Prints each finding as `path:line: [rule] excerpt`, then one line
// `privacy check: N file(s), M finding(s) (...)`; exit 0 when there is none, 1 when there are, 2 when it cannot run.
// The same rules as zero-core's and mtg-zero's tools/privacy_check.py (keep them in step).
import { execFileSync } from "node:child_process";
import { existsSync, readFileSync, statSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const USER_PLACEHOLDERS =
  String.raw`<[^<>\s/\\]*>|\.\.\.|\*|%USERNAME%|%USERPROFILE%|\$\{?USER(?:NAME)?\}?|USER(?:NAME)?|` +
  String.raw`user|you|me|someone|example|Public|Default|All Users|runner|runneradmin|[A-Za-z]`;
// a line carrying this marker is skipped (synthetic test data that must look real, e.g. a redactor's tests; never for
// real names)
export const ALLOW = "privacy-check: allow";
const SEP = String.raw`(?:\\\\|\\|/)`;
const END = String.raw`(?=[\\/"'\x60\s,;:)\]}]|$)`;

export const GENERIC = [
  ["tailscale-ip", new RegExp(String.raw`(?<![\d.])100\.(?:6[4-9]|[7-9]\d|1[01]\d|12[0-7])\.\d{1,3}\.\d{1,3}(?![\d.]*\d)`, "g")],
  ["tailscale-name", new RegExp(String.raw`\b[A-Za-z0-9-]+\.ts\.net\b`, "g")],
  ["farm-machine-id", new RegExp(String.raw`(?<![0-9A-Za-z])m_(?=[0-9]*[a-z])[0-9a-z]{26}(?![0-9A-Za-z])`, "g")],
  ["windows-home-path", new RegExp(String.raw`(?:\b[A-Za-z]:|(?<![\w.-])/[A-Za-z])` + SEP + "+Users" + SEP +
    "+(?!(?:" + USER_PLACEHOLDERS + ")" + END + String.raw`)[^\\/"'\x60\s<>]`, "gi")],
  ["mac-home-path", new RegExp(String.raw`(?<![\w.:\\/-])/Users/(?!(?:` + USER_PLACEHOLDERS + ")" + END +
    String.raw`)[A-Za-z0-9._-]+`, "g")],
  ["default-pc-hostname", new RegExp(String.raw`\b(?:DESKTOP-[A-Z0-9]{7}|LAPTOP-[A-Z0-9]{8})\b`, "gi")],
  ["default-mac-hostname", new RegExp(String.raw`\b[A-Z][a-z]+s-(?:MacBook-(?:Pro|Air)|iMac|Mac-mini|Mac-Studio)\b`, "g")],
  ["ssh-user-at-ip", new RegExp(String.raw`\b[a-z_][a-z0-9_.-]*@(?!(?:192\.0\.2|198\.51\.100|203\.0\.113|127\.0\.0)\.)` +
    String.raw`(?:\d{1,3}\.){3}\d{1,3}\b`, "gi")],
];

export function localPatternsPath() {
  return process.env.ZERO_PRIVACY_PATTERNS || join(homedir(), ".config", "zero", "privacy_patterns.txt");
}

export function loadLocal(path) {
  if (!existsSync(path)) return [];
  const out = [];
  readFileSync(path, "utf8").split(/\r?\n/).forEach((raw, i) => {
    const line = raw.trim();
    if (!line || line.startsWith("#")) return;
    try {
      out.push([`local pattern ${i + 1}`, new RegExp(line, "gi")]);
    } catch (e) {
      throw new Error(`${path}:${i + 1}: bad regex ${JSON.stringify(line)}: ${e.message}`);
    }
  });
  return out;
}

export function scanText(text, rules) {
  const hits = [];
  for (const [rule, rx] of rules) {
    rx.lastIndex = 0;
    for (const m of text.matchAll(rx)) {
      const start = m.index, stop = start + m[0].length;
      const s = text.lastIndexOf("\n", start - 1) + 1;
      const e = text.indexOf("\n", stop);
      if (text.slice(s, e < 0 ? text.length : e).includes(ALLOW)) continue;
      const line = text.slice(0, start).split("\n").length;
      const a = Math.max(s, start - 60);
      let b = text.indexOf("\n", stop);
      b = Math.min(b < 0 ? text.length : b, stop + 60);
      hits.push([line, rule, text.slice(a, b).trim()]);
      if (m[0].length === 0) break;
    }
  }
  return hits.sort((x, y) => x[0] - y[0] || (x[1] < y[1] ? -1 : x[1] > y[1] ? 1 : 0));
}

// a file with this marker in its first 5 lines is spared the generic rules (local patterns still apply): a file whose
// subject is user names, such as the agent's redactor
export const ALLOW_FILE = "privacy-check: allow-file";

export function scan(root, paths, rules) {
  const findings = [];
  const generic = new Set(GENERIC.map(([, rx]) => rx));
  let n = 0;
  for (const rel of paths) {
    const full = join(root, rel);
    if (!existsSync(full) || !statSync(full).isFile()) continue;
    const data = readFileSync(full);
    if (data.subarray(0, 8192).includes(0)) continue;
    n++;
    const text = data.toString("utf8");
    const use = text.split("\n", 5).join("\n").includes(ALLOW_FILE) ? rules.filter(([, rx]) => !generic.has(rx)) : rules;
    for (const h of scanText(text, use)) findings.push([rel.replace(/\\/g, "/"), ...h]);
  }
  return [n, findings];
}

function trackedFiles(root) {
  return execFileSync("git", ["ls-files", "-z"], { cwd: root, maxBuffer: 1 << 28 }).toString("utf8").split("\0").filter(Boolean);
}

export function main(argv) {
  let root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
  const paths = [];
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--root") root = resolve(argv[++i]);
    else paths.push(argv[i]);
  }
  const lp = localPatternsPath();
  let local, files;
  try {
    local = loadLocal(lp);
    files = paths.length ? paths : trackedFiles(root);
  } catch (e) {
    console.error(`privacy check: cannot run: ${e.message}`);
    return 2;
  }
  const [n, findings] = scan(root, files, [...GENERIC, ...local]);
  for (const [path, line, rule, excerpt] of findings) console.log(`${path}:${line}: [${rule}] ${excerpt.slice(0, 200)}`);
  const where = local.length ? `${local.length} local pattern(s) from ${lp}` : `no local patterns (${lp} absent or empty)`;
  console.log(`privacy check: ${n} file(s), ${findings.length} finding(s) (${GENERIC.length} generic rules, ${where})`);
  return findings.length ? 1 : 0;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  process.exit(main(process.argv.slice(2)));
}
