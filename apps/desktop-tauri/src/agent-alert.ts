import type { AgentCompletionAlert } from "@hilo/web-ui";

type Invoke = <T>(command: string, args?: Record<string, unknown>) => Promise<T>;

/** The part of `HTMLAudioElement` used here. */
export interface AlertSound {
  currentTime: number;
  play(): Promise<void>;
}

/** Served from `public/` (same origin, so the app's content policy allows it). */
export const AGENT_DONE_SOUND = "/sounds/agent-done.mp3";

/**
 * macOS (and Windows/Linux) never show a notification a process hasn't been explicitly
 * authorized for — `notify` alone does not request that, so without this the very first
 * notification (and, on a dev build whose ad-hoc signature changes every rebuild, potentially
 * every one after it) silently shows nothing. Memoized per `createAgentFinishedAlert` instance:
 * the request is made at most once, on the first agent completion, not at app startup.
 */
function ensureNotificationPermission(invoke: Invoke): Promise<boolean> {
  return (async () => {
    const granted = await invoke<boolean>("plugin:notification|is_permission_granted").catch(
      () => false,
    );
    if (granted) return true;
    const state = await invoke<"granted" | "denied" | "default">(
      "plugin:notification|request_permission",
    ).catch(() => "denied" as const);
    return state === "granted";
  })();
}

/**
 * What the app does when an agent finishes: a short sound every time — the cue that it's done,
 * even for the session on screen, which the user may not be reading at that moment — and a
 * system notification named after the session, only for one nobody was looking at. Neither can
 * fail the app: a muted webview, a refused permission, or a refused notification is simply silent
 * (a refused sound is still reported to the console, since it otherwise just looks broken).
 */
export function createAgentFinishedAlert(
  invoke: Invoke,
  createSound: (source: string) => AlertSound = (source) => new Audio(source),
): (completion: AgentCompletionAlert) => void {
  let sound: AlertSound | null = null;
  let permission: Promise<boolean> | null = null;
  return ({ title, attended }) => {
    sound ??= createSound(AGENT_DONE_SOUND);
    sound.currentTime = 0;
    void sound.play().catch((error: unknown) => {
      console.warn("[hilo] agent-finished sound could not play", error);
    });
    if (attended) return;
    permission ??= ensureNotificationPermission(invoke);
    void permission
      .then((granted) => {
        if (!granted) return undefined;
        return invoke("plugin:notification|notify", { options: { title, body: "Finished" } });
      })
      .catch(() => undefined);
  };
}
