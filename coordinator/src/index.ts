// zero-farm coordinator: hands out work to invited friends' idle PCs for zero-core training runs.
// The wire contract is ../PROTOCOL.md; why it works this way is ../DESIGN.md (sections 5-7).
//
//   friends (machine token):  /v1/enroll /v1/lease /v1/lease/:id/{beat,result/:seq,done}
//                             /v1/artifact/:key /v1/agent/latest /v1/crash /v1/diag/:name
//   desktop (admin token or a Cloudflare Access login):  /v1/admin/...
//   dashboard:                /admin (static; reads /v1/admin/stats)
//
// Every /v1/* answer, errors included, carries `x-zerofarm-coordinator: 1`: agents (0.2.5+) treat
// a 401/403 as "this PC was removed" only when the coordinator itself said so, never when a proxy,
// a firewall rule or a misplaced Access policy answered.
//   public:                   / (onboarding page), /download (302 to the newest installer on GitHub)

import * as admin from "./admin";
import { ADMIN_PAGE } from "./admin_page";
import { nowOf, type Ctx } from "./ctx";
import * as friend from "./friend";
import { runCron } from "./cron";
import * as onboard from "./onboard";
import { HttpError, errorResponse, json } from "./util";

export { LeaseBook } from "./leasebook";

type Handler = (c: Ctx, ...params: string[]) => Promise<Response>;

const page = async () =>
  new Response(ADMIN_PAGE, { headers: { "content-type": "text/html; charset=utf-8", "cache-control": "no-store" } });
const SEG = "([^/]+)";
const routes: [string, RegExp, Handler][] = [
  ["POST", /^\/v1\/enroll$/, friend.enroll],
  ["POST", /^\/v1\/lease$/, friend.lease],
  ["POST", new RegExp(`^/v1/lease/${SEG}/beat$`), friend.beat],
  ["PUT", new RegExp(`^/v1/lease/${SEG}/result/${SEG}$`), friend.result],
  ["POST", new RegExp(`^/v1/lease/${SEG}/done$`), friend.done],
  ["GET", /^\/v1\/artifact\/(.+)$/, friend.artifact],
  ["HEAD", /^\/v1\/artifact\/(.+)$/, friend.artifact],
  ["GET", /^\/v1\/agent\/latest$/, friend.agentLatest],
  ["POST", /^\/v1\/crash$/, friend.crash],
  ["POST", new RegExp(`^/v1/diag/${SEG}$`), friend.diagUpload],

  ["POST", /^\/v1\/admin\/invites$/, admin.invites],
  ["PUT", /^\/v1\/admin\/artifact\/(.+)$/, admin.putArtifact],
  ["POST", /^\/v1\/admin\/artifact\/(.+)$/, admin.postArtifact],
  ["GET", /^\/v1\/admin\/artifact\/(.+)$/, admin.getArtifact],
  ["HEAD", /^\/v1\/admin\/artifact\/(.+)$/, admin.getArtifact],
  ["POST", /^\/v1\/admin\/build$/, admin.registerBuild],
  ["PUT", /^\/v1\/admin\/definition$/, admin.putDefinition],
  ["POST", /^\/v1\/admin\/agent\/release$/, admin.agentRelease],
  ["PUT", new RegExp(`^/v1/admin/run/${SEG}$`), admin.putRun],
  ["GET", new RegExp(`^/v1/admin/run/${SEG}$`), admin.getRunRoute],
  ["PUT", new RegExp(`^/v1/admin/run/${SEG}/checkpoint/${SEG}$`), admin.putCheckpoint],
  ["GET", new RegExp(`^/v1/admin/run/${SEG}/results$`), admin.runResults],
  ["POST", new RegExp(`^/v1/admin/run/${SEG}/accept$`), admin.acceptRoute],
  ["POST", new RegExp(`^/v1/admin/run/${SEG}/ack$`), admin.runAck],
  ["PUT", new RegExp(`^/v1/admin/campaign/${SEG}$`), admin.putCampaign],
  ["GET", new RegExp(`^/v1/admin/campaign/${SEG}$`), admin.getCampaignRoute],
  ["GET", new RegExp(`^/v1/admin/campaign/${SEG}/failures$`), admin.failures],
  ["GET", new RegExp(`^/v1/admin/campaign/${SEG}/results$`), admin.campaignResults],
  ["POST", new RegExp(`^/v1/admin/campaign/${SEG}/ack$`), admin.campaignAck],
  ["GET", /^\/v1\/admin\/machines$/, admin.machines],
  ["POST", new RegExp(`^/v1/admin/machine/${SEG}/revoke$`), admin.revoke],
  ["POST", new RegExp(`^/v1/admin/machine/${SEG}$`), admin.updateMachine],
  ["POST", /^\/v1\/admin\/pause$/, admin.pause],
  ["POST", /^\/v1\/admin\/diag$/, admin.diag],
  ["GET", /^\/v1\/admin\/diag$/, admin.diagList],
  ["GET", /^\/v1\/admin\/stats$/, admin.stats],
  ["POST", /^\/v1\/admin\/external$/, admin.external],

  ["GET", /^\/admin\/?$/, page],
  ["GET", /^\/$/, onboard.home],
  ["GET", /^\/download$/, onboard.download],
  ["HEAD", /^\/download$/, onboard.download],
];

const COORDINATOR_HEADER = "x-zerofarm-coordinator";

// Marks an answer as the coordinator's own (every /v1/* path, PROTOCOL 1).
function mark(res: Response): Response {
  try {
    res.headers.set(COORDINATOR_HEADER, "1");
    return res;
  } catch {
    const out = new Response(res.body, res);
    out.headers.set(COORDINATOR_HEADER, "1");
    return out;
  }
}

async function handle(req: Request, env: Env, exec: ExecutionContext): Promise<Response> {
  const url = new URL(req.url);
  const res = await route(req, env, exec, url);
  return url.pathname.startsWith("/v1/") ? mark(res) : res;
}

async function route(req: Request, env: Env, exec: ExecutionContext, url: URL): Promise<Response> {
  const c: Ctx = { req, env, url, now: nowOf(req, env), exec };
  try {
    let pathMatched = false;
    for (const [method, re, h] of routes) {
      const m = re.exec(url.pathname);
      if (!m) continue;
      pathMatched = true;
      if (method === req.method) return await h(c, ...m.slice(1));
    }
    if (pathMatched) throw new HttpError(405, "bad_request", `${req.method} is not allowed here`);
    throw new HttpError(404, "not_found", `no route ${url.pathname}`);
  } catch (e) {
    if (e instanceof HttpError) return errorResponse(e);
    console.error("internal error", req.method, url.pathname, e);
    return json({ error: "internal", message: "internal error" }, 500);
  }
}

export default {
  fetch: (req, env, ctx) => handle(req, env, ctx),
  async scheduled(controller, env, ctx) {
    ctx.waitUntil(runCron(env, Math.floor(controller.scheduledTime / 1000)));
  },
} satisfies ExportedHandler<Env>;
