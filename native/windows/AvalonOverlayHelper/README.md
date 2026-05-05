# AvalonOverlayHelper for Windows

Build:

```powershell
dotnet build
dotnet publish -c Release -r win-x64 --self-contained
```

The published executable is:

```text
bin\Release\net8.0-windows\win-x64\publish\AvalonOverlayHelper.exe
```

Run modes:

```powershell
AvalonOverlayHelper.exe --mode map-overlay
AvalonOverlayHelper.exe --mode region
AvalonOverlayHelper.exe --mode portal-size
AvalonOverlayHelper.exe --mode capture-ocr --kind portal --x 10 --y 10 --width 320 --height 180 --output-dir C:\Temp
```

`stdout` is reserved for JSON events/results. Diagnostics are written to `stderr`.

Rust/Tauri integration sketch:

```rust
let helper = if cfg!(target_os = "windows") {
    app.path()
        .resource_dir()
        .map_err(|err| format!("Could not resolve resource directory: {err}"))?
        .join("AvalonOverlayHelper.exe")
} else if cfg!(target_os = "macos") {
    ensure_macos_overlay_helper(app)?
} else {
    return Err("Native overlay helper is not implemented for this platform".to_string());
};

let mut child = Command::new(helper)
    .arg("--mode")
    .arg("map-overlay")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()
    .map_err(|err| format!("Could not launch map overlay helper: {err}"))?;
```

Bundle the published `AvalonOverlayHelper.exe` as a Tauri resource, then use the same line-delimited JSON stdin/stdout handling that the macOS helper uses.
