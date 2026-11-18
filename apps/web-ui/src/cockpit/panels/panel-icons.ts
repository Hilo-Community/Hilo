const svg = (body: string) =>
  `<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${body}</svg>`;

/** A window with the file tree as its right-hand column; `arrow` is the chevron in the editor. */
const treeIcon = (arrow: string) =>
  `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" aria-hidden="true"><path d="M22 12C22 8.25027 22 6.3754 21.0451 5.06107C20.7367 4.6366 20.3634 4.26331 19.9389 3.95491C18.6246 3 16.7497 3 13 3H11C7.25027 3 5.3754 3 4.06107 3.95491C3.6366 4.26331 3.26331 4.6366 2.95491 5.06107C2 6.3754 2 8.25027 2 12C2 15.7497 2 17.6246 2.95491 18.9389C3.26331 19.3634 3.6366 19.7367 4.06107 20.0451C5.3754 21 7.25027 21 11 21H13C16.7497 21 18.6246 21 19.9389 20.0451C20.3634 19.7367 20.7367 19.3634 21.0451 18.9389C22 17.6246 22 15.7497 22 12Z"/><path d="M14.5 3.5L14.5 20.5"/><path d="M19 7H17.5" stroke-linecap="round"/><path d="M19 11H17.5" stroke-linecap="round"/><path d="${arrow}" stroke-linecap="round"/></svg>`;
/** Chevron pointing right: the tree is visible and this hides it (towards the right edge). */
export const TREE_HIDE_ICON = treeIcon(
  "M8 10L9.22654 11.0572C9.74218 11.5016 10 11.7239 10 12C10 12.2761 9.74218 12.4984 9.22654 12.9428L8 14",
);
/** The same icon with the chevron pointing left: the tree is hidden and this brings it in. */
export const TREE_SHOW_ICON = treeIcon(
  "M10 10L8.77346 11.0572C8.25782 11.5016 8 11.7239 8 12C8 12.2761 8.25782 12.4984 8.77346 12.9428L10 14",
);
/** The Codex mark: a cloud with a prompt (`>_`) cut out of it, painted in the text colour. */
export const CODEX_ICON = `<svg class="coloride-codex-icon" xmlns="http://www.w3.org/2000/svg" viewBox="0 0 128 128" width="14" height="14" fill="none" aria-hidden="true"><path fill="currentColor" d="M43.125 2.437A32.56 32.56 0 0 1 59.37.224q10.664 1.225 19.008 9.067a.62.62 0 0 0 .571.155q11.265-2.768 21.659 1.952l.336.16l.821.405c7.237 3.749 12.427 9.44 15.563 17.056a30 30 0 0 1 2.245 11.339a30.2 30.2 0 0 1-.96 8.699a.89.89 0 0 0 .213.827a31.9 31.9 0 0 1 8.416 15.419c2.053 10.139-.053 19.28-6.309 27.413l-.971 1.173a32.34 32.34 0 0 1-15.648 9.872a.86.86 0 0 0-.576.544c-1.36 3.925-2.725 7.275-5.264 10.624c-6.395 8.437-15.797 13.131-26.389 13.072q-12.663-.066-22.453-9.259a.77.77 0 0 0-.747-.171c-2.763.891-5.547 1.019-8.555.987a31.6 31.6 0 0 1-13.84-3.317a32.3 32.3 0 0 1-11.445-9.499c-1.083-1.435-2.155-2.784-2.939-4.379a41.3 41.3 0 0 1-2.64-6.843a32.6 32.6 0 0 1-.091-16.341a.9.9 0 0 0 .043-.395a.6.6 0 0 0-.197-.341a31.8 31.8 0 0 1-7.36-11.744a27.7 27.7 0 0 1-1.776-8.475a36.9 36.9 0 0 1 1.003-11.371q3.6-11.871 13.744-18.629c1.504-1.003 2.933-1.781 4.277-2.336a43 43 0 0 1 4.592-1.621a.69.69 0 0 0 .464-.464a32.1 32.1 0 0 1 5.888-11.483q5.44-6.77 13.072-9.883m-4.288 41.867a4.523 4.523 0 0 0-7.856 4.491l9.035 15.813l-9.003 15.189a4.528 4.528 0 0 0 7.787 4.608l10.347-17.451a4.53 4.53 0 0 0 .037-4.555zm29.045 33.28a4.528 4.528 0 0 0 0 9.04h25.856a4.528 4.528 0 0 0 0-9.045H67.882z"/></svg>`;
/** Panel actions use a 24-unit grid and a 2-unit stroke, displayed at 14 px. */
const panelIcon = (body: string) =>
  `<svg xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" aria-hidden="true"><path d="M0 0h24v24H0z" fill="none"/>${body}</svg>`;
/** A window cut by a vertical line: the new panel appears on the right. */
export const SPLIT_RIGHT_ICON = panelIcon(
  '<g fill="none" stroke="currentColor" stroke-width="2"><rect width="20" height="18" x="2" y="3" stroke-linecap="round" stroke-linejoin="round" rx="2"/><path d="M12 3v18"/></g>',
);
/** A window cut by a horizontal line: the new panel appears below. */
export const SPLIT_DOWN_ICON = panelIcon(
  '<g fill="none" stroke="currentColor" stroke-width="2"><rect width="20" height="18" x="2" y="3" stroke-linecap="round" stroke-linejoin="round" rx="2"/><path d="M22 12H2"/></g>',
);
/** Four corners opening outwards: show this panel alone. */
export const MAXIMIZE_ICON =
  '<svg xmlns="http://www.w3.org/2000/svg" width="15" height="15" viewBox="0 0 16 16" aria-hidden="true"><path d="M0 0h16v16H0z" fill="none"/><path fill="currentColor" stroke="currentColor" stroke-width="0.333" stroke-linejoin="round" d="M3.75 3a.75.75 0 0 0-.75.75V5.5a.5.5 0 0 1-1 0V3.75C2 2.784 2.784 2 3.75 2H5.5a.5.5 0 0 1 0 1zM10 2.5a.5.5 0 0 1 .5-.5h1.75c.966 0 1.75.784 1.75 1.75V5.5a.5.5 0 0 1-1 0V3.75a.75.75 0 0 0-.75-.75H10.5a.5.5 0 0 1-.5-.5M2.5 10a.5.5 0 0 1 .5.5v1.75c0 .414.336.75.75.75H5.5a.5.5 0 0 1 0 1H3.75A1.75 1.75 0 0 1 2 12.25V10.5a.5.5 0 0 1 .5-.5m11 0a.5.5 0 0 1 .5.5v1.75A1.75 1.75 0 0 1 12.25 14H10.5a.5.5 0 0 1 0-1h1.75a.75.75 0 0 0 .75-.75V10.5a.5.5 0 0 1 .5-.5"/></svg>';
/** Four corners pointing inwards: bring the other panels back. */
export const RESTORE_ICON = panelIcon(
  '<path fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M7 2v5H2m15-5v5h5M2 17h5v5m15-5h-5v5"/>',
);
/** A terminal window with a prompt; used for terminal panels and their menu entry. */
export const TERMINAL_ICON =
  '<svg class="coloride-terminal-icon" xmlns="http://www.w3.org/2000/svg" width="12" height="12" viewBox="0 0 12 12" aria-hidden="true"><path d="M0 0h12v12H0z" fill="none"/><g fill="currentColor"><path d="M2.15 3.13c.2-.2.51-.2.71 0l2.5 2.5c.2.2.2.51 0 .71l-2.5 2.5c-.1.1-.23.15-.35.15L2.5 9a.47.47 0 0 1-.35-.15c-.2-.2-.2-.51 0-.71L4.3 5.99L2.15 3.84c-.2-.2-.2-.51 0-.71M9.5 8c.28 0 .5.22.5.5s-.22.5-.5.5h-4c-.28 0-.5-.22-.5-.5s.22-.5.5-.5z"/><path fill-rule="evenodd" d="M9.5 0A2.5 2.5 0 0 1 12 2.5v7A2.5 2.5 0 0 1 9.5 12h-7A2.5 2.5 0 0 1 0 9.5v-7A2.5 2.5 0 0 1 2.5 0zm-7 1C1.67 1 1 1.67 1 2.5v7c0 .83.67 1.5 1.5 1.5h7c.83 0 1.5-.67 1.5-1.5v-7c0-.83-.67-1.5-1.5-1.5z" clip-rule="evenodd"/></g></svg>';
export const CLOSE_PANEL_ICON = svg('<path d="M6 6l12 12M18 6L6 18"/>');
