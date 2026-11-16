// node --test tools/privacy-check.test.mjs: the privacy check's rules (tools/privacy-check.mjs). `npm test` in
// coordinator/ runs the check on the repository and then these.
import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { GENERIC, loadLocal, scan, scanText } from "./privacy-check.mjs";

const rulesHit = (text) => [...new Set(scanText(text, GENERIC).map(([, r]) => r))].sort();

// ('|' splits each token so that this file itself passes the check; it is removed before scanning)
const FIRES = [
  ["ssh 100.|101.102.103", "tailscale-ip"],
  ["host pc-a.tail0a1b2c.|ts.net", "tailscale-name"],
  ["pool-m_|ab12cd34ef56gh78jk90mn12pq_000.npz", "farm-machine-id"],
  ['"C:/|Users/Jane Doe/x"', "windows-home-path"],
  ["cd /c/|Users/jdoe/x", "windows-home-path"],
  ["workdir='/Users/|jdoe/mtgz-remote'", "mac-home-path"],
  ["played by DESKTOP-|AB12CD3", "default-pc-hostname"],
  ["worker up on Janes-|MacBook-Pro.local", "default-mac-hostname"],
  ["ssh jdoe@|10.1.2.3", "ssh-user-at-ip"],
];

test("each generic rule fires", () => {
  for (const [text, rule] of FIRES) assert.ok(rulesHit(text.replace("|", "")).includes(rule), `${text} -> ${rule}`);
});

test("placeholders and ordinary text pass", () => {
  for (const text of ["C:/Users/<user>/x", "/Users/<user>/x", "C:\\Users\\x\\a.onnx", "/Users/user/x", "%USERPROFILE%",
    "user@mac", "ssh user@192.0.2.10", "m_00000000000000000000000000", "m_TEST0000000000000000000001",
    "version 100.2.3", "http://localhost:5400", "C:/Users/Public/x"]) {
    assert.deepEqual(rulesHit(text), [], text);
  }
});

test("markers spare synthetic data from the generic rules, never from local patterns", () => {
  const dir = mkdtempSync(join(tmpdir(), "privacy-"));
  const tok = "ssh jdoe@" + "10.9.8.7";
  writeFileSync(join(dir, "a.md"), `fine\n${tok}\n${tok} privacy-check: allow\n`);
  writeFileSync(join(dir, "b.rs"), `// privacy-check: allow-file\n${tok}\nsecret_host_7\n`);
  writeFileSync(join(dir, "c.bin"), Buffer.from([0, 0, 65]));
  writeFileSync(join(dir, "p.txt"), "# comment\n\nsecret_host_[0-9]+\n");
  const local = loadLocal(join(dir, "p.txt"));
  assert.equal(local.length, 1);
  const [n, findings] = scan(dir, ["a.md", "b.rs", "c.bin", "gone.txt"], [...GENERIC, ...local]);
  assert.equal(n, 2);
  assert.deepEqual(findings.map(([p, l, r]) => [p, l, r]), [["a.md", 2, "ssh-user-at-ip"], ["b.rs", 3, "local pattern 3"]]);
  assert.deepEqual(loadLocal(join(dir, "absent.txt")), []);
});
