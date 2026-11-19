import { SPINNER_ICON } from "../settings/agent-marks.js";
import { UI_STRINGS } from "../ui/ui-strings.js";
import type { AgentHandle, AgentStore } from "./store.js";

/**
 * The one-time code in an agent's sign-in message ("Sign in to ChatGPT and enter this code:
 * ABCD-1234"), or null when the message has none. Letters, digits and dashes only: anything else
 * is never shown as a code.
 */
export function signInCode(message: string): string | null {
  const found = /code:\s*(\S+)/i.exec(message);
  const code = found?.[1]?.replace(/[.,;:!]+$/, "");
  return code && /^[A-Za-z0-9-]{4,32}$/.test(code) ? code : null;
}

function hostOf(url: string): string {
  try {
    return new URL(url).hostname;
  } catch {
    return "";
  }
}

async function copy(doc: Document, text: string): Promise<boolean> {
  try {
    await doc.defaultView?.navigator.clipboard.writeText(text);
    return true;
  } catch {
    return false;
  }
}

/**
 * Sign-in requests from the agent. The page is opened by the Host (only the allowlisted address
 * the Host kept, never one the page supplies) as soon as the request arrives; this card shows
 * the code to type there and waits for the agent to report that the sign-in is complete.
 */
export function authenticationCards(
  doc: Document,
  store: AgentStore,
  handle: AgentHandle | null,
  run: (action: () => Promise<void>) => void,
): HTMLElement[] {
  return store.getState().elicitations.map((request) => {
    const card = doc.createElement("section");
    card.className = "coloride-agent-authentication";
    card.setAttribute("aria-label", "Agent account sign-in");

    const head = doc.createElement("div");
    head.className = "coloride-agent-authentication-head";
    const title = doc.createElement("strong");
    title.textContent = UI_STRINGS.agent.signInTitle;
    const host = doc.createElement("span");
    host.className = "coloride-agent-authentication-host";
    host.textContent = hostOf(request.url) || "Invalid authentication URL";
    head.append(title, host);
    card.append(head);

    const code = signInCode(request.message);
    const intro = doc.createElement("p");
    // Agent text is only ever shown as text, never as HTML.
    intro.textContent = code ? UI_STRINGS.agent.signInCode : UI_STRINGS.agent.signInNoCode;
    card.append(intro);

    if (code) {
      const row = doc.createElement("div");
      row.className = "coloride-agent-authentication-codebox";
      const value = doc.createElement("code");
      value.className = "coloride-agent-authentication-code";
      value.textContent = code;
      const copyButton = doc.createElement("button");
      copyButton.type = "button";
      copyButton.textContent = UI_STRINGS.agent.signInCopy;
      copyButton.addEventListener("click", () =>
        run(async () => {
          if (await copy(doc, code)) {
            copyButton.textContent = UI_STRINGS.agent.signInCopied;
            doc.defaultView?.setTimeout(() => {
              copyButton.textContent = UI_STRINGS.agent.signInCopy;
            }, 1500);
          }
        }),
      );
      row.append(value, copyButton);
      card.append(row);
    }

    const status = doc.createElement("p");
    status.className = "coloride-agent-authentication-status";
    if (request.accepted) {
      // The spinner is a constant of this app (agent-marks.ts), never agent-supplied markup.
      const spinner = doc.createElement("span");
      spinner.className = "coloride-agent-busy";
      spinner.setAttribute("aria-hidden", "true");
      spinner.innerHTML = SPINNER_ICON;
      status.append(spinner, UI_STRINGS.agent.signInWaiting);
    }
    card.append(status);

    const actions = doc.createElement("div");
    actions.className = "coloride-agent-authentication-actions";
    const act = (label: string, action: "accept" | "cancel" | "open", answers: boolean) => {
      const button = doc.createElement("button");
      button.type = "button";
      button.textContent = label;
      button.addEventListener("click", () =>
        run(async () => {
          if (!handle?.elicit) throw new Error("Account sign-in controls are unavailable.");
          await handle.elicit(request.id, action);
          if (answers)
            store.answeredElicitation(request.id, action === "accept" ? "accept" : "cancel");
        }),
      );
      actions.append(button);
    };
    // Normally already open: this is the way back when the browser did not open or was closed.
    if (request.accepted) act(UI_STRINGS.agent.signInOpenAgain, "open", false);
    else act(UI_STRINGS.agent.signInOpen, "accept", true);
    act(UI_STRINGS.agent.cancel, "cancel", true);
    card.append(actions);
    return card;
  });
}
