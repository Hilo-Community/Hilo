import type { AgentRegistryHost, InstalledAgent } from "@coloride/web-ui";
import { listen } from "@tauri-apps/api/event";

type Invoke = <T>(command: string, args?: Record<string, unknown>) => Promise<T>;

/** The Settings screen's window to the Host: the catalog, installs and removals. */
export function createTauriAgentRegistryHost(invoke: Invoke): AgentRegistryHost {
  const subscribe = <T>(name: string, callback: (payload: T) => void, ready?: () => void) => {
    let stop: (() => void) | undefined;
    let stopped = false;
    void listen<T>(name, (event) => {
      if (!stopped) callback(event.payload);
    }).then((unlisten) => {
      if (stopped) unlisten();
      else {
        stop = unlisten;
        ready?.();
      }
    });
    return () => {
      stopped = true;
      stop?.();
    };
  };
  return {
    list: (refresh) => invoke("agent_registry_list_command", { refresh }),
    install: (agentId, version) => invoke("agent_registry_install_command", { agentId, version }),
    uninstall: (installationId) => invoke("agent_registry_uninstall_command", { installationId }),
    open: (agentId) => invoke("agent_registry_open_command", { agentId }),
    onChanged: (listener) => subscribe("agent-registry-changed", listener, listener),
    onOpenRequested: (listener) => subscribe<InstalledAgent>("agent-open-requested", listener),
    onInstallConfirmed: (listener) => {
      let stop: (() => void) | undefined;
      let stopped = false;
      void listen<string>("agent-registry-install-confirmed", (event) => listener(event.payload))
        .then((unlisten) => {
          if (stopped) unlisten();
          else stop = unlisten;
        })
        .catch(() => {
          /* Without the event the bar simply stays in its waiting colour until the end. */
        });
      return () => {
        stopped = true;
        stop?.();
      };
    },
  };
}
