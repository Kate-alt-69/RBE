//! Real WASM execution for container workers.
//!
//! RBE modules export `run() -> i32`. Invocation data and bounded output are
//! exchanged through the small `rbe` host ABI; arbitrary host state, sockets,
//! environment variables, and files are not exposed to the guest.

use anyhow::Result;
use wasmtime::{Caller, Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};

#[derive(Debug, Clone, Copy)]
pub struct ExecutionLimits {
    pub fuel: u64,
    pub max_memory_bytes: u64,
    pub max_output_bytes: u64,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            fuel: 10_000_000,
            max_memory_bytes: 64 * 1024 * 1024,
            max_output_bytes: 2 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExecutionResult {
    pub exit_code: i32,
    pub fuel_consumed: u64,
    pub output: Vec<u8>,
}

struct ExecutionState {
    limits: StoreLimits,
    input: Vec<u8>,
    output: Vec<u8>,
    max_output_bytes: usize,
    abi_error: Option<String>,
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
        self.execute_with_input(wasm, &[], limits)
    }

    pub fn execute_with_input(
        &self,
        wasm: &[u8],
        input: &[u8],
        limits: ExecutionLimits,
    ) -> Result<ExecutionResult> {
        let module = Module::new(&self.engine, wasm)
            .map_err(|error| anyhow::anyhow!("compile WASM artifact: {error}"))?;

        let memory_limit = usize::try_from(limits.max_memory_bytes).map_err(|_| {
            anyhow::anyhow!("WASM memory limit does not fit this platform's address space")
        })?;
        let max_output_bytes = usize::try_from(limits.max_output_bytes).map_err(|_| {
            anyhow::anyhow!("WASM output limit does not fit this platform's address space")
        })?;
        if input.len() > i32::MAX as usize {
            anyhow::bail!("WASM invocation input is too large for the RBE ABI");
        }

        let store_limits: StoreLimits = StoreLimitsBuilder::new().memory_size(memory_limit).build();
        let mut store = Store::new(
            &self.engine,
            ExecutionState {
                limits: store_limits,
                input: input.to_vec(),
                output: Vec::new(),
                max_output_bytes,
                abi_error: None,
            },
        );
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(limits.fuel)
            .map_err(|error| anyhow::anyhow!("configure WASM fuel limit: {error}"))?;

        let mut linker = Linker::new(&self.engine);
        linker.func_wrap(
            "rbe",
            "input_len",
            |caller: Caller<'_, ExecutionState>| -> i32 {
                i32::try_from(caller.data().input.len()).unwrap_or(i32::MAX)
            },
        )?;
        linker.func_wrap(
            "rbe",
            "input_read",
            |mut caller: Caller<'_, ExecutionState>, ptr: i32, capacity: i32| -> i32 {
                if ptr < 0 || capacity < 0 {
                    return set_abi_error(
                        &mut caller,
                        "input_read received a negative pointer or capacity",
                    );
                }
                let input = caller.data().input.clone();
                if (capacity as usize) < input.len() {
                    return set_abi_error(
                        &mut caller,
                        "input_read capacity is smaller than invocation input",
                    );
                }
                let Some(memory) = caller
                    .get_export("memory")
                    .and_then(|export| export.into_memory())
                else {
                    return set_abi_error(
                        &mut caller,
                        "module does not export memory for input_read",
                    );
                };
                if memory.write(&mut caller, ptr as usize, &input).is_err() {
                    return set_abi_error(&mut caller, "input_read points outside guest memory");
                }
                input.len() as i32
            },
        )?;
        linker.func_wrap(
            "rbe",
            "output_write",
            |mut caller: Caller<'_, ExecutionState>, ptr: i32, length: i32| -> i32 {
                if ptr < 0 || length < 0 {
                    return set_abi_error(
                        &mut caller,
                        "output_write received a negative pointer or length",
                    );
                }
                let length = length as usize;
                if length > caller.data().max_output_bytes {
                    return set_abi_error(
                        &mut caller,
                        "module output exceeds the configured execution limit",
                    );
                }
                let Some(memory) = caller
                    .get_export("memory")
                    .and_then(|export| export.into_memory())
                else {
                    return set_abi_error(
                        &mut caller,
                        "module does not export memory for output_write",
                    );
                };
                let mut output = vec![0u8; length];
                if memory.read(&caller, ptr as usize, &mut output).is_err() {
                    return set_abi_error(&mut caller, "output_write points outside guest memory");
                }
                caller.data_mut().output = output;
                length as i32
            },
        )?;

        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|error| anyhow::anyhow!("instantiate WASM artifact: {error}"))?;
        let run = instance
            .get_typed_func::<(), i32>(&mut store, "run")
            .map_err(|error| anyhow::anyhow!("WASM artifact must export run() -> i32: {error}"))?;
        let exit_code = run
            .call(&mut store, ())
            .map_err(|error| anyhow::anyhow!("execute WASM run(): {error}"))?;
        if let Some(error) = store.data().abi_error.clone() {
            anyhow::bail!("WASM ABI violation: {error}");
        }
        let remaining = store.get_fuel().unwrap_or(0);
        let output = store.data().output.clone();

        Ok(ExecutionResult {
            exit_code,
            fuel_consumed: limits.fuel.saturating_sub(remaining),
            output,
        })
    }
}

fn set_abi_error(caller: &mut Caller<'_, ExecutionState>, message: &str) -> i32 {
    if caller.data().abi_error.is_none() {
        caller.data_mut().abi_error = Some(message.to_string());
    }
    -1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echo_module() -> Vec<u8> {
        wat::parse_str(
            r#"(module
                (import "rbe" "input_len" (func $input_len (result i32)))
                (import "rbe" "input_read" (func $input_read (param i32 i32) (result i32)))
                (import "rbe" "output_write" (func $output_write (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (func (export "run") (result i32)
                    (local $len i32)
                    call $input_len
                    local.set $len
                    i32.const 0
                    local.get $len
                    call $input_read
                    drop
                    i32.const 0
                    local.get $len
                    call $output_write
                    drop
                    i32.const 0))"#,
        )
        .unwrap()
    }

    #[test]
    fn rejects_invalid_wasm() {
        let executor = WasmExecutor::new().unwrap();
        assert!(executor
            .execute(b"not wasm", ExecutionLimits::default())
            .is_err());
    }

    #[test]
    fn host_abi_round_trips_binary_invocation_data() {
        let executor = WasmExecutor::new().unwrap();
        let result = executor
            .execute_with_input(&echo_module(), b"hello\0rbe", ExecutionLimits::default())
            .unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.output, b"hello\0rbe");
    }

    #[test]
    fn host_abi_enforces_output_limit_even_if_guest_ignores_return_code() {
        let executor = WasmExecutor::new().unwrap();
        let limits = ExecutionLimits {
            max_output_bytes: 4,
            ..ExecutionLimits::default()
        };
        let error = executor
            .execute_with_input(&echo_module(), b"hello", limits)
            .unwrap_err();
        assert!(error.to_string().contains("output exceeds"));
    }
}
