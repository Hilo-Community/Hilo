import type { JudgeEvent, JudgeHost } from "@hilo/web-ui";

type Invoke = <T>(command: string, args?: Record<string, unknown>) => Promise<T>;
type Listen = <T>(event: string, handler: (event: { payload: T }) => void) => Promise<() => void>;

/** Carries a `JudgeEvent` from the Rust side (`commands/judge.rs`). */
export const JUDGE_EVENT = "hilo-judge";

/** The Judge commands of the Rust Host (`LOT-015`). */
export function createTauriJudgeHost(invoke: Invoke, listen: Listen): JudgeHost {
  return {
    listen: (listener) => listen<JudgeEvent>(JUDGE_EVENT, (event) => listener(event.payload)),
    reviews: () => invoke("judge_reviews_command"),
    accept: (reviewId, path, documentId, sessionGeneration) =>
      invoke("judge_accept_command", { reviewId, path, documentId, sessionGeneration }),
    reject: (reviewId) => invoke("judge_reject_command", { reviewId }),
    adjust: (reviewId, note) => invoke("judge_adjust_command", { reviewId, note }),
    documentStates: (documentId, sessionGeneration) =>
      invoke("judge_document_states_command", { documentId, sessionGeneration }),
  };
}
