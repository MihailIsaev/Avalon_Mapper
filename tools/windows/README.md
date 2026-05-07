# Windows build

## Development

Run:

```cmd
tools\windows\run-dev.cmd
```

This installs the required development tools with `winget`, prepares OCR dependencies, publishes the overlay helper, and starts Tauri dev mode.

## Installer

Run on Windows:

```cmd
tools\windows\build-installer.cmd
```

The script builds:

- `AvalonOverlayHelper.exe` for the native overlay and capture UI.
- `AvalonOcrHelper.exe` as a bundled OCR worker.
- Tauri NSIS installer.

The final installer is copied to:

```text
dist\windows\AvalonMapperSetup.exe
```

Send that single file to users. The installer uses current-user install mode, installs WebView2 silently if needed, creates Start Menu shortcuts, and launches Avalon Mapper after installation.
