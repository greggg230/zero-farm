// zero-farm admin CLI: the desktop side of PROTOCOL 4.
//
//   npm run admin -- <command> [args]        (FARM_URL and FARM_ADMIN_TOKEN must be set)
//
// Runs on Node 22 with type stripping (no build step); erasable TypeScript only.

import { createHash } from "node:crypto";
import { closeSync, createReadStream, openSync, readFileSync, readSync, statSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const USAGE = `zero-farm admin (FARM_URL, FARM_ADMIN_TOKEN)

  invites [--count N] [--note TEXT] [--channel stable|canary]
  machines
  machine <machine_id> [--name N] [--channel stable|canary] [--paused true|false] [--unblock true]
                                                   --unblock: lift the builds it reported blocked
  revoke <machine_id>
  pause | resume                                   farm-wide: no new leases while paused

  put <key> <file>                                 upload under builds/ or ckpt/ (multipart if big)
  get <key> [out_file]                             download any object (results, diag, builds, ckpt)
  build <build_id>                                 register an uploaded build (checks hashes + signature)
  upload-build <dir>                               upload dir/manifest.json(.minisig) + its files, register
  agent-release <version> <build_id> <channel> <min_agent> [--github-url URL]
                                                   with --github-url: the installer on GitHub
                                                   Releases, where GET /download redirects

  run <run> <config.json|->                        create/update a run (PUT /v1/admin/run/<run>)
  run-get <run>
  checkpoint <run> <gen> <model.onnx> [reference.npz]   upload to ckpt/<run>/<gen>/ and publish
                                                   (older than current: registered, not current)
  results <run> [--after N] [--limit N]
  accept <run> <gen> <games>                       the trainer's total of accepted games for gen
  ack <run> <cursor>

  campaign <id> <config.json|->
  campaign-get <id>
  campaign-results <id> [--after N] [--limit N]
  campaign-ack <id> <cursor>
  failures <id> [--after N] [--limit N]

  diag (--machine <id> | --class <text>) <sweep|profile|stability|logs> [--params JSON]
  diag-list [--machine <id>]
  stats
`;

const PART = 64 * 1024 * 1024; // multipart part size (R2 needs >= 5 MiB except the last)

class Fail extends Error {}

function env(name: string): string {
  const v = process.env[name];
  if (!v) throw new Fail(`${name} is not set`);
  return v;
}

function flags(args: string[]): { pos: string[]; opt: Record<string, string> } {
  const pos: string[] = [];
  const opt: Record<string, string> = {};
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (a.startsWith("--")) {
      const next = args[i + 1];
      if (next === undefined || next.startsWith("--")) opt[a.slice(2)] = "true";
      else {
        opt[a.slice(2)] = next;
        i++;
      }
    } else pos.push(a);
  }
  return { pos, opt };
}

function need(pos: string[], n: number, usage: string): void {
  if (pos.length < n) throw new Fail(`usage: ${usage}`);
}

async function request(method: string, path: string, body?: unknown, headers: Record<string, string> = {}): Promise<Response> {
  const base = env("FARM_URL").replace(/\/+$/, "");
  const h: Record<string, string> = { authorization: `Bearer ${env("FARM_ADMIN_TOKEN")}`, ...headers };
  let b: BodyInit | undefined;
  if (body instanceof Uint8Array) b = body as Uint8Array<ArrayBuffer>;
  else if (body !== undefined) {
    b = JSON.stringify(body);
    h["content-type"] = "application/json";
  }
  return fetch(base + path, { method, headers: h, body: b });
}

async function api(method: string, path: string, body?: unknown, headers: Record<string, string> = {}): Promise<any> {
  const res = await request(method, path, body, headers);
  const text = await res.text();
  let parsed: any = text;
  try {
    parsed = JSON.parse(text);
  } catch {
    // not JSON
  }
  if (!res.ok) {
    const detail = parsed && typeof parsed === "object" ? `${parsed.error}: ${parsed.message}${parsed.problems ? "\n  " + parsed.problems.join("\n  ") : ""}` : text;
    throw new Fail(`${method} ${path} -> ${res.status} ${detail}`);
  }
  return parsed;
}

const print = (v: unknown) => console.log(JSON.stringify(v, null, 2));

function readConfig(file: string): unknown {
  const text = file === "-" ? readFileSync(0, "utf8") : readFileSync(file, "utf8");
  return JSON.parse(text);
}

function sha256File(file: string): Promise<string> {
  return new Promise((resolve, reject) => {
    const h = createHash("sha256");
    createReadStream(file)
      .on("data", (d) => h.update(d))
      .on("end", () => resolve(h.digest("hex")))
      .on("error", reject);
  });
}

function readPart(fd: number, offset: number, size: number): Uint8Array {
  const buf = Buffer.alloc(size);
  let got = 0;
  while (got < size) {
    const n = readSync(fd, buf, got, size - got, offset + got);
    if (n === 0) break;
    got += n;
  }
  return buf.subarray(0, got);
}

async function putFile(key: string, file: string): Promise<void> {
  const size = statSync(file).size;
  const path = `/v1/admin/artifact/${key}`;
  if (size <= PART) {
    const bytes = readFileSync(file);
    await api("PUT", path, new Uint8Array(bytes), { "x-sha256": createHash("sha256").update(bytes).digest("hex") });
    console.error(`put ${key} (${size} bytes)`);
    return;
  }
  const { upload_id } = await api("POST", `${path}?uploads`);
  const parts: { part: number; etag: string }[] = [];
  const fd = openSync(file, "r");
  try {
    for (let n = 1, off = 0; off < size; n++, off += PART) {
      const r = await api("PUT", `${path}?upload_id=${encodeURIComponent(upload_id)}&part=${n}`, readPart(fd, off, Math.min(PART, size - off)));
      parts.push(r);
      console.error(`put ${key} part ${n} (${Math.min(off + PART, size)}/${size})`);
    }
  } finally {
    closeSync(fd);
  }
  await api("POST", `${path}?upload_id=${encodeURIComponent(upload_id)}`, { parts });
}

function q(opt: Record<string, string>, keys: string[]): string {
  const s = new URLSearchParams();
  for (const k of keys) if (opt[k] !== undefined) s.set(k, opt[k]);
  const t = s.toString();
  return t ? `?${t}` : "";
}

async function main(argv: string[]): Promise<void> {
  const [cmd, ...rest] = argv;
  const { pos, opt } = flags(rest);
  switch (cmd) {
    case "invites":
      return print(
        await api("POST", "/v1/admin/invites", {
          count: opt.count ? Number(opt.count) : 1,
          note: opt.note,
          channel: opt.channel,
        }),
      );
    case "machines":
      return print(await api("GET", "/v1/admin/machines"));
    case "machine":
      need(pos, 1, "machine <machine_id> [--name N] [--channel C] [--paused true|false] [--unblock true]");
      return print(
        await api("POST", `/v1/admin/machine/${pos[0]}`, {
          name: opt.name,
          channel: opt.channel,
          paused: opt.paused === undefined ? undefined : opt.paused === "true",
          unblock: opt.unblock === undefined ? undefined : opt.unblock === "true",
        }),
      );
    case "revoke":
      need(pos, 1, "revoke <machine_id>");
      return print(await api("POST", `/v1/admin/machine/${pos[0]}/revoke`, {}));
    case "pause":
    case "resume":
      return print(await api("POST", "/v1/admin/pause", { paused: cmd === "pause" }));

    case "put":
      need(pos, 2, "put <key> <file>");
      return putFile(pos[0], pos[1]);
    case "get": {
      need(pos, 1, "get <key> [out_file]");
      const res = await request("GET", `/v1/admin/artifact/${pos[0]}`);
      if (!res.ok) throw new Fail(`GET ${pos[0]} -> ${res.status} ${await res.text()}`);
      const bytes = new Uint8Array(await res.arrayBuffer());
      if (pos[1]) writeFileSync(pos[1], bytes);
      else process.stdout.write(bytes);
      return;
    }
    case "build":
      need(pos, 1, "build <build_id>");
      return print(await api("POST", "/v1/admin/build", { build_id: pos[0] }));
    case "upload-build": {
      need(pos, 1, "upload-build <dir>");
      const dir = pos[0];
      const manifestBytes = readFileSync(join(dir, "manifest.json"));
      const id = createHash("sha256").update(manifestBytes).digest("hex").slice(0, 24);
      const manifest = JSON.parse(manifestBytes.toString("utf8"));
      await putFile(`builds/${id}/manifest.json`, join(dir, "manifest.json"));
      await putFile(`builds/${id}/manifest.json.minisig`, join(dir, "manifest.json.minisig"));
      for (const f of manifest.files) await putFile(`builds/${id}/${f.path}`, join(dir, ...f.path.split("/")));
      return print(await api("POST", "/v1/admin/build", { build_id: id }));
    }
    case "agent-release":
      need(pos, 4, "agent-release <version> <build_id> <channel> <min_agent> [--github-url URL]");
      return print(
        await api("POST", "/v1/admin/agent/release", {
          version: pos[0],
          build_id: pos[1],
          channel: pos[2],
          min_agent: pos[3],
          github_url: opt["github-url"] ?? null,
        }),
      );

    case "run":
      need(pos, 2, "run <run> <config.json|->");
      return print(await api("PUT", `/v1/admin/run/${pos[0]}`, readConfig(pos[1])));
    case "run-get":
      need(pos, 1, "run-get <run>");
      return print(await api("GET", `/v1/admin/run/${pos[0]}`));
    case "checkpoint": {
      need(pos, 3, "checkpoint <run> <gen> <model.onnx> [reference.npz]");
      const [run, gen, model, ref] = pos;
      const modelKey = `ckpt/${run}/${gen}/model.onnx`;
      await putFile(modelKey, model);
      const body: Record<string, unknown> = { model: modelKey, sha256: await sha256File(model) };
      if (ref) {
        const refKey = `ckpt/${run}/${gen}/reference.npz`;
        await putFile(refKey, ref);
        body.reference = refKey;
        body.reference_sha256 = await sha256File(ref);
      }
      return print(await api("PUT", `/v1/admin/run/${run}/checkpoint/${gen}`, body));
    }
    case "results":
      need(pos, 1, "results <run> [--after N] [--limit N]");
      return print(await api("GET", `/v1/admin/run/${pos[0]}/results${q(opt, ["after", "limit"])}`));
    case "accept":
      need(pos, 3, "accept <run> <gen> <games>");
      return print(await api("POST", `/v1/admin/run/${pos[0]}/accept`, { gen: Number(pos[1]), games: Number(pos[2]) }));
    case "ack":
      need(pos, 2, "ack <run> <cursor>");
      return print(await api("POST", `/v1/admin/run/${pos[0]}/ack`, { cursor: Number(pos[1]) }));

    case "campaign":
      need(pos, 2, "campaign <id> <config.json|->");
      return print(await api("PUT", `/v1/admin/campaign/${pos[0]}`, readConfig(pos[1])));
    case "campaign-get":
      need(pos, 1, "campaign-get <id>");
      return print(await api("GET", `/v1/admin/campaign/${pos[0]}`));
    case "campaign-results":
      need(pos, 1, "campaign-results <id> [--after N] [--limit N]");
      return print(await api("GET", `/v1/admin/campaign/${pos[0]}/results${q(opt, ["after", "limit"])}`));
    case "campaign-ack":
      need(pos, 2, "campaign-ack <id> <cursor>");
      return print(await api("POST", `/v1/admin/campaign/${pos[0]}/ack`, { cursor: Number(pos[1]) }));
    case "failures":
      need(pos, 1, "failures <id> [--after N] [--limit N]");
      return print(await api("GET", `/v1/admin/campaign/${pos[0]}/failures${q(opt, ["after", "limit"])}`));

    case "diag": {
      need(pos, 1, "diag (--machine <id> | --class <text>) <sweep|profile|stability|logs> [--params JSON]");
      const body: Record<string, unknown> = { kind: pos[0] };
      if (opt.machine) body.machine_id = opt.machine;
      if (opt.class) body.class = opt.class;
      if (opt.params) body.params = JSON.parse(opt.params);
      return print(await api("POST", "/v1/admin/diag", body));
    }
    case "diag-list":
      return print(await api("GET", `/v1/admin/diag${opt.machine ? `?machine_id=${encodeURIComponent(opt.machine)}` : ""}`));
    case "stats":
      return print(await api("GET", "/v1/admin/stats"));
    case undefined:
    case "help":
    case "--help":
      console.log(USAGE);
      return;
    default:
      throw new Fail(`unknown command ${cmd}\n\n${USAGE}`);
  }
}

main(process.argv.slice(2)).catch((e) => {
  console.error(e instanceof Fail ? e.message : e);
  process.exit(1);
});
