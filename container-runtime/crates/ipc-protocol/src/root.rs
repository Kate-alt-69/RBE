//! Shared IPC contracts for the standalone Container boundary.
//!
//! `control_plane` preserves the existing Container protocol while
//! `library_worker_proxy` adds the strict Backend -> Container handoff used by
//! interpreted RBE package workers.

#[path = "lib.rs"]
mod control_plane;
mod library_worker_proxy;
mod library_worker_proxy_frame;
mod library_worker_proxy_result;
mod library_worker_proxy_result_frame;
mod library_worker_proxy_status;

pub use control_plane::*;
pub use library_worker_proxy::*;
pub use library_worker_proxy_frame::*;
pub use library_worker_proxy_result::*;
pub use library_worker_proxy_result_frame::*;
pub use library_worker_proxy_status::*;
