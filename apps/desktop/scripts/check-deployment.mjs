import { readFileSync } from "node:fs";

const config = readFileSync("src-tauri/tauri.conf.json", "utf8");
const rust = readFileSync("src-tauri/src/lib.rs", "utf8");
const checks = [
  [config.includes('"binaries/agent-send-daemon"'), "daemon externalBin"],
  [rust.includes("start_daemon(&app.handle())"), "daemon starts during setup"],
  [rust.includes("RunEvent::Exit") && rust.includes("stop_daemon(app)"), "daemon cleanup on exit"],
  [rust.includes('"--bind", DAEMON_BIND'), "daemon has stable UI endpoint"],
];
const failed = checks.filter(([ok]) => !ok).map(([, name]) => name);
if (failed.length) throw new Error(`deployment checks failed: ${failed.join(", ")}`);
console.log(`deployment checks passed (${checks.length})`);
