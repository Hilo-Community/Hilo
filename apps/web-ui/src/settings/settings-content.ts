/** Categories of the Settings sidebar. Each group is a heading inside its category. */
export const SETTINGS_SECTIONS = [
  {
    id: "external-agents",
    label: "External Agents",
    groups: ["ACP Registry"],
    keywords: "acp registry agents install update remove",
  },
] as const;
export type SettingsSectionId = (typeof SETTINGS_SECTIONS)[number]["id"];
