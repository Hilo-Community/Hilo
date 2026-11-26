# Hilo app icons

`Hilo.icon` is the original Icon Composer source. `Icon Exports/` contains the six original
PNG exports: Default, Dark, TintedLight, TintedDark, ClearLight and ClearDark. Keep these
source assets intact when regenerating the application icons.

From the repository root, `npm run icons` regenerates Tauri's platform icons from the
Default export, including `src-tauri/icons/icon.icns` for macOS and the browser favicon.
The current Tauri bundle uses a static icon; retaining the other exports does not enable
automatic appearance switching.

The macOS icon places the original artwork at 824 × 824 pixels inside a transparent
1024 × 1024 canvas (100 pixels per edge), so it fits the Dock without appearing oversized.
This padding applies only to the `.icns`; the other platform exports keep their original sizing.
