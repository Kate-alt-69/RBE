//! Real WASM execution for container workers.
//!
//! The first execution ABI is intentionally small: an approved module exports
//! `run() -> i32`. Resource limits are applied through Wasmtime fuel here and
//! through the OS sandbox/resource layer around the worker process.

use anyhow::{Context, Result};
use wasmtime::{Config, Engine, Instance, Module, Store};

#[derive(Debug, Clone, Copy)]
pub struct ExecutionLimits {
    pub fuel: u64,
    pub max_memory_bytes: u64,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self { fuel: 10_000_000, max_memory_bytes: 64 * 1024 * 1024 }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ExecutionResult {
    pub exit_code: i32,
    pub fuel_consumed: u64,
}

pub struct WasmExecutor {
    engine: Engine,
}

impl WasmExecutor {
    pub fn new() -> Result<Self> {
        let mut config = Config::new();
        config.consume_fuel(true);
        config.cranelift_nan_canonicalization(true);
        Ok(Self { engine: Engine::new(&config)? })
    }

    pub fn execute(&self, wasm: &[u8], limits: ExecutionLimits) -> Result<ExecutionResult> {
        let module = Module::new(&self.engine, wasm).context("compile WASM artifact")?;
        let mut store = Store::new(&self.engine, ());
        store.set_fuel(limits.fuel).context("configure WASM fuel limit")?;

        let instance = Instance::new(&mut store, &module, &[])
            .context("instantiate WASM artifact")?;
        let run = instance
            .get_typed_func::<(), i32>(&mut store, "run")
            .context("WASM artifact must export run() -> i32")?;
        let exit_code = run.call(&mut store, ()).context("execute WASM run()")?;
        let remaining = store.get_fuel().unwrap_or(0);

        Ok(ExecutionResult {
            exit_code,
            fuel_consumed: limits.fuel.saturating_sub(remaining),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_wasm() {
        let executor = WasmExecutor::new().unwrap();
        assert!(executor.execute(b"not wasm", ExecutionLimits::default()).is_err());
    }
}
