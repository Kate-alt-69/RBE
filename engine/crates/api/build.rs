use std::env;
use std::fs;
use std::path::Path;

fn valid_hex(value: &str, bytes: usize) -> bool {
    value.len() == bytes * 2 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn main() {
    for name in [
        "RBE_ADMIN_AUTH_ROUNDS",
        "RBE_ADMIN_AUTH_SALT_HEX",
        "RBE_ADMIN_AUTH_VERIFIER_HEX",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR is set by Cargo");
    let destination = Path::new(&out_dir).join("dashboard_auth.rs");

    let rounds = env::var("RBE_ADMIN_AUTH_ROUNDS").ok();
    let salt = env::var("RBE_ADMIN_AUTH_SALT_HEX").ok();
    let verifier = env::var("RBE_ADMIN_AUTH_VERIFIER_HEX").ok();

    let generated = match (rounds, salt, verifier) {
        (Some(rounds), Some(salt), Some(verifier)) => {
            let rounds = rounds
                .parse::<u32>()
                .ok()
                .filter(|value| *value >= 10_000)
                .expect("RBE_ADMIN_AUTH_ROUNDS must be an integer >= 10000");
            if !valid_hex(&salt, 8) {
                panic!("RBE_ADMIN_AUTH_SALT_HEX must contain exactly 16 hexadecimal characters");
            }
            if !valid_hex(&verifier, 32) {
                panic!("RBE_ADMIN_AUTH_VERIFIER_HEX must contain exactly 64 hexadecimal characters");
            }
            format!(
                "pub const ADMIN_PASSWORD_CONFIGURED: bool = true;\n\
                 pub const ADMIN_PASSWORD_ROUNDS: u32 = {rounds};\n\
                 pub const ADMIN_PASSWORD_SALT_HEX: &str = {:?};\n\
                 pub const ADMIN_PASSWORD_VERIFIER_HEX: &str = {:?};\n",
                salt.to_ascii_lowercase(),
                verifier.to_ascii_lowercase(),
            )
        }
        (None, None, None) => {
            println!(
                "cargo:warning=api: admin dashboard password verifier is not configured; authenticated control room will stay locked"
            );
            "pub const ADMIN_PASSWORD_CONFIGURED: bool = false;\n\
             pub const ADMIN_PASSWORD_ROUNDS: u32 = 1;\n\
             pub const ADMIN_PASSWORD_SALT_HEX: &str = \"\";\n\
             pub const ADMIN_PASSWORD_VERIFIER_HEX: &str = \"\";\n"
                .to_string()
        }
        _ => panic!(
            "RBE admin verifier is incomplete; set rounds, salt, and verifier together"
        ),
    };

    fs::write(destination, generated).expect("write dashboard auth constants");
}
