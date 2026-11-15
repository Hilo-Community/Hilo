import { describe, expect, it } from "vitest";
import { diffLines, formatDiff } from "./diff.js";

const numbered = (count: number) => Array.from({ length: count }, (_, i) => `line ${i}`);

describe("proposal diff", () => {
  it("shows only the changed line and its context in a long file", () => {
    const old = numbered(100);
    const changed = [...old];
    changed[50] = "changed";
    expect(formatDiff(diffLines(old.join("\n"), changed.join("\n")))).toBe(
      [
        "… 47 unchanged lines",
        "  line 47",
        "  line 48",
        "  line 49",
        "- line 50",
        "+ changed",
        "  line 51",
        "  line 52",
        "  line 53",
        "… 46 unchanged lines",
      ].join("\n"),
    );
  });

  it("keeps unchanged lines between two changes as context, not as a removal", () => {
    const lines = diffLines("a\nkeep\nb", "A\nkeep\nB");
    expect(lines.filter((line) => line.kind === "-").length).toBe(2);
    expect(lines).toContainEqual({ kind: " ", text: "keep" });
  });

  it("shows insertions, deletions and a new file", () => {
    expect(formatDiff(diffLines("a\nc", "a\nb\nc"))).toBe("  a\n+ b\n  c");
    expect(formatDiff(diffLines("a\nb\nc", "a\nc"))).toBe("  a\n- b\n  c");
    expect(formatDiff(diffLines("", "new 🦀"))).toBe("- \n+ new 🦀");
    expect(formatDiff(diffLines("same", "same"))).toBe("(no change)");
  });
});
