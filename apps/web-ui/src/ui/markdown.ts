/**
 * Renders one Markdown block (text between blank lines) to HTML for the agent conversation.
 * Agent text is untrusted: everything is escaped first and only the tags built here are emitted.
 * Links are never made clickable (a click would navigate the webview), images are not loaded.
 */

function escapeHtml(raw: string): string {
  return raw
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");
}

/** Marks where a piece was lifted out of a line: a private-use character no text has. */
const HOLE = "\uE000";
const HOLES = new RegExp(`${HOLE}(\\d+)${HOLE}`, "g");
/** `[label](destination)`, the destination bare or between angle brackets (as agents write it). */
const LINK = /\[([^\]\n]{1,300})\]\((&lt;[^\n]{1,1000}?&gt;|[^)\s]{1,1000})\)/g;
/** Where an agent's snapshot lives: absolute paths in its text point into it. */
const SNAPSHOT_ROOT = /^.*\/agent-worktrees\/[^/]+\/acp-[^/]+\//;

/**
 * What a link in an agent's text becomes. A reference to a file of the project (`src/a.ts:12`)
 * becomes inline text the panel turns into "open that file there"; a web address is only shown, with
 * its address as a tooltip, because following it would take over the app's window.
 */
function renderLink(label: string, rawDestination: string): string {
  const destination = rawDestination.replace(/^&lt;|&gt;$/g, "").replace(SNAPSHOT_ROOT, "");
  const reference = /^([^\s:#?\\]+?)(?::(\d{1,7}))?$/.exec(destination);
  const path = reference?.[1];
  if (path && !path.startsWith("/") && !path.split("/").includes("..")) {
    const line = Number(reference?.[2] ?? 1) || 1;
    // Plain inline text that acts as a link: it flows and wraps with the sentence around it.
    return `<span role="link" tabindex="0" class="hilo-agent-ref" data-path="${path}" data-line="${line}" title="${path}:${line}">${label}</span>`;
  }
  if (/^https?:\/\//i.test(destination))
    return `<span class="hilo-agent-link" title="${destination}">${label}</span>`;
  return label;
}

/**
 * Code spans, links, bold and italics inside one line. Code and links are lifted out first so
 * they stay literal, and put back after, so bold can wrap code (`**the `latest` tag**`).
 */
export function renderInline(text: string): string {
  const parts: string[] = [];
  const lift = (html: string) => {
    parts.push(html);
    return `${HOLE}${parts.length - 1}${HOLE}`;
  };
  const restore = (value: string) =>
    value.replace(HOLES, (_, index: string) => parts[Number(index)] ?? "");
  return restore(
    escapeHtml(text.replaceAll(HOLE, ""))
      .replace(/`([^`]+)`/g, (_, code: string) => lift(`<code>${code}</code>`))
      .replace(LINK, (_, label: string, destination: string) =>
        lift(renderLink(restore(label), destination)),
      )
      .replace(/\*\*([^*]+?)\*\*/g, "<strong>$1</strong>")
      .replace(/(^|[^*])\*([^*\s][^*]*?)\*(?!\*)/g, "$1<em>$2</em>"),
  );
}

const SEPARATOR = /^\s*\|?\s*:?-{3,}:?\s*(\|\s*:?-{3,}:?\s*)*\|?\s*$/;
const LIST_ITEM = /^\s*([-*+]|\d+[.)])\s+(.*)$/;
const HEADING = /^(#{1,6}) +(.*)$/;
/** A table or list longer than this is cut: one message must not build an enormous DOM. */
const MAX_ROWS = 200;

function cells(line: string): string[] {
  return line
    .trim()
    .replace(/^\|/, "")
    .replace(/\|$/, "")
    .split("|")
    .map((cell) => cell.trim());
}

function table(lines: string[]): string {
  const head = cells(lines[0] ?? "");
  const align = cells(lines[1] ?? "").map((cell) =>
    cell.startsWith(":") && cell.endsWith(":")
      ? "center"
      : cell.endsWith(":")
        ? "right"
        : cell.startsWith(":")
          ? "left"
          : "",
  );
  const cell = (tag: string, text: string, i: number) =>
    `<${tag}${align[i] ? ` style="text-align:${align[i]}"` : ""}>${renderInline(text)}</${tag}>`;
  const rows = lines.slice(2, 2 + MAX_ROWS).map(
    (line) =>
      `<tr>${cells(line)
        .map((text, i) => cell("td", text, i))
        .join("")}</tr>`,
  );
  return (
    `<table><thead><tr>${head.map((text, i) => cell("th", text, i)).join("")}</tr></thead>` +
    `<tbody>${rows.join("")}</tbody></table>`
  );
}

export function renderMarkdownBlock(block: string): string {
  const trimmed = block.trim();
  if (trimmed.startsWith("```")) {
    const firstNewline = trimmed.indexOf("\n");
    const lang = firstNewline > 3 ? trimmed.slice(3, firstNewline).trim() : "";
    const body =
      firstNewline >= 0
        ? trimmed.slice(firstNewline + 1).replace(/```$/, "")
        : trimmed.slice(3).replace(/```$/, "");
    return `<pre data-lang="${escapeHtml(lang)}"><code>${escapeHtml(body)}</code></pre>`;
  }
  const lines = trimmed.split("\n");
  const heading = HEADING.exec(lines[0] ?? "");
  if (heading?.[1] && heading[2] !== undefined) {
    const level = heading[1].length;
    const rest = lines.slice(1).join("\n");
    return `<h${level}>${renderInline(heading[2])}</h${level}>${rest.trim() ? renderMarkdownBlock(rest) : ""}`;
  }
  if (lines.length >= 2 && lines[0]?.includes("|") && SEPARATOR.test(lines[1] ?? "")) {
    return table(lines);
  }
  if (lines.every((line) => LIST_ITEM.test(line))) {
    const ordered = /^\s*\d/.test(lines[0] ?? "");
    const items = lines
      .slice(0, MAX_ROWS)
      .map((line) => `<li>${renderInline(LIST_ITEM.exec(line)?.[2] ?? "")}</li>`);
    return `<${ordered ? "ol" : "ul"}>${items.join("")}</${ordered ? "ol" : "ul"}>`;
  }
  return `<p>${lines.map(renderInline).join("<br>")}</p>`;
}
