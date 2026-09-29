#![forbid(unsafe_code)]

mod library_worker_proxy;

pub use library_worker_proxy::{
    verify_library_worker_proxy, ContainerWorkerProxyError, VerifiedLibraryWorkerProxy,
};
