use rbe_install_request::{InstallCommand, InstallTarget};

#[test]
fn bare_sdk_alias_selects_latest_channel() {
    let command = InstallCommand::parse(&["sdk"]).expect("bare SDK alias must parse");
    assert!(matches!(
        command.target,
        InstallTarget::Named {
            ref key,
            version: None
        } if key == "sdk"
    ));
    assert!(command.flags.language.is_none());
    assert!(command.flags.path.is_none());
}

#[test]
fn bare_sdk_accepts_language_selection() {
    let command = InstallCommand::parse(&["sdk", "-language=rust"])
        .expect("bare SDK alias with language must parse");
    assert!(matches!(
        command.target,
        InstallTarget::Named {
            ref key,
            version: None
        } if key == "sdk"
    ));
    assert_eq!(command.flags.language.as_deref(), Some("rust"));
}

#[test]
fn sdk_latest_accepts_project_and_typescript_flags() {
    let command = InstallCommand::parse(&[
        "sdk.latest",
        "-path=.",
        "-language=typescript",
    ])
    .expect("sdk.latest authoring command must parse");
    assert!(matches!(
        command.target,
        InstallTarget::Named {
            ref key,
            version: None
        } if key == "sdk"
    ));
    assert_eq!(command.flags.path.as_deref(), Some(std::path::Path::new(".")));
    assert_eq!(command.flags.language.as_deref(), Some("typescript"));
}
