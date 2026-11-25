import { HILO_STATE_META, type HiloValidationState } from "@hilo/protocol-types";

export interface DesignStateTokens {
  readonly state: HiloValidationState;
  readonly icon: "✓" | "!" | "✕" | "⟳" | "•";
  readonly labelFr: string;
  readonly labelEn: string;
  readonly darkBg: string;
  readonly darkBorder: string;
  readonly darkText: string;
  readonly lightBg: string;
  readonly lightBorder: string;
  readonly lightText: string;
}

export const HILO_DESIGN_TOKENS: Record<HiloValidationState, DesignStateTokens> = {
  ValidatedGreen: {
    state: "ValidatedGreen",
    icon: HILO_STATE_META.ValidatedGreen.icon,
    labelFr: HILO_STATE_META.ValidatedGreen.labelFr,
    labelEn: HILO_STATE_META.ValidatedGreen.labelEn,
    darkBg: "#052e16",
    darkBorder: "#22c55e",
    darkText: "#86efac",
    lightBg: "#dcfce7",
    lightBorder: "#16a34a",
    lightText: "#14532d",
  },
  ReviewYellow: {
    state: "ReviewYellow",
    icon: HILO_STATE_META.ReviewYellow.icon,
    labelFr: HILO_STATE_META.ReviewYellow.labelFr,
    labelEn: HILO_STATE_META.ReviewYellow.labelEn,
    darkBg: "#422006",
    darkBorder: "#eab308",
    darkText: "#fde047",
    lightBg: "#fef9c3",
    lightBorder: "#ca8a04",
    lightText: "#713f12",
  },
  FailedRed: {
    state: "FailedRed",
    icon: HILO_STATE_META.FailedRed.icon,
    labelFr: HILO_STATE_META.FailedRed.labelFr,
    labelEn: HILO_STATE_META.FailedRed.labelEn,
    darkBg: "#450a0a",
    darkBorder: "#ef4444",
    darkText: "#fca5a5",
    lightBg: "#fee2e2",
    lightBorder: "#dc2626",
    lightText: "#7f1d1d",
  },
  StaleOrange: {
    state: "StaleOrange",
    icon: HILO_STATE_META.StaleOrange.icon,
    labelFr: HILO_STATE_META.StaleOrange.labelFr,
    labelEn: HILO_STATE_META.StaleOrange.labelEn,
    darkBg: "#431407",
    darkBorder: "#f97316",
    darkText: "#fdba74",
    lightBg: "#ffedd5",
    lightBorder: "#ea580c",
    lightText: "#7c2d12",
  },
  UnchangedBlue: {
    state: "UnchangedBlue",
    icon: HILO_STATE_META.UnchangedBlue.icon,
    labelFr: HILO_STATE_META.UnchangedBlue.labelFr,
    labelEn: HILO_STATE_META.UnchangedBlue.labelEn,
    darkBg: "#0f172a",
    darkBorder: "#3b82f6",
    darkText: "#93c5fd",
    lightBg: "#dbeafe",
    lightBorder: "#2563eb",
    lightText: "#1e3a8a",
  },
};
