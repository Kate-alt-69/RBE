//! Real WASM execution for container workers.
//!
//! The first execution ABI is intentionally small: an approved module exports
//! `run() -> i32`. Resource limits are applied through Wasmtime fuel and store
//! limits here, plus the OS sandbox/resource layer around the worker process.

use anyhow::Result;
use wasmtime::{Config, Engine, Instance, Module, Store, StoreLimits, StoreLimitsBuilder};

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
        let engine = Engine::new(&config)
            .map_err(|error| anyhow::anyhow!("initialize Wasmtime engine: {error}"))?;
        Ok(Self { engine })
    }

    pub fn execute(&self, wasm: &[u8], limits: ExecutionLimits) -> Result<ExecutionResult> {
        let module = Module::new(&self.engine, wasm)
            .map_err(|error| anyhow::anyhow!("compile WASM artifact: {error}"))?;

        let memory_limit = usize::try_from(limits.max_memory_bytes)
            .map_err(|_| anyhow::anyhow!("WASM memory limit does not fit this platform's address space"))?;
        let store_limits: StoreLimits = StoreLimitsBuilder::new()
            .memory_size(memory_limit)
            .build();
        let mut store = Store::new(&self.engine, store_limits);
        store.limiter(|state| state);
        store
            .set_fuel(limits.fuel)
            .map_err(|error| anyhow::anyhow!("configure WASM fuel limit: {error}"))?;

        let instance = Instance::new(&mut store, &module, &[])
            .map_err(|error| anyhow::anyhow!("instantiate WASM artifact: {error}"))?;
        let run = instance
            .get_typed_func::<(), i32>(&mut store, "run")
            .map_err(|error| anyhow::anyhow!("WASM artifact must export run() -> i32: {error}"))?;
        let exit_code = run
            .call(&mut store, ())
            .map_err(|error| anyhow::anyhow!("execute WASM run(): {error}"))?;
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
