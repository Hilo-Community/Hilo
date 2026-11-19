/** The actions of a panel's menu that have a keyboard shortcut. */
export type PanelShortcutAction = "reload" | "reauthenticate" | "logOut" | "close";

interface PanelShortcut {
  readonly action: PanelShortcutAction;
  /** `KeyboardEvent.code`: the physical key, since Option changes the typed character on macOS. */
  readonly code: string;
  readonly letter: string;
}

/** Cmd+Option (Ctrl+Alt elsewhere) and a letter: free in the app and in macOS's own shortcuts. */
const PANEL_SHORTCUTS: readonly PanelShortcut[] = [
  { action: "reload", code: "KeyR", letter: "R" },
  { action: "reauthenticate", code: "KeyA", letter: "A" },
  { action: "logOut", code: "KeyO", letter: "O" },
  { action: "close", code: "KeyW", letter: "W" },
];

export function isMacPlatform(doc: Document): boolean {
  const nav = doc.defaultView?.navigator;
  return /Mac|iPhone|iPad/.test(nav?.platform ?? "") || /Mac OS X/.test(nav?.userAgent ?? "");
}

/** How a shortcut reads in a menu: "⌥⌘R" on macOS, "Ctrl+Alt+R" elsewhere. */
export function shortcutLabel(doc: Document, action: PanelShortcutAction): string | undefined {
  const shortcut = PANEL_SHORTCUTS.find((item) => item.action === action);
  if (!shortcut) return undefined;
  return isMacPlatform(doc) ? `⌥⌘${shortcut.letter}` : `Ctrl+Alt+${shortcut.letter}`;
}

/** The Settings shortcut, bound by the shell (Cmd/Ctrl+,), as a menu shows it. */
export function settingsShortcutLabel(doc: Document): string {
  return isMacPlatform(doc) ? "⌘," : "Ctrl+,";
}

/** The panel action `event` asks for, or null. */
export function panelShortcutAction(
  doc: Document,
  event: KeyboardEvent,
): PanelShortcutAction | null {
  const primary = isMacPlatform(doc) ? event.metaKey && !event.ctrlKey : event.ctrlKey;
  if (!primary || !event.altKey || event.shiftKey) return null;
  return PANEL_SHORTCUTS.find((item) => item.code === event.code)?.action ?? null;
}
