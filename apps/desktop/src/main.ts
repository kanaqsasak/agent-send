import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
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
      method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(peer),
    });
    // Deliberately retain only the human-verifiable code. Pairing secrets must
    // never be rendered, logged, or persisted by the desktop client.
    const { peer_id, code, expires_in_seconds } = await response.json() as PairingResponse & { pairing_secret?: unknown };
    return { peer_id, code, expires_in_seconds };
  },
  async confirmPairing(peerId, code) {
    await this.request("/v1/pairings/confirm", {
      method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ peer_id: peerId, code }),
    });
  },
  async revoke(peerId) { await this.request(`/v1/peers/${encodeURIComponent(peerId)}`, { method: "DELETE" }); },
};

const app = document.querySelector<HTMLDivElement>("#app")!;
app.innerHTML = `
  <section class="popover-shell" aria-label="agent-send">
    <header class="titlebar" data-tauri-drag-region>
      <span class="app-mark" aria-hidden="true"><svg viewBox="0 0 32 32" focusable="false"><g fill="none" stroke="currentColor" stroke-width="2.35" stroke-linecap="round" stroke-linejoin="round"><path d="M10.5 21.5 19.25 12.75"/><path d="M10.5 15.5A6 6 0 0 1 16.5 21.5M10.5 9.5A12 12 0 0 1 22.5 21.5"/></g><circle cx="10.5" cy="21.5" r="2.5" fill="currentColor"/></svg></span>
      <span class="app-name">agent-send</span>
    </header>

    <main class="content">
      <section class="service-summary" aria-live="polite">
        <span id="service-indicator" class="service-indicator pending" aria-hidden="true"></span>
        <div><strong id="status">Connecting to local service…</strong><span id="status-detail">Checking agent-send on this device</span></div>
        <button id="refresh" class="icon-button refresh-button" type="button" title="Refresh" aria-label="Refresh status">↻</button>
      </section>

      <p id="error" class="error" role="alert" hidden></p>

      <section class="device-card" aria-labelledby="device-heading">
        <div class="section-label" id="device-heading">THIS DEVICE</div>
        <div class="identity-row"><span class="device-glyph" aria-hidden="true">⌁</span><code id="identity">Looking up identity…</code></div>
      </section>

      <section class="peers-section" aria-labelledby="peers-heading">
        <div class="section-heading">
          <div><h1 id="peers-heading">Nearby devices</h1><p>Only paired devices can send files.</p></div>
          <span id="peer-count" class="peer-count" hidden></span>
        </div>
        <div id="peers" class="peers" aria-live="polite"><p class="loading-copy">Looking for devices…</p></div>
      </section>

      <section id="pairing" class="pairing-card" aria-live="polite" hidden></section>

      <section id="about" class="about-card" aria-labelledby="about-heading" hidden>
        <div class="about-heading"><div><div class="section-label">ABOUT</div><h2 id="about-heading">agent-send</h2></div><button id="close-about" class="icon-button" type="button" aria-label="Close about">×</button></div>
        <p>Private, local-first file transfer for your trusted devices.</p>
        <dl><div><dt>Version</dt><dd id="about-version">—</dd></div><div><dt>Service</dt><dd id="about-service">Checking…</dd></div></dl>
      </section>
    </main>

    <footer class="footer"><span>Local network only</span><button id="open-about" type="button">agent-send <span id="version">v—</span> · About</button></footer>
  </section>
`;

const status = document.querySelector<HTMLSpanElement>("#status")!;
const statusDetail = document.querySelector<HTMLSpanElement>("#status-detail")!;
const identity = document.querySelector<HTMLElement>("#identity")!;
const peersElement = document.querySelector<HTMLDivElement>("#peers")!;
const peerCount = document.querySelector<HTMLSpanElement>("#peer-count")!;
const errorElement = document.querySelector<HTMLParagraphElement>("#error")!;
const pairingElement = document.querySelector<HTMLElement>("#pairing")!;
const aboutElement = document.querySelector<HTMLElement>("#about")!;
const indicator = document.querySelector<HTMLSpanElement>("#service-indicator")!;
const refreshButton = document.querySelector<HTMLButtonElement>("#refresh")!;
const version = document.querySelector<HTMLSpanElement>("#version")!;
const aboutVersion = document.querySelector<HTMLElement>("#about-version")!;
const aboutService = document.querySelector<HTMLElement>("#about-service")!;
const nativeWindow = getCurrentWindow();
let currentPeers: Peer[] = [];
let serviceAvailable = false;
let refreshing = false;

function showError(message: string) { errorElement.textContent = message; errorElement.hidden = false; }
function clearError() { errorElement.hidden = true; errorElement.textContent = ""; }
function setServiceState(state: "pending" | "online" | "offline", headline: string, detail: string) {
  indicator.className = `service-indicator ${state}`;
  status.textContent = headline;
  statusDetail.textContent = detail;
  aboutService.textContent = state === "online" ? "Connected" : state === "offline" ? "Unavailable" : "Checking";
}
function button(label: string, action: () => void | Promise<void>, className = "") {
  const element = document.createElement("button");
  element.type = "button";
  element.textContent = label;
  element.className = className;
  element.addEventListener("click", () => void action());
  return element;
}
function deviceName(peer: Peer) { return peer.advertisement.alias.trim() || "Unnamed device"; }
function initials(name: string) { return name.trim().slice(0, 1).toUpperCase() || "?"; }

function renderPeers() {
  peersElement.replaceChildren();
  peerCount.hidden = currentPeers.length === 0;
  peerCount.textContent = `${currentPeers.length}`;
  if (!currentPeers.length) {
    const empty = document.createElement("div");
    empty.className = "empty-state";
    empty.innerHTML = "<span class=\"empty-icon\" aria-hidden=\"true\">⌁</span><strong>No nearby devices yet</strong><p>Devices on your local network will appear here.</p>";
    peersElement.append(empty);
    return;
  }
  for (const peer of currentPeers) {
    const name = deviceName(peer);
    const row = document.createElement("article");
    row.className = "peer-row";
    const avatar = document.createElement("span");
    avatar.className = "peer-avatar";
    avatar.setAttribute("aria-hidden", "true");
    avatar.textContent = initials(name);
    const details = document.createElement("div");
    details.className = "peer-details";
    const title = document.createElement("strong");
    title.textContent = name;
    const state = document.createElement("small");
    state.innerHTML = peer.trusted ? "<span class=\"online-dot\"></span>Trusted · online" : "Visible on this network · not paired";
    details.append(title, state);
    row.append(avatar, details);
    if (peer.trusted) row.append(button("Revoke", () => confirmRevoke(peer), "secondary-button"));
    else row.append(button("Pair", () => startPairing(peer), "primary-button"));
    peersElement.append(row);
  }
}

function hidePairing() { pairingElement.hidden = true; pairingElement.replaceChildren(); }

async function startPairing(peer: Peer) {
  if (!serviceAvailable) return;
  clearError();
  pairingElement.hidden = false;
  pairingElement.textContent = "Creating a secure pairing request…";
  try {
    const result = await daemon.requestPairing(peer.advertisement);
    const name = deviceName(peer);
    pairingElement.replaceChildren();
    const eyebrow = document.createElement("div"); eyebrow.className = "section-label"; eyebrow.textContent = "PAIR A DEVICE";
    const title = document.createElement("h2"); title.textContent = `Confirm ${name}`;
    const instruction = document.createElement("p"); instruction.textContent = "Compare this code with the code shown on the other device. Enter it below only when they match.";
    const code = document.createElement("strong"); code.className = "pairing-code"; code.textContent = result.code;
    const expiry = document.createElement("p"); expiry.className = "expiry"; expiry.textContent = `This code expires in ${Math.ceil(result.expires_in_seconds / 60)} minutes.`;
    const input = document.createElement("input");
    input.inputMode = "numeric"; input.autocomplete = "one-time-code"; input.maxLength = 6; input.pattern = "[0-9]{6}"; input.placeholder = "6-digit code";
    input.setAttribute("aria-label", "Code shown on the other device");
    const actions = document.createElement("div"); actions.className = "pairing-actions";
    const confirm = button("Confirm pairing", async () => {
      const enteredCode = input.value.trim();
      if (enteredCode.length !== 6) { input.focus(); showError("Enter the six-digit code shown on the other device."); return; }
      confirm.disabled = true;
      try { await daemon.confirmPairing(result.peer_id, enteredCode); hidePairing(); await loadPeers(); }
      catch (error) { confirm.disabled = false; showError(`Pairing failed: ${error instanceof Error ? error.message : "try again"}`); }
    }, "primary-button");
    confirm.disabled = true;
    const cancel = button("Cancel", hidePairing, "secondary-button");
    input.addEventListener("input", () => { input.value = input.value.replace(/\D/g, ""); confirm.disabled = input.value.length !== 6; });
    input.addEventListener("keydown", (event) => { if (event.key === "Enter" && !confirm.disabled) confirm.click(); });
    actions.append(confirm, cancel);
    pairingElement.append(eyebrow, title, instruction, code, expiry, input, actions);
    input.focus();
  } catch (error) {
    hidePairing();
    showError(`Could not start pairing: ${error instanceof Error ? error.message : "try again"}`);
  }
}

function confirmRevoke(peer: Peer) {
  clearError();
  pairingElement.hidden = false;
  pairingElement.replaceChildren();
  const eyebrow = document.createElement("div"); eyebrow.className = "section-label"; eyebrow.textContent = "REMOVE TRUST";
  const title = document.createElement("h2"); title.textContent = `Revoke ${deviceName(peer)}?`;
  const message = document.createElement("p"); message.textContent = "This device will need to be paired again before it can send files.";
  const actions = document.createElement("div"); actions.className = "pairing-actions";
  const revoke = button("Revoke device", () => revokePeer(peer), "danger-button");
  actions.append(revoke, button("Cancel", hidePairing, "secondary-button"));
  pairingElement.append(eyebrow, title, message, actions);
  revoke.focus();
}

async function revokePeer(peer: Peer) {
  clearError();
  try { await daemon.revoke(peer.advertisement.id); hidePairing(); await loadPeers(); }
  catch (error) { showError(`Could not revoke trust: ${error instanceof Error ? error.message : "try again"}`); }
}

async function loadPeers() {
  if (!serviceAvailable) return;
  peersElement.innerHTML = "<p class=\"loading-copy\">Looking for devices…</p>";
  try { currentPeers = (await daemon.peers()).peers; renderPeers(); }
  catch (error) {
    peersElement.innerHTML = "<p class=\"loading-copy\">Could not load nearby devices.</p>";
    showError(`Peer list unavailable: ${error instanceof Error ? error.message : "try again"}`);
  }
}

async function healthWithRetry(): Promise<Health> {
  let lastError: unknown;
  for (let attempt = 0; attempt < 12; attempt += 1) {
    try { return await daemon.health(); }
    catch (error) {
      lastError = error;
      if (attempt < 11) await new Promise((resolve) => window.setTimeout(resolve, 250));
    }
  }
  throw lastError instanceof Error ? lastError : new Error("service did not start");
}

async function checkHealth() {
  if (refreshing) return;
  refreshing = true;
  refreshButton.disabled = true;
  setServiceState("pending", "Checking local service…", "Looking for agent-send on this device");
  try {
    const health = await healthWithRetry();
    serviceAvailable = true;
    identity.textContent = health.identity_id;
    setServiceState("online", "Ready to send locally", `Service connected · API v${health.version}`);
    clearError();
    await loadPeers();
  } catch {
    serviceAvailable = false;
    currentPeers = [];
    identity.textContent = "Local service unavailable";
    peersElement.innerHTML = "<div class=\"empty-state offline-empty\"><span class=\"empty-icon\" aria-hidden=\"true\">!</span><strong>Service is offline</strong><p>Keep agent-send running, then refresh to reconnect.</p></div>";
    peerCount.hidden = true;
    hidePairing();
    setServiceState("offline", "Local service unavailable", "We’ll keep checking in the background");
    showError("Can’t reach the local agent-send service.");
  } finally {
    refreshing = false;
    refreshButton.disabled = false;
  }
}

function showAbout() { aboutElement.hidden = false; document.querySelector<HTMLButtonElement>("#close-about")?.focus(); }
function hideAbout() { aboutElement.hidden = true; document.querySelector<HTMLButtonElement>("#open-about")?.focus(); }
function hideWindow() { void nativeWindow.hide().catch(() => window.close()); }

void invoke<string>("app_version").then((value) => {
  version.textContent = `v${value}`;
  aboutVersion.textContent = `agent-send v${value}`;
}).catch(() => { aboutVersion.textContent = "Development build"; });

refreshButton.addEventListener("click", () => void checkHealth());
document.querySelector("#open-about")?.addEventListener("click", showAbout);
document.querySelector("#close-about")?.addEventListener("click", hideAbout);
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape") {
    if (!pairingElement.hidden) hidePairing();
    else if (!aboutElement.hidden) hideAbout();
    else hideWindow();
  }
  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "r") { event.preventDefault(); void checkHealth(); }
});
void listen("agent-send://show-about", showAbout).catch(() => { /* Browser development has no native event bridge. */ });
void configureAutostart();
void checkHealth();
setInterval(() => void checkHealth(), 10_000);
