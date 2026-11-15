import type { AgentRegistryHost } from "@coloride/web-ui";

type Invoke = <T>(command: string, args?: Record<string, unknown>) => Promise<T>;

/** The Settings screen's window to the Host: the catalog, installs and removals. */
export function createTauriAgentRegistryHost(invoke: Invoke): AgentRegistryHost {
  return {
    list: (refresh) => invoke("agent_registry_list_command", { refresh }),
    install: (agentId, version) => invoke("agent_registry_install_command", { agentId, version }),
    uninstall: (installationId) => invoke("agent_registry_uninstall_command", { installationId }),
  };
}
