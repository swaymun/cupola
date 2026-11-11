import { cloudflareTest, readD1Migrations } from "@cloudflare/vitest-plugin";
import { fileURLToPath, URL } from "node:url";
import { defineConfig } from "vitest/config";
export default defineConfig({plugins:[cloudflareTest(async()=>({wrangler:{configPath:fileURLToPath(new URL("./wrangler.jsonc",import.meta.url))},miniflare:{bindings:{TEST_MIGRATIONS:await readD1Migrations(fileURLToPath(new URL("./migrations",import.meta.url)))}}}))],test:{include:["test/**/*.test.ts"]}});
