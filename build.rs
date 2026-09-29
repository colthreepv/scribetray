use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/app.manifest");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let assembly_version = format!(
            "{}.{}.{}.0",
            env::var("CARGO_PKG_VERSION_MAJOR").expect("Cargo package major version is missing"),
            env::var("CARGO_PKG_VERSION_MINOR").expect("Cargo package minor version is missing"),
            env::var("CARGO_PKG_VERSION_PATCH").expect("Cargo package patch version is missing"),
        );
        let manifest_template =
            fs::read_to_string("assets/app.manifest").expect("failed to read assets/app.manifest");
        assert!(
            manifest_template.contains("@ASSEMBLY_VERSION@"),
            "assets/app.manifest is missing the assembly version placeholder"
        );
        let manifest = manifest_template.replace("@ASSEMBLY_VERSION@", &assembly_version);
        let manifest_path = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is missing"))
            .join("scribetray.manifest");
        fs::write(&manifest_path, manifest).expect("failed to generate Windows manifest");

        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon("assets/scribetray.ico")
            .set_manifest_file(
                manifest_path
                    .to_str()
                    .expect("manifest path is not valid UTF-8"),
            );
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
