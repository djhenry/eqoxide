//! Invalid preview commands must fail before opening a window or starting networking.
#[test]
fn preview_requires_offline_mode_and_a_path() {
    for (args, expected) in [
        (vec!["--preview-glb", "missing.glb"], "requires --testzone"),
        (vec!["--testzone", "--preview-glb"], "requires a path"),
        (vec!["--testzone", "--preview-glb="], "non-empty path"),
    ] {
        let result = std::process::Command::new(env!("CARGO_BIN_EXE_eqoxide"))
            .args(args).output().unwrap();
        assert_eq!(result.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&result.stderr).contains(expected));
    }
}
