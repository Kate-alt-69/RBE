//! Pre-bootstrap backend CLI coordinator and Error Code Book facade.
//!
//! The long-standing Error Code Book/router implementation remains isolated in
//! `error_code_book_core.rs`. Named package installation is intercepted here
//! before that compatibility router so backend.exe can execute real registry
//! resolution without granting the standalone `service` binary install authority.

#[path = "error_code_book_core.rs"]
mod core;
#[path = "install_cli.rs"]
mod install_cli;

pub use core::{explain, list_codes};

pub fn requested(args: &[String]) -> Option<anyhow::Result<String>> {
    if backend_install_authority() {
        if let Some(result) = install_cli::requested(args) {
            match result {
                Ok(rendered) => return Some(Ok(rendered)),
                Err(failure) => {
                    eprintln!("{}", failure.message);
                    std::process::exit(failure.code.into());
                }
            }
        }
    }
    core::requested(args)
}

fn backend_install_authority() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.file_stem().map(|name| name.to_string_lossy().into_owned()))
        .is_some_and(|name| name.eq_ignore_ascii_case("backend"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_lookup_still_delegates_to_embedded_book() {
        let rendered = explain("SVC5002").expect("embedded code book must remain available");
        assert!(rendered.contains("SVC5002"));
    }
}
