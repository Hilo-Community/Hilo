# Hilo app icons

`Icon Exports/` contains the six PNG exports of the current icon (the brain-and-circuit mark):
Default, Dark, TintedLight, TintedDark, ClearLight and ClearDark. Keep them intact when
regenerating the application icons. `Hilo.icon` is the Icon Composer source of the previous
design (the double loop); it does not match these exports.

From the repository root, `npm run icons` regenerates Tauri's platform icons from the
Dark export, including `src-tauri/icons/icon.icns` for macOS and the browser favicon.
The current Tauri bundle uses a static icon; retaining the other exports does not enable
automatic appearance switching.

The macOS icon places the original artwork at 824 × 824 pixels inside a transparent
1024 × 1024 canvas (100 pixels per edge), so it fits the Dock without appearing oversized.
This padding applies only to the `.icns`; the other platform exports keep their original sizing.
