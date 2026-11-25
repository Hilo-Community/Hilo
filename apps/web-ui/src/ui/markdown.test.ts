import { describe, expect, it } from "vitest";
import { renderInline, renderMarkdownBlock } from "./markdown.js";

describe("agent Markdown", () => {
  it("renders a table with alignment and inline code", () => {
    const html = renderMarkdownBlock(
      "| File | Time |\n|---|---:|\n| `a.mp4` | 69,29 s |\n| b.mp4 | — |",
    );
    expect(html).toContain("<table>");
    expect(html).toContain("<th>File</th>");
    expect(html).toContain('<th style="text-align:right">Time</th>');
    expect(html).toContain("<td><code>a.mp4</code></td>");
    expect(html.match(/<tr>/g)).toHaveLength(3);
  });
  it("renders bold, italics and code, and leaves code literal", () => {
    expect(renderInline("a **b** and *c* and `**d**`")).toBe(
      "a <strong>b</strong> and <em>c</em> and <code>**d**</code>",
    );
  });
  it("lets bold wrap code and code sit inside italics, without stray asterisks", () => {
    expect(renderInline("**Deps in `latest`**: pin")).toBe(
      "<strong>Deps in <code>latest</code></strong>: pin",
    );
    expect(renderInline("** `dist/` and `public/` **")).toBe(
      "<strong> <code>dist/</code> and <code>public/</code> </strong>",
    );
    expect(renderInline("*see `x`*")).toBe("<em>see <code>x</code></em>");
    // Code stays literal, and text cannot forge a lifted span.
    expect(renderInline("`**not bold**`")).toBe("<code>**not bold**</code>");
    expect(renderInline("a\uE0000\uE000b")).toBe("a0b");
  });
  it("turns the references an agent writes into inline text that opens a file, not links", () => {
    const ref = (path: string, line: number, label: string) =>
      `<span role="link" tabindex="0" class="hilo-agent-ref" data-path="${path}" data-line="${line}" title="${path}:${line}">${label}</span>`;
    expect(renderInline("see [App.jsx](<src/App.jsx:6>) now")).toBe(
      `see ${ref("src/App.jsx", 6, "App.jsx")} now`,
    );
    expect(renderInline("[package.json](package.json)")).toBe(
      ref("package.json", 1, "package.json"),
    );
    expect(renderInline("[a](<src/a.ts:3>) · [b](<src/b.ts:9>)")).toBe(
      `${ref("src/a.ts", 3, "a")} · ${ref("src/b.ts", 9, "b")}`,
    );
    // Absolute paths into the agent's snapshot are made relative to it.
    expect(
      renderInline(
        "[x](</Users/me/Library/Application Support/dev.hilo.desktop/agent-worktrees/58f/acp-17-3/src/x.ts:4>)",
      ),
    ).toContain('data-path="src/x.ts" data-line="4"');
    // Code inside a label survives.
    expect(renderInline("[`Foo`](<src/foo.ts:2>)")).toContain("<code>Foo</code></span>");
  });
  it("never makes a link that navigates, and drops what could run", () => {
    const web = renderInline("[docs](https://example.com/a?b=1)");
    expect(web).toBe('<span class="hilo-agent-link" title="https://example.com/a?b=1">docs</span>');
    expect(web).not.toContain("<a ");
    for (const bad of [
      "[x](javascript:alert(1))",
      "[x](data:text/html,<b>)",
      "[x](/etc/passwd)",
      "[x](<../secret.txt:1>)",
      "[x](C:\\Windows\\a)",
    ]) {
      const html = renderInline(bad);
      expect(html).not.toContain("hilo-agent-ref");
      expect(html).not.toContain("<a ");
      expect(html).not.toContain("javascript:alert");
    }
    expect(renderInline('[x](<src/a"onclick="y.ts:1>)')).not.toContain('onclick="y');
  });
  it("renders lists and keeps single line breaks", () => {
    expect(renderMarkdownBlock("- one\n- **two**")).toBe(
      "<ul><li>one</li><li><strong>two</strong></li></ul>",
    );
    expect(renderMarkdownBlock("1. a\n2. b")).toContain("<ol>");
    expect(renderMarkdownBlock("line one\nline two")).toBe("<p>line one<br>line two</p>");
  });
  it("renders headings of any level, with text after them", () => {
    expect(renderMarkdownBlock("#### Small\nbody")).toBe("<h4>Small</h4><p>body</p>");
  });
  it("never emits agent HTML, links or images as markup", () => {
    const html = renderMarkdownBlock(
      "<img src=x onerror=alert(1)> [a](javascript:alert(1)) ![i](http://x/y.png)",
    );
    expect(html).not.toContain("<img");
    expect(html).not.toContain("<a ");
    expect(html).toContain("&lt;img");
    const cell = renderMarkdownBlock("| <b> |\n|---|\n| <script> |");
    expect(cell).not.toContain("<script>");
  });
  it("cuts very long tables", () => {
    const rows = Array.from({ length: 500 }, (_, i) => `| ${i} |`).join("\n");
    expect(renderMarkdownBlock(`| h |\n|---|\n${rows}`).match(/<tr>/g)?.length).toBe(201);
  });
});
