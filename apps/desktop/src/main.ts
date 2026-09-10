import { invoke } from "@tauri-apps/api/core";
import { enable as enableAutostart } from "@tauri-apps/plugin-autostart";
import "./style.css";

type Health = { version: number; status: "ok"; identity_id: string };
type Peer = {
  advertisement: { id: string; alias: string; address: string; api_version: number };
  trusted: boolean;
};
type PeersResponse = { version: number; peers: Peer[] };
type PairingResponse = { peer_id: string; code: string; expires_in_seconds: number };

interface DaemonClient {
  endpoint(): Promise<string>;
  request(path: string, options?: RequestInit): Promise<Response>;
  health(): Promise<Health>;
  peers(): Promise<PeersResponse>;
  requestPairing(peer: Peer["advertisement"]): Promise<PairingResponse>;
  confirmPairing(peerId: string, code: string): Promise<void>;
  revoke(peerId: string): Promise<void>;
}

async function configureAutostart() {
  try { await enableAutostart(); } catch { /* Browser development has no autostart backend. */ }
}

const daemon: DaemonClient = {
  async endpoint() {
    try { return await invoke<string>("daemon_endpoint"); }
    catch { return import.meta.env.VITE_DAEMON_URL ?? "http://127.0.0.1:8765"; }
  },
  async request(path, options) {
    const response = await fetch(`${await this.endpoint()}${path}`, options);
    if (!response.ok) {
      let detail = `HTTP ${response.status}`;
      try { detail = ((await response.json()) as { error?: string }).error ?? detail; } catch { /* non-JSON error */ }
      throw new Error(detail);
    }
    return response;
  },
  async health() { return (await this.request("/v1/health")).json() as Promise<Health>; },
  async peers() { return (await this.request("/v1/peers")).json() as Promise<PeersResponse>; },
  async requestPairing(peer) {
    const response = await this.request("/v1/pairings", {
      method: "POST", headers: { "Content-Type": "application/json" },
      body: JSON.stringify(peer),
    });
    // Deliberately destructure only the short code. The daemon's pairing secret
    // is never rendered, logged, or retained by the desktop client.
    const { peer_id, code, expires_in_seconds } = await response.json() as PairingResponse & { pairing_secret?: unknown };
    return { peer_id, code, expires_in_seconds };
  },
  async confirmPairing(peerId, code) {
    await this.request("/v1/pairings/confirm", {
      method: "POST", headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ peer_id: peerId, code }),
    });
  },
  async revoke(peerId) { await this.request(`/v1/peers/${encodeURIComponent(peerId)}`, { method: "DELETE" }); },
};

const app = document.querySelector<HTMLDivElement>("#app")!;
app.innerHTML = `
  <section class="shell">
    <header><span class="mark" aria-hidden="true">●</span><h1>agent-send</h1></header>
    <p class="subtitle">Private local file transfer</p>
    <div class="health" aria-live="polite"><span class="dot pending"></span><span id="status">Connecting…</span></div>
    <p class="endpoint" id="endpoint"></p>
    <p class="error" id="error" role="alert" hidden></p>

    <section class="panel identity-panel" aria-labelledby="identity-heading">
      <h2 id="identity-heading">This device</h2>
      <p class="muted">Your local device identity</p>
      <code id="identity">Loading…</code>
    </section>

    <section class="panel" aria-labelledby="peers-heading">
      <div class="section-heading"><div><h2 id="peers-heading">Nearby devices</h2><p class="muted">Discovered devices are untrusted until you pair them.</p></div><button id="refresh" type="button">Refresh</button></div>
      <div id="peers" class="peers" aria-live="polite"><p class="muted">Looking for devices…</p></div>
    </section>
    <div id="pairing" class="pairing" hidden></div>
  </section>
`;

const status = document.querySelector<HTMLSpanElement>("#status")!;
const endpoint = document.querySelector<HTMLParagraphElement>("#endpoint")!;
const identity = document.querySelector<HTMLElement>("#identity")!;
const peersElement = document.querySelector<HTMLDivElement>("#peers")!;
const errorElement = document.querySelector<HTMLParagraphElement>("#error")!;
const pairingElement = document.querySelector<HTMLDivElement>("#pairing")!;
const dot = document.querySelector<HTMLSpanElement>(".dot")!;
let currentPeers: Peer[] = [];

function showError(message: string) { errorElement.textContent = message; errorElement.hidden = false; }
function clearError() { errorElement.hidden = true; errorElement.textContent = ""; }
function button(label: string, action: () => void, className = "") {
  const element = document.createElement("button"); element.type = "button"; element.textContent = label; element.className = className;
  element.addEventListener("click", action); return element;
}

function renderPeers() {
  peersElement.replaceChildren();
  if (!currentPeers.length) { const empty = document.createElement("p"); empty.className = "muted"; empty.textContent = "No devices found on this network."; peersElement.append(empty); return; }
  for (const peer of currentPeers) {
    const row = document.createElement("article"); row.className = "peer";
    const details = document.createElement("div");
    const name = document.createElement("strong"); name.textContent = peer.advertisement.alias || "Unnamed device";
    const address = document.createElement("small"); address.textContent = `${peer.advertisement.address} · ${peer.trusted ? "Trusted" : "Not paired"}`;
    details.append(name, address); row.append(details);
    if (peer.trusted) {
      const trusted = document.createElement("span"); trusted.className = "trust"; trusted.textContent = "Trusted";
      row.append(trusted, button("Revoke", () => void revokePeer(peer), "secondary"));
    } else row.append(button("Pair", () => void startPairing(peer)));
    peersElement.append(row);
  }
}

async function startPairing(peer: Peer) {
  clearError(); pairingElement.hidden = false; pairingElement.textContent = "Creating pairing request…";
  try {
    const result = await daemon.requestPairing(peer.advertisement);
    pairingElement.replaceChildren();
    const title = document.createElement("h2"); title.textContent = `Confirm ${peer.advertisement.alias || "device"}`;
    const text = document.createElement("p"); text.textContent = "Compare this short code with the other device, then confirm.";
    const code = document.createElement("strong"); code.className = "code"; code.textContent = result.code;
    const expiry = document.createElement("p"); expiry.className = "muted"; expiry.textContent = `Expires in ${result.expires_in_seconds} seconds.`;
    const input = document.createElement("input"); input.inputMode = "numeric"; input.maxLength = 6; input.placeholder = "Enter the code"; input.setAttribute("aria-label", "Pairing code");
    const confirm = button("Confirm pairing", async () => {
      confirm.disabled = true;
      try { await daemon.confirmPairing(result.peer_id, input.value.trim()); pairingElement.hidden = true; await loadPeers(); }
      catch (error) { confirm.disabled = false; showError(`Pairing failed: ${error instanceof Error ? error.message : "try again"}`); }
    });
    const cancel = button("Cancel", () => { pairingElement.hidden = true; }, "secondary");
    pairingElement.append(title, text, code, expiry, input, confirm, cancel);
  } catch (error) { pairingElement.hidden = true; showError(`Could not start pairing: ${error instanceof Error ? error.message : "try again"}`); }
}

async function revokePeer(peer: Peer) {
  if (!window.confirm(`Revoke trust for ${peer.advertisement.alias || "this device"}?`)) return;
  clearError();
  try { await daemon.revoke(peer.advertisement.id); await loadPeers(); }
  catch (error) { showError(`Could not revoke trust: ${error instanceof Error ? error.message : "try again"}`); }
}

async function loadPeers() {
  peersElement.textContent = "Loading devices…";
  try { currentPeers = (await daemon.peers()).peers; renderPeers(); }
  catch (error) { peersElement.textContent = "Unable to load devices."; showError(`Peer list unavailable: ${error instanceof Error ? error.message : "try again"}`); }
}

async function checkHealth() {
  status.textContent = "Checking daemon…"; dot.className = "dot pending";
  try { const url = await daemon.endpoint(); endpoint.textContent = url; const health = await daemon.health(); identity.textContent = health.identity_id; status.textContent = `Daemon healthy · API v${health.version}`; dot.className = "dot online"; clearError(); await loadPeers(); }
  catch { status.textContent = "Daemon unavailable"; dot.className = "dot offline"; identity.textContent = "Unavailable"; showError("The local daemon is unavailable. Start it and try again."); }
}

document.querySelector("#refresh")!.addEventListener("click", () => void checkHealth());
void configureAutostart(); void checkHealth();
setInterval(() => void checkHealth(), 10_000);
