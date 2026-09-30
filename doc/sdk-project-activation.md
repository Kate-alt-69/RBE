# Project-local SDK command activation

RBE SDK installs are intentionally project-local. The SDK backend and RPX binaries live under the project that owns them:

```text
<project>/.rbe/bin/backend[.exe]
<project>/.rbe/bin/rpx[.exe]
```

RBE does **not** add these binaries to the user PATH or machine PATH. A package project must never silently make its SDK toolchain global to unrelated projects.

## Windows PowerShell

The official SDK bootstrap writes:

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
deactivate-rbe
```

## Linux and macOS

The official SDK bootstrap writes:

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
deactivate_rbe
```

## Why activation is required

A child executable cannot modify the environment of its parent shell. Therefore this command:

```powershell
.\backend.exe install sdk.latest -path=. -language=typescript
```

can install `.rbe/bin/backend.exe` and `.rbe/bin/rpx.exe`, but it cannot permanently inject their directory into the already-running parent PowerShell process without either a global PATH mutation or a shell activation step.

RBE deliberately chooses the activation step because it preserves project isolation.

## Official bootstrap behavior

The official Kastrick SDK bootstrap generates the activation file immediately after the verified SDK archive is installed. When the PowerShell bootstrap itself is invoked directly in the current shell, it also activates the project for that process. When the bootstrap is launched as a child by production `backend.exe`, run `.\.rbe\activate.ps1` afterward in the parent PowerShell session.

This behavior applies equally to `backend` and `rpx`; neither tool is intended to become machine-global merely because an SDK was installed into one package project.
