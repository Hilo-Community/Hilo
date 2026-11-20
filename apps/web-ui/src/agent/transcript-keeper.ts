import type { AgentStore } from "./store.js";
import { type AgentTranscripts, parseTranscript, serializeTranscript } from "./transcript.js";

/** Wait this long after the last change before saving, so streaming is not written chunk by chunk. */
const SAVE_DELAY_MS = 600;

/**
 * Keeps one panel's conversation on disk: loads it when the panel comes back from a previous run,
 * saves it as it changes, and deletes it when the panel is closed for good. What is saved is
 * always what the panel shows, so emptying the panel empties the file.
 */
export class TranscriptKeeper {
  private timer: ReturnType<typeof setTimeout> | undefined;
  private saved = "";
  private onDisk = false;
  private wasBusy = false;
  private done = false;
  constructor(
    private readonly store: AgentStore,
    private readonly transcripts: AgentTranscripts,
    private readonly chatId: string,
    private readonly agentId: string,
  ) {}

  /** Show the conversation of the previous run, unless the panel already has one. */
  restore(): void {
    void this.transcripts
      .load(this.chatId)
      .then((json) => {
        if (this.done || !json || this.store.getState().timeline.length > 0) return;
        const snapshot = parseTranscript(json, this.agentId);
        if (!snapshot) return;
        this.store.restore(snapshot);
        this.onDisk = true;
        this.saved = json;
      })
      .catch(() => undefined);
  }

  /** The panel's state changed: save soon, or at once when a turn has just ended. */
  changed(): void {
    if (this.done) return;
    const state = this.store.getState();
    if (state.timeline.length === 0) {
      clearTimeout(this.timer);
      if (this.onDisk) this.erase();
      return;
    }
    const turnEnded = this.wasBusy && !state.busy;
    this.wasBusy = state.busy;
    clearTimeout(this.timer);
    if (turnEnded) this.flush();
    else this.timer = setTimeout(() => this.flush(), SAVE_DELAY_MS);
  }

  /** Save now what has not been saved yet. */
  flush(): void {
    clearTimeout(this.timer);
    if (this.done) return;
    const json = serializeTranscript(this.store.getState(), this.agentId);
    if (!json || json === this.saved) return;
    this.saved = json;
    this.onDisk = true;
    void this.transcripts.save(this.chatId, json).catch(() => undefined);
  }

  /** The panel was closed: its conversation goes with it, and nothing is written again. */
  discard(): void {
    clearTimeout(this.timer);
    this.done = true;
    this.erase();
  }

  private erase(): void {
    this.onDisk = false;
    this.saved = "";
    void this.transcripts.remove(this.chatId).catch(() => undefined);
  }
}
