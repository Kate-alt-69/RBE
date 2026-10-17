//! Bounded Argon2 work shared by synchronous compatibility calls and the async VM.
//! Admission happens before spawning; cancellation keeps the permit in the worker
//! until hashing actually finishes, rather than admitting replacement work early.

use std::sync::{Arc, OnceLock};

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Argon2, Params};
use tokio::sync::Semaphore;

use super::{crypto_error, crypto_expect_arity, crypto_string, ModuleError};
use crate::ast::Value;

const MAX_MEMORY_KIB: u32 = 64 * 1024;
const MAX_PASSES: u32 = 6;
const MAX_LANES: u32 = 4;
const MAX_MEMORY_PASSES: u64 = 256 * 1024;
const MAX_WORKERS: usize = 2;

enum Job {
    Hash(String),
    Verify { password: String, encoded: String },
}

fn worker_gate() -> Arc<Semaphore> {
    static GATE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    GATE.get_or_init(|| Arc::new(Semaphore::new(MAX_WORKERS)))
        .clone()
}

/// Validate encoded costs before acquiring a worker or allocating hash memory.
fn validate_hash(encoded: &str) -> Result<(), ModuleError> {
    let parsed = PasswordHash::new(encoded)
        .map_err(|_| crypto_error("CRY1001", "argon encoded hash is invalid"))?;
    if parsed.algorithm.as_str() != "argon2id" {
        return Err(crypto_error(
            "CRY1001",
            "argon verification accepts Argon2id hashes only",
        ));
    }
    // Check raw decimal costs first: malformed lane counts must not reach a
    // library parameter constructor that performs arithmetic on those values.
    let memory = parsed
        .params
        .get_decimal("m")
        .unwrap_or(Params::DEFAULT_M_COST);
    let passes = parsed
        .params
        .get_decimal("t")
        .unwrap_or(Params::DEFAULT_T_COST);
    let lanes = parsed
        .params
        .get_decimal("p")
        .unwrap_or(Params::DEFAULT_P_COST);
    if memory > MAX_MEMORY_KIB
        || passes > MAX_PASSES
        || lanes > MAX_LANES
        || u64::from(memory) * u64::from(passes) > MAX_MEMORY_PASSES
        || parsed.hash.is_some_and(|hash| hash.len() > 64)
    {
        return Err(crypto_error(
            "CRY1001",
            "argon encoded hash exceeds the verification work budget",
        ));
    }
    Params::try_from(&parsed)
        .map_err(|_| crypto_error("CRY1001", "argon encoded parameters are invalid"))?;
    Ok(())
}

fn prepare(function: &str, args: &[Value]) -> Result<Job, ModuleError> {
    match function {
        "hash" | "hashPassword" | "hash_password" => {
            crypto_expect_arity(function, args, 1)?;
            Ok(Job::Hash(
                crypto_string(
                    function,
                    args,
                    0,
                    "password",
                    super::CRYPTO_MAX_PASSWORD_BYTES,
                )?
                .to_owned(),
            ))
        }
        "verify" | "verifyPassword" | "verify_password" => {
            crypto_expect_arity(function, args, 2)?;
            let password = crypto_string(
                function,
                args,
                0,
                "password",
                super::CRYPTO_MAX_PASSWORD_BYTES,
            )?;
            let encoded = crypto_string(
                function,
                args,
                1,
                "encoded hash",
                super::CRYPTO_MAX_PASSWORD_HASH_BYTES,
            )?;
            validate_hash(encoded)?;
            Ok(Job::Verify {
                password: password.to_owned(),
                encoded: encoded.to_owned(),
            })
        }
        _ => Err(crypto_error("CRY1002", "unknown argon operation")),
    }
}

impl Job {
    fn execute(self) -> Result<Value, ModuleError> {
        match self {
            Self::Hash(password) => {
                let salt_bytes = super::secure_random_bytes(16)?;
                let salt = SaltString::encode_b64(&salt_bytes)
                    .map_err(|_| crypto_error("CRY4001", "Argon2id salt encoding failed"))?;
                Argon2::default()
                    .hash_password(password.as_bytes(), &salt)
                    .map(|encoded| Value::String(encoded.to_string()))
                    .map_err(|_| crypto_error("CRY4001", "Argon2id password hashing failed"))
            }
            Self::Verify { password, encoded } => {
                let parsed = PasswordHash::new(&encoded)
                    .map_err(|_| crypto_error("CRY1001", "argon encoded hash is invalid"))?;
                match Argon2::default().verify_password(password.as_bytes(), &parsed) {
                    Ok(()) => Ok(Value::Bool(true)),
                    Err(argon2::password_hash::Error::Password) => Ok(Value::Bool(false)),
                    Err(_) => Err(crypto_error(
                        "CRY1001",
                        "argon encoded hash cannot be verified",
                    )),
                }
            }
        }
    }
}

fn busy() -> ModuleError {
    crypto_error("CRY4001", "argon worker capacity is exhausted; retry later")
}

pub(super) fn call(function: &str, args: &[Value]) -> Result<Value, ModuleError> {
    let job = prepare(function, args)?;
    // Legacy synchronous execution must not block an asynchronous runtime thread.
    if tokio::runtime::Handle::try_current().is_ok() {
        return Err(crypto_error(
            "CRY4001",
            "argon requires the async evaluator inside a Tokio runtime",
        ));
    }
    let _permit = worker_gate().try_acquire_owned().map_err(|_| busy())?;
    job.execute()
}

pub(super) async fn call_async(function: &str, args: &[Value]) -> Result<Value, ModuleError> {
    let job = prepare(function, args)?;
    run_blocking(worker_gate(), move || job.execute()).await
}

async fn run_blocking<F>(gate: Arc<Semaphore>, work: F) -> Result<Value, ModuleError>
where
    F: FnOnce() -> Result<Value, ModuleError> + Send + 'static,
{
    if tokio::runtime::Handle::try_current().is_err() {
        return Err(crypto_error(
            "CRY4001",
            "argon async evaluation requires a Tokio runtime",
        ));
    }
    let permit = gate.try_acquire_owned().map_err(|_| busy())?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
    .map_err(|_| crypto_error("CRY4001", "argon worker failed"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_encoded_work_before_any_hash_execution() {
        for costs in [
            "m=65537,t=1,p=1",
            "m=8,t=7,p=1",
            "m=40,t=1,p=5",
            "m=65536,t=5,p=1",
            "m=4294967295,t=1,p=1",
            "m=8,t=1,p=4294967295",
        ] {
            let encoded = format!("$argon2id$v=19${costs}$c29tZXNhbHQ$AAAAAAAAAAAAAAAAAAAAAA");
            let error = validate_hash(&encoded).unwrap_err();
            assert!(error.message.contains("work budget"), "{}", error.message);
        }
        validate_hash("$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$AAAAAAAAAAAAAAAAAAAAAA").unwrap();
    }

    #[tokio::test]
    async fn cancelled_waiter_keeps_worker_capacity_reserved() {
        let gate = Arc::new(Semaphore::new(1));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let task_gate = gate.clone();
        let task = tokio::spawn(run_blocking(task_gate, move || {
            let _ = started_tx.send(());
            finish_rx.recv().unwrap();
            Ok(Value::Null)
        }));
        started_rx.await.unwrap();
        task.abort();
        let _ = task.await;
        assert_eq!(gate.available_permits(), 0);
        assert!(
            run_blocking(gate.clone(), || panic!("overload must not execute"))
                .await
                .is_err()
        );
        finish_tx.send(()).unwrap();
        let permit = tokio::time::timeout(std::time::Duration::from_secs(5), gate.acquire())
            .await
            .unwrap()
            .unwrap();
        drop(permit);
        assert_eq!(gate.available_permits(), 1);
    }
}
