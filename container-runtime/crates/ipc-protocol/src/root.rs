//! Shared IPC contracts for the standalone Container boundary.
//!
//! `control_plane` preserves the existing Container protocol while
//! `library_worker_proxy` adds the strict Backend -> Container handoff used by
//! interpreted RBE package workers. `container_task_image` is the shared CTI
//! compiler/runtime binary contract; it does not grant Container authority.

#[path = "lib.rs"]
mod control_plane;
pub mod container_task_event;
pub mod container_task_image;
mod library_worker_proxy;
mod library_worker_proxy_frame;
mod library_worker_proxy_result;
mod library_worker_proxy_result_frame;
mod library_worker_proxy_status;

pub use container_task_event::*;
pub use container_task_image::*;
pub use control_plane::*;
pub use library_worker_proxy::*;
pub use library_worker_proxy_frame::*;
pub use library_worker_proxy_result::*;
pub use library_worker_proxy_result_frame::*;
pub use library_worker_proxy_status::*;
