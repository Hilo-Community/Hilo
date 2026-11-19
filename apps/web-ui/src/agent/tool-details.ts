/** Retain display previews only, never arbitrary ACP payloads or binary content. */
export const TOOL_OUTPUT_LIMIT = 16 * 1024;
export const TOOL_INPUT_LIMIT = 4 * 1024;
export const TOOL_DETAILS_BUDGET = 640 * 1024;
/** A tool call can change several files; each side of a file is kept up to this many characters. */
export const TOOL_DIFF_FILES = 8;
export const TOOL_DIFF_TEXT = 32 * 1024;

/** One file changed by a tool, as the agent reported it (`oldText` is empty for a new file). */
export interface FileDiff {
  path: string;
  oldText: string;
  newText: string;
  /** The texts were cut at the display limit, so the diff shows only their start. */
  truncated?: boolean;
}
/** A path inside the worktree: nothing absolute, no drive, no way out of it. */
export function isWorktreePath(path: string): boolean {
  return (
    path.length > 0 &&
    !path.startsWith("/") &&
    !path.includes("\\") &&
    !path.includes(":") &&
    !path.split("/").includes("..")
  );
}

export interface ToolDetails {
  command?: string;
  output?: string;
  arguments?: [string, string][];
  exitCode?: number;
  truncated?: boolean;
  diffs?: FileDiff[];
}
/** What one update adds to a tool: a replacement preview, and maybe more output as it runs. */
export interface ToolUpdate extends ToolDetails {
  /** Output produced since the last update (agents that stream a command's output send it). */
  delta?: string;
}
// Terminal escape sequences all start with the ESC control character, so the patterns need it.
// biome-ignore lint/suspicious/noControlCharactersInRegex: matching ESC is the point
const OSC_SEQUENCE = /\u001b\][^\u0007\u001b]*(?:\u0007|\u001b\\)/g;
// biome-ignore lint/suspicious/noControlCharactersInRegex: matching ESC is the point
const CSI_SEQUENCE = /\u001b\[[0-?]*[ -/]*[@-~]/g;
// biome-ignore lint/suspicious/noControlCharactersInRegex: matching ESC is the point
const SHORT_SEQUENCE = /\u001b[@-Z\\-_]/g;
/** Terminal escape sequences (colors, cursor moves, titles): output is shown as plain text. */
export function stripTerminalEscapes(text: string): string {
  return text
    .replace(OSC_SEQUENCE, "")
    .replace(CSI_SEQUENCE, "")
    .replace(SHORT_SEQUENCE, "")
    .replace(/\r(?!\n)/g, "");
}
export function record(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === "object" ? (value as Record<string, unknown>) : {};
}

/** ACP tool content is a replacement snapshot; omitted fields keep the previous preview. */
export function toolDetails(update: Record<string, unknown>): ToolUpdate {
  const details: ToolUpdate = {};
  const input = record(update.rawInput);
  const output = record(update.rawOutput);
  const command = input.command ?? input.cmd ?? update.command;
  if (typeof command === "string") {
    details.command = command.slice(0, TOOL_INPUT_LIMIT);
    if (command.length > TOOL_INPUT_LIMIT) details.truncated = true;
  }
  if (update.rawInput !== undefined) {
    let remaining = TOOL_INPUT_LIMIT;
    details.arguments = [];
    for (const key of Object.keys(input).slice(0, 24)) {
      const value = input[key];
      if (typeof value === "object" && value !== null) continue;
      const name = key.slice(0, Math.min(128, remaining));
      remaining -= name.length;
      const text = String(value).slice(0, remaining);
      remaining -= text.length;
      details.arguments.push([name, text]);
      if (remaining <= 0) {
        details.truncated = true;
        break;
      }
    }
  }
  if (output.truncated === true) details.truncated = true;
  const code = output.exitCode ?? record(output.exitStatus).exitCode ?? update.exitCode;
  if (typeof code === "number" && Number.isSafeInteger(code)) details.exitCode = code;
  let text = "";
  let hasText = false;
  const append = (value: unknown) => {
    if (typeof value !== "string") return;
    hasText = true;
    const available = Math.max(0, TOOL_OUTPUT_LIMIT - text.length);
    if (value.length > available) details.truncated = true;
    text += value.slice(0, available);
  };
  if (Array.isArray(update.content)) {
    for (const block of update.content.slice(0, 128)) {
      const item = record(block);
      const content = record(item.content);
      if (item.type === "content" && content.type === "text") {
        if (hasText) append("\n");
        append(content.text);
      }
    }
    if (update.content.length > 128) details.truncated = true;
    // Edits come as one diff per file: old and new text, which the card shows as a diff.
    const diffs: FileDiff[] = [];
    for (const block of update.content.slice(0, 128)) {
      const item = record(block);
      if (item.type !== "diff" || typeof item.path !== "string" || !isWorktreePath(item.path))
        continue;
      if (typeof item.newText !== "string") continue;
      if (diffs.length >= TOOL_DIFF_FILES) {
        details.truncated = true;
        break;
      }
      const oldText = typeof item.oldText === "string" ? item.oldText : "";
      const cut = oldText.length > TOOL_DIFF_TEXT || item.newText.length > TOOL_DIFF_TEXT;
      diffs.push({
        path: item.path,
        oldText: oldText.slice(0, TOOL_DIFF_TEXT),
        newText: item.newText.slice(0, TOOL_DIFF_TEXT),
        ...(cut ? { truncated: true } : {}),
      });
    }
    if (diffs.length) details.diffs = diffs;
  }
  if (!hasText) {
    if (typeof update.rawOutput === "string") append(update.rawOutput);
    else {
      append(output.stdout ?? output.output);
      if (typeof output.stderr === "string") {
        if (hasText) append("\n");
        append(output.stderr);
      }
    }
  }
  if (hasText) details.output = text;
  // Streaming agents put the output of a running command in `_meta`, a chunk at a time.
  const meta = record(update._meta);
  const chunk = record(meta.terminal_output_delta ?? meta.terminal_output);
  if (!hasText && typeof chunk.data === "string") {
    details.delta = stripTerminalEscapes(chunk.data.slice(0, TOOL_OUTPUT_LIMIT));
    if (chunk.data.length > TOOL_OUTPUT_LIMIT) details.truncated = true;
  }
  const exit = record(meta.terminal_exit);
  if (typeof exit.exit_code === "number" && Number.isSafeInteger(exit.exit_code))
    details.exitCode = exit.exit_code;
  return details;
}
export function detailsSize(details: ToolDetails): number {
  return (
    (details.command?.length ?? 0) +
    (details.output?.length ?? 0) +
    (details.arguments?.reduce((sum, [key, value]) => sum + key.length + value.length, 0) ?? 0) +
    (details.diffs?.reduce(
      (sum, file) => sum + file.path.length + file.oldText.length + file.newText.length,
      0,
    ) ?? 0)
  );
}

/** Drop excess previews without dropping tool lifecycle updates. */
export function fitDetails<T extends ToolDetails>(details: T, available: number): T {
  if (detailsSize(details) <= available) return details;
  let remaining = Math.max(0, available);
  const next = { ...details, truncated: true };
  next.command = (details.command ?? "").slice(0, remaining);
  remaining -= next.command.length;
  if (details.diffs) {
    // A file is shown whole or not at all: half a file would read as a different change.
    next.diffs = [];
    for (const file of details.diffs) {
      const size = file.path.length + file.oldText.length + file.newText.length;
      if (size > remaining) break;
      next.diffs.push(file);
      remaining -= size;
    }
  }
  next.arguments = [];
  for (const [key, value] of details.arguments ?? []) {
    if (key.length > remaining) break;
    const text = value.slice(0, remaining - key.length);
    next.arguments.push([key, text]);
    remaining -= key.length + text.length;
  }
  next.output = (details.output ?? "").slice(0, remaining);
  return next;
}
