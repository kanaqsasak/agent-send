import { invoke } from "@tauri-apps/api/core";
import "./style.css";

type Health = { version: number; status: "ok"; identity_id: string };

interface DaemonClient {
  endpoint(): Promise<string>;
  health(): Promise<Health>;
}

const daemon: DaemonClient = {
  async endpoint() {
    // The fallback keeps `npm run dev` useful in a browser, while Tauri owns
    // the packaged endpoint and the future daemon start/connect adapter.
    try {
      return await invoke<string>("daemon_endpoint");
    } catch {
      return import.meta.env.VITE_DAEMON_URL ?? "http://127.0.0.1:8765";
    }
  },
  async health() {
    const response = await fetch(`${await this.endpoint()}/v1/health`);
    if (!response.ok) throw new Error(`daemon returned HTTP ${response.status}`);
    return (await response.json()) as Health;
  }
};

const app = document.querySelector<HTMLDivElement>("#app")!;
app.innerHTML = `
  <section class="shell">
    <header><span class="mark">●</span><h1>agent-send</h1></header>
    <p class="subtitle">Private local file transfer</p>
    <div class="health" aria-live="polite">
      <span class="dot pending"></span><span id="status">Checking daemon…</span>
    </div>
    <p class="endpoint" id="endpoint"></p>
    <button id="retry" type="button">Check again</button>
  </section>
`;

const status = document.querySelector<HTMLSpanElement>("#status")!;
const endpoint = document.querySelector<HTMLParagraphElement>("#endpoint")!;
const dot = document.querySelector<HTMLSpanElement>(".dot")!;

async function checkHealth() {
  status.textContent = "Checking daemon…";
  dot.className = "dot pending";
  try {
    const url = await daemon.endpoint();
    endpoint.textContent = url;
    const health = await daemon.health();
    status.textContent = `Daemon healthy · API v${health.version}`;
    dot.className = "dot online";
  } catch {
    status.textContent = "Daemon unavailable";
    dot.className = "dot offline";
  }
}

document.querySelector("#retry")!.addEventListener("click", checkHealth);
void checkHealth();
setInterval(checkHealth, 10_000);
