import type { AgentHandle, AgentStore } from "./store.js";

export function authenticationCards(
  doc: Document,
  store: AgentStore,
  handle: AgentHandle | null,
  run: (action: () => Promise<void>) => void,
): HTMLElement[] {
  return store.getState().elicitations.map((request) => {
    const card = doc.createElement("section");
    card.className = "coloride-agent-permission coloride-agent-authentication";
    card.setAttribute("aria-label", "Agent account sign-in");
    const message = doc.createElement("p");
    message.textContent = request.message;
    const url = doc.createElement("p");
    // Plain text, never agent-generated HTML or an automatic navigation.
    url.textContent = request.url;
    url.className = "coloride-agent-authentication-url";
    const host = doc.createElement("strong");
    try {
      host.textContent = new URL(request.url).hostname;
    } catch {
      host.textContent = "Invalid authentication URL";
    }
    card.append(host, message, url);
    if (request.accepted) {
      const pending = doc.createElement("p");
      pending.textContent = "Complete sign-in in your browser. Waiting for the agent…";
      card.append(pending);
    } else {
      for (const [label, action] of [
        ["Open in browser", "accept"],
        ["Decline", "decline"],
        ["Cancel", "cancel"],
      ] as const) {
        const button = doc.createElement("button");
        button.type = "button";
        button.textContent = label;
        button.addEventListener("click", () =>
          run(async () => {
            if (!handle?.elicit) throw new Error("Account sign-in controls are unavailable.");
            await handle.elicit(request.id, action);
            store.answeredElicitation(request.id, action);
          }),
        );
        card.append(button);
      }
    }
    return card;
  });
}
