// @vitest-environment happy-dom
import { describe, expect, it, vi } from "vitest";
import {
  type AgentAccountHost,
  AgentAccountPanel,
  type AgentAccountStatus,
} from "./agent-account-panel.js";
import type { AgentRegistryHost } from "./agent-registry.js";

const settle = async () => {
  for (let i = 0; i < 20; i++) await Promise.resolve();
};
const status = (): AgentAccountStatus => ({
  preferences: { mode: "auto", provider: null, methodId: null, extraAllowedHosts: [] },
  providers: [{ id: "provider", name: "Provider", keySaved: false }],
  authMethods: [],
  knownProfile: true,
});
const registry = { open: vi.fn(async () => {}) } as unknown as AgentRegistryHost;
const installed = {
  id: "agent@1",
  agentId: "agent",
  name: "Agent",
  version: "1",
  distribution: "binary",
};
function button(root: HTMLElement, label: string): HTMLButtonElement {
  const button = [...root.querySelectorAll("button")].find((item) => item.textContent === label);
  if (!button) throw new Error(`Missing ${label}`);
  return button;
}

describe("agent account boundary", () => {
  it("clears the password input before the OS-store operation resolves and never displays the key", async () => {
    let resolve = () => {};
    const stored = new Promise<void>((done) => {
      resolve = done;
    });
    const host: AgentAccountHost = {
      status: vi.fn(async () => status()),
      setKey: vi.fn(() => stored),
      save: vi.fn(async () => {}),
      signOut: vi.fn(async () => {}),
    };
    const panel = new AgentAccountPanel(document, host, registry, installed);
    await settle();
    const input = panel.element.querySelector<HTMLInputElement>('input[type="password"]');
    if (!input) throw new Error("Missing password field");
    input.value = "synthetic-private-key";
    button(panel.element, "Save API key").click();
    expect(input.value).toBe("");
    expect(host.setKey).toHaveBeenCalledWith("provider", "synthetic-private-key");
    expect(panel.element.textContent).not.toContain("synthetic-private-key");
    resolve();
    await settle();
    expect(host.save).toHaveBeenCalledWith(
      "agent",
      expect.objectContaining({ mode: "apiKey", provider: "provider" }),
    );
    panel.dispose();
  });

  it("uses advertised login metadata and keeps unknown-agent network denied by default", async () => {
    const snapshot = status();
    snapshot.knownProfile = false;
    snapshot.providers = [];
    snapshot.authMethods = [{ id: "arbitrary-login-id", name: "Provider account" }];
    const host: AgentAccountHost = {
      status: vi.fn(async () => snapshot),
      setKey: vi.fn(async () => {}),
      save: vi.fn(async () => {}),
      signOut: vi.fn(async () => {}),
    };
    const panel = new AgentAccountPanel(document, host, registry, installed);
    await settle();
    expect(panel.element.textContent).toContain("Network is denied");
    expect(panel.element.querySelector("textarea")?.value).toBe("");
    button(panel.element, "Sign in · Provider account").click();
    await settle();
    expect(host.save).toHaveBeenCalledWith(
      "agent",
      expect.objectContaining({
        mode: "account",
        methodId: "arbitrary-login-id",
        extraAllowedHosts: [],
      }),
    );
    expect(registry.open).toHaveBeenCalledWith("agent");
    panel.dispose();
  });
});
