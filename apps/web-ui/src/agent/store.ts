import { AtomicStore } from "../state/atomic-store.js";
import { IncrementalMarkdownCache } from "../ui/stream-renderer.js";

export interface ProposedEdit {
  id: string;
  path: string;
  baseContentHash: string;
  oldText: string;
  newText: string;
}
export type AcpEvent =
  | { type: "terminalAuth"; name: string; args: string[]; env: Record<string, string> }
  | { type: "terminalAuthOutput"; data: number[] }
  | { type: "terminalAuthComplete"; success: boolean }
  | { type: "authMethods"; methods: unknown[] }
  | { type: "authChoice"; methods: unknown[] }
  | { type: "authenticated" }
  | { type: "elicitation"; id: string; url: string; message: string }
  | { type: "elicitationComplete"; id: string }
  | { type: "ready"; sessionId: string }
  | { type: "update"; update: Record<string, unknown> }
  | { type: "proposedEdit"; edit: ProposedEdit }
  | { type: "permission"; id: string; reason: string; options: unknown }
  | {
      type: "tool";
      id: string;
      kind: string;
      status: string;
      locations: unknown;
      reason: string | null;
    }
  | { type: "message"; role: string; text: string }
  | { type: "stopped" | "error"; reason: string };
export interface AgentHandle {
  startLogin?(): Promise<void>;
  /** Opens the sign-in page again, when the browser did not open or was closed. */
  openLogin?(): Promise<void>;
  /** The user's pick among the sign-in methods the agent offered. */
  chooseAuth?(methodId: string): Promise<void>;
  writeLogin?(data: string): Promise<void>;
  resizeLogin?(cols: number, rows: number): Promise<void>;
  prompt(text: string): Promise<void>;
  cancel(): Promise<void>;
  stop(): Promise<void>;
  answer(id: string, allow: boolean): Promise<void>;
  /** `open` shows the sign-in page again without answering the agent. */
  elicit?(id: string, action: "accept" | "decline" | "cancel" | "open"): Promise<void>;
}
export interface AgentHost {
  start(
    config: { installedAgentId: string },
    onEvent: (event: AcpEvent) => void,
  ): Promise<AgentHandle>;
  /**
   * Signs the agent out: the Host stops its sessions first, then removes its saved sign-in, so
   * the next connection asks how to sign in again.
   */
  signOut?(installedAgentId: string): Promise<void>;
}
export interface ToolCard {
  id: string;
  kind: string;
  title: string;
  status: "pending" | "running" | "done" | "denied" | "failed";
  locations: { path: string; line: number }[];
  reason?: string | undefined;
  edit?: ProposedEdit | undefined;
}
export interface AgentState {
  terminalAuth: { name: string; args: string[]; env: Record<string, string> } | null;
  /** The sign-in methods to choose from, while the Host waits for the user's pick. */
  authChoice: { id: string; name: string }[] | null;
  ready: boolean;
  busy: boolean;
  messages: { role: string; text: string; html: readonly string[] }[];
  plan: { content: string; status: string }[];
  tools: ToolCard[];
  permissions: { id: string; reason: string }[];
  elicitations: { id: string; url: string; message: string; accepted: boolean }[];
  error: string;
}
function record(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === "object" ? (value as Record<string, unknown>) : {};
}
function locations(value: unknown): ToolCard["locations"] {
  if (!Array.isArray(value)) return [];
  return value.slice(0, 128).flatMap((v) => {
    const item = record(v);
    const path = typeof item.path === "string" ? item.path : "";
    if (
      !path ||
      path.startsWith("/") ||
      path.includes("\\") ||
      path.includes(":") ||
      path.split("/").includes("..")
    )
      return [];
    return [
      {
        path,
        line:
          typeof item.line === "number" && Number.isSafeInteger(item.line) && item.line > 0
            ? item.line
            : 1,
      },
    ];
  });
}
function status(value: unknown): ToolCard["status"] {
  if (value === "completed" || value === "done") return "done";
  if (value === "in_progress" || value === "running") return "running";
  if (value === "denied" || value === "failed") return value;
  return "pending";
}

/**
 * What an event adds to the retained state, in characters. Tool updates are reduced to their
 * kind, title, status and locations, so their output (file contents, base64 images) is never
 * kept and must not count: a single image read would otherwise exhaust the session's budget.
 */
function retainedSize(event: AcpEvent): number {
  switch (event.type) {
    case "message":
      return event.text.length;
    case "terminalAuthOutput":
    case "authMethods":
    case "authChoice":
    case "authenticated":
      return 0;
    case "update": {
      const u = event.update;
      if (u.sessionUpdate === "agent_message_chunk") {
        const content = record(u.content);
        return typeof content.text === "string" ? content.text.length : 0;
      }
      if (u.sessionUpdate === "tool_call" || u.sessionUpdate === "tool_call_update") {
        const title = typeof u.title === "string" ? u.title.length : 0;
        const where = Array.isArray(u.locations) ? JSON.stringify(u.locations).length : 0;
        return 64 + title + where;
      }
      if (u.sessionUpdate === "plan") return JSON.stringify(u.entries ?? []).length;
      return 64;
    }
    default:
      return JSON.stringify(event).length;
  }
}

/** Own subscription domain: streaming never updates Zone 4 or the editor's mount container. */
export class AgentStore extends AtomicStore<AgentState> {
  private queue: AcpEvent[] = [];
  private queuedBytes = 0;
  private retainedBytes = 0;
  private scheduled = false;
  private disposed = false;
  private readonly markdown = new IncrementalMarkdownCache();
  constructor(
    private readonly frame: (callback: () => void) => void = (callback) =>
      requestAnimationFrame(callback),
  ) {
    super({
      ready: false,
      busy: false,
      messages: [],
      plan: [],
      tools: [],
      permissions: [],
      elicitations: [],
      terminalAuth: null,
      authChoice: null,
      error: "",
    });
  }
  enqueue(event: AcpEvent): void {
    if (this.disposed) return;
    const bytes = retainedSize(event);
    if (
      event.type !== "stopped" &&
      (this.queuedBytes + bytes > 512 * 1024 || this.retainedBytes + bytes > 2 * 1024 * 1024)
    ) {
      this.setState((s) => ({
        ...s,
        error: "Agent display limit reached. Cancel or start a new session.",
      }));
      return;
    }
    this.queue.push(event);
    this.queuedBytes += bytes;
    this.retainedBytes += bytes;
    if (!this.scheduled) {
      this.scheduled = true;
      this.frame(() => this.flush());
    }
  }
  flush(): void {
    this.scheduled = false;
    if (this.disposed || !this.queue.length) return;
    const queue = this.queue;
    this.queue = [];
    this.queuedBytes = 0;
    this.setState((previous) => {
      const s = {
        ...previous,
        messages: [...previous.messages],
        tools: [...previous.tools],
        permissions: [...previous.permissions],
        elicitations: [...previous.elicitations],
      };
      const tool = (id: string, patch: Partial<ToolCard>) => {
        const index = s.tools.findIndex((t) => t.id === id);
        const old = s.tools[index];
        const next: ToolCard = {
          id,
          kind: "other",
          title: "Tool call",
          status: "pending",
          locations: [],
          ...old,
          ...patch,
        };
        if (index >= 0) s.tools[index] = next;
        else if (s.tools.length < 512) s.tools.push(next);
      };
      const message = (role: string, text: string, append: boolean) => {
        const old = s.messages.at(-1);
        const combine = append && old?.role === role;
        const full = combine ? old.text + text : text;
        const next = { role, text: full, html: this.markdown.update(full) };
        if (combine) s.messages[s.messages.length - 1] = next;
        else if (s.messages.length < 512) s.messages.push(next);
      };
      for (const event of queue) {
        switch (event.type) {
          case "terminalAuth":
            s.terminalAuth = event;
            break;
          case "terminalAuthOutput":
            // Routed directly to the login terminal; never retained in chat/model context.
            break;
          case "terminalAuthComplete":
            s.terminalAuth = null;
            break;
          case "authMethods":
          case "authenticated":
            break;
          case "authChoice":
            s.authChoice = event.methods.flatMap((item) => {
              const method = record(item);
              return typeof method.id === "string" && typeof method.name === "string"
                ? [{ id: method.id, name: method.name }]
                : [];
            });
            break;
          case "elicitation":
            if (!s.elicitations.some((item) => item.id === event.id) && s.elicitations.length < 32)
              s.elicitations.push({ ...event, accepted: false });
            break;
          case "elicitationComplete":
            s.elicitations = s.elicitations.filter((item) => item.id !== event.id);
            break;
          case "ready":
            s.ready = true;
            s.authChoice = null;
            s.error = "";
            break;
          case "message":
            message(event.role, event.text, false);
            s.busy = true;
            break;
          case "proposedEdit":
            tool(event.edit.id, {
              edit: event.edit,
              kind: "edit",
              title: "Proposed edit",
              locations: [{ path: event.edit.path, line: 1 }],
            });
            break;
          case "permission":
            if (!s.permissions.some((p) => p.id === event.id) && s.permissions.length < 32)
              s.permissions.push(event);
            break;
          case "tool":
            tool(event.id, {
              kind: event.kind,
              status: status(event.status),
              locations: locations(event.locations),
              reason: event.reason ?? undefined,
            });
            break;
          case "error":
            s.error = event.reason;
            s.busy = false;
            break;
          case "stopped":
            s.ready = false;
            s.busy = false;
            s.permissions = [];
            s.elicitations = [];
            s.terminalAuth = null;
            s.authChoice = null;
            s.error = event.reason;
            s.tools = s.tools.map((t) =>
              t.status === "pending" || t.status === "running"
                ? { ...t, status: "denied", reason: event.reason }
                : t,
            );
            break;
          case "update": {
            const u = event.update;
            if (u.sessionUpdate === "agent_message_chunk") {
              const content = record(u.content);
              if (content.type === "text" && typeof content.text === "string")
                message("assistant", content.text, true);
            } else if (u.sessionUpdate === "plan" && Array.isArray(u.entries)) {
              s.plan = u.entries.slice(0, 128).map((v) => {
                const p = record(v);
                return { content: String(p.content ?? ""), status: String(p.status ?? "pending") };
              });
            } else if (
              (u.sessionUpdate === "tool_call" || u.sessionUpdate === "tool_call_update") &&
              typeof u.toolCallId === "string"
            ) {
              const patch: Partial<ToolCard> = {};
              if (typeof u.kind === "string") patch.kind = u.kind;
              if (typeof u.title === "string") patch.title = u.title;
              if (u.status !== undefined) patch.status = status(u.status);
              if (u.locations !== undefined) patch.locations = locations(u.locations);
              tool(u.toolCallId, patch);
            } else if (u.sessionUpdate === "turn_complete") s.busy = false;
            break;
          }
        }
      }
      return s;
    });
  }
  answered(id: string): void {
    this.setState((s) => ({ ...s, permissions: s.permissions.filter((p) => p.id !== id) }));
  }
  /** The user picked a method: the card goes away while the agent signs in. */
  chose(): void {
    this.setState((s) => ({ ...s, authChoice: null }));
  }
  answeredElicitation(id: string, action: "accept" | "decline" | "cancel"): void {
    this.setState((s) => ({
      ...s,
      elicitations:
        action === "accept"
          ? s.elicitations.map((item) => (item.id === id ? { ...item, accepted: true } : item))
          : s.elicitations.filter((item) => item.id !== id),
    }));
  }
  dispose(): void {
    this.disposed = true;
    this.queue = [];
  }
  reset(): void {
    this.queue = [];
    this.queuedBytes = 0;
    this.retainedBytes = 0;
    this.setState(() => ({
      ready: false,
      busy: false,
      messages: [],
      tools: [],
      plan: [],
      permissions: [],
      elicitations: [],
      terminalAuth: null,
      authChoice: null,
      error: "",
    }));
  }
}
