import { existsSync, readdirSync, readFileSync } from "node:fs";

const config = readFileSync("src-tauri/tauri.conf.json", "utf8");
const binariesDir = "src-tauri/binaries";
const staged = existsSync(binariesDir) ? readdirSync(binariesDir) : [];
const rust = readFileSync("src-tauri/src/lib.rs", "utf8");
const checks = [
  [config.includes('"binaries/agent-send-daemon"'), "daemon externalBin"],
  [config.includes('"binaries/agent-send-daemon-*": ""'), "daemon resource mapping"],
  [rust.includes("start_daemon(&app.handle())"), "daemon starts during setup"],
  [rust.includes("RunEvent::Exit") && rust.includes("stop_daemon(app)"), "daemon cleanup on exit"],
  [rust.includes('"--bind", DAEMON_BIND'), "daemon has stable UI endpoint"],
];
if (process.env.REQUIRE_STAGED_SIDECARS === "1") {
  checks.push(
    [staged.some((name) => name.startsWith("agent-send-daemon-")), "daemon sidecar is staged"],
    [staged.some((name) => name.startsWith("agent-send-cli-")), "cli sidecar is staged"],
    [staged.some((name) => name.startsWith("agent-send-mcp-")), "mcp sidecar is staged"],
  );
}
const failed = checks.filter(([ok]) => !ok).map(([, name]) => name);
if (failed.length) throw new Error(`deployment checks failed: ${failed.join(", ")}`);
console.log(`deployment checks passed (${checks.length})`);
