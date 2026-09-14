import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { enable as enableAutostart } from "@tauri-apps/plugin-autostart";
import "./style.css";

type Health = { version: number; status: "ok"; identity_id: string };
type Peer = { advertisement: { id: string; alias: string; address: string; api_version: number }; trusted: boolean };
type PairingResponse = { peer_id: string; code: string; pairing_secret: string; expires_in_seconds: number };
interface DaemonClient {
  endpoint(): Promise<string>; request(path: string, options?: RequestInit): Promise<Response>; health(): Promise<Health>; peers(): Promise<{ peers: Peer[] }>;
  requestPairing(peer: Peer["advertisement"]): Promise<PairingResponse>;
  confirmPairing(peerId: string, code: string): Promise<void>;
}
const daemon: DaemonClient = {
  async endpoint() { try { return await invoke<string>("daemon_endpoint"); } catch { return import.meta.env.VITE_DAEMON_URL ?? "http://127.0.0.1:8765"; } },
  async request(path, options) { const response = await fetch(`${await this.endpoint()}${path}`, options); if (!response.ok) throw new Error(`HTTP ${response.status}`); return response; },
  async health() { return await (await this.request("/v1/health")).json() as Health; },
  async peers() { return await (await this.request("/v1/peers")).json() as { peers: Peer[] }; },
  async requestPairing(peer) { return await (await this.request("/v1/pairings", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(peer) })).json() as PairingResponse; },
  async confirmPairing(peerId, code) { await this.request("/v1/pairings/confirm", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ peer_id: peerId, code }) }); },
};

type Screen = "empty" | "discovering" | "devices" | "no-devices" | "connected" | "composing" | "selected" | "sending" | "success" | "settings" | "error";
type QueueItem = { name: string; detail: string; kind: "file" | "folder" };
const app = document.querySelector<HTMLDivElement>("#app")!;
let screen: Screen = "empty";
let peers: Peer[] = [];
let selectedPeer: Peer | null = null;
let queue: QueueItem[] = [];
let message = "";
let pairing: { peer: Peer; request: PairingResponse } | null = null;
let health: Health | null = null;
let errorMessage = "";
let refreshTimer: number | undefined;
const nativeWindow = (() => { try { return getCurrentWindow(); } catch { return { hide: async () => undefined } as ReturnType<typeof getCurrentWindow>; } })();

function escape(value: string) { return value.replace(/[&<>\"']/g, (char) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", "\"": "&quot;", "'": "&#039;" }[char]!)); }
function name(peer: Peer) { return peer.advertisement.alias.trim() || "Unnamed device"; }
function icon(kind: "laptop" | "tablet" | "desktop" | "file" | "folder" | "text" | "shield" | "arrow") {
  const paths: Record<string, string> = { laptop: '<rect x="3" y="4" width="18" height="13" rx="2"/><path d="M1 20h22"/>', tablet: '<rect x="6" y="2" width="12" height="20" rx="2"/><path d="M11 19h2"/>', desktop: '<rect x="3" y="3" width="18" height="14" rx="2"/><path d="M8 21h8M12 17v4"/>', file: '<path d="M6 2h8l4 4v16H6z"/><path d="M14 2v5h5"/>', folder: '<path d="M3 6h7l2 2h9v12H3z"/>', text: '<path d="M5 4h14M5 8h14M5 12h9M5 16h14"/>', shield: '<path d="M12 3 20 6v5c0 5-3.4 8.5-8 10-4.6-1.5-8-5-8-10V6z"/><path d="m8 12 2.5 2.5L16 9"/>', arrow: '<path d="M5 17c4-1 5-5 5-9 0-2 1-3 3-3h6"/><path d="M5 17c2-1 4 0 5 2"/><path d="M13 5h5v5"/>' };
  return `<svg viewBox="0 0 24 24" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">${paths[kind]}</svg>`;
}
function deviceIcon(peer: Peer) { const value = name(peer).toLowerCase(); return value.includes("ipad") || value.includes("phone") ? "tablet" : value.includes("pc") || value.includes("desktop") ? "desktop" : "laptop"; }
function setScreen(next: Screen) { screen = next; if (next !== "error") errorMessage = ""; render(); }
function shell(content: string, title = "agent-send") { return `<section class="shell" aria-label="${title}"><header class="header" data-tauri-drag-region><div class="brand"><span class="brand-mark">${icon("arrow")}</span><strong>AgentSend</strong></div><div class="header-actions"><span class="privacy"><span class="shield">${icon("shield")}</span>Local network only</span><button class="icon-btn" data-action="settings" aria-label="Settings">⚙</button><button class="icon-btn" data-action="about" aria-label="More options">⋮</button></div></header>${content}</section>`; }
function primary(label: string, action: string, extra = "") { return `<button class="btn primary ${extra}" data-action="${action}">${label}</button>`; }
function secondary(label: string, action: string, extra = "") { return `<button class="btn secondary ${extra}" data-action="${action}">${label}</button>`; }
function deviceHeader(peer: Peer) { return `<div class="connected-device"><span class="device-icon">${icon(deviceIcon(peer))}</span><div><strong>${escape(name(peer))}</strong><span class="state"><i></i> Connected</span></div><button class="icon-btn" data-action="disconnect" aria-label="Disconnect">×</button></div>`; }
function render() {
  let content = "";
  if (screen === "connected" || screen === "composing" || screen === "selected" || screen === "sending" || screen === "success") content = connectedView();
  else if (screen === "devices" || screen === "discovering" || screen === "no-devices") content = devicesView();
  else if (screen === "settings") content = settingsView();
  else if (screen === "error") content = errorView();
  else content = emptyView();
  app.innerHTML = shell(content);
}
function emptyView() { return `<main class="content empty-content"><div class="hero-icon">${icon("arrow")}</div><h1>Send content<br>to another device</h1><p>Files, folders, and text.<br>Private on your local network.</p><div class="actions">${primary("Connect a device", "discover")}<button class="text-btn" data-action="how">How it works <span>→</span></button></div></main>`; }
function devicesView() {
  const heading = screen === "discovering" ? "Looking for devices…" : screen === "no-devices" ? "No devices found" : "Choose a device";
  const copy = screen === "no-devices" ? "Open agent-send on the other device and keep both devices on the same network." : "Select a trusted device, or pair a new one once.";
  const rows = screen === "discovering" ? `<div class="searching"><span class="spinner"></span><span>Scanning your local network</span></div>` : peers.length ? peers.map((peer) => `<article class="device-row"><span class="device-icon">${icon(deviceIcon(peer))}</span><div class="device-info"><strong>${escape(name(peer))}</strong><span class="state ${peer.trusted ? "trusted" : "unpaired"}"><i></i>${peer.trusted ? "Paired" : "Not paired"}</span></div>${peer.trusted ? primary("Connect", `connect:${escape(peer.advertisement.id)}`) : secondary("Pair", `pair:${escape(peer.advertisement.id)}`)}</article>`).join("") : `<div class="no-devices"><span class="soft-icon">${icon("laptop")}</span><strong>No nearby devices yet</strong><p>Make sure agent-send is open on both devices.</p></div>`;
  return `<main class="content"><button class="back-btn" data-action="back">← Back</button><div class="view-heading"><div><h1>${heading}</h1><p>${copy}</p></div>${peers.length && screen === "devices" ? `<span class="count">${peers.length}</span>` : ""}</div><div class="device-list">${rows}</div>${screen === "no-devices" ? primary("Try again", "discover") : screen === "devices" ? secondary("Refresh devices", "discover", "wide") : secondary("Cancel", "back", "wide")}</main>`;
}
function connectedView() {
  if (!selectedPeer) return emptyView();
  if (screen === "success") return `<main class="content">${deviceHeader(selectedPeer)}<div class="success-state"><span class="success-icon">✓</span><h1>Sent successfully</h1><p>${queue.length} ${queue.length === 1 ? "item" : "items"} sent to ${escape(name(selectedPeer))}</p></div><div class="actions">${primary("Send more", "clear-queue")} ${secondary("Close", "hide")}</div></main>`;
  if (screen === "sending") return `<main class="content">${deviceHeader(selectedPeer)}<div class="progress-card"><div class="eyebrow">SENDING</div><h2>${escape(queue[0]?.name ?? "Your content")}</h2><p>Sending to ${escape(name(selectedPeer))}</p><div class="progress"><span></span></div><div class="progress-meta"><span>Preparing transfer…</span><span>0%</span></div></div>${queue.length > 1 ? `<p class="muted-center">${queue.length - 1} more items</p>` : ""}${secondary("Cancel", "connected", "wide")}</main>`;
  if (screen === "composing") return `<main class="content">${deviceHeader(selectedPeer)}<div class="eyebrow">SEND TEXT</div><h1 class="small-title">Send text to ${escape(name(selectedPeer))}</h1><textarea id="message" class="text-composer" autofocus placeholder="Type or paste something…">${escape(message)}</textarea><div class="actions-row">${secondary("Cancel", "connected")}${primary("Send text", "send-text")}</div></main>`;
  if (screen === "selected") return `<main class="content">${deviceHeader(selectedPeer)}<div class="eyebrow">READY TO SEND</div><h1 class="small-title">Send to ${escape(name(selectedPeer))}</h1><div class="type-tabs"><button data-action="pick-file">${icon("file")} Files</button><button data-action="pick-folder">${icon("folder")} Folder</button><button data-action="compose">${icon("text")} Text</button></div><div class="queue">${queue.map((item, index) => `<div class="queue-item"><span class="queue-kind">${icon(item.kind === "folder" ? "folder" : "file")}</span><div><strong>${escape(item.name)}</strong><span>${escape(item.detail)}</span></div><button data-action="remove:${index}" aria-label="Remove ${escape(item.name)}">×</button></div>`).join("")}</div><div class="actions">${primary(`Send ${queue.length} ${queue.length === 1 ? "item" : "items"}`, "send")} ${secondary("Add more", "pick-file", "wide")}</div></main>`;
  return `<main class="content">${deviceHeader(selectedPeer)}<div class="drop-zone" data-action="pick-file" role="button" tabindex="0"><div class="drop-icons"><span>${icon("file")}</span><span>${icon("folder")}</span><span>${icon("text")}</span></div><h1>Choose something to send</h1><p>Files, folders, or text</p><div class="drop-hint">or drag and drop here</div></div><div class="actions"><button class="btn primary large" data-action="pick-file">Choose files</button><div class="quick-actions">${secondary("Folder", "pick-folder")} ${secondary("Text", "compose")}</div></div></main>`;
}
function settingsView() { return `<main class="content settings-view"><button class="back-btn" data-action="back">← Back</button><div class="view-heading"><div><div class="eyebrow">PREFERENCES</div><h1>Settings</h1><p>Keep AgentSend ready in your tray.</p></div></div><label class="setting-row"><span><strong>Device name</strong><small>How other devices see you</small></span><input value="This device" aria-label="Device name"></label><div class="setting-link"><span><strong>Paired devices</strong><small>Manage trusted devices</small></span><span>›</span></div><div class="setting-link"><span><strong>Help &amp; about</strong><small>Learn how AgentSend works</small></span><span>›</span></div></main>`; }
function errorView() { return `<main class="content centered"><span class="error-icon">!</span><h1>Something went wrong</h1><p>${escape(errorMessage || "The device may have turned off or left the network.")}</p><div class="actions">${primary("Try again", "connect-again")} ${secondary("Back", "back")}</div></main>`; }
function findPeer(id: string) { return peers.find((peer) => peer.advertisement.id === id) ?? null; }
async function discover() { setScreen("discovering"); try { health ??= await daemon.health(); peers = (await daemon.peers()).peers; setScreen(peers.length ? "devices" : "no-devices"); } catch { errorMessage = "Make sure agent-send is open on both devices and both are on the same network."; setScreen("error"); } }
async function pair(peer: Peer) { try { pairing = { peer, request: await daemon.requestPairing(peer.advertisement) }; renderPairing(); } catch { errorMessage = "Couldn't start pairing. Make sure the other device is still available."; setScreen("error"); } }
function renderPairing() { if (!pairing) return; app.innerHTML = shell(`<main class="content centered pairing"><button class="back-btn" data-action="cancel-pair">← Back</button><span class="pair-icon">${icon("shield")}</span><div class="eyebrow">PAIR A DEVICE</div><h1>Pair with ${escape(name(pairing.peer))}</h1><p>Confirm this code matches the one shown on the other device.</p><div class="pair-code">${pairing.request.code.split("").map((digit) => `<span>${digit}</span>`).join("")}</div><small>Pair once to send content easily next time.</small><div class="actions-row">${secondary("Cancel", "back")}${primary("Confirm pairing", "confirm-pair")}</div></main>`); }
async function confirmPair() { if (!pairing) return; try { await daemon.confirmPairing(pairing.request.peer_id, pairing.request.code); selectedPeer = pairing.peer; pairing = null; screen = "connected"; await refresh(); } catch { errorMessage = "Couldn't pair with this device. Make sure agent-send is still open on both devices."; pairing = null; setScreen("error"); } }
async function refresh() { if (pairing || screen === "composing") return; try { health = await daemon.health(); peers = (await daemon.peers()).peers; if (selectedPeer) { const refreshed = findPeer(selectedPeer.advertisement.id); if (!refreshed) { selectedPeer = null; queue = []; errorMessage = "The connected device is no longer available."; setScreen("error"); return; } selectedPeer = refreshed; } if (!selectedPeer && screen === "empty") render(); else if (screen !== "error") render(); } catch { if (screen !== "empty") { errorMessage = "The local service is unavailable."; setScreen("error"); } } }
function addFiles(files: File[]) { queue.push(...files.map((file) => ({ name: file.name, detail: `${(file.size / 1024 / 1024).toFixed(1)} MB`, kind: "file" as const }))); if (queue.length) setScreen("selected"); }
function chooseFiles() { const input = document.createElement("input"); input.type = "file"; input.multiple = true; input.onchange = () => addFiles(Array.from(input.files ?? [])); input.click(); }
function chooseFolder() { const input = document.createElement("input"); input.type = "file"; input.multiple = true; input.setAttribute("webkitdirectory", ""); input.onchange = () => { const files = Array.from(input.files ?? []); if (files.length) { queue.push({ name: files[0].webkitRelativePath?.split("/")[0] ?? "Selected folder", detail: `${files.length} files`, kind: "folder" }); setScreen("selected"); } }; input.click(); }
function send() { if (!queue.length) return; errorMessage = "Transfer transport is not connected to the new send flow yet."; setScreen("error"); }
app.addEventListener("click", (event) => { const target = (event.target as HTMLElement).closest<HTMLElement>("[data-action]"); if (!target) return; const action = target.dataset.action!; if (action === "discover" || action === "connect-again") void discover(); else if (action === "back") { pairing = null; selectedPeer = null; setScreen("empty"); } else if (action === "cancel-pair") { pairing = null; setScreen(peers.length ? "devices" : "no-devices"); } else if (action === "pick-file") chooseFiles(); else if (action === "pick-folder") chooseFolder(); else if (action === "compose") { message = ""; setScreen("composing"); } else if (action === "send-text") { message = (document.querySelector<HTMLTextAreaElement>("#message")?.value ?? "").trim(); if (message) { queue = [{ name: "Text message", detail: `${message.length} characters`, kind: "file" }]; send(); } } else if (action === "send") send(); else if (action === "connected") setScreen("connected"); else if (action === "disconnect") { selectedPeer = null; queue = []; setScreen("empty"); } else if (action === "clear-queue") { queue = []; setScreen("connected"); } else if (action === "hide") void nativeWindow.hide(); else if (action === "confirm-pair") void confirmPair(); else if (action.startsWith("pair:")) { const peer = findPeer(action.slice(5)); if (peer) void pair(peer); } else if (action.startsWith("connect:")) { selectedPeer = findPeer(action.slice(8)); if (selectedPeer) setScreen("connected"); } else if (action.startsWith("remove:")) { const index = Number(action.slice(7)); if (Number.isInteger(index)) queue.splice(index, 1); if (queue.length) render(); else setScreen("connected"); } else if (action === "settings") setScreen("settings"); else if (action === "about" || action === "how") { errorMessage = "AgentSend sends files, folders, and text directly between trusted devices on your local network."; setScreen("error"); } });
app.addEventListener("input", (event) => { if ((event.target as HTMLElement).id === "message") message = (event.target as HTMLTextAreaElement).value; });
app.addEventListener("dragover", (event) => { if ((event.target as HTMLElement).closest(".drop-zone")) event.preventDefault(); });
app.addEventListener("drop", (event) => { if (!(event.target as HTMLElement).closest(".drop-zone")) return; event.preventDefault(); addFiles(Array.from(event.dataTransfer?.files ?? [])); });
app.addEventListener("keydown", (event) => { if ((event.key === "Enter" || event.key === " ") && (event.target as HTMLElement).classList.contains("drop-zone")) { event.preventDefault(); chooseFiles(); return; } if (event.key === "Escape") { if (screen === "composing") setScreen("connected"); else void nativeWindow.hide(); } if ((event.metaKey || event.ctrlKey) && event.key === "Enter" && screen === "composing") { event.preventDefault(); document.querySelector<HTMLElement>("[data-action='send-text']")?.click(); } });
void enableAutostart().catch(() => {});
void refresh();
refreshTimer = window.setInterval(() => void refresh(), 10_000);
window.addEventListener("beforeunload", () => { if (refreshTimer) window.clearInterval(refreshTimer); });
