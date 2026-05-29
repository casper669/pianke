fn main() {
    // Force rebuild when frontend files change.
    // tauri_build::build() does NOT add rerun-if-changed for frontendDist files,
    // so we add it manually to ensure frontend changes are picked up.
    println!("cargo:rerun-if-changed=frontend/index.html");
    println!("cargo:rerun-if-changed=tauri.conf.json");
    tauri_build::build()
}
