# Script sandbox contract

REL `script` is a host capability, not a shell escape.

Required behavior:

- infer normal runtimes from `.js`, `.ts`, and `.py`;
- expose PyPy and Rust only through explicit helpers/options;
- resolve only verified `rbe.sys.*` runtimes from the managed cache;
- never accept a raw executable path from REL;
- never fall back to PATH;
- execute with no shell;
- clear ambient environment and inject only approved runtime environment values;
- deny direct networking unless a separately granted Container network capability exists;
- bound stdout/stderr, wall-clock time, memory and process count;
- re-verify source/runtime identities before execution;
- resolve `$$/` and `??/` through trusted root authority;
- fail closed when the platform has no equivalent hardened Container executor.

The existing hardened Library Worker/one-shot Container path is the intended implementation base.
