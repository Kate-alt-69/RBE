use rbe_install_request::{InstallCommand, InstallTarget};

fn assert_sdk_command(command: InstallCommand, language: Option<&str>) {
    let InstallTarget::Named { key, version } = command.target else {
        panic!("SDK install must resolve to a named target");
    };
    assert_eq!(key, "sdk");
    assert!(version.is_none());
    assert_eq!(command.flags.language.as_deref(), language);
}

#[test]
fn bare_sdk_alias_selects_latest_channel() {
    let command = InstallCommand::parse(&["sdk"]).unwrap();
    assert_sdk_command(command, None);
}

#[test]
fn bare_sdk_accepts_language_selection() {
    let command = InstallCommand::parse(&["sdk", "-language=rust"]).unwrap();
    assert_sdk_command(command, Some("rust"));
}

#[test]
fn sdk_latest_accepts_project_and_typescript_flags() {
    let command = InstallCommand::parse(&["sdk.latest", "-path=.", "-language=typescript"]).unwrap();
    assert_eq!(command.flags.path.as_deref(), Some(std::path::Path::new(".")));
    assert_sdk_command(command, Some("typescript"));
}
