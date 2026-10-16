// Wrapper keeps the sealed Git execution implementation on the existing
// rbe-install-runtime dependency edge. The implementation itself refers to
// `rbe_install_executor`; alias that name to install-runtime's public re-export
// surface so Backend does not gain a second direct dependency or lockfile edge.
use rbe_install_runtime as rbe_install_executor;

include!("git_exec_impl.rs");
