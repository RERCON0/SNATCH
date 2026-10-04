fn main() {
    println!("cargo:rerun-if-changed=icons/icon-cli.rc");
    println!("cargo:rerun-if-changed=icons/icon-cli.ico");
    println!("cargo:rerun-if-changed=icons/icon-app.rc");
    println!("cargo:rerun-if-changed=icons/icon-app.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // egui's with_icon() only sets the *runtime* window/taskbar icon
        // while the app is actually running - it has no effect on the .exe's
        // own PE resource icon, which is what Explorer, shortcuts and
        // Alt-Tab-before-launch actually read, so it still needs embedding
        // here too. snatch and snatch-app get their own distinct icon each
        // (orbital S for the CLI, folded ribbon S for the GUI).
        embed_resource::compile_for("icons/icon-cli.rc", ["snatch"], embed_resource::NONE)
            .manifest_optional()
            .unwrap();
        embed_resource::compile_for("icons/icon-app.rc", ["snatch-app"], embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }
}
