fn main() {
    // The development app embeds the macOS .icns at compile time. Rebuild when an
    // icon changes so `tauri dev` does not restart with a cached application icon.
    println!("cargo:rerun-if-changed=icons");
    tauri_build::build();
}
