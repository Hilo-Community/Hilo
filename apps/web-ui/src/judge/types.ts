import type { DiagnosticSource, HiloValidationState } from "@hilo/protocol-types";

/** The Judge's verification steps, in the order they run (`LOT-015`). */
export type JudgeStepKind = "ImpactedTests" | "Lint" | "TypeCheck" | "FullSuite";

export type ReviewStatus = "Pending" | "Accepted" | "Rejected" | "Adjusting";

export interface ReviewFile {
  readonly path: string;
  /** The text the change was judged against. */
  readonly base_text: string;
  /** Exactly the content the Judge verified. */
  readonly proposed_text: string;
}

export interface ReviewStep {
  readonly kind: JudgeStepKind;
  readonly passed: boolean;
  readonly exitCode: number;
  readonly durationMs: number;
}

export interface ReviewDiagnostic {
  readonly filePath: string;
  readonly line: number;
  readonly column: number;
  readonly source: DiagnosticSource;
  readonly code: string;
  readonly message: string;
}

/** A judged change waiting for the user (the Host's `ReviewSummary`). */
export interface JudgeReview {
  readonly reviewId: string;
  readonly agentId: number;
  readonly verdict: HiloValidationState;
  readonly attempts: number;
  readonly status: ReviewStatus;
  /** Why automatic corrections stopped: `RetriesExhausted` or `SameFailureRepeated`. */
  readonly stopped: string | null;
  readonly files: readonly ReviewFile[];
  readonly steps: readonly ReviewStep[];
  readonly diagnostics: readonly ReviewDiagnostic[];
}

/** A validation range as the editor draws it (1-based lines). */
export interface JudgeStateDecoration {
  readonly fromLine: number;
  readonly toLine: number;
  readonly state: HiloValidationState;
  readonly snapshotHash: string;
}

export type JudgeEvent =
  | { readonly type: "verifying"; readonly agentId: number; readonly attempt: number }
  | {
      readonly type: "firstVerdict";
      readonly agentId: number;
      readonly attempt: number;
      readonly state: HiloValidationState;
      readonly elapsedMs: number;
    }
  | { readonly type: "retrying"; readonly agentId: number; readonly attempt: number }
  | { readonly type: "reviewReady"; readonly review: JudgeReview }
  | { readonly type: "reviewClosed"; readonly reviewId: string; readonly status: ReviewStatus }
  | { readonly type: "error"; readonly agentId: number; readonly reason: string }
  | {
      readonly type: "states";
      readonly documentId: string;
      readonly sessionGeneration: number;
      readonly contentHash: string;
      readonly decorations: readonly JudgeStateDecoration[];
    };

export interface AcceptedEdit {
  readonly from: number;
  readonly to: number;
  readonly insert: string;
}

export type AcceptResponse =
  | {
      readonly type: "apply";
      readonly taskId: string;
      readonly baseRevision: number;
      readonly changes: readonly AcceptedEdit[];
      readonly state: HiloValidationState;
    }
  | {
      readonly type: "conflict";
      readonly path: string;
      readonly reason: string;
      readonly baseText: string;
      readonly currentText: string;
      readonly proposedText: string;
    };

/** The Host side of the Judge. Only the Host decides; the UI asks and shows. */
export interface JudgeHost {
  listen(listener: (event: JudgeEvent) => void): Promise<() => void>;
  reviews(): Promise<readonly JudgeReview[]>;
  accept(
    reviewId: string,
    path: string,
    documentId: string,
    sessionGeneration: number,
  ): Promise<AcceptResponse>;
  reject(reviewId: string): Promise<void>;
  adjust(reviewId: string, note: string): Promise<void>;
  documentStates(
    documentId: string,
    sessionGeneration: number,
  ): Promise<readonly JudgeStateDecoration[]>;
}
