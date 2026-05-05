param(
    [ValidateSet("run", "setup")]
    [string]$Mode = "run"
)

$ErrorActionPreference = "Stop"

function Write-Step($Message) {
    Write-Host ""
    Write-Host "==> $Message" -ForegroundColor Cyan
}

function Add-PathForProcess($PathValue) {
    if ([string]::IsNullOrWhiteSpace($PathValue) -or -not (Test-Path $PathValue)) {
        return
    }
    $parts = $env:Path -split ';'
    if ($parts -notcontains $PathValue) {
        $env:Path = "$PathValue;$env:Path"
    }
}

function Has-Command($Name) {
    return $null -ne (Get-Command $Name -ErrorAction SilentlyContinue)
}

function Get-HostArch {
    $arch = $env:PROCESSOR_ARCHITECTURE
    if ([string]::Equals($arch, "ARM64", [StringComparison]::OrdinalIgnoreCase)) {
        return "arm64"
    }
    return "x64"
}

function Get-RustToolchain($Arch) {
    if ($Arch -eq "arm64") {
        return "stable-aarch64-pc-windows-msvc"
    }
    return "stable-x86_64-pc-windows-msvc"
}

function Get-DotnetRuntime($Arch) {
    if ($Arch -eq "arm64") {
        return "win-arm64"
    }
    return "win-x64"
}

function Require-Winget {
    if (-not (Has-Command "winget")) {
        throw "winget is not available. Install 'App Installer' from Microsoft Store, then run tools\windows\run-dev.cmd again."
    }
}

function Install-WingetPackage($Id, $Name, $ExtraArgs = @()) {
    Require-Winget
    Write-Step "Installing $Name"
    $args = @(
        "install",
        "--id", $Id,
        "--exact",
        "--accept-package-agreements",
        "--accept-source-agreements"
    ) + $ExtraArgs
    & winget @args
    if ($LASTEXITCODE -ne 0) {
        throw "winget failed to install $Name"
    }
}

function Ensure-Node {
    Add-PathForProcess "C:\Program Files\nodejs"
    if (Has-Command "node" -and Has-Command "npm.cmd") {
        return
    }
    Install-WingetPackage "OpenJS.NodeJS.LTS" "Node.js LTS"
    Add-PathForProcess "C:\Program Files\nodejs"
}

function Ensure-Dotnet {
    Add-PathForProcess "C:\Program Files\dotnet"
    Add-PathForProcess "C:\Program Files\dotnet\x64"
    if (Has-Command "dotnet") {
        return
    }
    Install-WingetPackage "Microsoft.DotNet.SDK.8" ".NET 8 SDK"
    Add-PathForProcess "C:\Program Files\dotnet"
    Add-PathForProcess "C:\Program Files\dotnet\x64"
}

function Ensure-Rust($Arch) {
    Add-PathForProcess "$env:USERPROFILE\.cargo\bin"
    if (-not (Has-Command "rustup")) {
        Install-WingetPackage "Rustlang.Rustup" "Rustup"
        Add-PathForProcess "$env:USERPROFILE\.cargo\bin"
    }
    if (-not (Has-Command "rustup")) {
        throw "rustup is still not available after install. Close this terminal, open it again, and rerun tools\windows\run-dev.cmd."
    }

    $toolchain = Get-RustToolchain $Arch
    Write-Step "Configuring Rust $Arch MSVC toolchain ($toolchain)"
    & rustup toolchain install $toolchain
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to install Rust $Arch MSVC toolchain"
    }
    & rustup default $toolchain
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to set Rust $Arch MSVC toolchain as default"
    }
}

function Find-VsDevCmd {
    $vswhereCandidates = @(
        "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe",
        "$env:ProgramFiles\Microsoft Visual Studio\Installer\vswhere.exe"
    )
    foreach ($candidate in $vswhereCandidates) {
        if (Test-Path $candidate) {
            $installPath = & $candidate -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
            if ($LASTEXITCODE -eq 0 -and -not [string]::IsNullOrWhiteSpace($installPath)) {
                $devCmd = Join-Path $installPath "Common7\Tools\VsDevCmd.bat"
                if (Test-Path $devCmd) {
                    return $devCmd
                }
            }
        }
    }

    $fallbacks = @(
        "${env:ProgramFiles(x86)}\Microsoft Visual Studio\18\BuildTools\Common7\Tools\VsDevCmd.bat",
        "${env:ProgramFiles(x86)}\Microsoft Visual Studio\2022\BuildTools\Common7\Tools\VsDevCmd.bat",
        "${env:ProgramFiles}\Microsoft Visual Studio\18\BuildTools\Common7\Tools\VsDevCmd.bat",
        "${env:ProgramFiles}\Microsoft Visual Studio\2022\BuildTools\Common7\Tools\VsDevCmd.bat"
    )
    foreach ($fallback in $fallbacks) {
        if (Test-Path $fallback) {
            return $fallback
        }
    }

    return $null
}

function Ensure-BuildTools {
    $devCmd = Find-VsDevCmd
    if ($devCmd) {
        return $devCmd
    }

    Install-WingetPackage `
        "Microsoft.VisualStudio.2022.BuildTools" `
        "Visual Studio Build Tools with C++ workload" `
        @("--override", "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended")

    $devCmd = Find-VsDevCmd
    if (-not $devCmd) {
        throw "Visual Studio Build Tools were installed, but VsDevCmd.bat was not found. Open Visual Studio Installer and add 'Desktop development with C++'."
    }
    return $devCmd
}

function Invoke-InVs($DevCmd, $Arch, $Command) {
    $escapedDevCmd = $DevCmd.Replace('"', '\"')
    $escapedCommand = $Command.Replace('"', '\"')
    & cmd.exe /d /s /c "`"$escapedDevCmd`" -arch=$Arch -host_arch=$Arch && $escapedCommand"
    if ($LASTEXITCODE -ne 0) {
        throw "Command failed: $Command"
    }
}

function Stop-Port1420 {
    $listeners = Get-NetTCPConnection -LocalPort 1420 -State Listen -ErrorAction SilentlyContinue
    foreach ($listener in $listeners) {
        if ($listener.OwningProcess -and $listener.OwningProcess -ne $PID) {
            Write-Host "Stopping process using port 1420: PID $($listener.OwningProcess)"
            Stop-Process -Id $listener.OwningProcess -Force -ErrorAction SilentlyContinue
        }
    }
}

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..\..")
Set-Location $repoRoot
$hostArch = Get-HostArch
$dotnetRuntime = Get-DotnetRuntime $hostArch

Write-Step "Preparing Avalon Mapper Windows dev environment ($hostArch)"
Ensure-Node
Ensure-Dotnet
Ensure-Rust $hostArch
$devCmd = Ensure-BuildTools

Write-Step "Tool versions"
& node --version
& npm.cmd --version
& dotnet --version
& rustc -vV

Write-Step "Installing npm dependencies"
& npm.cmd install
if ($LASTEXITCODE -ne 0) {
    throw "npm install failed"
}

Write-Step "Publishing Windows overlay helper"
Push-Location "native\windows\AvalonOverlayHelper"
try {
    & dotnet publish -c Release -r $dotnetRuntime --self-contained
    if ($LASTEXITCODE -ne 0) {
        throw "dotnet publish failed"
    }
}
finally {
    Pop-Location
}

if ($Mode -eq "setup") {
    Write-Step "Setup complete"
    exit 0
}

Write-Step "Starting Avalon Mapper"
Stop-Port1420
Invoke-InVs $devCmd $hostArch "cd /d `"$repoRoot`" && npm.cmd run dev"
