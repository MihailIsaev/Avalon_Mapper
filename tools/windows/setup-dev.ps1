param(
    [ValidateSet("run", "setup")]
    [string]$Mode = "run"
)

$ErrorActionPreference = "Stop"
if (Get-Variable -Name PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue) {
    $PSNativeCommandUseErrorActionPreference = $false
}

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

function Test-PythonImport($PythonExe, $ModuleName) {
    $stdoutPath = [System.IO.Path]::GetTempFileName()
    $stderrPath = [System.IO.Path]::GetTempFileName()
    $script:LastPythonImportError = ""
    try {
        $process = Start-Process `
            -FilePath $PythonExe `
            -ArgumentList "-c `"import $ModuleName`"" `
            -NoNewWindow `
            -Wait `
            -PassThru `
            -RedirectStandardOutput $stdoutPath `
            -RedirectStandardError $stderrPath
        $stdout = Get-Content $stdoutPath -Raw -ErrorAction SilentlyContinue
        $stderr = Get-Content $stderrPath -Raw -ErrorAction SilentlyContinue
        $script:LastPythonImportError = (($stdout, $stderr) -join "`n").Trim()
        return $process.ExitCode -eq 0
    } finally {
        Remove-Item $stdoutPath -Force -ErrorAction SilentlyContinue
        Remove-Item $stderrPath -Force -ErrorAction SilentlyContinue
    }
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

function Ensure-PythonOcr {
    Add-PathForProcess "$env:LOCALAPPDATA\Programs\Python\Python311"
    Add-PathForProcess "$env:LOCALAPPDATA\Programs\Python\Python311\Scripts"
    if (-not (Has-Command "python") -and -not (Has-Command "py")) {
        Install-WingetPackage "Python.Python.3.11" "Python 3.11"
        Add-PathForProcess "$env:LOCALAPPDATA\Programs\Python\Python311"
        Add-PathForProcess "$env:LOCALAPPDATA\Programs\Python\Python311\Scripts"
    }

    $venvPython = Join-Path $repoRoot ".venv\Scripts\python.exe"
    if (-not (Test-Path $venvPython)) {
        Write-Step "Creating Python OCR virtualenv"
        if (Has-Command "py") {
            & py -3.11 -m venv ".venv"
        } else {
            & python -m venv ".venv"
        }
        if ($LASTEXITCODE -ne 0) {
            throw "Could not create Python virtualenv for OCR"
        }
    }

    if (-not (Test-PythonImport $venvPython "paddle")) {
        Write-Step "Installing PaddlePaddle CPU runtime"
        & $venvPython -m pip install --upgrade pip
        if ($LASTEXITCODE -ne 0) {
            throw "Could not upgrade pip in OCR virtualenv"
        }
        & $venvPython -m pip install paddlepaddle==3.2.0 -i https://www.paddlepaddle.org.cn/packages/stable/cpu/
        if ($LASTEXITCODE -ne 0) {
            throw "Could not install paddlepaddle in OCR virtualenv"
        }
        if (-not (Test-PythonImport $venvPython "paddle")) {
            if (-not [string]::IsNullOrWhiteSpace($script:LastPythonImportError)) {
                Write-Host $script:LastPythonImportError -ForegroundColor Red
            }
            throw "paddlepaddle was installed but cannot be imported from $venvPython"
        }
    } else {
        Write-Host "PaddlePaddle runtime already installed"
    }

    if (-not (Test-PythonImport $venvPython "paddleocr")) {
        Write-Step "Installing Python OCR dependencies"
        & $venvPython -m pip install paddleocr
        if ($LASTEXITCODE -ne 0) {
            throw "Could not install paddleocr in OCR virtualenv"
        }
        if (-not (Test-PythonImport $venvPython "paddleocr")) {
            if (-not [string]::IsNullOrWhiteSpace($script:LastPythonImportError)) {
                Write-Host $script:LastPythonImportError -ForegroundColor Red
            }
            throw "paddleocr was installed but cannot be imported from $venvPython"
        }
    } else {
        Write-Host "Python OCR dependencies already installed"
    }
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
    $tempCmd = Join-Path $env:TEMP ("avalon-mapper-dev-" + [Guid]::NewGuid().ToString("N") + ".cmd")
    $content = @(
        "@echo off",
        "setlocal",
        "call `"$DevCmd`" -arch=$Arch -host_arch=$Arch",
        "if errorlevel 1 exit /b %errorlevel%",
        $Command,
        "exit /b %errorlevel%"
    )

    try {
        Set-Content -Path $tempCmd -Value $content -Encoding ASCII
        & cmd.exe /d /s /c "`"$tempCmd`""
        $exitCode = $LASTEXITCODE
    }
    finally {
        Remove-Item $tempCmd -Force -ErrorAction SilentlyContinue
    }

    if ($exitCode -ne 0) {
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

function Stop-StaleOverlayHelper {
    $helpers = Get-Process -Name "AvalonOverlayHelper" -ErrorAction SilentlyContinue
    foreach ($helper in $helpers) {
        Write-Host "Stopping stale AvalonOverlayHelper.exe: PID $($helper.Id)"
        Stop-Process -Id $helper.Id -Force -ErrorAction SilentlyContinue
    }
    foreach ($helper in $helpers) {
        try {
            Wait-Process -Id $helper.Id -Timeout 5 -ErrorAction SilentlyContinue
        } catch {
        }
    }
}

function Stop-StalePaddleOcrWorker {
    $query = "CommandLine LIKE '%paddle_ocr_helper.py%'"
    $workers = Get-CimInstance Win32_Process -Filter $query -ErrorAction SilentlyContinue
    foreach ($worker in $workers) {
        if ($worker.ProcessId -and $worker.ProcessId -ne $PID) {
            Write-Host "Stopping stale PaddleOCR worker: PID $($worker.ProcessId)"
            Stop-Process -Id $worker.ProcessId -Force -ErrorAction SilentlyContinue
        }
    }
}

function Wait-FileWritable($PathValue, $TimeoutSeconds = 10) {
    if (-not (Test-Path $PathValue)) {
        return
    }

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        try {
            $stream = [System.IO.File]::Open($PathValue, [System.IO.FileMode]::Open, [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::None)
            $stream.Dispose()
            return
        } catch {
            Start-Sleep -Milliseconds 250
        }
    }

    throw "File is still locked and cannot be overwritten: $PathValue"
}

function Ensure-TauriWindowsIcon {
    $icoPath = Join-Path $repoRoot "src-tauri\icons\icon.ico"
    $pngPath = Join-Path $repoRoot "src-tauri\icons\icon.png"
    if (Test-Path $icoPath) {
        return
    }
    if (-not (Test-Path $pngPath)) {
        throw "Missing Tauri icon sources: $icoPath and $pngPath"
    }

    Write-Step "Generating Windows icon"
    Add-Type -AssemblyName System.Drawing
    $source = [System.Drawing.Bitmap]::FromFile($pngPath)
    try {
        $size = 256
        $bitmap = New-Object System.Drawing.Bitmap $size, $size
        $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
        try {
            $graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
            $graphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::HighQuality
            $graphics.Clear([System.Drawing.Color]::Transparent)
            $graphics.DrawImage($source, 0, 0, $size, $size)
        }
        finally {
            $graphics.Dispose()
        }

        $pngStream = New-Object System.IO.MemoryStream
        try {
            $bitmap.Save($pngStream, [System.Drawing.Imaging.ImageFormat]::Png)
            $pngBytes = $pngStream.ToArray()
        }
        finally {
            $pngStream.Dispose()
            $bitmap.Dispose()
        }

        $icoStream = [System.IO.File]::Create($icoPath)
        $writer = New-Object System.IO.BinaryWriter $icoStream
        try {
            $writer.Write([UInt16]0)
            $writer.Write([UInt16]1)
            $writer.Write([UInt16]1)
            $writer.Write([Byte]0)
            $writer.Write([Byte]0)
            $writer.Write([Byte]0)
            $writer.Write([Byte]0)
            $writer.Write([UInt16]1)
            $writer.Write([UInt16]32)
            $writer.Write([UInt32]$pngBytes.Length)
            $writer.Write([UInt32]22)
            $writer.Write($pngBytes)
        }
        finally {
            $writer.Dispose()
            $icoStream.Dispose()
        }
    }
    finally {
        $source.Dispose()
    }
}

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..\..")
Set-Location $repoRoot
$hostArch = Get-HostArch
$dotnetRuntime = Get-DotnetRuntime $hostArch

Write-Step "Preparing Avalon Mapper Windows dev environment ($hostArch)"
Stop-StalePaddleOcrWorker
Ensure-Node
Ensure-Dotnet
Ensure-PythonOcr
Ensure-Rust $hostArch
$devCmd = Ensure-BuildTools
Ensure-TauriWindowsIcon

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
Stop-StaleOverlayHelper
$publishedHelper = Join-Path $repoRoot "native\windows\AvalonOverlayHelper\bin\Release\net8.0-windows\$dotnetRuntime\publish\AvalonOverlayHelper.exe"
Wait-FileWritable $publishedHelper 10
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
Stop-StaleOverlayHelper
try {
    Invoke-InVs $devCmd $hostArch "cd /d `"$repoRoot`" && npm.cmd run dev"
}
finally {
    Stop-StaleOverlayHelper
    Stop-StalePaddleOcrWorker
}
