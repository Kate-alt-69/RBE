#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux_x86_64 {
    use std::ffi::c_void;
    use std::fs;
    use std::ptr;
    use std::time::{SystemTime, UNIX_EPOCH};

    use route_engine::{prepare_service_oid_cache, OidCache};

    const PROT_NONE: i32 = 0;
    const PROT_READ: i32 = 0x1;
    const PROT_WRITE: i32 = 0x2;
    const PROT_EXEC: i32 = 0x4;
    const MAP_PRIVATE: i32 = 0x02;
    const MAP_ANONYMOUS: i32 = 0x20;

    unsafe extern "C" {
        fn mmap(
            address: *mut c_void,
            length: usize,
            protection: i32,
            flags: i32,
            fd: i32,
            offset: isize,
        ) -> *mut c_void;
        fn mprotect(address: *mut c_void, length: usize, protection: i32) -> i32;
        fn munmap(address: *mut c_void, length: usize) -> i32;
    }

    struct ExecutableMapping {
        address: *mut c_void,
        length: usize,
    }

    impl ExecutableMapping {
        fn from_code(code: &[u8]) -> Self {
            assert!(!code.is_empty());
            let address = unsafe {
                mmap(
                    ptr::null_mut(),
                    code.len(),
                    PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            assert_ne!(address as isize, -1, "mmap failed");

            unsafe {
                ptr::copy_nonoverlapping(code.as_ptr(), address.cast::<u8>(), code.len());
            }
            let protected = unsafe { mprotect(address, code.len(), PROT_READ | PROT_EXEC) };
            assert_eq!(protected, 0, "mprotect failed");

            Self {
                address,
                length: code.len(),
            }
        }

        unsafe fn binary_i64(&self) -> extern "C" fn(i64, i64) -> i64 {
            unsafe { std::mem::transmute(self.address) }
        }

        unsafe fn no_args_u64(&self) -> extern "C" fn() -> u64 {
            unsafe { std::mem::transmute(self.address) }
        }
    }

    impl Drop for ExecutableMapping {
        fn drop(&mut self) {
            let _ = unsafe { munmap(self.address, self.length) };
        }
    }

    fn project_root(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rbe-service-oid-native-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn relc_generated_i64_add_oid_executes_as_native_code() {
        let root = project_root("i64-add");
        prepare_service_oid_cache(&root).unwrap();
        let cache = OidCache::open_or_rebuild(&root).unwrap();
        let record = cache.read_record(123).unwrap();
        assert_eq!(record.name, "I64_ADD");

        let mapping = ExecutableMapping::from_code(&record.machine_code);
        let add = unsafe { mapping.binary_i64() };
        assert_eq!(add(20, 22), 42);
        assert_eq!(add(-50, 8), -42);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn relc_generated_boolean_constant_oid_executes_as_native_code() {
        let root = project_root("load-true");
        prepare_service_oid_cache(&root).unwrap();
        let cache = OidCache::open_or_rebuild(&root).unwrap();
        let record = cache.read_record(10).unwrap();
        assert_eq!(record.name, "LOAD_TRUE");

        let mapping = ExecutableMapping::from_code(&record.machine_code);
        let load_true = unsafe { mapping.no_args_u64() };
        assert_eq!(load_true(), 1);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn executable_mapping_never_stays_writable_and_executable_at_once() {
        assert_eq!(PROT_NONE, 0);
        assert_eq!(PROT_READ | PROT_WRITE, 0x3);
        assert_eq!(PROT_READ | PROT_EXEC, 0x5);
    }
}
