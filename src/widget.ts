// Widget mode: an opt-in, always-on-screen window with a drag bar that can
// collapse to a slim usage ticker. The window side (size, blur,
// staying above the taskbar) lives in src-tauri/src/widget.rs; this module
// wires the bar and keeps the body classes in sync with the config.
import { invoke } from "@tauri-apps/api/core";
import { t } from "./i18n";
import type { Config } from "./main";

export interface WidgetDeps {
  getConfig: () => Config;
  patchConfig: (patch: Partial<Config>) => Promise<void>;
  brandColor: (providerId: string) => string;
}

let deps: WidgetDeps | null = null;

const $ = <T extends Element = HTMLElement>(sel: string) => document.querySelector<T>(sel);

export function initWidget(d: WidgetDeps): void {
  deps = d;

  $<HTMLInputElement>("#widget-mode-setting")?.addEventListener("change", (e) => {
    const on = (e.target as HTMLInputElement).checked;
    void update(on ? { widgetMode: true } : { widgetMode: false, widgetCollapsed: false });
  });
  $("#widget-collapse")?.addEventListener("click", () => {
    void update({ widgetCollapsed: !d.getConfig().widgetCollapsed });
  });
  $("#widget-lock")?.addEventListener("click", () => {
    void update({ widgetLocked: !d.getConfig().widgetLocked });
  });
  $("#widget-minimize")?.addEventListener("click", () => void invoke("hide_popover"));

  // The window is frameless, so the whole bar (minus its buttons) is the
  // drag handle — unless the position is locked.
  const bar = $("#widget-bar");
  bar?.addEventListener("mousedown", (e) => {
    if (e.button !== 0 || (e.target as Element).closest("button") || d.getConfig().widgetLocked) return;
    e.preventDefault();
    void invoke("widget_start_drag");
  });

  const logo = $("#app-logo img")?.getAttribute("src");
  if (logo) $("#widget-logo")?.replaceChildren(Object.assign(new Image(), { src: logo, alt: "" }));

  // Cards re-render on every refresh; the ticker follows while visible.
  const providers = $("#providers");
  if (providers) {
    new MutationObserver(() => {
      if (document.body.classList.contains("widget-collapsed") && !rolling) paintTicker();
    }).observe(providers, { childList: true, subtree: true });
  }
  setInterval(roll, 8000);

  // Dialogs (What's New, star prompt, confirms) need the full window.
  new MutationObserver((records) => {
    const dialog = records.some((r) => [...r.addedNodes].some((n) => n instanceof HTMLElement && n.id.endsWith("-overlay")));
    if (dialog && document.body.classList.contains("widget-collapsed")) void update({ widgetCollapsed: false });
  }).observe(document.body, { childList: true });

  void applyWidgetState();
}

async function update(patch: Partial<Config>): Promise<void> {
  await deps?.patchConfig(patch);
  await applyWidgetState();
}

// Collapsed ticker: one provider at a time (its primary limit), rolling
// to the next every 8 s. Built from the rendered cards so it stays
// decoupled from main.ts; hovering the bar pauses it.
let index = 0;
let rolling = false;

const usageCards = () =>
  [...document.querySelectorAll<HTMLElement>("#providers .provider:not(.total-spend, .welcome-card, .muted)")].filter(
    (card) => card.querySelector(".bar .fill"),
  );

function span(cls: string, text = ""): HTMLElement {
  const el = document.createElement("span");
  el.className = cls;
  el.textContent = text;
  return el;
}

function paintTicker(): void {
  const ticker = $("#widget-ticker");
  const cards = usageCards();
  $("#widget-bar")?.classList.toggle("has-usage", cards.length > 0);
  const card = cards[index % cards.length];
  if (!ticker || !card) return;

  // The fill is cloned as-is: its width is used% and its warn/low class
  // colors it exactly like the card (the headline text flips with the
  // "show used" setting, the bar does not).
  const fill = card.querySelector<HTMLElement>(".bar .fill")!;
  const row = fill.closest(".metric");
  const text = (el: Element | null | undefined) => el?.textContent?.trim() ?? "";
  const state = ["low", "warn"].find((c) => fill.classList.contains(c)) ?? "";

  const icon = span("provider-icon");
  icon.innerHTML = card.querySelector(".provider-icon")?.innerHTML ?? ""; // the app's own SVG
  icon.style.color = deps?.brandColor(card.dataset.provider ?? "") ?? "";
  const title = span("ticker-title", text(row?.querySelector(".metric-label")));
  title.prepend(span("ticker-name", text(card.querySelector(".provider-name"))));

  // "Resets in 2h 13m" → "↻ 2h 13m", "Expires in 2h" → "⌛ 2h": the icon
  // names the event, the pill keeps only the countdown. The card shows the
  // exact time instead when resetExact is on, with the countdown as its
  // tooltip, so both forms are matched against the localized templates.
  const el = row?.querySelector<HTMLElement>('[data-flip="reset"]');
  const full = text(el);
  const forms = [full, el?.title || el?.dataset.tip || ""];
  const countdown = (key: string) => {
    const [pre, post] = t(key, { time: "|" }).split("|");
    const f = forms.find((f) => f.length > pre.length + post.length && f.startsWith(pre) && f.endsWith(post));
    return f?.slice(pre.length, f.length - post.length);
  };
  const expires = countdown("card.expiresIn");
  const reset = span("ticker-reset", expires ?? countdown("card.resetsIn") ?? full);
  reset.title = full;
  reset.insertAdjacentHTML("afterbegin", expires ? EXPIRES_ICON : RESET_ICON);

  const bar = span("bar");
  bar.append(fill.cloneNode());
  const left = t("card.pctLeft", { n: Math.round(100 - (parseFloat(fill.style.width) || 0)) });
  ticker.replaceChildren(icon, title, full ? reset : span(""), bar, span(`ticker-pct ${state}`, left));
}

const ICON = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">';
const RESET_ICON = `${ICON}<path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"/><path d="M3 3v5h5"/></svg>`;
const EXPIRES_ICON = `${ICON}<path d="M5 22h14M5 2h14M17 22v-4.17a2 2 0 0 0-.59-1.42L12 12l-4.41 4.41A2 2 0 0 0 7 17.83V22M7 2v4.17a2 2 0 0 0 .59 1.42L12 12l4.41-4.41A2 2 0 0 0 17 6.17V2"/></svg>`;

function roll(): void {
  const ticker = $("#widget-ticker");
  const paused = document.hidden || !document.body.classList.contains("widget-collapsed") || $("#widget-bar")?.matches(":hover");
  if (!ticker || rolling || paused || usageCards().length < 2) return;
  index++;
  if (document.body.classList.contains("reduce-anim")) return paintTicker();

  rolling = true;
  const timing = { duration: 280, easing: "cubic-bezier(0.4, 0, 0.2, 1)" };
  ticker
    .animate([{}, { transform: "translateY(-50%)", opacity: 0 }], timing)
    .finished.then(() => {
      paintTicker();
      return ticker.animate([{ transform: "translateY(50%)", opacity: 0 }, {}], timing).finished;
    })
    .finally(() => (rolling = false));
}

/// Apply the config to the body classes, button hints and Rust window
/// state. Called on init, after every change and on settings reset.
export async function applyWidgetState(): Promise<void> {
  if (!deps) return;
  const cfg = deps.getConfig();
  const on = cfg.widgetMode === true;
  const collapsed = on && cfg.widgetCollapsed === true;
  const locked = on && cfg.widgetLocked === true;
  document.body.classList.toggle("widget-on", on);
  document.body.classList.toggle("widget-collapsed", collapsed);
  document.body.classList.toggle("widget-locked", locked);

  const toggle = $<HTMLInputElement>("#widget-mode-setting");
  if (toggle) toggle.checked = on;
  const hint = (sel: string, key: string) => {
    const btn = $(sel);
    btn?.setAttribute("title", t(key));
    btn?.setAttribute("aria-label", t(key));
  };
  hint("#widget-lock", locked ? "widget.unlock" : "widget.lock");
  hint("#widget-collapse", collapsed ? "widget.expand" : "widget.collapse");
  hint("#widget-minimize", "widget.minimize");
  if (collapsed) paintTicker();

  await invoke("widget_apply", { enabled: on, collapsed });
}
