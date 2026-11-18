// @vitest-environment happy-dom
import { describe, expect, it, vi } from "vitest";
import {
  type AgentAccountHost,
  type AgentAccountStatus,
  AgentUnlinkPanel,
} from "./agent-account-panel.js";

const settle = async () => {
  for (let i = 0; i < 20; i++) await Promise.resolve();
};
const status = (keySaved: boolean): AgentAccountStatus => ({
  preferences: { mode: "auto", provider: null, methodId: null, extraAllowedHosts: [] },
  providers: [{ id: "openai", name: "OpenAI", keySaved }],
  authMethods: [],
  knownProfile: true,
});
const installed = {
  id: "codex-acp@1",
  agentId: "codex-acp",
  name: "Codex",
  version: "1",
  distribution: "npm",
};
function button(root: HTMLElement, label: string): HTMLButtonElement {
  const found = [...root.querySelectorAll("button")].find((item) => item.textContent === label);
  if (!found) throw new Error(`Missing ${label}`);
  return found;
}
function fake(keySaved: boolean, overrides: Partial<AgentAccountHost> = {}) {
  const order: string[] = [];
  const host: AgentAccountHost = {
    status: vi.fn(async () => status(keySaved)),
    setKey: vi.fn(async (provider: string) => {
      order.push(`key:${provider}`);
    }),
    save: vi.fn(async () => {}),
    signOut: vi.fn(async () => {
      order.push("signOut");
    }),
    ...overrides,
  };
  return { host, order };
}

describe("unlinking an agent", () => {
  it("asks first and changes nothing until it is confirmed", async () => {
    const { host } = fake(false);
    const close = vi.fn();
    const panel = new AgentUnlinkPanel(document, host, installed, close);
    await settle();
    expect(panel.element.textContent).toContain("Signs Codex out");
    expect(panel.element.textContent).not.toContain("API key");
    expect(host.signOut).not.toHaveBeenCalled();
    button(panel.element, "Cancel").click();
    expect(close).toHaveBeenCalledOnce();
    expect(host.signOut).not.toHaveBeenCalled();
    expect(host.setKey).not.toHaveBeenCalled();
    panel.dispose();
  });

  it("signs the agent out and removes the saved key, naming the sharing", async () => {
    const { host, order } = fake(true);
    const panel = new AgentUnlinkPanel(document, host, installed, () => {});
    await settle();
    expect(panel.element.textContent).toContain("saved OpenAI API key");
    expect(panel.element.textContent).toContain("every agent that uses OpenAI");
    button(panel.element, "Unlink").click();
    await settle();
    expect(host.signOut).toHaveBeenCalledWith("codex-acp");
    expect(host.setKey).toHaveBeenCalledWith("openai", null);
    // The agent's sessions stop before the key goes.
    expect(order).toEqual(["signOut", "key:openai"]);
    expect(panel.element.textContent).toContain("Unlinked. Connect Codex again");
    expect(panel.element.querySelector("button")).toBeNull();
    panel.dispose();
  });

  it("leaves a key that was never saved alone", async () => {
    const { host } = fake(false);
    const panel = new AgentUnlinkPanel(document, host, installed, () => {});
    await settle();
    button(panel.element, "Unlink").click();
    await settle();
    expect(host.signOut).toHaveBeenCalledWith("codex-acp");
    expect(host.setKey).not.toHaveBeenCalled();
    panel.dispose();
  });

  it("reports a failure instead of claiming success, and lets the user try again", async () => {
    const signOut = vi.fn().mockRejectedValueOnce(new Error("Could not stop the agent"));
    const { host } = fake(true, { signOut });
    const panel = new AgentUnlinkPanel(document, host, installed, () => {});
    await settle();
    button(panel.element, "Unlink").click();
    await settle();
    expect(panel.element.textContent).toContain("Could not stop the agent");
    expect(panel.element.textContent).not.toContain("Unlinked.");
    // The key stays when the sign-out did not happen.
    expect(host.setKey).not.toHaveBeenCalled();
    expect(button(panel.element, "Unlink").disabled).toBe(false);
    button(panel.element, "Unlink").click();
    await settle();
    expect(panel.element.textContent).toContain("Unlinked.");
    expect(host.setKey).toHaveBeenCalledWith("openai", null);
    panel.dispose();
  });
});
