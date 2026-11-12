import { cloudflareTest, readD1Migrations } from "@cloudflare/vitest-pool-workers";
import { defineConfig } from "vitest/config";

// Tests run inside local workerd (Miniflare): D1, R2, the LeaseBook Durable Object and Analytics
// Engine are all simulated. No Cloudflare account is involved.
export default defineConfig(async () => {
  const migrations = await readD1Migrations("./migrations");
  return {
    plugins: [
      cloudflareTest({
        wrangler: { configPath: "./wrangler.jsonc" },
        miniflare: {
          bindings: {
            TEST_MIGRATIONS: migrations,
            TEST_CLOCK: "1", // honour x-test-now (src/ctx.ts)
            ADMIN_TOKEN: "test-admin-token",
            // wrangler.jsonc carries the real release key; test builds carry fake signatures
            // (signature checks have their own tests in units.test.ts).
            RELEASE_PUBKEYS: "",
            // A throwaway data key that only these tests use (its secret half is in
            // test/helpers.ts); no agent trusts it. Definitions in tests are really signed.
            DATA_PUBKEYS: "RWT3hsgFThU0IyTBqihmNcFGFp14r/JzL0rYHibveCJB3GW3w6oaYaQs",
            IP_SALT: "test-ip-salt",
            // Tests hold several leases per machine on purpose; the cap has its own test (and the
            // fleet simulation runs with the real default, per request).
            MAX_ACTIVE_LEASES: "100",
          },
        },
      }),
    ],
    test: {
      setupFiles: ["./test/setup.ts"],
      testTimeout: 120_000,
      hookTimeout: 60_000,
    },
  };
});
