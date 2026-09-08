from pathlib import Path

cargo = Path("engine/crates/service-runtime/Cargo.toml")
text = cargo.read_text()
anchor = "[target.'cfg(unix)'.dependencies]\nlibc = \"0.2\"\n"
replacement = anchor + (
    "\n[target.'cfg(windows)'.dependencies]\n"
    "windows-sys = { version = \"0.59\", features = [\"Win32_Foundation\", "
    "\"Win32_Security\", \"Win32_System_JobObjects\", \"Win32_System_Threading\"] }\n"
)
if "[target.'cfg(windows)'.dependencies]" not in text:
    if anchor not in text:
        raise SystemExit("service-runtime Cargo.toml dependency anchor changed")
    cargo.write_text(text.replace(anchor, replacement, 1))

lib = Path("engine/crates/service-runtime/src/lib.rs")
source = lib.read_text()
old = '''#[cfg(not(unix))]
fn apply_memory_limit(_memory_limit_mb: u64) -> anyhow::Result<()> {
    Ok(())
}'''
new = '''#[cfg(windows)]
fn apply_memory_limit(memory_limit_mb: u64) -> anyhow::Result<()> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    if memory_limit_mb == 0 {
        return Ok(());
    }

    let limit_bytes = memory_limit_mb
        .checked_mul(1024 * 1024)
        .ok_or_else(|| anyhow::anyhow!("service memoryLimitMb is too large"))?;
    let limit_bytes = usize::try_from(limit_bytes)
        .map_err(|_| anyhow::anyhow!("service memoryLimitMb exceeds Windows addressable memory"))?;

    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(anyhow::anyhow!(
            "create Windows service memory Job Object: {}",
            std::io::Error::last_os_error()
        ));
    }

    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY;
    limits.ProcessMemoryLimit = limit_bytes;

    let configured = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if configured == 0 {
        let error = std::io::Error::last_os_error();
        unsafe {
            CloseHandle(job);
        }
        return Err(anyhow::anyhow!(
            "configure Windows service memory Job Object: {error}"
        ));
    }

    let assigned = unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) };
    if assigned == 0 {
        let error = std::io::Error::last_os_error();
        unsafe {
            CloseHandle(job);
        }
        return Err(anyhow::anyhow!(
            "assign service process to Windows memory Job Object: {error}"
        ));
    }

    // Windows keeps a job alive while it still has associated processes, even
    // after the last userspace handle closes. KILL_ON_JOB_CLOSE is deliberately
    // not enabled, so closing our setup handle preserves this process limit.
    unsafe {
        CloseHandle(job);
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn apply_memory_limit(memory_limit_mb: u64) -> anyhow::Result<()> {
    if memory_limit_mb == 0 {
        Ok(())
    } else {
        anyhow::bail!("service memoryLimitMb is not supported on this platform")
    }
}'''
if old not in source:
    raise SystemExit("service memory limit anchor changed")
lib.write_text(source.replace(old, new, 1))
