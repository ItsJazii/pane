import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getVersion } from "@tauri-apps/api/app";

// Injected by vite.config.ts at build time, e.g. "0707.1432".
declare const __BUILD_STAMP__: string;

// Official provider marks from the MIT-licensed macOS OpenUsage, rendered
// inline so CSS can recolor them like template icons.
import antigravityIcon from "./assets/providers/antigravity.svg?raw";
import claudeIcon from "./assets/providers/claude.svg?raw";
import codexIcon from "./assets/providers/codex.svg?raw";
import copilotIcon from "./assets/providers/copilot.svg?raw";
import cursorIcon from "./assets/providers/cursor.svg?raw";
import devinIcon from "./assets/providers/devin.svg?raw";
import grokIcon from "./assets/providers/grok.svg?raw";
import minimaxIcon from "./assets/providers/minimax.svg?raw";
import opencodeIcon from "./assets/providers/opencode.svg?raw";
import openrouterIcon from "./assets/providers/openrouter.svg?raw";
import openusageIcon from "./assets/providers/openusage.svg?raw";
import zaiIcon from "./assets/providers/zai.svg?raw";

const PROVIDER_ICONS: Record<string, string> = {
  antigravity: antigravityIcon,
  claude: claudeIcon,
  codex: codexIcon,
  copilot: copilotIcon,
  cursor: cursorIcon,
  devin: devinIcon,
  grok: grokIcon,
  minimax: minimaxIcon,
  opencode: opencodeIcon,
  openrouter: openrouterIcon,
  zai: zaiIcon,
};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

interface Metric {
  label: string;
  kind: string;
  used_percent: number | null;
  detail: string | null;
  value: string | null;
  resets_at: number | null;
  period_ms: number | null;
}

interface Snapshot {
  id: string;
  name: string;
  plan: string | null;
  status: string;
  error: string | null;
  metrics: Metric[];
  stale: boolean;
  warning: string | null;
}

interface ModelSpend {
  model: string;
  cost: number;
  tokens: number;
}

interface SpendWindow {
  cost: number;
  tokens: number;
  models: ModelSpend[];
}

interface ProviderSpend {
  id: string;
  name: string;
  today: SpendWindow;
  yesterday: SpendWindow;
  last30: SpendWindow;
  trend: number[];
  unpriced: number;
  unpriced_models: string[];
}

/// ⚠ shown when events were excluded because no catalog prices the model —
/// totals would otherwise silently under-report.
function unpricedWarn(sp: ProviderSpend | undefined): string {
  if (!sp || sp.unpriced <= 0) return "";
  const models = sp.unpriced_models.join(", ") || "unknown models";
  return `<span class="stale" title="${escapeHtml(
    `${sp.unpriced} events excluded — no price known for: ${models}. Totals may under-report.`,
  )}">⚠</span>`;
}

type SpendTab = "today" | "yesterday" | "last30";

// Per-provider layout: which rows show, their order, which are tucked
// behind the caret ("On Demand"), and which are starred for the tray strip.
interface ProviderLayout {
  metricOrder: string[];
  onDemand: string[];
  hidden: string[];
  starred: string[];
  expanded: boolean;
}

interface Layout {
  providerOrder: string[];
  providers: Record<string, ProviderLayout>;
}

interface Config {
  refreshMinutes: number;
  disabled: string[];
  pinned: { provider: string; label: string } | null;
  trayProviders: string[];
  pacingAlways: boolean;
  notifyAlmostOut: boolean;
  notifyCuttingClose: boolean;
  notifyWillRunOut: boolean;
  spendTab: SpendTab;
  showUsed: boolean;
  resetExact: boolean;
  timeFormat: "auto" | "12" | "24";
  layout: Layout | null;
  appearance: "system" | "light" | "dark";
  density: "regular" | "compact";
  shortcut: string;
  proxy: { enabled: boolean; url: string };
  showTotalSpend: boolean;
  welcomeDismissed: boolean;
}

const ALL_PROVIDERS: [string, string][] = [
  ["claude", "Claude"],
  ["codex", "Codex"],
  ["cursor", "Cursor"],
  ["opencode", "OpenCode"],
  ["copilot", "Copilot"],
  ["grok", "Grok"],
  ["devin", "Devin"],
  ["minimax", "MiniMax"],
  ["openrouter", "OpenRouter"],
  ["zai", "Z.ai"],
  ["antigravity", "Antigravity"],
  ["deepseek", "DeepSeek"],
  ["moonshot", "Moonshot"],
  ["elevenlabs", "ElevenLabs"],
  ["deepgram", "Deepgram"],
  ["openai", "OpenAI"],
  ["venice", "Venice"],
  ["ollama", "Ollama"],
];

// Same quick links the Mac app ships (status pages + vendor dashboards).
const PROVIDER_LINKS: Record<string, { label: string; url: string }[]> = {
  claude: [
    { label: "Status", url: "https://status.anthropic.com/" },
    { label: "Dashboard", url: "https://claude.ai/settings/usage" },
  ],
  codex: [
    { label: "Status", url: "https://status.openai.com/" },
    { label: "Dashboard", url: "https://chatgpt.com/codex/settings/usage" },
  ],
  cursor: [
    { label: "Status", url: "https://status.cursor.com/" },
    { label: "Dashboard", url: "https://www.cursor.com/dashboard" },
  ],
  copilot: [
    { label: "Status", url: "https://www.githubstatus.com/" },
    { label: "Dashboard", url: "https://github.com/settings/billing" },
  ],
  grok: [{ label: "Usage", url: "https://grok.com/?_s=usage" }],
  devin: [{ label: "Dashboard", url: "https://app.devin.ai/settings/plans" }],
  minimax: [{ label: "Platform", url: "https://platform.minimax.io/" }],
  openrouter: [
    { label: "Activity", url: "https://openrouter.ai/activity" },
    { label: "Credits", url: "https://openrouter.ai/settings/credits" },
  ],
  zai: [
    { label: "Dashboard", url: "https://z.ai/manage-apikey/coding-plan/personal/my-plan" },
    { label: "API Keys", url: "https://z.ai/manage-apikey/apikey-list" },
  ],
  opencode: [{ label: "Console", url: "https://opencode.ai/console" }],
  deepseek: [
    { label: "Status", url: "https://status.deepseek.com/" },
    { label: "Platform", url: "https://platform.deepseek.com/usage" },
  ],
  moonshot: [{ label: "Console", url: "https://platform.moonshot.ai/console" }],
  elevenlabs: [
    { label: "Status", url: "https://status.elevenlabs.io/" },
    { label: "Usage", url: "https://elevenlabs.io/app/usage" },
  ],
  deepgram: [
    { label: "Status", url: "https://status.deepgram.com/" },
    { label: "Console", url: "https://console.deepgram.com/" },
  ],
  openai: [
    { label: "Status", url: "https://status.openai.com/" },
    { label: "Usage", url: "https://platform.openai.com/usage" },
  ],
  venice: [{ label: "Dashboard", url: "https://venice.ai/settings/api" }],
  ollama: [{ label: "Library", url: "https://ollama.com/library" }],
};

// Brand palette for the Total Spend ring (Mac parity); unknown providers
// get a stable hue derived from their id.
const SPEND_COLORS: Record<string, string> = {
  claude: "#de7356",
  codex: "#10a37f",
  openrouter: "#6467f2",
  antigravity: "#4285f4",
  copilot: "#a855f7",
  minimax: "#f5433c",
  grok: "#8b93a1",
  opencode: "#b7b1b1",
  cursor: "#e8eaed",
};

function spendColor(id: string): string {
  const fixed = SPEND_COLORS[id];
  if (fixed) return fixed;
  let hash = 0;
  for (const ch of id) hash = (hash * 31 + ch.charCodeAt(0)) >>> 0;
  return `hsl(${hash % 360} 62% 58%)`;
}

const SPEND_KEYS: [string, SpendTab][] = [
  ["Today", "today"],
  ["Yesterday", "yesterday"],
  ["Last 30 Days", "last30"],
];
const TREND_KEY = "Usage Trend";
const DIVIDER = "__ondemand__";

const STALE_MS = 60 * 1000;
let config: Config = {
  refreshMinutes: 5,
  disabled: [],
  pinned: null,
  trayProviders: [],
  pacingAlways: false,
  notifyAlmostOut: false,
  notifyCuttingClose: false,
  notifyWillRunOut: false,
  spendTab: "today",
  showUsed: false,
  resetExact: false,
  timeFormat: "auto",
  layout: null,
  appearance: "system",
  density: "regular",
  shortcut: "",
  proxy: { enabled: false, url: "" },
  showTotalSpend: true,
  welcomeDismissed: false,
};
let lastFetch = 0;
let refreshing = false;
let refreshTimer: number | undefined;
let lastSnapshots: Snapshot[] = [];
let lastSpend: ProviderSpend[] = [];
let spendLoaded = false;
let spendTab: SpendTab = "today";
let customizeOpen = false;
let revealTimer = 0;
let animateExpandId: string | null = null;

/// One pass of entrance animations (cards slide in, bars fill) — played when
/// the popover opens or the first data lands, never on background re-renders.
function playReveal(): void {
  const el = document.querySelector<HTMLElement>("#providers");
  if (!el) return;
  el.classList.remove("reveal");
  void el.offsetWidth; // restart CSS animations
  el.classList.add("reveal");
  clearTimeout(revealTimer);
  revealTimer = window.setTimeout(() => el.classList.remove("reveal"), 950);
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

function escapeHtml(text: string): string {
  return text.replace(/[&<>"']/g, (c) => {
    const map: Record<string, string> = {
      "&": "&amp;",
      "<": "&lt;",
      ">": "&gt;",
      '"': "&quot;",
      "'": "&#39;",
    };
    return map[c];
  });
}

function clampPercent(value: number): number {
  return Math.min(100, Math.max(0, value));
}

function fmtMoney(v: number): string {
  if (v >= 1000) return `$${(v / 1000).toFixed(1)}K`;
  return `$${v.toFixed(2)}`;
}

function fmtTokens(v: number): string {
  if (v >= 1e9) return `${(v / 1e9).toFixed(1)}B`;
  if (v >= 1e6) return `${(v / 1e6).toFixed(1)}M`;
  if (v >= 1e3) return `${(v / 1e3).toFixed(1)}K`;
  return String(Math.round(v));
}

function fmtDuration(ms: number): string {
  const mins = Math.max(1, Math.round(ms / 60000));
  const days = Math.floor(mins / 1440);
  const hours = Math.floor((mins % 1440) / 60);
  const rem = mins % 60;
  if (days > 0) return `${days}d ${hours}h`;
  if (hours > 0) return `${hours}h ${String(rem).padStart(2, "0")}m`;
  return `${rem}m`;
}

// "today at 6:38 PM" / "tomorrow at 18:38" / "Sat, Jul 11 at 9:00 AM",
// honoring the Time Format setting.
function fmtExact(ts: number): string {
  const d = new Date(ts);
  const now = new Date();
  const hour12 =
    config.timeFormat === "12" ? true : config.timeFormat === "24" ? false : undefined;
  const time = d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit", hour12 });
  const dayStart = (x: Date) => new Date(x.getFullYear(), x.getMonth(), x.getDate()).getTime();
  const diffDays = Math.round((dayStart(d) - dayStart(now)) / 86400000);
  if (diffDays === 0) return `today at ${time}`;
  if (diffDays === 1) return `tomorrow at ${time}`;
  return `${d.toLocaleDateString([], { weekday: "short", month: "short", day: "numeric" })} at ${time}`;
}

async function patchConfig(patch: Partial<Config>): Promise<void> {
  config = await invoke<Config>("set_config", { patch });
}

// ---------------------------------------------------------------------------
// Layout: defaults, repair, persistence
// ---------------------------------------------------------------------------

function defaultProviderLayout(s: Snapshot | undefined, spend: ProviderSpend | undefined, migrateStar: boolean): ProviderLayout {
  const order: string[] = [];
  const onDemand: string[] = [];
  for (const m of s?.metrics ?? []) {
    order.push(m.label);
    if (m.kind !== "progress") onDemand.push(m.label); // balances etc. tuck away
  }
  if (spend) {
    order.push(TREND_KEY); // trend stays always-visible, like the Mac
    for (const [label] of SPEND_KEYS) {
      order.push(label);
      onDemand.push(label);
    }
  }
  const starred = migrateStar
    ? (s?.metrics ?? []).filter((m) => m.kind === "progress").slice(0, 2).map((m) => m.label)
    : [];
  return { metricOrder: order, onDemand, hidden: [], starred, expanded: false };
}

function rankSnapshot(s: Snapshot): number {
  const FREE = /free|trial/i;
  if (s.status === "ok") {
    if (s.plan && !FREE.test(s.plan)) return 0;
    if (s.plan) return 2;
    return 1;
  }
  return s.status === "error" ? 3 : 4;
}

/// Builds the layout on first run and folds in newly-appeared providers or
/// metrics afterwards. Saves only when something actually changed.
function ensureLayout(): void {
  let changed = false;
  let layout = config.layout;

  if (!layout) {
    const orderedIds = [...lastSnapshots].sort((a, b) => rankSnapshot(a) - rankSnapshot(b)).map((s) => s.id);
    for (const [id] of ALL_PROVIDERS) if (!orderedIds.includes(id)) orderedIds.push(id);
    layout = { providerOrder: orderedIds, providers: {} };
    changed = true;
  }

  for (const s of lastSnapshots) {
    if (!layout.providerOrder.includes(s.id)) {
      layout.providerOrder.push(s.id);
      changed = true;
    }
    const spend = lastSpend.find((sp) => sp.id === s.id);
    let L = layout.providers[s.id];
    if (!L) {
      // One-time migration: providers picked in the old tray-strip setting
      // become starred so the strip carries over.
      L = defaultProviderLayout(s, spend, config.trayProviders.includes(s.id));
      layout.providers[s.id] = L;
      changed = true;
      continue;
    }
    // New metrics ship once; spend rows appear when spend data first exists.
    for (const m of s.metrics) {
      if (!L.metricOrder.includes(m.label)) {
        L.metricOrder.push(m.label);
        if (m.kind !== "progress") L.onDemand.push(m.label);
        changed = true;
      }
    }
    if (spend) {
      if (!L.metricOrder.includes(TREND_KEY)) {
        L.metricOrder.push(TREND_KEY);
        changed = true;
      }
      for (const [label] of SPEND_KEYS) {
        if (!L.metricOrder.includes(label)) {
          L.metricOrder.push(label);
          L.onDemand.push(label);
          changed = true;
        }
      }
    }
  }

  config.layout = layout;
  if (changed) void patchConfig({ layout });
}

function providerLayout(id: string): ProviderLayout {
  return (
    config.layout?.providers[id] ?? {
      metricOrder: [],
      onDemand: [],
      hidden: [],
      starred: [],
      expanded: false,
    }
  );
}

function saveLayout(): void {
  if (!config.layout) return;
  // Undo history: remember the state we're moving away from.
  const next = JSON.stringify(config.layout);
  if (lastLayoutSnapshot && lastLayoutSnapshot !== next) {
    undoStack.push(lastLayoutSnapshot);
    if (undoStack.length > 50) undoStack.shift();
  }
  lastLayoutSnapshot = next;
  void patchConfig({ layout: config.layout });
}

// ---------------------------------------------------------------------------
// Pace engine (unchanged from Wave 1/4)
// ---------------------------------------------------------------------------

interface Pace {
  cls: string;
  note: string;
  noteClass: string;
  title: string;
  tick: number | null;
}

function computePace(m: Metric): Pace {
  const used = clampPercent(m.used_percent ?? 0);
  const left = 100 - used;
  const none: Pace = { cls: "", note: "", noteClass: "", title: "", tick: null };

  if (left < 0.5) {
    return { cls: "low", note: "🔥 Limit reached", noteClass: "danger", title: "Limit reached", tick: null };
  }

  const byLevel = (): Pace => {
    if (left <= 10) return { ...none, cls: "low", title: `${Math.round(left)}% left` };
    if (used >= 80) return { ...none, cls: "warn", title: `${Math.round(used)}% used` };
    return none;
  };
  if (!m.resets_at || !m.period_ms) return byLevel();

  const now = Date.now();
  const remainMs = Math.max(0, m.resets_at - now);
  const elapsedMs = m.period_ms - remainMs;
  const frac = elapsedMs / m.period_ms;
  if (frac < 0.05 || elapsedMs < 5 * 60000) return byLevel();

  const projected = used / frac;
  const tick = clampPercent(frac * 100);

  if (projected >= 100) {
    const over = Math.round(projected - 100);
    const runOutAt = now + (left * elapsedMs) / used;
    if (runOutAt < m.resets_at - 60000) {
      const when = config.resetExact
        ? `Limit ${fmtExact(runOutAt)}`
        : `Limit in ${fmtDuration(runOutAt - now)}`;
      return { cls: "low", note: `🔥 ${when}`, noteClass: "danger", title: `~${over}% over limit at reset`, tick };
    }
    return { cls: "low", note: "🔥", noteClass: "danger", title: "~100% used at reset", tick };
  }

  const spare = Math.max(1, Math.round(100 - projected));
  if (projected >= 90) {
    return {
      cls: "warn",
      note: `~${spare}% spare`,
      noteClass: "warn",
      title: `~${Math.round(projected)}% used at reset`,
      tick,
    };
  }
  return {
    cls: "",
    note: config.pacingAlways ? `~${spare}% left at reset` : "",
    noteClass: "",
    title: `~${spare}% left at reset`,
    tick: config.pacingAlways ? tick : null,
  };
}

// ---------------------------------------------------------------------------
// Dashboard rendering
// ---------------------------------------------------------------------------

function renderMetric(m: Metric): string {
  if (m.kind === "progress" && m.used_percent !== null) {
    const used = clampPercent(m.used_percent);
    const left = Math.round(100 - used);
    const pace = computePace(m);
    const tick =
      pace.tick !== null && pace.tick > 1 && pace.tick < 99
        ? `<span class="tick" style="left:${pace.tick}%"></span>`
        : "";
    const note = pace.note
      ? `<span class="pace-note ${pace.noteClass}" title="${escapeHtml(pace.title)}">${escapeHtml(pace.note)}</span>`
      : "";
    const headline = config.showUsed ? `${Math.round(used)}% used` : `${left}% left`;
    const headlineAlt = config.showUsed ? `${left}% left` : `${Math.round(used)}% used`;

    let resetHtml = "";
    if (m.resets_at !== null && m.resets_at > Date.now()) {
      const countdown = `Resets in ${fmtDuration(m.resets_at - Date.now())}`;
      const exact = `Resets ${fmtExact(m.resets_at)}`;
      const [text, alt] = config.resetExact ? [exact, countdown] : [countdown, exact];
      resetHtml = `<span class="clickable" data-flip="reset" title="${escapeHtml(alt)}">${escapeHtml(text)}</span>`;
    }
    const detailHtml = [m.detail ? escapeHtml(m.detail) : "", resetHtml].filter(Boolean).join(" · ");
    return `
      <div class="metric">
        <div class="metric-head">
          <span class="metric-label">${escapeHtml(m.label)}</span>
          ${note}
        </div>
        <div class="bar" title="${escapeHtml(pace.title)}">
          <div class="fill ${pace.cls}" style="width:${used}%"></div>
          ${tick}
        </div>
        <div class="metric-foot">
          <span class="left-val clickable" data-flip="usage" title="${escapeHtml(headlineAlt)}">${headline}</span>
          <span class="detail">${detailHtml}</span>
        </div>
      </div>`;
  }
  // Actionable row (Codex reset credits): exact expiry + a Use button that
  // spends the credit after a confirm. A credit dying within 24h gets an
  // amber dot so it isn't wasted.
  if (m.kind === "action" && m.detail) {
    const expiry =
      m.resets_at !== null
        ? `Expires ${fmtExact(m.resets_at)}`
        : (m.value ?? "Available");
    const soon =
      m.resets_at !== null && m.resets_at - Date.now() < 86_400_000
        ? `<span class="warn-dot" title="${escapeHtml(`This credit expires in ${fmtDuration(Math.max(0, m.resets_at - Date.now()))} — use it or lose it.`)}">●</span> `
        : "";
    return `
      <div class="metric-text action-row">
        <span>${soon}${escapeHtml(m.label)}</span>
        <span class="action-right">
          <span class="detail">${escapeHtml(expiry)}</span>
          <button class="redeem-btn" data-redeem="${escapeHtml(m.detail)}" title="Spend this credit to reset your Codex rate limits now">Use</button>
        </span>
      </div>`;
  }
  return `
    <div class="metric-text">
      <span>${escapeHtml(m.label)}</span>
      <span class="detail">${escapeHtml(m.value ?? "")}</span>
    </div>`;
}

function renderTrend(spend: ProviderSpend): string {
  if (!spend.trend.some((v) => v > 0)) return "";
  const max = Math.max(...spend.trend);
  const peakIdx = spend.trend.indexOf(max);
  const dayMs = 86_400_000;
  const dateOf = (i: number) =>
    new Date(Date.now() - (29 - i) * dayMs).toLocaleDateString([], { month: "short", day: "numeric" });
  // Each day is a group: the visible bar plus a full-height invisible hit
  // area so thin bars are easy to hover; [data-trend] drives the tooltip.
  const bars = spend.trend
    .map((v, i) => {
      const h = v > 0 ? Math.max(2, (v / max) * 30) : 1;
      return `<g class="trend-day">
        <rect class="${v > 0 ? "trend-bar" : "trend-zero"}" x="${i * 10}" y="${32 - h}" width="7" height="${h}" rx="1.5"/>
        <rect class="trend-hit" data-trend="${spend.id}|${i}" x="${i * 10 - 1.5}" y="0" width="10" height="32" fill="transparent"/>
      </g>`;
    })
    .join("");
  const title = `Last 30 days (${dateOf(0)} – ${dateOf(29)}) · peak ${fmtTokens(max)} tokens on ${dateOf(peakIdx)} · from local logs`;
  return `
    <div class="metric trend">
      <div class="metric-head"><span class="metric-label" title="${escapeHtml(title)}">Usage Trend</span></div>
      <svg class="trend-chart" viewBox="0 0 297 32" preserveAspectRatio="none">${bars}</svg>
    </div>`;
}

function renderSpendRow(
  providerId: string,
  label: string,
  key: SpendTab,
  w: SpendWindow,
  sp?: ProviderSpend,
): string {
  const text =
    w.tokens > 0 || w.cost > 0.005 ? `${fmtMoney(w.cost)} · ${fmtTokens(w.tokens)} tokens` : "No data";
  const warn = key === "last30" ? unpricedWarn(sp) : "";
  return `
    <div class="metric-text spend-row" data-spend="${providerId}|${key}">
      <span>${label} ${warn}</span>
      <span class="detail">${text}</span>
    </div>`;
}

/// One card row addressed by its layout key.
function renderItem(s: Snapshot, spend: ProviderSpend | undefined, key: string): string {
  if (key === TREND_KEY) return spend ? renderTrend(spend) : "";
  const spendKey = SPEND_KEYS.find(([label]) => label === key);
  if (spendKey)
    return spend ? renderSpendRow(s.id, spendKey[0], spendKey[1], spend[spendKey[1]], spend) : "";
  const metric = s.metrics.find((m) => m.label === key);
  return metric ? renderMetric(metric) : "";
}

function renderCard(s: Snapshot): string {
  const plan = s.plan ? `<span class="plan">${escapeHtml(s.plan)}</span>` : "";
  const icon = PROVIDER_ICONS[s.id] ?? "";
  const muted = s.status === "ok" ? "" : " muted";

  let body: string;
  let caret = "";
  if (s.status === "ok") {
    const L = providerLayout(s.id);
    const spend = lastSpend.find((sp) => sp.id === s.id);
    const visible = L.metricOrder.filter((k) => !L.hidden.includes(k));
    const always = visible.filter((k) => !L.onDemand.includes(k));
    const onDemand = visible.filter((k) => L.onDemand.includes(k));

    body = always.map((k) => renderItem(s, spend, k)).join("");
    const onDemandHtml = onDemand.map((k) => renderItem(s, spend, k)).join("");
    if (onDemandHtml.trim()) {
      const anim = L.expanded && animateExpandId === s.id ? " anim" : "";
      caret = `
        <button class="card-caret" data-caret="${s.id}" title="${L.expanded ? "Show less" : "Show more"}">${L.expanded ? "⌃" : "⌄"}</button>
        ${L.expanded ? `<div class="on-demand${anim}">${onDemandHtml}</div>` : ""}`;
    }
  } else {
    body = `<p class="placeholder">${escapeHtml(s.error ?? "Not connected")}</p>`;
  }

  const stale = s.stale
    ? `<span class="stale" title="${escapeHtml(`${s.warning ?? "Refresh failed"} — showing the last good data.`)}">⚠ Outdated</span>`
    : "";
  const links = (PROVIDER_LINKS[s.id] ?? [])
    .map((l) => `<button class="quick-link" data-link="${escapeHtml(l.url)}">${escapeHtml(l.label)}</button>`)
    .join("<span class='quick-sep'>·</span>");
  const linksRow = links ? `<div class="quick-links">${links}</div>` : "";
  const share =
    s.status === "ok"
      ? `<button class="share-btn" data-share="${s.id}" title="Copy card as image">⧉</button>`
      : "";
  return `
    <article class="provider${muted}" data-provider="${s.id}">
      <div class="provider-head">
        <span class="provider-name">${escapeHtml(s.name)}</span>
        ${plan}
        ${stale}
        <span class="spacer"></span>
        ${share}
        <span class="provider-icon">${icon}</span>
      </div>
      ${body}
      ${linksRow}
      ${caret}
    </article>`;
}

function orderedSnapshots(): Snapshot[] {
  const order = config.layout?.providerOrder ?? [];
  return [...lastSnapshots].sort((a, b) => {
    const ia = order.indexOf(a.id);
    const ib = order.indexOf(b.id);
    if (ia !== -1 && ib !== -1) return ia - ib;
    return rankSnapshot(a) - rankSnapshot(b);
  });
}

const DONUT_R = 34;
const DONUT_CIRC = 2 * Math.PI * DONUT_R;

type DonutEntry = { s: ProviderSpend; w: SpendWindow };

function donutEntries(tab: SpendTab): DonutEntry[] {
  return lastSpend
    .map((s) => ({ s, w: s[tab] }))
    .filter((e) => e.w.cost > 0.005 || e.w.tokens > 0)
    .sort((a, b) => b.w.cost - a.w.cost);
}

/// Arc lengths + offsets per provider (min-sliver rescaled), shared by the
/// initial render and the tab-switch morph.
function donutGeometry(entries: DonutEntry[]): { total: number; geo: Map<string, { len: number; offset: number }> } {
  const total = entries.reduce((sum, e) => sum + e.w.cost, 0);
  const spenders = entries.filter((e) => e.w.cost > 0);
  const MIN_LEN = 3;
  let lens = spenders.map((e) =>
    total > 0 ? Math.max(MIN_LEN, (e.w.cost / total) * DONUT_CIRC) : 0,
  );
  const lenSum = lens.reduce((a, b) => a + b, 0);
  if (lenSum > 0) lens = lens.map((l) => (l / lenSum) * DONUT_CIRC);
  const geo = new Map<string, { len: number; offset: number }>();
  let offset = 0;
  spenders.forEach((e, i) => {
    geo.set(e.s.id, { len: lens[i], offset });
    offset += lens[i];
  });
  return { total, geo };
}

function legendHtml(entries: DonutEntry[]): string {
  return entries
    .map(
      (e) => `
        <div class="legend-row">
          <span class="dot" style="background:${spendColor(e.s.id)}"></span>
          <span class="legend-name">${escapeHtml(e.s.name)} ${unpricedWarn(e.s)}</span>
          <span class="legend-val">${fmtMoney(e.w.cost)}</span>
        </div>`,
    )
    .join("");
}

/// Tab switch morphs the existing arcs in place (identity-keyed per
/// provider, CSS-transitioned) instead of rebuilding the card.
function switchSpendTab(tab: SpendTab): void {
  spendTab = tab;
  void patchConfig({ spendTab });
  const card = document.querySelector<HTMLElement>(".total-spend");
  const circles = card ? Array.from(card.querySelectorAll<SVGCircleElement>("circle.seg")) : [];
  const entries = donutEntries(tab);
  const { total, geo } = donutGeometry(entries);
  const morphable =
    card &&
    circles.length > 0 &&
    [...geo.keys()].every((id) => circles.some((c) => c.dataset.pid === id));
  if (!morphable) {
    renderAll();
    return;
  }
  for (const c of circles) {
    const g = geo.get(c.dataset.pid ?? "");
    if (g) {
      c.style.opacity = "1";
      c.style.strokeDasharray = `${g.len} ${DONUT_CIRC - g.len}`;
      c.style.strokeDashoffset = `${-g.offset}`;
    } else {
      c.style.opacity = "0";
    }
  }
  const totalEl = card.querySelector(".donut-total");
  if (totalEl) totalEl.textContent = fmtMoney(total);
  const legend = card.querySelector(".legend");
  if (legend) legend.innerHTML = legendHtml(entries);
  card.querySelectorAll(".tab").forEach((t) => {
    t.classList.toggle("active", t.getAttribute("data-tab") === tab);
  });
  const wrap = card.querySelector<HTMLElement>(".donut-wrap");
  if (wrap) wrap.title = `$${total.toFixed(2)} — computed locally from session logs`;
}

function renderTotalSpend(): string {
  if (!config.showTotalSpend) return "";
  const entries = donutEntries(spendTab);
  if (lastSpend.length === 0) {
    if (spendLoaded) return "";
    return `
      <article class="provider total-spend">
        <div class="provider-head">
          <span class="provider-name">Total Spend</span>
        </div>
        <p class="placeholder" style="margin:8px 0 4px">Scanning session logs…</p>
      </article>`;
  }

  const { total, geo } = donutGeometry(entries);
  const segments = entries
    .filter((e) => geo.has(e.s.id))
    .map((e) => {
      const g = geo.get(e.s.id)!;
      return `<circle class="seg" data-pid="${e.s.id}" r="${DONUT_R}" cx="44" cy="44" fill="none" stroke="${spendColor(e.s.id)}" stroke-width="11"
        style="stroke-dasharray:${g.len} ${DONUT_CIRC - g.len};stroke-dashoffset:${-g.offset}" transform="rotate(-90 44 44)"/>`;
    })
    .join("");
  const track = `<circle class="donut-track" r="${DONUT_R}" cx="44" cy="44" fill="none" stroke-width="11"/>`;

  const legend = legendHtml(entries);

  const tab = (id: SpendTab, label: string) =>
    `<button class="tab${spendTab === id ? " active" : ""}" data-tab="${id}">${label}</button>`;

  const exact = `$${total.toFixed(2)} — computed locally from session logs`;
  const body = entries.length
    ? `
      <div class="donut-wrap" title="${escapeHtml(exact)}">
        <svg width="88" height="88" viewBox="0 0 88 88">
          ${track}${segments}
          <text class="donut-total" x="44" y="49" text-anchor="middle" font-size="14" font-weight="600">${fmtMoney(total)}</text>
        </svg>
        <div class="legend">${legend}</div>
      </div>`
    : `<p class="placeholder" style="margin-top:8px">No spend in this period.</p>`;

  const contributors = lastSpend.map((s) => s.name).join(", ");
  return `
    <article class="provider total-spend">
      <div class="provider-head">
        <span class="provider-name">Total Spend</span>
        <span class="info" title="Fed by: ${escapeHtml(contributors)}. All figures are local estimates from each tool's own logs.">&#9432;</span>
        <span class="spacer"></span>
        <button class="share-btn" data-share="__total__" title="Copy card as image">⧉</button>
      </div>
      <div class="tabs">
        ${tab("today", "Today")}${tab("yesterday", "Yesterday")}${tab("last30", "30 Days")}
      </div>
      ${body}
    </article>`;
}

// ---------------------------------------------------------------------------
// Share cards — a canvas replica of the card, copied to the clipboard as PNG
// ---------------------------------------------------------------------------

function themeColors() {
  const light = document.documentElement.dataset.theme === "light";
  return light
    ? { bg: "#fafafa", text: "#09090b", muted: "#71717a", track: "#e4e4e7", card: "#ffffff" }
    : { bg: "#18181b", text: "#fafafa", muted: "#a1a1aa", track: "#27272a", card: "#18181b" };
}

function paceColor(m: Metric): string {
  const cls = computePace(m).cls;
  if (cls === "low") return "#ef4444";
  if (cls === "warn") return "#f59e0b";
  return "#3b82f6";
}

/// Draws at 2x of the Mac's 360pt design width and copies the PNG.
async function shareCard(id: string): Promise<void> {
  const status = document.querySelector("#status")!;
  try {
    const S = 2;
    const W = 360 * S;
    const PAD = 18 * S;
    const C = themeColors();

    type Row =
      | { kind: "bar"; label: string; right: string; used: number; color: string }
      | { kind: "text"; label: string; right: string }
      | { kind: "legend"; label: string; right: string; dot: string };
    let title = "";
    let subtitle = "";
    const rows: Row[] = [];

    if (id === "__total__") {
      title = "Total Spend";
      subtitle = SPEND_KEYS.find(([, k]) => k === spendTab)?.[0] ?? "";
      const entries = lastSpend
        .map((s) => ({ s, w: s[spendTab] }))
        .filter((e) => e.w.cost > 0.005 || e.w.tokens > 0)
        .sort((a, b) => b.w.cost - a.w.cost);
      const total = entries.reduce((sum, e) => sum + e.w.cost, 0);
      rows.push({ kind: "text", label: "Total", right: fmtMoney(total) });
      for (const e of entries) {
        rows.push({
          kind: "legend",
          label: e.s.name,
          right: fmtMoney(e.w.cost),
          dot: SPEND_COLORS[e.s.id] ?? "#4c8dff",
        });
      }
    } else {
      const s = lastSnapshots.find((x) => x.id === id);
      if (!s || s.status !== "ok") return;
      title = s.name;
      subtitle = s.plan ?? "";
      const L = providerLayout(id);
      const visible = L.metricOrder.filter((k) => !L.hidden.includes(k));
      const shown = visible.filter((k) => !L.onDemand.includes(k) || L.expanded);
      for (const key of shown) {
        const m = s.metrics.find((x) => x.label === key);
        if (!m) continue;
        if (m.kind === "progress") {
          const used = clampPercent(m.used_percent ?? 0);
          rows.push({
            kind: "bar",
            label: m.label,
            right: `${Math.round(100 - used)}% left`,
            used,
            color: paceColor(m),
          });
        } else if (m.value) {
          rows.push({ kind: "text", label: m.label, right: m.value });
        }
      }
      if (!rows.length) return;
    }

    const rowH = (r: Row) => (r.kind === "bar" ? 34 * S : 24 * S);
    const headH = 46 * S;
    const footH = 34 * S;
    const H = headH + rows.reduce((sum, r) => sum + rowH(r), 0) + footH + PAD;

    const canvas = document.createElement("canvas");
    canvas.width = W;
    canvas.height = H;
    const ctx = canvas.getContext("2d")!;
    ctx.fillStyle = C.card;
    ctx.fillRect(0, 0, W, H);

    ctx.fillStyle = C.text;
    ctx.font = `600 ${15 * S}px "Segoe UI", sans-serif`;
    ctx.fillText(title, PAD, PAD + 15 * S);
    if (subtitle) {
      ctx.fillStyle = C.muted;
      ctx.font = `400 ${12 * S}px "Segoe UI", sans-serif`;
      ctx.fillText(subtitle, PAD + ctx.measureText(title).width + 46 * S, PAD + 15 * S);
    }

    let y = headH + PAD / 2;
    const rr = (x: number, ry: number, w: number, h: number, r: number) => {
      ctx.beginPath();
      ctx.roundRect(x, ry, Math.max(w, h), h, r);
      ctx.fill();
    };
    for (const r of rows) {
      ctx.font = `500 ${12 * S}px "Segoe UI", sans-serif`;
      ctx.fillStyle = C.text;
      let labelX = PAD;
      if (r.kind === "legend") {
        ctx.fillStyle = r.dot;
        ctx.beginPath();
        ctx.arc(PAD + 4 * S, y + 8 * S, 4 * S, 0, Math.PI * 2);
        ctx.fill();
        ctx.fillStyle = C.text;
        labelX = PAD + 14 * S;
      }
      ctx.fillText(r.label, labelX, y + 11 * S);
      ctx.fillStyle = r.kind === "bar" ? C.text : C.muted;
      const rightW = ctx.measureText(r.right).width;
      ctx.fillText(r.right, W - PAD - rightW, y + 11 * S);
      if (r.kind === "bar") {
        const barY = y + 18 * S;
        ctx.fillStyle = C.track;
        rr(PAD, barY, W - 2 * PAD, 5 * S, 2.5 * S);
        if (r.used > 0.5) {
          ctx.fillStyle = r.color;
          rr(PAD, barY, (W - 2 * PAD) * (r.used / 100), 5 * S, 2.5 * S);
        }
      }
      y += rowH(r);
    }

    ctx.fillStyle = C.muted;
    ctx.font = `400 ${11 * S}px "Segoe UI", sans-serif`;
    ctx.fillText("Monitor Your AI Subscriptions with Pane", PAD, H - PAD);

    const dataUrl = canvas.toDataURL("image/png");
    const pngBase64 = dataUrl.slice(dataUrl.indexOf(",") + 1);
    await invoke("copy_share_image", { pngBase64 });
    status.textContent = "Copied to clipboard";
  } catch (err) {
    status.textContent = `Share failed: ${err}`;
  }
}

// ---------------------------------------------------------------------------
// Liquid glass lens (prasen.dev original). A rounded-rect signed-distance
// field drives the displacement map, so refraction is concentrated at the
// rim while the center stays optically flat — like iOS Liquid Glass.
// ---------------------------------------------------------------------------

function generateLensMap(w: number, h: number): string | null {
  const canvas = document.createElement("canvas");
  canvas.width = w;
  canvas.height = h;
  const ctx = canvas.getContext("2d");
  if (!ctx) return null;
  const img = ctx.createImageData(w, h);
  const data = img.data;
  const cx = w / 2;
  const cy = h / 2;
  const radius = Math.min(w, h) / 2;
  const halfW = Math.max(w / 2 - radius, 0);
  const halfH = Math.max(h / 2 - radius, 0);
  const rim = 1.1 * radius; // bend zone width, measured inward from the edge
  let i = 0;
  for (let y = 0; y < h; y++) {
    for (let x = 0; x < w; x++) {
      const ax = x + 0.5 - cx;
      const ay = y + 0.5 - cy;
      const px = Math.abs(ax) - halfW;
      const py = Math.abs(ay) - halfH;
      const sdf =
        Math.min(Math.max(px, py), 0) + Math.hypot(Math.max(px, 0), Math.max(py, 0)) - radius;
      let g = 0;
      if (sdf > -rim) {
        const e = Math.min(Math.max(1 + sdf / rim, 0), 1);
        g = e * e * (3 - 2 * e); // smoothstep toward the edge
      }
      data[i++] = Math.round(128 + (ax / (w / 2)) * g * 110);
      data[i++] = Math.round(128 + (ay / (h / 2)) * g * 110);
      data[i++] = 128;
      data[i++] = 255;
    }
  }
  ctx.putImageData(img, 0, 0);
  return canvas.toDataURL();
}

function applyLens(el: HTMLElement | null, filterId: string, imgId: string): void {
  if (!el) return;
  const w = 4 * Math.round(el.offsetWidth / 4);
  const h = 4 * Math.round(el.offsetHeight / 4);
  if (w < 8 || h < 8) return;
  const filter = document.getElementById(filterId);
  const img = document.getElementById(imgId);
  const map = generateLensMap(w, h);
  if (!filter || !img || !map) return;
  filter.setAttribute("width", String(w));
  filter.setAttribute("height", String(h));
  img.setAttribute("width", String(w));
  img.setAttribute("height", String(h));
  img.setAttribute("href", map);
  const f = `url(#${filterId}) blur(2px) saturate(1.8) brightness(1.04)`;
  el.style.backdropFilter = f;
  (el.style as unknown as Record<string, string>).webkitBackdropFilter = f;
}

function initLiquidLens(): void {
  const surfaces: [string, string, HTMLElement | null][] = [
    ["lens-side", "lens-map-side", document.querySelector(".sidebar")],
    ["lens-footer", "lens-map-footer", document.querySelector(".main-col footer")],
  ];
  for (const [filterId, imgId, el] of surfaces) {
    if (!el) continue;
    applyLens(el, filterId, imgId);
    new ResizeObserver(() => applyLens(el, filterId, imgId)).observe(el);
  }

  // Panel header bars (Customize / Settings) share one lens sized to the
  // window width. Applied through a CSS variable so re-rendered bars keep
  // the effect without JS re-application.
  const w = 4 * Math.round(window.innerWidth / 4);
  const h = 44;
  const filter = document.getElementById("lens-bar");
  const img = document.getElementById("lens-map-bar");
  const map = generateLensMap(w, h);
  if (filter && img && map) {
    filter.setAttribute("width", String(w));
    filter.setAttribute("height", String(h));
    img.setAttribute("width", String(w));
    img.setAttribute("height", String(h));
    img.setAttribute("href", map);
    document.documentElement.style.setProperty(
      "--bar-filter",
      "url(#lens-bar) blur(2px) saturate(1.8) brightness(1.04)",
    );
  }
}

// ---------------------------------------------------------------------------
// Appearance (System / Light / Dark) + density (Regular / Compact)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Tooltip bubbles: every `title` attribute is silently upgraded to a custom
// bubble — 400ms deliberate dwell, balanced wrapping, anchored to the item.
// ---------------------------------------------------------------------------

function setupTooltips(): void {
  const tip = document.createElement("div");
  tip.id = "hover-tip";
  tip.hidden = true;
  document.body.appendChild(tip);
  let timer = 0;
  let anchor: HTMLElement | null = null;

  const hide = () => {
    clearTimeout(timer);
    tip.hidden = true;
    anchor = null;
  };

  document.addEventListener("mouseover", (e) => {
    const el = (e.target as HTMLElement).closest<HTMLElement>("[title], [data-tip]");
    if (!el) return;
    const title = el.getAttribute("title");
    if (title) {
      el.dataset.tip = title;
      el.removeAttribute("title"); // suppress the native tooltip
    }
    if (!el.dataset.tip || el === anchor) return;
    anchor = el;
    clearTimeout(timer);
    timer = window.setTimeout(() => {
      if (anchor !== el || !document.contains(el)) return;
      tip.textContent = el.dataset.tip ?? "";
      tip.hidden = false;
      const r = el.getBoundingClientRect();
      const w = tip.offsetWidth;
      const h = tip.offsetHeight;
      const x = Math.max(6, Math.min(r.left + r.width / 2 - w / 2, window.innerWidth - w - 6));
      let y = r.top - h - 8;
      if (y < 6) y = r.bottom + 8;
      tip.style.left = `${x}px`;
      tip.style.top = `${y}px`;
    }, 400);
  });
  document.addEventListener("mouseout", (e) => {
    const el = (e.target as HTMLElement).closest<HTMLElement>("[data-tip]");
    const to = e.relatedTarget as HTMLElement | null;
    if (el && (!to || !el.contains(to))) hide();
  });
  document.addEventListener("scroll", hide, true);
  document.addEventListener("mousedown", hide, true);
}

// ---------------------------------------------------------------------------
// Customize undo — whole-layout snapshots, Ctrl+Z restores.
// ---------------------------------------------------------------------------

const undoStack: string[] = [];
let lastLayoutSnapshot = "";

function undoLayout(): void {
  const prev = undoStack.pop();
  if (!prev) return;
  config.layout = JSON.parse(prev) as Layout;
  lastLayoutSnapshot = prev;
  void patchConfig({ layout: config.layout });
  renderAll();
  void updateTrayStrip();
  document.querySelector("#status")!.textContent = "Layout change undone";
}

// ---------------------------------------------------------------------------
// Party mode 🎉 — ↑↑↓↓←→←→BA. Purely cosmetic, never persisted.
// ---------------------------------------------------------------------------

const KONAMI = [
  "ArrowUp", "ArrowUp", "ArrowDown", "ArrowDown",
  "ArrowLeft", "ArrowRight", "ArrowLeft", "ArrowRight", "b", "a",
];
let konamiAt = 0;

function konamiListen(e: KeyboardEvent): void {
  const key = e.key.length === 1 ? e.key.toLowerCase() : e.key;
  konamiAt = key === KONAMI[konamiAt] ? konamiAt + 1 : key === KONAMI[0] ? 1 : 0;
  if (konamiAt === KONAMI.length) {
    konamiAt = 0;
    const on = document.body.classList.toggle("party");
    document.querySelector("#status")!.textContent = on ? "🎉 Party mode!" : "Party's over.";
  }
}

const systemLight = window.matchMedia("(prefers-color-scheme: light)");

function applyAppearance(): void {
  const mode =
    config.appearance === "system" ? (systemLight.matches ? "light" : "dark") : config.appearance;
  document.documentElement.dataset.theme = mode;
  document.documentElement.dataset.density = config.density;
  const btn = document.querySelector<HTMLElement>("#theme-btn");
  if (btn) {
    btn.textContent = mode === "light" ? "☾" : "☀";
    btn.title = mode === "light" ? "Switch to dark mode" : "Switch to light mode";
  }
}

/// Day/night toggle with the circular wipe from jazii.dev: the new theme
/// expands as a clip-path circle from the button via the View Transitions
/// API. Falls back to an instant switch where unsupported.
function toggleTheme(e: Event): void {
  const next = document.documentElement.dataset.theme === "light" ? "dark" : "light";
  const apply = () => {
    config.appearance = next;
    applyAppearance();
    const select = document.querySelector<HTMLSelectElement>("#appearance");
    if (select) select.value = next;
  };

  const btn = e.currentTarget as HTMLElement;
  const rect = btn.getBoundingClientRect();
  const x = rect.left + rect.width / 2;
  const y = rect.top + rect.height / 2;
  const maxRadius = Math.hypot(
    Math.max(x, window.innerWidth - x),
    Math.max(y, window.innerHeight - y),
  );

  const doc = document as Document & { startViewTransition?: (cb: () => void) => { ready: Promise<void> } };
  if (doc.startViewTransition) {
    const transition = doc.startViewTransition(apply);
    transition.ready
      .then(() => {
        document.documentElement.animate(
          [
            { clipPath: `circle(0px at ${x}px ${y}px)` },
            { clipPath: `circle(${maxRadius}px at ${x}px ${y}px)` },
          ],
          {
            duration: 500,
            easing: "cubic-bezier(0.4, 0, 0.2, 1)",
            pseudoElement: "::view-transition-new(root)",
          },
        );
      })
      .catch(() => {});
  } else {
    apply();
  }
  void patchConfig({ appearance: next });
}

systemLight.addEventListener("change", () => {
  if (config.appearance === "system") applyAppearance();
});

// ---------------------------------------------------------------------------
// Customize screen
// ---------------------------------------------------------------------------

function isStarrable(s: Snapshot | undefined, key: string): boolean {
  return s?.metrics.some((m) => m.label === key && m.kind === "progress") ?? false;
}

// Providers start collapsed in Customize; only what you're editing unfolds.
// Session-only — collapsing again on reopen keeps the list scannable.
const custExpanded = new Set<string>();

function renderCustomize(): string {
  const order = config.layout?.providerOrder ?? ALL_PROVIDERS.map(([id]) => id);
  const blocks = order
    .map((id) => {
      const name = ALL_PROVIDERS.find(([pid]) => pid === id)?.[1] ?? id;
      const snapshot = lastSnapshots.find((s) => s.id === id);
      const L = providerLayout(id);
      const enabled = !config.disabled.includes(id);

      const row = (key: string) => {
        const starrable = isStarrable(snapshot, key);
        const starred = L.starred.includes(key);
        const visible = !L.hidden.includes(key);
        return `
          <div class="cust-row" draggable="true" data-cust-row="${id}|${escapeHtml(key)}">
            <span class="grip" title="Drag to reorder">⠿</span>
            <label class="toggle mini"><input type="checkbox" data-visible="${id}|${escapeHtml(key)}"${visible ? " checked" : ""} /></label>
            <span class="cust-label">${escapeHtml(key)}</span>
            ${starrable ? `<button class="star${starred ? " on" : ""}" data-star="${id}|${escapeHtml(key)}" title="Star for tray strip (max 2)">★</button>` : ""}
          </div>`;
      };

      const always = L.metricOrder.filter((k) => !L.onDemand.includes(k));
      const onDemand = L.metricOrder.filter((k) => L.onDemand.includes(k));
      const rows = L.metricOrder.length
        ? `${always.map(row).join("")}
           <div class="cust-divider" data-divider="${id}">On Demand — behind the card's caret</div>
           ${onDemand.map(row).join("")}`
        : `<p class="placeholder">No data yet — refresh with this provider enabled first.</p>`;

      const open = custExpanded.has(id);
      return `
        <article class="provider customize-block${enabled ? "" : " muted"}${open ? " open" : ""}" data-cust-provider="${id}" draggable="true">
          <div class="provider-head">
            <span class="grip" title="Drag to reorder providers">⠿</span>
            <button class="cust-expand" data-cust-expand="${id}" title="${open ? "Collapse" : "Expand"}">
              <span class="provider-name">${escapeHtml(name)}</span>
              <span class="chev">⌄</span>
            </button>
            <span class="spacer"></span>
            <button class="mini-btn" data-reset="${id}" title="Restore this card's default layout — does not touch your usage limits">Reset layout</button>
            <label class="toggle mini" title="Enable provider"><input type="checkbox" data-enable="${id}"${enabled ? " checked" : ""} /></label>
          </div>
          <div class="acc-body"><div class="acc-inner cust-rows">${rows}</div></div>
        </article>`;
    })
    .join("");

  const starCount = Object.values(config.layout?.providers ?? {}).reduce((n, l) => n + l.starred.length, 0);
  return `
    <div class="customize-bar glass-bar">
      <button class="dock-btn" data-customize-close>← Done</button>
      <span class="detail">${starCount} starred · drag ⠿ to reorder</span>
      <button class="dock-btn danger" data-reset-all title="Restore all cards' default layouts — does not touch your usage limits">↺ Reset all</button>
    </div>
    ${blocks}`;
}

// ---------------------------------------------------------------------------
// Render root
// ---------------------------------------------------------------------------

function renderWelcome(): string {
  if (config.welcomeDismissed || !lastSnapshots.length) return "";
  return `
    <article class="provider welcome-card">
      <div class="provider-head">
        <span class="provider-name">Welcome 👋</span>
        <span class="spacer"></span>
        <button class="share-btn welcome-close" data-welcome-close title="Dismiss">✕</button>
      </div>
      <p class="placeholder" style="margin:2px 0 8px">
        You're set up with the AI tools found on this PC. Arrange cards, star
        tray metrics, and hide rows in Customize.
      </p>
      <button class="mini-btn" data-welcome-customize>Open Customize</button>
    </article>`;
}

function renderAll(): void {
  const el = document.querySelector("#providers")!;
  el.innerHTML =
    renderWelcome() + renderTotalSpend() + orderedSnapshots().map(renderCard).join("");
  if (customizeOpen) renderDrawerBody();
  rebuildTrail();
}

function renderDrawerBody(): void {
  const body = document.querySelector<HTMLElement>("#drawer-body");
  if (body) body.innerHTML = renderCustomize();
}

/// Customize lives in a drawer that slides in from the left edge.
function setDrawer(open: boolean): void {
  customizeOpen = open;
  if (open) renderDrawerBody();
  document.body.classList.toggle("drawer-open", open);
  document.querySelector("#customize-btn")?.classList.toggle("active", open);
}

// ---------------------------------------------------------------------------
// Navigation trail: a slim rail of ticks — one per card — that shows where
// you are in the scroll and jumps to a card on click.
// ---------------------------------------------------------------------------

function trailCards(): HTMLElement[] {
  return Array.from(document.querySelectorAll<HTMLElement>("#providers > article"));
}

function rebuildTrail(): void {
  const trail = document.querySelector<HTMLElement>("#trail")!;
  const cards = trailCards();
  if (cards.length < 2) {
    trail.innerHTML = "";
    trail.hidden = true;
    return;
  }
  trail.hidden = false;
  trail.innerHTML = cards
    .map((card, i) => {
      const name = card.querySelector(".provider-name")?.textContent ?? `Card ${i + 1}`;
      return `<button class="trail-tick" data-trail="${i}" title="${escapeHtml(name)}"></button>`;
    })
    .join("");
  // Minimap feel: tick width follows the card's height, like Codex's rail.
  const ticks = trail.querySelectorAll<HTMLElement>(".trail-tick");
  ticks.forEach((tick, i) => {
    const h = cards[i]?.offsetHeight ?? 80;
    tick.style.width = `${Math.max(7, Math.min(16, Math.round(5 + h / 45)))}px`;
  });
  updateTrailActive();
}

/// Codex-style magnetic rail: ticks near the cursor stretch and brighten
/// with a smooth falloff; everything settles back when the mouse leaves.
function setupTrailFisheye(): void {
  const sidebar = document.querySelector<HTMLElement>(".sidebar")!;
  let raf = 0;

  const reset = () => {
    cancelAnimationFrame(raf);
    document.querySelectorAll<HTMLElement>("#trail .trail-tick").forEach((t) => {
      t.style.transform = "";
      t.style.background = "";
    });
  };

  sidebar.addEventListener("mousemove", (e) => {
    const y = e.clientY;
    cancelAnimationFrame(raf);
    raf = requestAnimationFrame(() => {
      document.querySelectorAll<HTMLElement>("#trail .trail-tick").forEach((tick) => {
        const r = tick.getBoundingClientRect();
        const d = Math.abs(y - (r.top + r.height / 2));
        const g = Math.exp(-(d * d) / (2 * 26 * 26)); // gaussian falloff, σ≈26px
        const active = tick.classList.contains("active");
        tick.style.transform = `scaleX(${(1 + 0.9 * g).toFixed(3)})`;
        const mix = Math.round(Math.max(g * 85, active ? 100 : 12));
        tick.style.background = `color-mix(in srgb, var(--foreground) ${mix}%, var(--border))`;
      });
    });
  });
  sidebar.addEventListener("mouseleave", reset);
}

function updateTrailActive(): void {
  const providersEl = document.querySelector<HTMLElement>("#providers")!;
  const cards = trailCards();
  if (!cards.length) return;
  const anchor = providersEl.scrollTop + 70;
  let active = 0;
  for (let i = 0; i < cards.length; i++) {
    if (cards[i].offsetTop <= anchor) active = i;
  }
  // Bottom of the list: light up the last tick even if a tall card above
  // still owns the anchor line.
  if (providersEl.scrollTop + providersEl.clientHeight >= providersEl.scrollHeight - 4) {
    active = cards.length - 1;
  }
  document.querySelectorAll<HTMLElement>("#trail .trail-tick").forEach((tick, i) => {
    tick.classList.toggle("active", i === active);
  });
}

// ---------------------------------------------------------------------------
// Spend row model tooltip
// ---------------------------------------------------------------------------

/// Tooltip for one Usage Trend bar: date, tokens used, share of 30 days.
function showTrendTip(el: HTMLElement): void {
  const tip = document.querySelector<HTMLElement>("#model-tip")!;
  const [id, idxStr] = (el.dataset.trend ?? "").split("|");
  const spend = lastSpend.find((s) => s.id === id);
  const i = Number(idxStr);
  if (!spend || Number.isNaN(i)) return;

  const tokens = spend.trend[i] ?? 0;
  const total = spend.trend.reduce((a, b) => a + b, 0);
  const share = total > 0 ? (tokens / total) * 100 : 0;
  const date = new Date(Date.now() - (29 - i) * 86_400_000).toLocaleDateString([], {
    weekday: "short",
    month: "short",
    day: "numeric",
  });
  tip.innerHTML = `
    <div class="tip-line"><span class="tip-name">${escapeHtml(date)}</span><span>${
      tokens > 0 ? `${fmtTokens(tokens)} tokens` : "No usage"
    }</span></div>
    ${tokens > 0 ? `<div class="tip-line detail"><span>${share < 1 ? "<1" : share.toFixed(0)}% of the last 30 days</span></div>` : ""}`;

  const rect = el.getBoundingClientRect();
  tip.hidden = false;
  const top = Math.min(rect.bottom + 6, window.innerHeight - tip.offsetHeight - 8);
  tip.style.top = `${Math.max(4, top)}px`;
  tip.style.left = `${Math.max(8, Math.min(rect.left - 50, window.innerWidth - tip.offsetWidth - 8))}px`;
}

function showModelTip(row: HTMLElement): void {
  const tip = document.querySelector<HTMLElement>("#model-tip")!;
  const [id, key] = (row.dataset.spend ?? "").split("|");
  const spend = lastSpend.find((s) => s.id === id);
  const w = spend?.[key as SpendTab];
  if (!w) return;

  if (!w.models.length) {
    tip.innerHTML = `<p class="placeholder">No model data for this period.</p>`;
  } else {
    tip.innerHTML = w.models
      .map((m) => {
        const share = w.cost > 0 ? (m.cost / w.cost) * 100 : 0;
        return `
          <div class="tip-model">
            <div class="tip-line"><span class="tip-name">${escapeHtml(m.model)}</span><span>${fmtMoney(m.cost)}</span></div>
            <div class="tip-line detail"><span>${share.toFixed(0)}%</span><span>${fmtTokens(m.tokens)} tokens</span></div>
            <div class="tip-bar"><div style="width:${Math.max(2, share)}%"></div></div>
          </div>`;
      })
      .join("");
  }

  const rect = row.getBoundingClientRect();
  tip.hidden = false;
  const top = Math.min(rect.bottom + 4, window.innerHeight - tip.offsetHeight - 8);
  tip.style.top = `${Math.max(4, top)}px`;
  tip.style.left = `${Math.max(8, Math.min(rect.left + 20, window.innerWidth - tip.offsetWidth - 8))}px`;
}

// ---------------------------------------------------------------------------
// Refresh + tray strip
// ---------------------------------------------------------------------------

async function refresh(force = false): Promise<void> {
  if (refreshing) return;
  if (!force && Date.now() - lastFetch < STALE_MS) return;
  refreshing = true;

  const status = document.querySelector("#status")!;
  status.textContent = "Refreshing…";
  // The spend scan re-reads every session log on a cold start and can take
  // tens of seconds — it must never hold up the usage cards' first paint.
  const spendPromise = invoke<ProviderSpend[]>("fetch_spend").catch(() => null);
  try {
    let snapshots = await invoke<Snapshot[]>("fetch_usage");
    // First launch ever (no layout yet): start with only the providers that
    // actually have credentials on this PC, like the Mac app's first-run
    // detection. The rest stay available in Customize.
    if (config.layout === null && snapshots.some((s) => s.status === "ok")) {
      const noCreds = snapshots.filter((s) => s.status === "no_credentials").map((s) => s.id);
      if (noCreds.length) {
        snapshots = snapshots.filter((s) => !noCreds.includes(s.id));
        await patchConfig({ disabled: noCreds }).catch(() => {});
      }
    } else if (config.layout) {
      // App updates ship new providers; ones this PC has no credentials for
      // start disabled instead of piling up dead cards. Seen once (a layout
      // entry marks that), so enabling one in Customize sticks.
      const known = config.layout.providers;
      const fresh = snapshots
        .filter(
          (s) => s.status === "no_credentials" && !(s.id in known) && !config.disabled.includes(s.id)
        )
        .map((s) => s.id);
      if (fresh.length) {
        for (const id of fresh) known[id] = providerLayout(id);
        await patchConfig({
          disabled: [...config.disabled, ...fresh],
          layout: config.layout,
        }).catch(() => {});
      }
    }
    const firstData = lastSnapshots.length === 0;
    lastFetch = Date.now();
    lastSnapshots = snapshots;
    ensureLayout();
    if (!lastLayoutSnapshot && config.layout) {
      lastLayoutSnapshot = JSON.stringify(config.layout);
    }
    renderAll();
    if (firstData && !customizeOpen) playReveal();
    populatePinnedOptions();
    void updateTrayStrip();
    const time = new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
    status.textContent = `Updated ${time}`;
  } catch (err) {
    status.textContent = `Refresh failed: ${err}`;
  }
  const spend = await spendPromise;
  spendLoaded = true;
  if (spend) lastSpend = spend;
  if (!customizeOpen && lastSnapshots.length) renderAll();
  refreshing = false;
}

function scheduleAutoRefresh(): void {
  if (refreshTimer !== undefined) clearInterval(refreshTimer);
  const minutes = Math.max(1, config.refreshMinutes || 5);
  refreshTimer = setInterval(() => void refresh(), minutes * 60 * 1000);
}

const logoPixels = new Map<string, number[]>();

async function rasterizeLogo(id: string): Promise<number[] | null> {
  const cached = logoPixels.get(id);
  if (cached) return cached;
  const svg = PROVIDER_ICONS[id];
  if (!svg) return null;

  const white = svg
    .replace(/fill="(?!none)[^"]*"/g, 'fill="#ffffff"')
    .replace(/stroke="(?!none)[^"]*"/g, 'stroke="#ffffff"');
  const url = URL.createObjectURL(new Blob([white], { type: "image/svg+xml" }));
  try {
    const img = new Image();
    await new Promise<void>((resolve, reject) => {
      img.onload = () => resolve();
      img.onerror = () => reject(new Error("svg load failed"));
      img.src = url;
    });
    const canvas = document.createElement("canvas");
    canvas.width = 32;
    canvas.height = 32;
    const ctx = canvas.getContext("2d")!;
    const scale = 28 / Math.max(img.width || 28, img.height || 28);
    const w = (img.width || 28) * scale;
    const h = (img.height || 28) * scale;
    ctx.drawImage(img, (32 - w) / 2, (32 - h) / 2, w, h);
    const pixels = Array.from(ctx.getImageData(0, 0, 32, 32).data);
    logoPixels.set(id, pixels);
    return pixels;
  } catch {
    return null;
  } finally {
    URL.revokeObjectURL(url);
  }
}

/// The tray strip now follows the stars: any provider with starred metrics
/// gets a [logo][numbers] pair, in customize order (max 4 providers).
async function updateTrayStrip(): Promise<void> {
  const entries = [];
  const order = config.layout?.providerOrder ?? [];
  for (const id of order) {
    if (entries.length >= 4) break;
    const L = providerLayout(id);
    if (!L.starred.length) continue;
    const snap = lastSnapshots.find((s) => s.id === id && s.status === "ok");
    if (!snap) continue;
    const starredMetrics = L.starred
      .map((label) => snap.metrics.find((m) => m.label === label && m.kind === "progress"))
      .filter((m): m is Metric => Boolean(m))
      .slice(0, 2);
    if (!starredMetrics.length) continue;
    const logo = await rasterizeLogo(id);
    if (!logo) continue;
    const values = starredMetrics.map((m) => Math.round(100 - clampPercent(m.used_percent ?? 0)));
    const tooltip = `${snap.name}\n${starredMetrics
      .map((m) => `${m.label}: ${Math.round(100 - clampPercent(m.used_percent ?? 0))}% left`)
      .join("\n")}`;
    entries.push({ id, logo, values, tooltip });
  }
  try {
    await invoke("update_tray_strip", { entries });
  } catch {
    // Tray strip is cosmetic — never let it break a refresh.
  }
}

// ---------------------------------------------------------------------------
// Customize interactions
// ---------------------------------------------------------------------------

interface DragPayload {
  t: "row" | "provider";
  id: string;
  key?: string;
}

let dragPayload: DragPayload | null = null;

/// Rebuilds order + On-Demand membership after a row drop. The sequence is
/// [always..., DIVIDER, onDemand...]; where the row lands relative to the
/// divider decides which side it lives on.
function moveRow(L: ProviderLayout, key: string, target: string): void {
  const always = L.metricOrder.filter((k) => !L.onDemand.includes(k));
  const onDemand = L.metricOrder.filter((k) => L.onDemand.includes(k));
  const seq = [...always, DIVIDER, ...onDemand].filter((k) => k !== key);
  const at = target === DIVIDER ? seq.indexOf(DIVIDER) + 1 : seq.indexOf(target);
  if (at < 0) return;
  seq.splice(at, 0, key);
  const dividerIdx = seq.indexOf(DIVIDER);
  L.metricOrder = seq.filter((k) => k !== DIVIDER);
  L.onDemand = seq.slice(dividerIdx + 1).filter((k) => k !== DIVIDER);
}

function handleCustomizeClick(target: HTMLElement): boolean {
  const expand = target.closest<HTMLElement>("[data-cust-expand]");
  if (expand) {
    const id = expand.dataset.custExpand!;
    if (custExpanded.has(id)) {
      custExpanded.delete(id);
    } else {
      custExpanded.add(id);
    }
    // Toggle in place so the accordion animates instead of re-rendering.
    expand.closest(".customize-block")?.classList.toggle("open", custExpanded.has(id));
    return true;
  }
  const closeBtn = target.closest("[data-customize-close]");
  if (closeBtn) {
    setDrawer(false);
    return true;
  }
  const resetAll = target.closest("[data-reset-all]");
  if (resetAll) {
    if (
      window.confirm(
        "Reset all layout customization? Order, stars, and hidden rows go back to defaults, and installed AI tools are re-detected. (Your usage limits are not affected.)",
      )
    ) {
      // Clearing layout + disabled re-arms the first-launch detection path:
      // the next refresh probes every provider and re-disables only the
      // ones with no credentials on this PC.
      config.layout = null;
      config.disabled = [];
      void patchConfig({ layout: null, disabled: [] }).then(() => {
        setDrawer(false);
        void refresh(true).then(() => void updateTrayStrip());
      });
    }
    return true;
  }
  const reset = target.closest<HTMLElement>("[data-reset]");
  if (reset && config.layout) {
    const id = reset.dataset.reset!;
    const snapshot = lastSnapshots.find((s) => s.id === id);
    const spend = lastSpend.find((sp) => sp.id === id);
    config.layout.providers[id] = defaultProviderLayout(snapshot, spend, false);
    saveLayout();
    renderAll();
    void updateTrayStrip();
    return true;
  }
  const star = target.closest<HTMLElement>("[data-star]");
  if (star) {
    const [id, key] = star.dataset.star!.split("|");
    const L = providerLayout(id);
    if (L.starred.includes(key)) {
      L.starred = L.starred.filter((k) => k !== key);
    } else if (L.starred.length >= 2) {
      document.querySelector("#status")!.textContent = "Up to 2 stars per provider";
      return true;
    } else {
      L.starred.push(key);
    }
    saveLayout();
    renderAll();
    void updateTrayStrip();
    return true;
  }
  return false;
}

function handleCustomizeChange(target: HTMLInputElement): void {
  if (target.dataset.enable !== undefined) {
    const id = target.dataset.enable;
    const disabled = new Set(config.disabled);
    if (target.checked) disabled.delete(id);
    else disabled.add(id);
    void patchConfig({ disabled: [...disabled] }).then(() => refresh(true));
    return;
  }
  if (target.dataset.visible !== undefined) {
    const [id, key] = target.dataset.visible.split("|");
    const L = providerLayout(id);
    if (target.checked) L.hidden = L.hidden.filter((k) => k !== key);
    else if (!L.hidden.includes(key)) L.hidden.push(key);
    saveLayout();
  }
}

// Chromium's default drag snapshot on backdrop-filtered elements captures the
// glass layers behind the card too — a smeared ghost of the whole list. Hand
// it a small opaque pill instead and dim the real card while it's in flight.
let dragGhost: HTMLElement | null = null;

function setDragGhost(e: DragEvent, src: HTMLElement): void {
  const rect = src.getBoundingClientRect();
  const g = src.cloneNode(true) as HTMLElement;
  g.classList.add("drag-ghost");
  g.classList.remove("open"); // ghost of a provider card shows just its header bar
  g.style.width = `${rect.width}px`;
  document.body.appendChild(g);
  e.dataTransfer?.setDragImage(g, e.clientX - rect.left, e.clientY - rect.top);
  dragGhost = g;
  requestAnimationFrame(() => src.classList.add("drag-src"));
}

function setupCustomizeDnD(providersEl: HTMLElement): void {
  providersEl.addEventListener("dragstart", (e) => {
    const row = (e.target as HTMLElement).closest<HTMLElement>("[data-cust-row]");
    if (row) {
      const [id, key] = row.dataset.custRow!.split("|");
      dragPayload = { t: "row", id, key };
      setDragGhost(e as DragEvent, row);
      e.stopPropagation();
      return;
    }
    const block = (e.target as HTMLElement).closest<HTMLElement>("[data-cust-provider]");
    if (block) {
      dragPayload = { t: "provider", id: block.dataset.custProvider! };
      setDragGhost(e as DragEvent, block);
    }
  });

  providersEl.addEventListener("dragend", () => {
    dragGhost?.remove();
    dragGhost = null;
    providersEl.querySelectorAll(".drag-src").forEach((el) => el.classList.remove("drag-src"));
  });

  providersEl.addEventListener("dragover", (e) => {
    if (dragPayload) e.preventDefault();
  });

  providersEl.addEventListener("drop", (e) => {
    if (!dragPayload) return;
    e.preventDefault();
    const target = e.target as HTMLElement;

    if (dragPayload.t === "row") {
      const L = providerLayout(dragPayload.id);
      const divider = target.closest<HTMLElement>("[data-divider]");
      const row = target.closest<HTMLElement>("[data-cust-row]");
      if (divider && divider.dataset.divider === dragPayload.id) {
        moveRow(L, dragPayload.key!, DIVIDER);
      } else if (row) {
        const [tid, tkey] = row.dataset.custRow!.split("|");
        if (tid === dragPayload.id && tkey !== dragPayload.key) moveRow(L, dragPayload.key!, tkey);
      }
      saveLayout();
      renderAll();
    } else if (config.layout) {
      const block = target.closest<HTMLElement>("[data-cust-provider]");
      if (block && block.dataset.custProvider !== dragPayload.id) {
        const order = config.layout.providerOrder.filter((p) => p !== dragPayload!.id);
        const at = order.indexOf(block.dataset.custProvider!);
        order.splice(at < 0 ? order.length : at, 0, dragPayload.id);
        config.layout.providerOrder = order;
        saveLayout();
        renderAll();
        void updateTrayStrip();
      }
    }
    dragPayload = null;
    // renderAll() replaces the dragged node, so dragend may never bubble
    // back up — clean the ghost here too.
    dragGhost?.remove();
    dragGhost = null;
  });
}

// ---------------------------------------------------------------------------
// Settings pane
// ---------------------------------------------------------------------------

async function saveApiKey(provider: string): Promise<void> {
  const input = document.querySelector<HTMLInputElement>(`#key-${provider}`)!;
  const status = document.querySelector("#status")!;
  try {
    await invoke("set_api_key", { provider, key: input.value });
    input.value = "";
    status.textContent = `${provider} key saved`;
    await refresh(true);
  } catch (err) {
    status.textContent = `Could not save key: ${err}`;
  }
}

function populatePinnedOptions(): void {
  const select = document.querySelector<HTMLSelectElement>("#pinned")!;
  const current = config.pinned ? `${config.pinned.provider}::${config.pinned.label}` : "";
  const options = ['<option value="">Auto (first live metric)</option>'];
  for (const s of lastSnapshots) {
    if (s.status !== "ok") continue;
    for (const m of s.metrics) {
      if (m.kind !== "progress") continue;
      const value = `${s.id}::${m.label}`;
      const selected = value === current ? " selected" : "";
      options.push(`<option value="${value}"${selected}>${escapeHtml(`${s.name} — ${m.label}`)}</option>`);
    }
  }
  select.innerHTML = options.join("");
}

async function initSettings(): Promise<void> {
  config = await invoke<Config>("get_config");
  if (["today", "yesterday", "last30"].includes(config.spendTab)) {
    spendTab = config.spendTab;
  }

  const interval = document.querySelector<HTMLInputElement>("#interval")!;
  interval.value = String(config.refreshMinutes);
  interval.addEventListener("change", () => {
    const minutes = Math.max(1, Math.min(120, Number(interval.value) || 5));
    interval.value = String(minutes);
    void patchConfig({ refreshMinutes: minutes }).then(scheduleAutoRefresh);
  });

  const autostart = document.querySelector<HTMLInputElement>("#autostart")!;
  autostart.checked = await invoke<boolean>("get_autostart");
  autostart.addEventListener("change", () => {
    void invoke("set_autostart", { enabled: autostart.checked }).catch((err) => {
      document.querySelector("#status")!.textContent = `Autostart failed: ${err}`;
      autostart.checked = !autostart.checked;
    });
  });

  const pacing = document.querySelector<HTMLInputElement>("#pacing")!;
  pacing.checked = config.pacingAlways;
  pacing.addEventListener("change", () => {
    void patchConfig({ pacingAlways: pacing.checked }).then(renderAll);
  });

  const timeFormat = document.querySelector<HTMLSelectElement>("#timeformat")!;
  timeFormat.value = config.timeFormat;
  timeFormat.addEventListener("change", () => {
    void patchConfig({ timeFormat: timeFormat.value as Config["timeFormat"] }).then(renderAll);
  });

  const notifyToggles: [string, keyof Config][] = [
    ["#notify-almost", "notifyAlmostOut"],
    ["#notify-close", "notifyCuttingClose"],
    ["#notify-runout", "notifyWillRunOut"],
  ];
  for (const [selector, key] of notifyToggles) {
    const box = document.querySelector<HTMLInputElement>(selector)!;
    box.checked = Boolean(config[key]);
    box.addEventListener("change", () => {
      void patchConfig({ [key]: box.checked } as Partial<Config>);
    });
  }

  const pinned = document.querySelector<HTMLSelectElement>("#pinned")!;
  pinned.addEventListener("change", () => {
    const [provider, label] = pinned.value.split("::");
    const value = provider && label ? { provider, label } : null;
    void patchConfig({ pinned: value }).then(() => refresh(true));
  });

  const showSpend = document.querySelector<HTMLInputElement>("#show-total-spend")!;
  showSpend.checked = config.showTotalSpend;
  showSpend.addEventListener("change", () => {
    void patchConfig({ showTotalSpend: showSpend.checked }).then(renderAll);
  });

  applyAppearance();
  const appearance = document.querySelector<HTMLSelectElement>("#appearance")!;
  appearance.value = config.appearance;
  appearance.addEventListener("change", () => {
    void patchConfig({ appearance: appearance.value as Config["appearance"] }).then(applyAppearance);
  });

  const density = document.querySelector<HTMLInputElement>("#density")!;
  density.checked = config.density === "compact";
  density.addEventListener("change", () => {
    void patchConfig({ density: density.checked ? "compact" : "regular" }).then(applyAppearance);
  });

  const shortcut = document.querySelector<HTMLInputElement>("#shortcut")!;
  shortcut.value = config.shortcut;
  shortcut.addEventListener("change", async () => {
    const status = document.querySelector("#status")!;
    try {
      await invoke("set_shortcut", { shortcut: shortcut.value });
      await patchConfig({ shortcut: shortcut.value });
      status.textContent = shortcut.value.trim() ? "Shortcut saved" : "Shortcut cleared";
    } catch (err) {
      status.textContent = `${err}`;
    }
  });

  const proxyEnabled = document.querySelector<HTMLInputElement>("#proxy-enabled")!;
  const proxyUrl = document.querySelector<HTMLInputElement>("#proxy-url")!;
  proxyEnabled.checked = config.proxy?.enabled ?? false;
  proxyUrl.value = config.proxy?.url ?? "";
  const saveProxy = () => {
    void patchConfig({ proxy: { enabled: proxyEnabled.checked, url: proxyUrl.value.trim() } }).then(
      () => {
        document.querySelector("#status")!.textContent = "Proxy saved — takes effect after restart";
      },
    );
  };
  proxyEnabled.addEventListener("change", saveProxy);
  proxyUrl.addEventListener("change", saveProxy);

  populatePinnedOptions();
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

window.addEventListener("DOMContentLoaded", () => {
  document.querySelector("#app-logo")!.innerHTML = openusageIcon;
  document.querySelector("#theme-btn")!.addEventListener("click", toggleTheme);
  setupTrailFisheye();
  setupTooltips();
  window.setTimeout(initLiquidLens, 150);
  window.addEventListener("keydown", (e) => {
    konamiListen(e);
    if (e.ctrlKey && e.key.toLowerCase() === "z" && customizeOpen) {
      e.preventDefault();
      undoLayout();
    }
  });
  void getVersion().then((v) => {
    const el = document.querySelector("#build-info");
    if (el) el.textContent = `v${v} · build ${__BUILD_STAMP__}`;
  });
  document.querySelector("#refresh")!.addEventListener("click", () => void refresh(true));

  const setSettings = (open: boolean) => {
    document.body.classList.toggle("settings-open", open);
    document.querySelector("#settings-btn")?.classList.toggle("active", open);
  };
  document.querySelector("#settings-btn")!.addEventListener("click", () => {
    setDrawer(false);
    setSettings(!document.body.classList.contains("settings-open"));
  });
  document.querySelector("#settings-close")!.addEventListener("click", () => setSettings(false));
  document.querySelectorAll<HTMLElement>(".acc-head").forEach((head) => {
    head.addEventListener("click", () => head.parentElement!.classList.toggle("open"));
  });
  document.querySelector("#customize-btn")!.addEventListener("click", () => {
    setSettings(false);
    setDrawer(!customizeOpen);
  });
  const drawerBody = document.querySelector<HTMLElement>("#drawer-body")!;
  drawerBody.addEventListener("click", (e) => {
    handleCustomizeClick(e.target as HTMLElement);
  });
  drawerBody.addEventListener("change", (e) => {
    handleCustomizeChange(e.target as HTMLInputElement);
  });
  setupCustomizeDnD(drawerBody);
  document.querySelectorAll<HTMLButtonElement>("[data-save]").forEach((btn) => {
    btn.addEventListener("click", () => void saveApiKey(btn.dataset.save!));
  });

  const providersEl = document.querySelector<HTMLElement>("#providers")!;
  providersEl.addEventListener("click", (e) => {
    const target = e.target as HTMLElement;

    const link = target.closest<HTMLElement>("[data-link]");
    if (link) {
      void invoke("open_link", { url: link.dataset.link }).catch((err) => {
        document.querySelector("#status")!.textContent = `Could not open link: ${err}`;
      });
      return;
    }
    const shareBtn = target.closest<HTMLElement>("[data-share]");
    if (shareBtn) {
      void shareCard(shareBtn.dataset.share!);
      return;
    }
    if (target.closest("[data-welcome-close]")) {
      config.welcomeDismissed = true;
      void patchConfig({ welcomeDismissed: true });
      renderAll();
      return;
    }
    if (target.closest("[data-welcome-customize]")) {
      config.welcomeDismissed = true;
      void patchConfig({ welcomeDismissed: true });
      renderAll();
      setDrawer(true);
      return;
    }
    const redeem = target.closest<HTMLElement>("[data-redeem]");
    if (redeem) {
      const creditId = redeem.dataset.redeem!;
      if (
        window.confirm(
          "Use one Codex reset credit now?\n\nThis resets your Codex rate-limit windows immediately and cannot be undone.",
        )
      ) {
        const status = document.querySelector("#status")!;
        status.textContent = "Redeeming reset credit…";
        void invoke<string>("codex_redeem_credit", { creditId })
          .then((msg) => {
            status.textContent = msg;
            void refresh(true);
          })
          .catch((err) => {
            status.textContent = `Redeem failed: ${err}`;
          });
      }
      return;
    }
    const tab = target.closest("[data-tab]");
    if (tab) {
      switchSpendTab(tab.getAttribute("data-tab") as SpendTab);
      return;
    }
    const caret = target.closest<HTMLElement>("[data-caret]");
    if (caret) {
      const id = caret.dataset.caret!;
      const L = providerLayout(id);
      L.expanded = !L.expanded;
      saveLayout();
      animateExpandId = L.expanded ? id : null;
      renderAll();
      animateExpandId = null;
      return;
    }
    const flip = target.closest<HTMLElement>("[data-flip]");
    if (flip) {
      if (flip.dataset.flip === "usage") {
        config.showUsed = !config.showUsed;
        void patchConfig({ showUsed: config.showUsed });
      } else {
        config.resetExact = !config.resetExact;
        void patchConfig({ resetExact: config.resetExact });
      }
      renderAll();
    }
  });


  const tip = document.querySelector<HTMLElement>("#model-tip")!;
  providersEl.addEventListener("mouseover", (e) => {
    if (customizeOpen) return;
    const target = e.target as HTMLElement;
    const bar = target.closest<HTMLElement>("[data-trend]");
    if (bar) {
      showTrendTip(bar);
      return;
    }
    const row = target.closest<HTMLElement>("[data-spend]");
    if (row) showModelTip(row);
  });
  providersEl.addEventListener("mouseout", (e) => {
    const target = e.target as HTMLElement;
    const hovered = target.closest<HTMLElement>("[data-spend], [data-trend]");
    const to = e.relatedTarget as HTMLElement | null;
    if (hovered && (!to || !hovered.contains(to))) tip.hidden = true;
  });
  let scrollRaf = 0;
  providersEl.addEventListener("scroll", () => {
    tip.hidden = true;
    cancelAnimationFrame(scrollRaf);
    scrollRaf = requestAnimationFrame(updateTrailActive);
  });

  document.querySelector("#trail")!.addEventListener("click", (e) => {
    const tick = (e.target as HTMLElement).closest<HTMLElement>("[data-trail]");
    if (!tick) return;
    const card = trailCards()[Number(tick.dataset.trail)];
    card?.scrollIntoView({ behavior: "smooth", block: "start" });
  });

  // Update banner: shows once the background checker finds a newer release.
  void listen<string>("update-available", (e) => {
    if (document.querySelector("#update-banner")) return;
    const banner = document.createElement("button");
    banner.id = "update-banner";
    banner.textContent = `⬆ Update to v${e.payload} — restart`;
    banner.addEventListener("click", () => {
      banner.textContent = "Downloading update…";
      banner.disabled = true;
      invoke("install_update").catch((err) => {
        banner.textContent = `Update failed: ${err}`;
        banner.disabled = false;
      });
    });
    document.body.appendChild(banner);
  });

  void listen("popover-shown", () => {
    // Always reopen on the main page, at the top — leftover Customize/
    // Settings panels or a stale scroll position from the previous visit
    // feel like the app is stuck mid-page.
    setDrawer(false);
    setSettings(false);
    providersEl.scrollTop = 0;
    updateTrailActive();
    if (lastSnapshots.length && !customizeOpen) playReveal();
    void refresh();
  });
  void initSettings().then(() => {
    scheduleAutoRefresh();
    void refresh(true);
  });

  setInterval(() => {
    if (lastSnapshots.length && !customizeOpen) renderAll();
  }, 30_000);
});
