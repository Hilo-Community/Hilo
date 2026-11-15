/** A line of a unified diff: unchanged context, removed, added, or a gap of skipped lines. */
export type DiffLine = { kind: " " | "-" | "+"; text: string } | { kind: "…"; skipped: number };

/** Lines of unchanged context shown around each change. */
const CONTEXT = 3;
/** Above this many cells, the middle is shown as one replacement instead of a minimal diff. */
const MAX_LCS_CELLS = 4_000_000;

/**
 * The changes from `oldText` to `newText`, line by line, with a little context around each
 * one, as a reviewer reads them. The common start and end are matched first; the rest gets a
 * minimal (longest common subsequence) diff when it is small enough.
 */
export function diffLines(oldText: string, newText: string): DiffLine[] {
  const a = oldText.split("\n");
  const b = newText.split("\n");
  let start = 0;
  while (start < a.length && start < b.length && a[start] === b[start]) start++;
  let endA = a.length;
  let endB = b.length;
  while (endA > start && endB > start && a[endA - 1] === b[endB - 1]) {
    endA--;
    endB--;
  }
  const lines: { kind: " " | "-" | "+"; text: string }[] = a
    .slice(0, start)
    .map((text) => ({ kind: " ", text }));
  lines.push(...middle(a.slice(start, endA), b.slice(start, endB)));
  lines.push(...a.slice(endA).map((text) => ({ kind: " " as const, text })));
  return withContext(lines);
}

function middle(a: string[], b: string[]): { kind: " " | "-" | "+"; text: string }[] {
  if (a.length * b.length > MAX_LCS_CELLS) {
    return [
      ...a.map((text) => ({ kind: "-" as const, text })),
      ...b.map((text) => ({ kind: "+" as const, text })),
    ];
  }
  // lcs[i * width + j]: length of the longest common subsequence of a[i..] and b[j..].
  const width = b.length + 1;
  const lcs = new Uint32Array((a.length + 1) * width);
  for (let i = a.length - 1; i >= 0; i--) {
    for (let j = b.length - 1; j >= 0; j--) {
      lcs[i * width + j] =
        a[i] === b[j]
          ? (lcs[(i + 1) * width + j + 1] ?? 0) + 1
          : Math.max(lcs[(i + 1) * width + j] ?? 0, lcs[i * width + j + 1] ?? 0);
    }
  }
  const out: { kind: " " | "-" | "+"; text: string }[] = [];
  let i = 0;
  let j = 0;
  while (i < a.length || j < b.length) {
    if (i < a.length && j < b.length && a[i] === b[j]) {
      out.push({ kind: " ", text: a[i++] ?? "" });
      j++;
    } else if (
      j < b.length &&
      (i === a.length || (lcs[i * width + j + 1] ?? 0) > (lcs[(i + 1) * width + j] ?? 0))
    ) {
      out.push({ kind: "+", text: b[j++] ?? "" });
    } else {
      out.push({ kind: "-", text: a[i++] ?? "" });
    }
  }
  return out;
}

/** Keeps only the context near a change; longer unchanged runs become a gap. */
function withContext(lines: { kind: " " | "-" | "+"; text: string }[]): DiffLine[] {
  const near = new Uint8Array(lines.length);
  lines.forEach((line, index) => {
    if (line.kind === " ") return;
    const from = Math.max(0, index - CONTEXT);
    const to = Math.min(lines.length, index + CONTEXT + 1);
    near.fill(1, from, to);
  });
  const out: DiffLine[] = [];
  let skipped = 0;
  lines.forEach((line, index) => {
    if (near[index]) {
      if (skipped) out.push({ kind: "…", skipped });
      skipped = 0;
      out.push(line);
    } else {
      skipped++;
    }
  });
  if (skipped && out.length) out.push({ kind: "…", skipped });
  return out;
}

/** Plain-text rendering, for a `<pre>`. */
export function formatDiff(lines: DiffLine[]): string {
  if (!lines.length) return "(no change)";
  return lines
    .map((line) =>
      line.kind === "…" ? `… ${line.skipped} unchanged lines` : `${line.kind} ${line.text}`,
    )
    .join("\n");
}
