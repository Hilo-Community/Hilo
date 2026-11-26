import type { CockpitChatTab, CockpitStoreBundle } from "../stores/index.js";

/** A session whose agent just finished its turn. */
export interface AgentCompletion {
  readonly chatId: string;
  readonly title: string;
}

/** An [`AgentCompletion`] as handed to the app's alert, with whether the user was watching it. */
export interface AgentCompletionAlert extends AgentCompletion {
  /** The session was the active one in a focused window when its agent finished. */
  readonly attended: boolean;
}

/**
 * The sessions whose agent finished between two snapshots: an agent panel whose turn ended
 * cleanly (its badge became "done"), or a terminal whose recognized agent CLI went from busy to
 * idle. A session that merely appeared, or stopped being an agent, is not a completion.
 */
export function finishedAgents(
  previous: readonly CockpitChatTab[],
  next: readonly CockpitChatTab[],
): readonly AgentCompletion[] {
  const before = new Map(previous.map((chat) => [chat.chatId, chat]));
  const finished: AgentCompletion[] = [];
  for (const chat of next) {
    const was = before.get(chat.chatId);
    if (!was) continue;
    const done =
      was.kind === "agent" && chat.kind === "agent"
        ? was.statusBadge !== "done" && chat.statusBadge === "done"
        : was.kind === "terminal" &&
          chat.kind === "terminal" &&
          was.detectedAgentId !== undefined &&
          was.detectedAgentBusy === true &&
          chat.detectedAgentId === was.detectedAgentId &&
          chat.detectedAgentBusy !== true;
    if (done) finished.push({ chatId: chat.chatId, title: chat.title });
  }
  return finished;
}

/**
 * Tells `notify` every time an agent finishes, with whether the user was looking at that session
 * right then (`isAttended`) — the alert decides what that changes (e.g. no system notification
 * for an answer the user just watched arrive). Returns the unsubscribe function.
 */
export function watchAgentCompletions(
  stores: CockpitStoreBundle,
  notify: (completion: AgentCompletionAlert) => void,
  isAttended: (chatId: string) => boolean,
): () => void {
  return stores.zone3.subscribe((next, previous) => {
    if (next.chats === previous.chats) return;
    for (const completion of finishedAgents(previous.chats, next.chats)) {
      notify({ ...completion, attended: isAttended(completion.chatId) });
    }
  });
}
