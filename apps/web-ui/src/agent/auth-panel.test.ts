// @vitest-environment happy-dom
import { afterEach, describe, expect, it, vi } from "vitest";
import { signInCode } from "./auth-panel.js";
import { AgentPanel } from "./panel.js";
import { type AcpEvent, type AgentHandle, type AgentHost, AgentStore } from "./store.js";

afterEach(() => vi.unstubAllGlobals());

const settle = async () => {
  for (let i = 0; i < 10; i++) await Promise.resolve();
};
const button = (root: HTMLElement, label: string) =>
  [...root.querySelectorAll("button")].find((b) => b.textContent === label) as
    | HTMLButtonElement
    | undefined;
const request: AcpEvent = {
  type: "elicitation",
  id: "7",
  url: "https://auth.openai.com/codex/device",
  message: "Sign in to ChatGPT and enter this code: ABCD-1234",
};

function fake() {
  let receive: (event: AcpEvent) => void = () => undefined;
  const handle: AgentHandle = {
    prompt: vi.fn(async () => undefined),
    cancel: vi.fn(async () => undefined),
    stop: vi.fn(async () => undefined),
    answer: vi.fn(async () => undefined),
    elicit: vi.fn(async () => undefined),
  };
  const host: AgentHost = {
    start: vi.fn(async (_, onEvent) => {
      receive = onEvent;
      return handle;
    }),
  };
  const store = new AgentStore(() => undefined);
  const panel = new AgentPanel(
    document,
    host,
    async () => undefined,
    { agentId: "codex-acp", name: "Codex" },
    true,
    store,
  );
  document.body.replaceChildren(panel.element);
  return { handle, store, panel, emit: (event: AcpEvent) => receive(event) };
}

describe("agent sign-in", () => {
  it("reads only a plain code out of the agent's message", () => {
    expect(signInCode("Sign in to ChatGPT and enter this code: ABCD-1234")).toBe("ABCD-1234");
    expect(signInCode("enter this code: ABCD-1234.")).toBe("ABCD-1234");
    expect(signInCode("Sign in on the page")).toBeNull();
    expect(signInCode("enter this code: <img src=x onerror=alert(1)>")).toBeNull();
    expect(signInCode(`enter this code: ${"A".repeat(40)}`)).toBeNull();
  });

  it("opens the page by itself once, shows the code and waits for the agent", async () => {
    const f = fake();
    await settle();
    f.emit(request);
    f.store.flush();
    await settle();
    expect(f.handle.elicit).toHaveBeenCalledTimes(1);
    expect(f.handle.elicit).toHaveBeenCalledWith("7", "accept");
    const card = f.panel.element.querySelector(".coloride-agent-authentication");
    expect(card?.querySelector(".coloride-agent-authentication-code")?.textContent).toBe(
      "ABCD-1234",
    );
    expect(card?.textContent).toContain("auth.openai.com");
    // A normal step, not a warning: it must not wear the permission card's yellow border.
    expect(card?.classList.contains("coloride-agent-permission")).toBe(false);
    expect(card?.querySelector(".coloride-agent-authentication-status svg")).not.toBeNull();
    expect(card?.textContent).toContain("Waiting for you to finish");
    // More renders never open the page again.
    f.store.setState((s) => ({ ...s }));
    await settle();
    expect(f.handle.elicit).toHaveBeenCalledTimes(1);
    // The way back when the browser did not open: show the page again, without answering twice.
    button(f.panel.element, "Open page again")?.click();
    await settle();
    expect(f.handle.elicit).toHaveBeenLastCalledWith("7", "open");
    f.emit({ type: "elicitationComplete", id: "7" });
    f.store.flush();
    expect(f.panel.element.querySelector(".coloride-agent-authentication")).toBeNull();
  });

  it("keeps a button to open the page when opening it automatically failed", async () => {
    const f = fake();
    vi.mocked(f.handle.elicit as NonNullable<AgentHandle["elicit"]>).mockRejectedValueOnce(
      new Error("Could not open the authentication URL"),
    );
    await settle();
    f.emit(request);
    f.store.flush();
    await settle();
    expect(f.store.getState().elicitations[0]?.accepted).toBe(false);
    expect(f.store.getState().error).toBe("");
    button(f.panel.element, "Open page")?.click();
    await settle();
    expect(f.handle.elicit).toHaveBeenLastCalledWith("7", "accept");
    expect(f.store.getState().elicitations[0]?.accepted).toBe(true);
  });

  it("shows only the code, and copies it when it is clicked", async () => {
    const writeText = vi.fn(async () => undefined);
    vi.stubGlobal("navigator", { clipboard: { writeText } });
    vi.useFakeTimers();
    try {
      const f = fake();
      await settle();
      f.emit(request);
      f.store.flush();
      await settle();
      expect(button(f.panel.element, "Copy code")).toBeUndefined();
      const code = f.panel.element.querySelector<HTMLElement>(
        ".coloride-agent-authentication-code",
      );
      expect(code?.textContent).toBe("ABCD-1234");
      expect(code?.dataset.copied).toBeUndefined();
      code?.click();
      await settle();
      expect(writeText).toHaveBeenCalledWith("ABCD-1234");
      expect(code?.dataset.copied).toBe("true");
      // The confirmation is brief, and the code stays the only text in the box.
      expect(code?.textContent).toBe("ABCD-1234");
      vi.advanceTimersByTime(1600);
      expect(code?.dataset.copied).toBeUndefined();
    } finally {
      vi.useRealTimers();
    }
  });

  it("copies from the keyboard", async () => {
    const writeText = vi.fn(async () => undefined);
    vi.stubGlobal("navigator", { clipboard: { writeText } });
    const f = fake();
    await settle();
    f.emit(request);
    f.store.flush();
    await settle();
    const code = f.panel.element.querySelector<HTMLElement>(".coloride-agent-authentication-code");
    expect(code?.getAttribute("role")).toBe("button");
    expect(code?.tabIndex).toBe(0);
    code?.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", cancelable: true }));
    await settle();
    expect(writeText).toHaveBeenCalledWith("ABCD-1234");
    expect(code?.dataset.copied).toBe("true");
  });

  it("does not claim the code was copied when there is no clipboard", async () => {
    vi.stubGlobal("navigator", {});
    const f = fake();
    await settle();
    f.emit(request);
    f.store.flush();
    await settle();
    const code = f.panel.element.querySelector<HTMLElement>(".coloride-agent-authentication-code");
    code?.click();
    await settle();
    expect(code?.dataset.copied).toBeUndefined();
    expect(f.store.getState().error).toBe("");
  });

  it("cancels the sign-in", async () => {
    const f = fake();
    await settle();
    f.emit(request);
    f.store.flush();
    await settle();
    button(f.panel.element, "Cancel")?.click();
    await settle();
    expect(f.handle.elicit).toHaveBeenLastCalledWith("7", "cancel");
    expect(f.store.getState().elicitations).toEqual([]);
  });
});

describe("choosing how to sign in", () => {
  it("offers a button per method, hides API-key methods, and reports the pick", async () => {
    const { authChoiceCard } = await import("./auth-panel.js");
    const chosen: string[] = [];
    const card = authChoiceCard(
      document,
      "Codex",
      [
        { id: "chatgpt-device", name: "ChatGPT (device code)" },
        { id: "chatgpt", name: "ChatGPT" },
        { id: "api-key", name: "API Key" },
      ],
      (id) => chosen.push(id),
    );
    expect(card?.textContent).toContain("Authenticate to Codex");
    const labels = [...(card?.querySelectorAll("button") ?? [])].map((b) => b.textContent);
    expect(labels).toEqual(["ChatGPT (device code)", "ChatGPT"]);
    card?.querySelectorAll("button")[1]?.click();
    expect(chosen).toEqual(["chatgpt"]);
    expect(authChoiceCard(document, "X", [{ id: "api_key", name: "x" }], () => {})).toBeNull();
  });
});
