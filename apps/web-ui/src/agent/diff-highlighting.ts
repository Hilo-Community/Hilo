import { detectLanguageFromPath, resolveLanguageSupport } from "@hilo/editor-cm6";
import { parser as cppParser } from "@lezer/cpp";
import { classHighlighter, highlightTree, tagHighlighter, tags } from "@lezer/highlight";

interface Token {
  from: number;
  to: number;
  classes: string;
}
export interface CodePart {
  text: string;
  classes: string;
}
const functionHighlighter = tagHighlighter([
  {
    tag: [tags.function(tags.variableName), tags.function(tags.propertyName)],
    class: "tok-function",
  },
]);

/** Local preview string indices are UTF-16; these never address an authoritative document. */
export function highlightedDiffSource(source: string, path: string): CodePart[][] {
  // A large proposal remains readable without making parsing block the conversation stream.
  if (source.length > 128 * 1024) return [];
  const tokens: Token[] = [];
  const tree = /\.(c|h|cc|hh|cpp|hpp|cxx|hxx)$/i.test(path)
    ? cppParser.parse(source)
    : /\.(tsx?|mts|cts|jsx?|mjs|cjs|json|css|html?|md|markdown)$/i.test(path)
      ? resolveLanguageSupport(detectLanguageFromPath(path)).language.parser.parse(source)
      : undefined;
  if (tree) {
    highlightTree(tree, [classHighlighter, functionHighlighter], (from, to, classes) => {
      tokens.push({ from, to, classes });
    });
  }
  let offset = 0;
  let tokenIndex = 0;
  return source.split("\n").map((line) => {
    const end = offset + line.length;
    while (tokens[tokenIndex] && (tokens[tokenIndex]?.to ?? 0) <= offset) tokenIndex++;
    const parts: CodePart[] = [];
    let cursor = offset;
    for (let i = tokenIndex; i < tokens.length; i++) {
      const token = tokens[i];
      if (!token || token.from >= end) break;
      const start = Math.max(cursor, token.from);
      const stop = Math.min(end, token.to);
      if (stop <= start) continue;
      if (cursor < start) parts.push({ text: source.slice(cursor, start), classes: "" });
      parts.push({ text: source.slice(start, stop), classes: token.classes });
      cursor = stop;
    }
    if (cursor < end) parts.push({ text: source.slice(cursor, end), classes: "" });
    offset = end + 1;
    return parts;
  });
}
