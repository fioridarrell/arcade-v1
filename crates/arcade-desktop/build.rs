fn main() {
    // The app's own commands, so each window gets only those its capability grants
    // (capabilities/): content windows get none.
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(tauri_build::AppManifest::new().commands(&["api", "open_app", "open_content", "save_file"])))
        .expect("tauri build");
}
