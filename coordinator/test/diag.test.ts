// User-initiated diagnostics: the friend's "Send diagnostics" uploads logs without a request from
// the desktop (x-farm-diag: user), once an hour per machine, up to 20 MB.

import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { MAX_USER_DIAG } from "../src/friend";
import { admin, advance, ask, call, enroll, err, ok } from "./helpers";

const send = (token: string, raw: BodyInit, user = true) =>
  call("POST", "/v1/diag/logs", { token, raw, headers: user ? { "x-farm-diag": "user" } : {} });

describe("user-initiated diagnostics", () => {
  it("are accepted without a request, listed as user uploads, once an hour", async () => {
    const m = await enroll("pc");
    err(await send(m.token, "no header", false), 409, "conflict");
    const up = ok(await send(m.token, "===== agent.log =====\nhello"));
    expect(up.key).toMatch(new RegExp(`^diag/${m.machine_id}/\\d+-logs$`));
    expect((await admin("GET", `/v1/admin/artifact/${up.key}`)).body).toBe("===== agent.log =====\nhello");
    const list = ok(await admin("GET", `/v1/admin/diag?machine_id=${m.machine_id}`));
    expect(list.uploads).toHaveLength(1);
    expect(list.uploads[0]).toMatchObject({ name: "logs", key: up.key, user: 1, request_id: null, size: 27 });
    // Once an hour.
    const again = err(await send(m.token, "again"), 429, "rate_limited");
    expect(again.retry_after).toBe(3600);
    advance(1800);
    expect(err(await send(m.token, "again"), 429, "rate_limited").retry_after).toBe(1800);
    advance(1800);
    ok(await send(m.token, "an hour later"));
    // Per machine.
    const other = await enroll("other");
    ok(await send(other.token, "mine"));
  });

  it("are capped at 20 MB, and a refused oversize upload doesn't use up the hour", async () => {
    const m = await enroll("pc");
    err(await send(m.token, new Uint8Array(MAX_USER_DIAG + 1)), 413, "too_large");
    ok(await send(m.token, new Uint8Array(MAX_USER_DIAG)));
  });

  it("a requested upload is neither limited nor capped at 20 MB, header or not", async () => {
    const m = await enroll("pc");
    ok(await admin("POST", "/v1/admin/diag", { machine_id: m.machine_id, kind: "logs" }));
    expect(ok(await ask(m, { kinds: [] })).upload_diag).toEqual(["logs"]);
    ok(await send(m.token, new Uint8Array(MAX_USER_DIAG + 1000)));
    const list = ok(await admin("GET", `/v1/admin/diag?machine_id=${m.machine_id}`));
    expect(list.uploads[0]).toMatchObject({ user: 0, size: MAX_USER_DIAG + 1000 });
    expect(list.uploads[0].request_id).toBe(list.requests[0].id);
    // The user's own upload right after is still allowed (its hour wasn't touched).
    ok(await send(m.token, "user"));
  });

  it("a user upload also answers a request not delivered yet", async () => {
    const m = await enroll("pc");
    ok(await admin("POST", "/v1/admin/diag", { machine_id: m.machine_id, kind: "logs" }));
    ok(await send(m.token, "logs"));
    const r = await env.DB.prepare(`SELECT done_at FROM diag_requests WHERE machine_id = ?`).bind(m.machine_id).first<any>();
    expect(r.done_at).not.toBeNull();
    expect(ok(await ask(m, { kinds: [] })).upload_diag).toBeUndefined();
  });
});
