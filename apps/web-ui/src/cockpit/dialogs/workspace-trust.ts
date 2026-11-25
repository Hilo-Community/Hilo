export type WorkspaceTrustState = "Untrusted" | "Trusted";

export interface WorkspaceTrustReport {
  readonly state: WorkspaceTrustState;
  /** The open folder as the host sees it; null when no folder is open. */
  readonly workspaceRoot: string | null;
}

/**
 * How the cockpit learns and changes the trust decision. The decision itself is taken and kept
 * by the host (a native prompt): the UI can only ask for that prompt, never answer it.
 */
export interface WorkspaceTrustHost {
  getTrust(): Promise<WorkspaceTrustReport>;
  /** Shows the host's trust prompt and resolves with the decision once the user has answered. */
  requestTrust(): Promise<WorkspaceTrustReport>;
}

/**
 * Tracks the open folder's trust decision. A folder starts restricted (editing and highlighting
 * work, nothing from the folder is run) and there is no standing notice about it: the host's
 * native prompt is asked for when the user first tries to use the folder (a terminal or an agent),
 * through `ensureTrusted`.
 */
export class WorkspaceTrust {
  private report: WorkspaceTrustReport = { state: "Untrusted", workspaceRoot: null };
  /** Bumped on every query, so a slow answer about a previous folder is dropped. */
  private sequence = 0;
  private destroyed = false;
  /** The prompt being shown, so two sessions started together ask once. */
  private prompt: Promise<boolean> | null = null;

  constructor(private readonly host: WorkspaceTrustHost) {}

  getReport(): WorkspaceTrustReport {
    return this.report;
  }

  /** Asks the host for the decision about the folder that is open now. */
  refresh(): Promise<void> {
    return this.track(() => this.host.getTrust());
  }

  /**
   * Whether the open folder is trusted, asking the host's prompt first when it is not. Declining
   * resolves false (the host then refuses what was about to start and says why); nothing is
   * remembered, so the next attempt asks again.
   */
  ensureTrusted(): Promise<boolean> {
    this.prompt ??= this.askIfNeeded().finally(() => {
      this.prompt = null;
    });
    return this.prompt;
  }

  private async askIfNeeded(): Promise<boolean> {
    await this.refresh();
    if (this.report.workspaceRoot === null) return false;
    if (this.report.state === "Trusted") return true;
    await this.track(() => this.host.requestTrust());
    // `track` replaced the report: read it again rather than the narrowed value above.
    return this.getReport().state === "Trusted";
  }

  private async track(query: () => Promise<WorkspaceTrustReport>): Promise<void> {
    const sequence = ++this.sequence;
    let report: WorkspaceTrustReport;
    try {
      report = await query();
    } catch {
      // Unknown is untrusted: keep (or go back to) restricted mode.
      report = { state: "Untrusted", workspaceRoot: this.report.workspaceRoot };
    }
    if (this.destroyed || sequence !== this.sequence) return;
    this.report = report;
  }

  destroy(): void {
    this.destroyed = true;
  }
}
