# Project-local SDK command activation

RBE SDK installs are intentionally project-local. The SDK backend and RPX binaries live under the project that owns them:

```text
<project>/.rbe/bin/backend[.exe]
<project>/.rbe/bin/rpx[.exe]
```

RBE does **not** add these binaries to the user PATH or machine PATH. A package project must never silently make its SDK toolchain global to unrelated projects.

The project-local SDK backend owns activation-file generation. Any successful SDK install writes the activation file for the current platform, regardless of whether the SDK bundle was reached through the Kastrick bootstrap or another verified SDK-bundle path.

Activation prepends `<project>/.rbe/bin` to the **current shell/process PATH only**. It also installs guarded `backend` and `rpx` command functions so normal command names fail closed once the shell moves outside the owning project tree.

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

The activation changes only the current PowerShell process. It removes a previously active RBE project bin from that process PATH before prepending this project's `.rbe\bin`, then installs guarded `backend` and `rpx` functions. If the current directory is moved outside that project, the guarded commands refuse execution instead of silently using the previous project's binaries.

The activation does not write User PATH, Machine PATH, the Windows registry, or the PowerShell profile. A new terminal must activate the project again.

Deactivate explicitly with:

```powershell
Deactivate-RbeProject
```

Deactivation removes the active `.rbe\bin` entry from the current process PATH and removes the project command functions.

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

The activation removes any previously active RBE project bin from the current shell PATH, prepends the new project's `.rbe/bin`, exports the project identity for that shell, and defines guarded `backend` / `rpx` functions. The guard verifies that the current working directory remains inside the owning project tree.

Deactivate with:

```sh
rbe_deactivate
```

Deactivation removes the active project bin from the current shell PATH and removes the activation functions/variables.

## Why activation is required

A child executable cannot modify the environment of its parent shell. Therefore this command:

```powershell
.\backend.exe install sdk.latest -path=. -language=typescript
```

can install `.rbe/bin/backend.exe` and `.rbe/bin/rpx.exe`, but it cannot permanently or globally register project commands in the parent environment without an explicit shell activation step.

RBE deliberately chooses a project activation file instead of User/Machine PATH mutation because it preserves project isolation.

The process-local PATH entry is a developer-experience convenience, not package authority. Library/package authority still comes from verified artifacts, project locks, explicit capability approval, and Library Host session grants.

## Official bootstrap behavior

The official Kastrick SDK bootstrap downloads and verifies the SDK release, then invokes the SDK backend. New SDK bundles write the canonical activation file as part of SDK installation. The bootstrap preserves that file; for older SDK releases it may generate a compatibility activation with the same process-local PATH and project-scope rules.

When the PowerShell bootstrap is invoked directly in the current shell, it activates the newly created project commands for convenience. When the bootstrap is launched as a child by production `backend.exe`, activate `.rbe/activate.ps1` in the parent PowerShell session afterward.

This behavior applies equally to `backend` and `rpx`; neither tool becomes machine-global merely because an SDK was installed into one package project.
