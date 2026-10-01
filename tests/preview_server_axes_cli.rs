//! Invalid server-axis opt-ins must stop before creating a renderer or network session.
#[test]
fn server_axes_requires_both_preview_path_and_offline_mode() {
    for (args, expected) in [
        (vec!["--preview-server-axes"], "--preview-server-axes requires --preview-glb and --testzone"),
        (vec!["--testzone", "--preview-server-axes"], "--preview-server-axes requires --preview-glb"),
        (vec!["--preview-glb", "missing.glb", "--preview-server-axes"], "--preview-server-axes requires --testzone"),
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_eqoxide"))
            .args(&args).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.lines().any(|line| line == format!("error: {expected}")), "{stderr}");
    }
}
