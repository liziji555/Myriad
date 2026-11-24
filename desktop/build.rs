fn main() {
    // 列出命令，capabilities/ 里才能说哪个窗口能调哪几个。
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "desktop_state",
            "connect_server",
            "test_server",
            "install_key",
            "cancel_connect",
            "use_local",
            "pick_key",
            "pick_workdir",
            "retry",
        ]),
    ))
    .expect("tauri 构建失败")
}
