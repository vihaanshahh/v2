import { spawn } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { defineConfig, type Plugin } from "vite";

const root = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(root, "..");

function resolveV2Bin(): string {
  for (const profile of ["release", "debug"]) {
    const bin = path.join(repo, "target", profile, "v2");
    try {
      fs.accessSync(bin, fs.constants.X_OK);
      return bin;
    } catch {
      /* try next */
    }
  }
  return path.join(repo, "target", "release", "v2");
}

const v2Bin = resolveV2Bin();

function v2ScanApi(): Plugin {
  return {
    name: "v2-scan-api",
    configureServer(server) {
      server.middlewares.use("/api/scan", (req, res) => {
        if (req.method !== "GET") {
          res.statusCode = 405;
          res.end("Method not allowed");
          return;
        }

        const url = new URL(req.url ?? "/", "http://localhost");
        const ctx = url.searchParams.get("ctx") ?? "4096";
        const source = url.searchParams.get("source") ?? "auto";
        const family = url.searchParams.get("family") ?? "";

        const args = ["--json", "--ctx", ctx, "--source", source];
        if (family) args.push("--family", family);

        const child = spawn(v2Bin, args, { cwd: path.resolve(root, "..") });
        let stdout = "";
        let stderr = "";

        child.stdout.on("data", (chunk: Buffer) => {
          stdout += chunk.toString();
        });
        child.stderr.on("data", (chunk: Buffer) => {
          stderr += chunk.toString();
        });

        child.on("close", (code) => {
          if (code !== 0) {
            res.statusCode = 500;
            res.setHeader("Content-Type", "application/json");
            res.end(
              JSON.stringify({
                error: stderr.trim() || `v2 exited with code ${code}`,
              }),
            );
            return;
          }

          res.statusCode = 200;
          res.setHeader("Content-Type", "application/json");
          res.end(stdout);
        });
      });
    },
  };
}

export default defineConfig({
  plugins: [v2ScanApi()],
  server: {
    port: 5173,
    strictPort: true,
  },
});
