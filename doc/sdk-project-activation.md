# Project-local SDK command activation

RBE SDK installs are intentionally project-local. The SDK backend and RPX binaries live under the project that owns them:

```text
<project>/.rbe/bin/backend[.exe]
<project>/.rbe/bin/rpx[.exe]
```

RBE does **not** add these binaries to the user PATH or machine PATH. A package project must never silently make its SDK toolchain global to unrelated projects.

The project-local SDK backend owns activation-file generation. Any successful SDK install writes the activation file for the current platform, regardless of whether the SDK bundle was reached through the Kastrick bootstrap or another verified SDK-bundle path.

## Windows PowerShell

The SDK backend writes:

```text
.rbe/activate.ps1
```

Activate the project in the current PowerShell process:

```powershell
& .\.rbe\activate.ps1
```

After activation, normal command names work:

```powershell
backend sdk status -path=.
rpx check .
rpx compile .
```

The activation exposes project-local `backend` and `rpx` commands only for the owning project tree. If the current directory is moved outside that project, those commands fail closed instead of accidentally using that project's binaries elsewhere.

The activation is process-local. It does not write User PATH, Machine PATH, the Windows registry, or the PowerShell profile. A new terminal must activate the project again.

Deactivate explicitly with:

```powershell
Deactivate-RbeProject
```

## Linux and macOS

The SDK backend writes:

```text
.rbe/activate.sh
```

Source it into the current shell:

```sh
. ./.rbe/activate.sh
```

Then the same project-local command names are available:

```sh
backend sdk status -path=.
rpx check .
rpx compile .
```

The shell functions verify that the current working directory remains inside the owning project tree. Moving outside that tree makes the project commands fail closed.

Deactivate with:

```sh
rbe_deactivate
```

## Why activation is required

A child executable cannot modify the environment of its parent shell. Therefore this command:

```powershell
.\backend.exe install sdk.latest -path=. -language=typescript
```

can install `.rbe/bin/backend.exe` and `.rbe/bin/rpx.exe`, but it cannot inject project commands into the already-running parent PowerShell process without either a global environment mutation or a shell activation step.

RBE deliberately chooses the activation step because it preserves project isolation.

The generated activation does **not** permanently append `.rbe/bin` to any PATH. It creates current-shell command wrappers bound to the owning project and those wrappers verify the current working directory before every invocation. This prevents an activated `mail` project from accidentally supplying `backend` or `rpx` to an unrelated project after `cd`.

## Official bootstrap behavior

The official Kastrick SDK bootstrap downloads and verifies the SDK release, then invokes the SDK backend. The SDK backend writes the activation file as part of the SDK installation itself.

When the PowerShell bootstrap is invoked directly in the current shell, the bootstrap may activate the newly created project command wrapper for convenience. When the bootstrap is launched as a child by production `backend.exe`, activate `.rbe/activate.ps1` in the parent PowerShell session afterward.

This behavior applies equally to `backend` and `rpx`; neither tool is intended to become machine-global merely because an SDK was installed into one package project.
