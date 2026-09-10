import { execFileSync } from "node:child_process";
import { mkdirSync, copyFileSync, chmodSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const desktop = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repo = resolve(desktop, "../..");
const target = execFileSync("rustc", ["-vV"], { cwd: repo, encoding: "utf8" })
  .match(/^host: (.+)$/m)?.[1];
if (!target) throw new Error("Unable to determine Rust host target");

const suffix = process.platform === "win32" ? ".exe" : "";
const destination = resolve(desktop, "src-tauri/binaries");
mkdirSync(destination, { recursive: true });

for (const name of ["agent-send-daemon", "agent-send-cli", "agent-send-mcp"]) {
  execFileSync("cargo", ["build", "-p", name], { cwd: repo, stdio: "inherit" });
  const source = resolve(repo, `target/debug/${name}${suffix}`);
  const staged = resolve(destination, `${name}-${target}${suffix}`);
  copyFileSync(source, staged);
  if (process.platform !== "win32") chmodSync(staged, 0o755);
}

console.log(`Staged development sidecars for ${target}`);
