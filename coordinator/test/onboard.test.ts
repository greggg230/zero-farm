// The public onboarding page (GET /), the installer download (GET /download: a redirect to GitHub
// Releases since rev 4) and the GitHub URLs agent releases carry.

import { describe, expect, it } from "vitest";
import { admin, call, err, makeBuild, ok, PROFILE } from "./helpers";

const gh = (version: string) => `https://github.com/greggg230/zero-farm/releases/download/agent-v${version}/ZeroFarmSetup.exe`;

async function release(version: string, o: { min_agent?: string; channel?: string; github?: boolean } = {}) {
  const id = await makeBuild({ kind: "agent", version });
  return ok(
    await admin("POST", "/v1/admin/agent/release", {
      version,
      build_id: id,
      channel: o.channel ?? "stable",
      min_agent: o.min_agent ?? "0.1.0",
      github_url: o.github ? gh(version) : null,
    }),
  );
}

const get = (path: string, headers: Record<string, string> = {}) => call("GET", path, { agent: null, headers });

function redirectsTo(r: { status: number; headers: Headers; body: unknown }, url: string) {
  expect(r.status, JSON.stringify(r.body)).toBe(302);
  expect(r.headers.get("location")).toBe(url);
  expect(r.headers.get("cache-control")).toBe("no-store");
}

describe("onboarding page and download", () => {
  it("before any installer: a friendly page without a download, and /download is 404", async () => {
    const r = await get("/");
    expect(r.status).toBe(200);
    expect(r.headers.get("content-type")).toContain("text/html");
    const html = r.body as string;
    expect(html).toContain("<title>Zero Farm</title>");
    expect(html).toContain("No download yet");
    expect(html).toContain("invite code from Gregg");
    expect(html).toContain("No admin rights");
    expect(html).toContain("Apps &amp; features");
    expect(html).toContain("Stops when you come back");
    expect(html).toContain(`<a href="https://github.com/greggg230/zero-farm">Source code and all releases on\nGitHub</a>`);
    const d = err(await get("/download"), 404, "not_found");
    expect(d.message).toContain("no installer");
    // A release without a GitHub installer doesn't change that.
    await release("0.1.0");
    expect((await get("/")).body).toContain("No download yet");
    err(await get("/download"), 404, "not_found");
  });

  it("only a canary installer: redirected to, and the page says it is a test build", async () => {
    await release("0.2.0", { channel: "canary", github: true });
    const html = (await get("/")).body as string;
    expect(html).toContain("Download Zero Farm 0.2.0");
    expect(html).toContain(`href="/download"`);
    expect(html).toContain("test build");
    expect(html).toContain("from GitHub Releases");
    const d = await get("/download");
    redirectsTo(d, gh("0.2.0"));
    expect(d.headers.get("x-farm-channel")).toBe("canary");
    expect(d.headers.get("x-farm-version")).toBe("0.2.0");
    // HEAD answers the same redirect; no installer bytes come from the coordinator.
    redirectsTo(await call("HEAD", "/download", { agent: null }), gh("0.2.0"));
    expect(d.body).toBe("");
  });

  it("a stable installer wins over canary; ?channel= picks one", async () => {
    await release("0.2.0", { channel: "canary", github: true });
    await release("0.1.5", { channel: "stable", github: true });
    const html = (await get("/")).body as string;
    expect(html).toContain("Download Zero Farm 0.1.5");
    expect(html).not.toContain("test build");
    redirectsTo(await get("/download"), gh("0.1.5"));
    redirectsTo(await get("/download?channel=canary"), gh("0.2.0"));
    expect((await get("/download?channel=stable")).headers.get("x-farm-channel")).toBe("stable");
    err(await get("/download?channel=beta"), 400, "bad_request");
    // A later stable release without a GitHub installer: the older stable installer is still fine...
    await release("0.1.6", { channel: "stable", min_agent: "0.1.0" });
    redirectsTo(await get("/download"), gh("0.1.5"));
    // ...until the farm no longer accepts it (enroll would answer 426): then canary's.
    await release("0.1.7", { channel: "stable", min_agent: "0.1.6" });
    const d = await get("/download");
    redirectsTo(d, gh("0.2.0"));
    expect(d.headers.get("x-farm-channel")).toBe("canary");
    err(await get("/download?channel=stable"), 404, "not_found");
  });

  it("an agent release's github_url must be a GitHub release asset; setup (R2) is refused", async () => {
    const id = await makeBuild({ kind: "agent", version: "0.3.0" });
    const body = (extra: Record<string, unknown>) => ({ version: "0.3.0", build_id: id, channel: "stable", min_agent: "0.1.0", ...extra });
    for (const bad of [
      "http://github.com/greggg230/zero-farm/releases/download/agent-v0.3.0/ZeroFarmSetup.exe",
      "https://github.com.evil.example/greggg230/zero-farm/releases/download/agent-v0.3.0/ZeroFarmSetup.exe",
      "https://example.com/ZeroFarmSetup.exe",
      "https://github.com/greggg230/zero-farm/releases/tag/agent-v0.3.0",
      "https://github.com/greggg230/zero-farm/releases/download/agent-v0.3.0/../x/ZeroFarmSetup.exe",
      42,
    ]) {
      err(await admin("POST", "/v1/admin/agent/release", body({ github_url: bad })), 400, "bad_request");
    }
    const old = err(
      await admin("POST", "/v1/admin/agent/release", body({ setup: { key: "setup/0.3.0/ZeroFarmSetup.exe", sha256: "0".repeat(64), size: 1 } })),
      400,
      "bad_request",
    );
    expect(old.message).toContain("github_url");
    // Installers no longer go to R2.
    err(await admin("PUT", "/v1/admin/artifact/setup/0.3.0/ZeroFarmSetup.exe", undefined, { raw: "MZ" }), 400, "bad_request");
    const r = ok(await admin("POST", "/v1/admin/agent/release", body({ github_url: gh("0.3.0") })));
    expect(r.github_url).toBe(gh("0.3.0"));
    redirectsTo(await get("/download"), gh("0.3.0"));
    // Releases without an installer still work as before.
    expect(ok(await admin("POST", "/v1/admin/agent/release", body({}))).github_url).toBeNull();
    // The dashboard's release list shows the URL of the channel's latest release (null now).
    const s = ok(await admin("GET", "/v1/admin/stats"));
    expect(s.releases).toEqual([expect.objectContaining({ channel: "stable", version: "0.3.0", github_url: null })]);
  });

  it("enroll's 426 points at the download for the invite's channel", async () => {
    await release("0.4.0", { min_agent: "0.4.0", github: true });
    await release("0.5.0", { channel: "canary", min_agent: "0.5.0" });
    const { codes } = ok(await admin("POST", "/v1/admin/invites", {}));
    const r = err(await call("POST", "/v1/enroll", { agent: "0.3.0", body: { invite: codes[0], name: "pc", profile: PROFILE } }), 426, "update_required");
    expect(r.download).toBe("https://farm.test/download");
    redirectsTo(await get("/download"), gh("0.4.0"));
    const canary = ok(await admin("POST", "/v1/admin/invites", { channel: "canary" })).codes[0];
    const c = err(await call("POST", "/v1/enroll", { agent: "0.4.0", body: { invite: canary, name: "pc", profile: PROFILE } }), 426, "update_required");
    expect(c.download).toBe("https://farm.test/download?channel=canary");
  });
});
