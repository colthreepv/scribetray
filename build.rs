fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon("assets/scribetray.ico")
            .set_manifest_file("assets/app.manifest");
        // Tray icons use numeric resource IDs 2..=11 so they can be loaded with
        // LoadIconMetric without colliding with the application icon (ID 1).
        // The order matches `tray_icon_id` in `src/windows_ui.rs`.
        const TRAY_ICONS: [(&str, &str); 10] = [
            ("tray-idle-dark", "2"),
            ("tray-idle-light", "3"),
            ("tray-recording-dark", "4"),
            ("tray-recording-light", "5"),
            ("tray-working-dark", "6"),
            ("tray-working-light", "7"),
            ("tray-error-dark", "8"),
            ("tray-error-light", "9"),
            ("tray-off-dark", "10"),
            ("tray-off-light", "11"),
        ];
        for (name, id) in TRAY_ICONS {
            resource.set_icon_with_id(&format!("assets/tray/{name}.ico"), id);
        }
        resource
            .compile()
            .expect("failed to embed Windows resources");
    }
}
