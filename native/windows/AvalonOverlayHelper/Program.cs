using System.Diagnostics;
using System.Drawing.Drawing2D;
using System.Runtime.InteropServices;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace AvalonOverlayHelper;

internal static class Program
{
    internal static readonly JsonSerializerOptions JsonOptions = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
        PropertyNameCaseInsensitive = true
    };

    [STAThread]
    private static int Main(string[] args)
    {
        try
        {
            Application.SetHighDpiMode(HighDpiMode.PerMonitorV2);
            Application.EnableVisualStyles();
            Application.SetCompatibleTextRenderingDefault(false);

            var mode = Args.Value(args, "--mode") ?? "region";
            switch (mode)
            {
                case "map-overlay":
                    var overlay = new MapOverlayForm(
                        Args.Value(args, "--bounds-state"),
                        !Args.HasFlag(args, "--disable-hotkeys"));
                    _ = overlay.Handle;
                    Application.Run();
                    return 0;
                case "region":
                case "portal-size":
                case "diagnostic":
                    Application.Run(new SelectionForm(mode));
                    return 0;
                case "capture-ocr":
                    CaptureOcr.Run(args);
                    return 0;
                default:
                    Console.Error.WriteLine($"Unknown mode: {mode}");
                    return 2;
            }
        }
        catch (Exception ex)
        {
            Console.Error.WriteLine(ex);
            return 1;
        }
    }

    internal static void WriteJson<T>(T value)
    {
        Console.Out.WriteLine(JsonSerializer.Serialize(value, JsonOptions));
        Console.Out.Flush();
    }
}

internal static class Args
{
    internal static string? Value(string[] args, string name)
    {
        for (var i = 0; i < args.Length - 1; i++)
        {
            if (string.Equals(args[i], name, StringComparison.OrdinalIgnoreCase))
            {
                return args[i + 1];
            }
        }

        return null;
    }

    internal static int IntValue(string[] args, string name, int fallback)
    {
        return int.TryParse(Value(args, name), out var value) ? value : fallback;
    }

    internal static double? DoubleValue(string[] args, string name)
    {
        return double.TryParse(Value(args, name), out var value) ? value : null;
    }

    internal static bool HasFlag(string[] args, string name)
    {
        return args.Any(arg => string.Equals(arg, name, StringComparison.OrdinalIgnoreCase));
    }
}

internal sealed class MapOverlayForm : Form
{
    private const int MinOverlayWidth = 374;
    private const int MinOverlayHeight = 220;
    private const int MaxOverlayWidth = 800;
    private const int MaxOverlayHeight = 700;
    private const int WmHotkey = 0x0312;

    private readonly string? _boundsStatePath;
    private readonly bool _hotkeysEnabled;
    private readonly object _dataLock = new();
    private MapOverlayData _data = MapOverlayData.Empty;
    private bool _interactive;
    private bool _visible;
    private int? _selectedLocationId;
    private readonly List<(int Id, RectangleF Rect)> _nodeRects = [];
    private Point _dragStartCursor;
    private Rectangle _dragStartBounds;
    private bool _draggingHeader;
    private bool _resizing;
    private bool _boundsDirty;
    private bool _panningGraph;
    private Point _panStartPoint;
    private PointF _panStartOffset;
    private PointF _mapPan;
    private float _mapZoom = 1f;
    private readonly System.Windows.Forms.Timer _topmostTimer = new();
    private string _routeFromText = "";
    private string _routeToText = "";
    private RouteField? _activeRouteField;

    public MapOverlayForm(string? boundsStatePath, bool hotkeysEnabled)
    {
        _boundsStatePath = boundsStatePath;
        _hotkeysEnabled = hotkeysEnabled;

        FormBorderStyle = FormBorderStyle.None;
        ShowInTaskbar = false;
        TopMost = true;
        StartPosition = FormStartPosition.Manual;
        DoubleBuffered = true;
        ResizeRedraw = true;
        BackColor = Color.Black;
        Opacity = 0.78;
        KeyPreview = true;
        Bounds = BoundsFromTopLeft(LoadBounds() ?? new OverlayBounds(80, 120, 360, 300));
        Hide();
        _visible = false;
        _topmostTimer.Interval = 1000;
        _topmostTimer.Tick += (_, _) => KeepTopmost();
        _topmostTimer.Start();

        SetStyle(ControlStyles.AllPaintingInWmPaint | ControlStyles.OptimizedDoubleBuffer | ControlStyles.UserPaint, true);
        UpdateWindowRegion();
    }

    protected override CreateParams CreateParams
    {
        get
        {
            var cp = base.CreateParams;
            cp.ExStyle |= NativeMethods.WS_EX_TOOLWINDOW | NativeMethods.WS_EX_TOPMOST | NativeMethods.WS_EX_NOACTIVATE | NativeMethods.WS_EX_LAYERED;
            if (!_interactive)
            {
                cp.ExStyle |= NativeMethods.WS_EX_TRANSPARENT;
            }

            return cp;
        }
    }

    protected override bool ShowWithoutActivation => !_interactive;

    protected override void OnHandleCreated(EventArgs e)
    {
        base.OnHandleCreated(e);
        ApplyClickThrough();
        if (_hotkeysEnabled)
        {
            RegisterHotkeys();
        }
        StartCommandReader();
    }

    protected override void OnFormClosed(FormClosedEventArgs e)
    {
        _topmostTimer.Stop();
        _topmostTimer.Dispose();
        NativeMethods.UnregisterHotKey(Handle, 1);
        NativeMethods.UnregisterHotKey(Handle, 2);
        NativeMethods.UnregisterHotKey(Handle, 3);
        base.OnFormClosed(e);
    }

    protected override void OnResize(EventArgs e)
    {
        base.OnResize(e);
        UpdateWindowRegion();
        ForceRedraw();
    }

    protected override void WndProc(ref Message m)
    {
        if (m.Msg == WmHotkey)
        {
            Console.Error.WriteLine($"Hotkey pressed id={m.WParam.ToInt32()}");
            switch (m.WParam.ToInt32())
            {
                case 1:
                    ToggleOverlay();
                    break;
                case 2:
                    Program.WriteJson(new { @event = "capture_current" });
                    break;
                case 3:
                    Program.WriteJson(new { @event = "capture_portal" });
                    break;
            }
        }

        base.WndProc(ref m);
    }

    private void StartCommandReader()
    {
        Task.Run(() =>
        {
            string? line;
            while ((line = Console.In.ReadLine()) != null)
            {
                try
                {
                    using var doc = JsonDocument.Parse(line);
                    var command = doc.RootElement.Clone();
                    if (!IsDisposed)
                    {
                        BeginInvoke((Action)(() => HandleCommand(command)));
                    }
                }
                catch (Exception ex)
                {
                    Console.Error.WriteLine($"Ignoring malformed overlay command: {ex.Message}");
                }
            }
        });
    }

    private void HandleCommand(JsonElement command)
    {
        if (!command.TryGetProperty("type", out var typeElement))
        {
            return;
        }

        var type = typeElement.GetString();
        try
        {
            switch (type)
            {
                case "show":
                    ShowOverlay();
                    break;
                case "hide":
                    HideOverlay();
                    break;
                case "toggle":
                    ToggleOverlay();
                    break;
                case "interactive":
                    SetInteractive(command.TryGetProperty("enabled", out var enabled) && enabled.GetBoolean());
                    break;
                case "bounds":
                    if (command.TryGetProperty("bounds", out var boundsElement))
                    {
                        var bounds = boundsElement.Deserialize<OverlayBounds>(Program.JsonOptions);
                        if (bounds is not null)
                        {
                            SetOverlayBounds(bounds);
                        }
                    }
                    break;
                case "data":
                    if (command.TryGetProperty("data", out var dataElement))
                    {
                        lock (_dataLock)
                        {
                            _data = dataElement.Deserialize<MapOverlayData>(Program.JsonOptions) ?? MapOverlayData.Empty;
                        }

                        Invalidate();
                    }
                    break;
                case "hotkeys":
                    if (_hotkeysEnabled)
                    {
                        RegisterHotkeys(command);
                    }
                    break;
                case "capture_current":
                    Program.WriteJson(new { @event = "capture_current" });
                    break;
                case "capture_portal":
                    Program.WriteJson(new { @event = "capture_portal" });
                    break;
                case "reset":
                    SetOverlayBounds(new OverlayBounds(80, 120, 360, 300));
                    break;
            }
        }
        catch (Exception ex)
        {
            Console.Error.WriteLine($"Overlay command failed ({type}): {ex.Message}");
        }
    }

    private void ShowOverlay()
    {
        _visible = true;
        Show();
        NativeMethods.SetWindowPos(Handle, NativeMethods.HWND_TOPMOST, Left, Top, Width, Height, NativeMethods.SWP_NOACTIVATE | NativeMethods.SWP_SHOWWINDOW);
        Program.WriteJson(new { @event = "visible", visible = true });
    }

    private void HideOverlay()
    {
        _visible = false;
        Hide();
        Program.WriteJson(new { @event = "visible", visible = false });
    }

    private void KeepTopmost()
    {
        if (_visible && IsHandleCreated)
        {
            NativeMethods.SetWindowPos(Handle, NativeMethods.HWND_TOPMOST, Left, Top, Width, Height, NativeMethods.SWP_NOACTIVATE | NativeMethods.SWP_SHOWWINDOW);
        }
    }

    private void ToggleOverlay()
    {
        if (_visible)
        {
            HideOverlay();
        }
        else
        {
            ShowOverlay();
        }
    }

    private void SetInteractive(bool enabled)
    {
        _interactive = enabled;
        Opacity = enabled ? 0.86 : 0.78;
        ApplyClickThrough();
        Invalidate();
        if (enabled && _visible)
        {
            NativeMethods.SetWindowPos(Handle, NativeMethods.HWND_TOPMOST, Left, Top, Width, Height, NativeMethods.SWP_NOACTIVATE | NativeMethods.SWP_SHOWWINDOW);
            Activate();
            Focus();
        }

        Program.WriteJson(new { @event = "interactive", enabled });
    }

    private void ApplyClickThrough()
    {
        if (!IsHandleCreated)
        {
            return;
        }

        var style = NativeMethods.GetWindowLongPtr(Handle, NativeMethods.GWL_EXSTYLE).ToInt64();
        style |= NativeMethods.WS_EX_LAYERED | NativeMethods.WS_EX_TOPMOST | NativeMethods.WS_EX_TOOLWINDOW;
        if (_interactive)
        {
            style &= ~NativeMethods.WS_EX_TRANSPARENT;
            style &= ~NativeMethods.WS_EX_NOACTIVATE;
        }
        else
        {
            style |= NativeMethods.WS_EX_TRANSPARENT;
            style |= NativeMethods.WS_EX_NOACTIVATE;
        }

        NativeMethods.SetWindowLongPtr(Handle, NativeMethods.GWL_EXSTYLE, new IntPtr(style));
    }

    private void SetOverlayBounds(OverlayBounds raw)
    {
        var bounds = Sanitize(raw);
        Bounds = BoundsFromTopLeft(bounds);
        PersistBounds();
        Invalidate();
    }

    private static OverlayBounds Sanitize(OverlayBounds bounds)
    {
        return bounds with
        {
            Width = Math.Clamp(bounds.Width, MinOverlayWidth, MaxOverlayWidth),
            Height = Math.Clamp(bounds.Height, MinOverlayHeight, MaxOverlayHeight)
        };
    }

    private static Rectangle BoundsFromTopLeft(OverlayBounds bounds)
    {
        var screen = Screen.PrimaryScreen?.Bounds ?? new Rectangle(0, 0, 1440, 900);
        return new Rectangle(screen.Left + bounds.X, screen.Top + bounds.Y, bounds.Width, bounds.Height);
    }

    private OverlayBounds TopLeftBounds()
    {
        var screen = Screen.PrimaryScreen?.Bounds ?? new Rectangle(0, 0, 1440, 900);
        return new OverlayBounds(Left - screen.Left, Top - screen.Top, Width, Height);
    }

    private OverlayBounds? LoadBounds()
    {
        if (string.IsNullOrWhiteSpace(_boundsStatePath) || !File.Exists(_boundsStatePath))
        {
            return null;
        }

        try
        {
            return JsonSerializer.Deserialize<OverlayBounds>(File.ReadAllText(_boundsStatePath), Program.JsonOptions);
        }
        catch (Exception ex)
        {
            Console.Error.WriteLine($"Could not load overlay bounds: {ex.Message}");
            return null;
        }
    }

    private void PersistBounds()
    {
        var bounds = TopLeftBounds();
        try
        {
            if (!string.IsNullOrWhiteSpace(_boundsStatePath))
            {
                Directory.CreateDirectory(Path.GetDirectoryName(_boundsStatePath)!);
                File.WriteAllText(_boundsStatePath, JsonSerializer.Serialize(bounds, Program.JsonOptions));
            }
        }
        catch (Exception ex)
        {
            Console.Error.WriteLine($"Could not persist overlay bounds: {ex.Message}");
        }

        Program.WriteJson(new { @event = "bounds", bounds });
    }

    private void RegisterHotkeys(JsonElement? command = null)
    {
        NativeMethods.UnregisterHotKey(Handle, 1);
        NativeMethods.UnregisterHotKey(Handle, 2);
        NativeMethods.UnregisterHotKey(Handle, 3);

        var toggle = HotkeyBinding.DefaultToggle;
        var current = HotkeyBinding.DefaultCurrent;
        var portal = HotkeyBinding.DefaultPortal;

        if (command is { } root)
        {
            Console.Error.WriteLine($"RegisterHotkeys command={root.GetRawText()}");
            if (root.TryGetProperty("toggle_overlay", out var toggleElement))
            {
                toggle = HotkeyBinding.FromJson(toggleElement, toggle);
            }
            if (root.TryGetProperty("capture_current", out var currentElement))
            {
                current = HotkeyBinding.FromJson(currentElement, current);
            }
            if (root.TryGetProperty("capture_portal", out var portalElement))
            {
                portal = HotkeyBinding.FromJson(portalElement, portal);
            }
        }

        RegisterHotkey(1, toggle);
        RegisterHotkey(2, current);
        RegisterHotkey(3, portal);
    }

    private void RegisterHotkey(int id, HotkeyBinding binding)
    {
        var key = binding.ToWindowsKey();
        var modifiers = binding.ToWindowsModifiers() | NativeMethods.MOD_NOREPEAT;
        if (key == Keys.None)
        {
            Console.Error.WriteLine($"Skipping unsupported hotkey id={id} key_code={binding.KeyCode}");
            return;
        }

        if (!NativeMethods.RegisterHotKey(Handle, id, modifiers, (uint)key))
        {
            var error = Marshal.GetLastWin32Error();
            Program.WriteJson(new { @event = "hotkey_error", id, status = error });
            Console.Error.WriteLine($"RegisterHotKey failed id={id} key={key} modifiers={modifiers} error={error}");
        }
        else
        {
            Console.Error.WriteLine($"RegisterHotKey ok id={id} key={key} modifiers={modifiers} label={binding.Label}");
        }
    }

    protected override void OnPaint(PaintEventArgs e)
    {
        e.Graphics.Clear(Color.Black);
        e.Graphics.SmoothingMode = SmoothingMode.AntiAlias;

        MapOverlayData data;
        lock (_dataLock)
        {
            data = _data;
        }

        using var panelBrush = new SolidBrush(Color.FromArgb(15, 15, 15));
        using var borderPen = new Pen(Color.FromArgb(_interactive ? 90 : 45, Color.White), 1f);
        var panel = new RectangleF(0, 0, Math.Max(1, ClientRectangle.Width - 1), Math.Max(1, ClientRectangle.Height - 1));
        e.Graphics.FillRoundedRectangle(panelBrush, panel, 8);
        e.Graphics.DrawRoundedRectangle(borderPen, panel, 8);

        DrawHeader(e.Graphics, data);
        DrawGraph(e.Graphics, data, GraphRect());
        DrawLastPortal(e.Graphics, data);
        DrawControls(e.Graphics, data);
        if (_interactive)
        {
            DrawResizeHandle(e.Graphics);
        }
    }

    private void UpdateWindowRegion()
    {
        if (Width <= 0 || Height <= 0)
        {
            return;
        }

        using var path = GraphicsExtensions.CreateRoundedPath(
            new RectangleF(0, 0, Math.Max(1, Width), Math.Max(1, Height)),
            8
        );
        Region?.Dispose();
        Region = new Region(path);
    }

    private void ForceRedraw()
    {
        if (!IsHandleCreated)
        {
            return;
        }

        NativeMethods.RedrawWindow(
            Handle,
            IntPtr.Zero,
            IntPtr.Zero,
            NativeMethods.RDW_INVALIDATE | NativeMethods.RDW_ERASE | NativeMethods.RDW_UPDATENOW | NativeMethods.RDW_ALLCHILDREN
        );
    }

    private void DrawHeader(Graphics g, MapOverlayData data)
    {
        using var titleFont = new Font("Segoe UI", 10, FontStyle.Bold);
        using var smallFont = new Font("Segoe UI", 8);
        using var textBrush = new SolidBrush(Color.FromArgb(235, 245, 247, 250));
        using var mutedBrush = new SolidBrush(Color.FromArgb(160, 245, 247, 250));
        var title = SelectedLocationName(data) ?? data.CurrentLocation ?? "No location selected";
        g.DrawString(TrimTo(title, 44), titleFont, textBrush, new PointF(14, 10));
        var status = $"{data.LastCaptureStatus ?? "No captures yet"} | {data.KnownLocationsCount ?? data.Locations.Count} nodes | {data.KnownEdgesCount ?? data.Edges.Count} edges";
        g.DrawString(TrimTo(status, 64), smallFont, mutedBrush, new PointF(14, 30));
        if (!_interactive)
        {
            g.DrawString("click-through", smallFont, mutedBrush, new PointF(Math.Max(14, Width - 88), 12));
        }
        else
        {
            DrawButton(g, PassClicksButtonRect(), "Pass clicks", Color.FromArgb(32, 118, 118), smallFont);
        }

        DrawButton(g, UndoButtonRect(), "Undo", Color.FromArgb(205, 126, 32), smallFont);
    }

    private void DrawGraph(Graphics g, MapOverlayData data, RectangleF rect)
    {
        using var graphBrush = new SolidBrush(Color.FromArgb(10, 10, 10));
        using var graphPen = new Pen(Color.FromArgb(35, Color.White), 1f);
        g.FillRoundedRectangle(graphBrush, rect, 6);
        g.DrawRoundedRectangle(graphPen, rect, 6);

        var locations = data.Locations.OrderBy(location => location.Id).ToList();
        _nodeRects.Clear();
        if (locations.Count == 0)
        {
            using var font = new Font("Segoe UI", 9);
            using var brush = new SolidBrush(Color.FromArgb(150, Color.White));
            g.DrawString("No graph data", font, brush, rect.Left + 12, rect.Top + 12);
            return;
        }

        var graphState = g.Save();
        g.SetClip(rect);
        var points = LayoutLocations(data, rect, _mapZoom, _mapPan);
        var routeEdgeIds = data.RouteEdges.Select(e => e.Id).ToHashSet();
        var routePairs = data.RouteEdges.Select(e => PairKey(e.FromLocationId, e.ToLocationId)).ToHashSet();

        foreach (var edge in data.Edges)
        {
            if (!points.TryGetValue(edge.FromLocationId, out var from) || !points.TryGetValue(edge.ToLocationId, out var to))
            {
                continue;
            }

            var highlighted = routeEdgeIds.Contains(edge.Id) || routePairs.Contains(PairKey(edge.FromLocationId, edge.ToLocationId));
            var color = highlighted
                ? Color.FromArgb(242, 249, 115, 22)
                : EdgeColor(edge);
            using var pen = new Pen(color, highlighted ? 3f : edge.Source == "traversed" ? 2.4f : 1.4f);
            g.DrawLine(pen, from, to);
        }

        foreach (var edge in data.BridgeEdges)
        {
            if (!points.TryGetValue(edge.FromLocationId, out var from) || !points.TryGetValue(edge.ToLocationId, out var to))
            {
                continue;
            }

            using var pen = new Pen(Color.FromArgb(72, Color.White), 1.2f);
            g.DrawLine(pen, from, to);
        }

        foreach (var edge in data.RouteEdges)
        {
            if (!points.TryGetValue(edge.FromLocationId, out var from) || !points.TryGetValue(edge.ToLocationId, out var to))
            {
                continue;
            }

            using var pen = new Pen(Color.FromArgb(250, 250, 204, 21), 3f);
            g.DrawLine(pen, from, to);
        }

        using var nameFont = new Font("Segoe UI", 7);
        foreach (var location in locations)
        {
            if (!points.TryGetValue(location.Id, out var point))
            {
                continue;
            }

            var selected = _selectedLocationId == location.Id;
            var current = string.Equals(location.Name, data.CurrentLocation, StringComparison.OrdinalIgnoreCase);
            var radius = selected ? 8f : current ? 6.5f : 5.5f;
            var nodeRect = new RectangleF(point.X - radius, point.Y - radius, radius * 2, radius * 2);
            _nodeRects.Add((location.Id, Inflate(nodeRect, 8f)));

            using var fill = new SolidBrush(NodeColor(location, data.CurrentLocation));
            using var outline = new Pen(Color.FromArgb(current || selected ? 242 : 115, Color.White), selected ? 3f : current ? 2f : 1f);
            g.FillEllipse(fill, nodeRect);
            g.DrawEllipse(outline, nodeRect);

            if (selected || string.Equals(location.Name, data.CurrentLocation, StringComparison.OrdinalIgnoreCase))
            {
                using var text = new SolidBrush(Color.FromArgb(230, Color.White));
                g.DrawString(TrimTo(location.Name, 18), nameFont, text, point.X + 8, point.Y - 8);
            }
        }

        foreach (var location in data.BridgeLocations)
        {
            if (!points.TryGetValue(location.Id, out var point))
            {
                continue;
            }

            var radius = 4.5f;
            var nodeRect = new RectangleF(point.X - radius, point.Y - radius, radius * 2, radius * 2);
            _nodeRects.Add((location.Id, Inflate(nodeRect, 8f)));

            using var fill = new SolidBrush(WithAlpha(NodeColor(location, null), 184));
            using var outline = new Pen(Color.FromArgb(107, Color.White), 1.2f);
            g.FillEllipse(fill, nodeRect);
            g.DrawEllipse(outline, nodeRect);
        }

        foreach (var location in data.RouteLocations)
        {
            if (!points.TryGetValue(location.Id, out var point))
            {
                continue;
            }

            var alreadyExists = data.Locations.Any(existing => existing.Id == location.Id);
            var radius = alreadyExists ? 9f : 7f;
            var nodeRect = new RectangleF(point.X - radius, point.Y - radius, radius * 2, radius * 2);
            _nodeRects.Add((location.Id, Inflate(nodeRect, 8f)));

            if (!alreadyExists)
            {
                using var fill = new SolidBrush(NodeColor(location, null));
                g.FillEllipse(fill, nodeRect);
            }

            using var outline = new Pen(Color.FromArgb(245, 249, 115, 22), alreadyExists ? 3.5f : 2.5f);
            g.DrawEllipse(outline, nodeRect);
        }
        g.Restore(graphState);
    }

    private static Dictionary<int, PointF> LayoutLocations(MapOverlayData data, RectangleF rect, float zoom, PointF pan)
    {
        var locations = data.Locations.OrderBy(location => location.Id).ToList();
        var rawPositions = new Dictionary<int, PointF>();
        for (var i = 0; i < locations.Count; i++)
        {
            var location = locations[i];
            if (location.X.HasValue && location.Y.HasValue)
            {
                rawPositions[location.Id] = new PointF((float)location.X.Value, (float)location.Y.Value);
            }
            else
            {
                var columns = Math.Max(1, (int)Math.Ceiling(Math.Sqrt(Math.Max(1, locations.Count))));
                var col = i % columns;
                var row = i / columns;
                rawPositions[location.Id] = new PointF(col * 140f, row * 140f);
            }
        }

        foreach (var location in data.RouteLocations)
        {
            if (rawPositions.ContainsKey(location.Id) || !location.X.HasValue || !location.Y.HasValue)
            {
                continue;
            }

            rawPositions[location.Id] = new PointF((float)location.X.Value, (float)location.Y.Value + 220f);
        }

        foreach (var location in data.BridgeLocations)
        {
            if (rawPositions.ContainsKey(location.Id) || !location.X.HasValue || !location.Y.HasValue)
            {
                continue;
            }

            rawPositions[location.Id] = new PointF((float)location.X.Value, (float)location.Y.Value);
        }

        if (rawPositions.Count == 0)
        {
            return [];
        }

        var minX = rawPositions.Values.Min(point => point.X);
        var maxX = rawPositions.Values.Max(point => point.X);
        var minY = rawPositions.Values.Min(point => point.Y);
        var maxY = rawPositions.Values.Max(point => point.Y);
        var graphWidth = Math.Max(1f, maxX - minX);
        var graphHeight = Math.Max(1f, maxY - minY);
        const float padding = 18f;
        var baseScale = Math.Min(
            (rect.Width - padding * 2f) / graphWidth,
            (rect.Height - padding * 2f) / graphHeight);
        var scale = baseScale * zoom;
        var contentWidth = graphWidth * scale;
        var contentHeight = graphHeight * scale;
        var offsetX = rect.Left + rect.Width / 2f - contentWidth / 2f;
        var offsetY = rect.Top + rect.Height / 2f - contentHeight / 2f;

        return rawPositions.ToDictionary(
            pair => pair.Key,
            pair => new PointF(
                offsetX + (pair.Value.X - minX) * scale + pan.X,
                offsetY + (pair.Value.Y - minY) * scale + pan.Y));
    }

    private void DrawControls(Graphics g, MapOverlayData data)
    {
        using var tinyFont = new Font("Segoe UI", 7, FontStyle.Regular);
        using var smallFont = new Font("Segoe UI", 8, FontStyle.Bold);
        DrawInputBox(g, RouteFromRect(), string.IsNullOrWhiteSpace(_routeFromText) ? "from=current" : _routeFromText, _activeRouteField == RouteField.From, tinyFont);
        DrawInputBox(g, RouteToRect(), string.IsNullOrWhiteSpace(_routeToText) ? "to" : _routeToText, _activeRouteField == RouteField.To, tinyFont);
        DrawButton(g, FindButtonRect(), "Find", Color.FromArgb(95, 34, 197, 94), smallFont);
        DrawButton(g, DelRoutesButtonRect(), "DelRoutes", Color.FromArgb(100, 239, 68, 68), tinyFont);
        DrawButton(g, CopyButtonRect(), "Copy", Color.FromArgb(95, 59, 130, 246), smallFont);
        using var routeCountBrush = new SolidBrush(Color.FromArgb(165, Color.White));
        g.DrawString($"{data.RouteEdgesCount ?? data.RouteEdges.Count}", smallFont, routeCountBrush, RouteCountRect().Left + 10, RouteCountRect().Top + 4);
    }

    private void DrawLastPortal(Graphics g, MapOverlayData data)
    {
        using var muted = new SolidBrush(Color.FromArgb(165, Color.White));
        using var font = new Font("Segoe UI", 8);
        var portal = data.LastPortalDestination is { Length: > 0 }
            ? $"Last portal: {data.LastPortalDestination} / ttl {FormatDuration(data.LastPortalExpiresInSeconds)}"
            : "Last portal: none";
        g.DrawString(TrimTo(portal, 72), font, muted, LastPortalTextPoint());
    }

    private static void DrawInputBox(Graphics g, RectangleF rect, string text, bool active, Font font)
    {
        using var fill = new SolidBrush(Color.FromArgb(35, 35, 35));
        using var border = new Pen(active ? Color.FromArgb(230, 250, 204, 21) : Color.FromArgb(65, Color.White), active ? 1.5f : 1f);
        using var brush = new SolidBrush(Color.FromArgb(180, Color.White));
        g.FillRoundedRectangle(fill, rect, 5);
        g.DrawRoundedRectangle(border, rect, 5);
        g.SetClip(new RectangleF(rect.Left + 7, rect.Top + 3, rect.Width - 14, rect.Height - 6));
        g.DrawString(text, font, brush, rect.Left + 7, rect.Top + 5);
        g.ResetClip();
    }

    private static void DrawButton(Graphics g, RectangleF rect, string text, Color color, Font font)
    {
        using var fill = new SolidBrush(color);
        using var brush = new SolidBrush(Color.White);
        g.FillRoundedRectangle(fill, rect, 5);
        var size = g.MeasureString(text, font);
        g.DrawString(text, font, brush, rect.Left + (rect.Width - size.Width) / 2, rect.Top + (rect.Height - size.Height) / 2 - 1);
    }

    private void DrawResizeHandle(Graphics g)
    {
        using var pen = new Pen(Color.FromArgb(120, Color.White), 1f);
        var right = Width - 9;
        var bottom = Height - 9;
        g.DrawLine(pen, right - 14, bottom, right, bottom - 14);
        g.DrawLine(pen, right - 8, bottom, right, bottom - 8);
    }

    protected override void OnMouseDown(MouseEventArgs e)
    {
        if (!_interactive)
        {
            return;
        }

        Focus();

        if (Contains(RouteFromRect(), e.Location))
        {
            _activeRouteField = RouteField.From;
            Invalidate();
            return;
        }

        if (Contains(RouteToRect(), e.Location))
        {
            _activeRouteField = RouteField.To;
            Invalidate();
            return;
        }

        if (Contains(FindButtonRect(), e.Location))
        {
            var data = _data;
            var from = _routeFromText.Trim();
            var to = _routeToText.Trim();
            if (string.IsNullOrWhiteSpace(to))
            {
                to = SelectedLocationName(data) ?? "";
            }
            Program.WriteJson(new { @event = "find_route", from, to });
            return;
        }

        if (Contains(DelRoutesButtonRect(), e.Location))
        {
            _activeRouteField = null;
            Program.WriteJson(new { @event = "clear_route" });
            return;
        }

        if (Contains(CopyButtonRect(), e.Location))
        {
            CopyRouteToClipboard();
            return;
        }

        if (Contains(UndoButtonRect(), e.Location))
        {
            Program.WriteJson(new { @event = "undo_last_action" });
            return;
        }

        if (Contains(PassClicksButtonRect(), e.Location))
        {
            SetInteractive(false);
            return;
        }

        if (Contains(GraphRect(), e.Location))
        {
            var graphHit = _nodeRects.LastOrDefault(n => Contains(n.Rect, e.Location));
            if (graphHit.Id != 0)
            {
                _selectedLocationId = graphHit.Id;
            }

            _panningGraph = true;
            _panStartPoint = e.Location;
            _panStartOffset = _mapPan;
            Cursor = Cursors.SizeAll;
            Invalidate();
            return;
        }

        var hit = _nodeRects.LastOrDefault(n => Contains(n.Rect, e.Location));
        if (hit.Id != 0)
        {
            _selectedLocationId = hit.Id;
            Invalidate();
            return;
        }

        _dragStartCursor = Cursor.Position;
        _dragStartBounds = Bounds;
        if (Contains(ResizeHandleRect(), e.Location))
        {
            _resizing = true;
            Cursor = Cursors.SizeNWSE;
        }
        else if (Contains(HeaderRect(), e.Location))
        {
            _draggingHeader = true;
            Cursor = Cursors.SizeAll;
        }
    }

    protected override void OnMouseMove(MouseEventArgs e)
    {
        if (!_interactive)
        {
            return;
        }

        if (!_draggingHeader && !_resizing && !_panningGraph)
        {
            Cursor = Contains(ResizeHandleRect(), e.Location)
                ? Cursors.SizeNWSE
                : Contains(HeaderRect(), e.Location) || Contains(GraphRect(), e.Location) ? Cursors.SizeAll : Cursors.Default;
        }

        if (_panningGraph)
        {
            _mapPan = new PointF(
                _panStartOffset.X + e.Location.X - _panStartPoint.X,
                _panStartOffset.Y + e.Location.Y - _panStartPoint.Y);
            Invalidate();
        }
        else if (_draggingHeader)
        {
            var delta = new Size(Cursor.Position.X - _dragStartCursor.X, Cursor.Position.Y - _dragStartCursor.Y);
            Bounds = new Rectangle(_dragStartBounds.Location + delta, _dragStartBounds.Size);
            _boundsDirty = true;
            ForceRedraw();
        }
        else if (_resizing)
        {
            var delta = new Size(Cursor.Position.X - _dragStartCursor.X, Cursor.Position.Y - _dragStartCursor.Y);
            Bounds = new Rectangle(
                _dragStartBounds.X,
                _dragStartBounds.Y,
                Math.Clamp(_dragStartBounds.Width + delta.Width, MinOverlayWidth, MaxOverlayWidth),
                Math.Clamp(_dragStartBounds.Height + delta.Height, MinOverlayHeight, MaxOverlayHeight));
            _boundsDirty = true;
            ForceRedraw();
        }
    }

    protected override void OnMouseUp(MouseEventArgs e)
    {
        var changed = _boundsDirty;
        _draggingHeader = false;
        _resizing = false;
        _panningGraph = false;
        _boundsDirty = false;
        Cursor = Cursors.Default;
        if (changed)
        {
            PersistBounds();
        }
    }

    protected override void OnMouseWheel(MouseEventArgs e)
    {
        if (!_interactive || !Contains(GraphRect(), e.Location))
        {
            base.OnMouseWheel(e);
            return;
        }

        var oldZoom = _mapZoom;
        var zoomFactor = 1f + Math.Clamp(e.Delta / 120f, -6f, 6f) * 0.12f;
        _mapZoom = Math.Clamp(_mapZoom * zoomFactor, 0.25f, 4f);
        var ratio = _mapZoom / oldZoom;
        var rect = GraphRect();
        var center = new PointF(rect.Left + rect.Width / 2f, rect.Top + rect.Height / 2f);
        _mapPan = new PointF(
            e.Location.X - center.X - (e.Location.X - center.X - _mapPan.X) * ratio,
            e.Location.Y - center.Y - (e.Location.Y - center.Y - _mapPan.Y) * ratio);
        Invalidate();
    }

    protected override void OnKeyDown(KeyEventArgs e)
    {
        if (!_interactive || _activeRouteField is null)
        {
            base.OnKeyDown(e);
            return;
        }

        if (e.KeyCode == Keys.Escape)
        {
            _activeRouteField = null;
            Invalidate();
            return;
        }

        if (e.KeyCode == Keys.Enter)
        {
            var data = _data;
            var to = _routeToText.Trim();
            if (string.IsNullOrWhiteSpace(to))
            {
                to = SelectedLocationName(data) ?? "";
            }
            Program.WriteJson(new { @event = "find_route", from = _routeFromText.Trim(), to });
            _activeRouteField = null;
            Invalidate();
            return;
        }

        if (e.KeyCode == Keys.Back)
        {
            if (_activeRouteField == RouteField.From && _routeFromText.Length > 0)
            {
                _routeFromText = _routeFromText[..^1];
            }
            else if (_activeRouteField == RouteField.To && _routeToText.Length > 0)
            {
                _routeToText = _routeToText[..^1];
            }
            Invalidate();
            return;
        }

        base.OnKeyDown(e);
    }

    protected override void OnKeyPress(KeyPressEventArgs e)
    {
        if (!_interactive || _activeRouteField is null || char.IsControl(e.KeyChar))
        {
            base.OnKeyPress(e);
            return;
        }

        if (_activeRouteField == RouteField.From)
        {
            _routeFromText += e.KeyChar;
        }
        else
        {
            _routeToText += e.KeyChar;
        }
        Invalidate();
        e.Handled = true;
    }

    private RectangleF HeaderRect() => new(0, 0, Width, 44);
    private RectangleF GraphRect() => new(12, 56, Math.Max(20, Width - 24), Math.Max(40, Height - 138));
    private RectangleF RouteFromRect() => new(14, Height - 74, 105, 24);
    private RectangleF RouteToRect() => new(124, Height - 74, 105, 24);
    private RectangleF FindButtonRect() => new(234, Height - 74, 48, 24);
    private RectangleF DelRoutesButtonRect() => new(288, Height - 74, 72, 24);
    private RectangleF CopyButtonRect() => new(234, Height - 24, 48, 22);
    private RectangleF RouteCountRect() => new(288, Height - 24, 72, 22);
    private PointF LastPortalTextPoint() => new(14, Height - 45);
    private RectangleF UndoButtonRect() => new(Math.Max(14, Width - 62), 7, 48, 24);
    private RectangleF PassClicksButtonRect() => new(Math.Max(14, Width - 156), 7, 88, 24);
    private RectangleF ResizeHandleRect() => new(Width - 28, Height - 28, 28, 28);

    private static bool Contains(RectangleF rect, Point point) => rect.Contains(point.X, point.Y);

    private static string FormatDuration(int? seconds)
    {
        if (!seconds.HasValue)
        {
            return "unknown ttl";
        }

        var value = Math.Max(0, seconds.Value);
        var h = value / 3600;
        var m = value % 3600 / 60;
        var s = value % 60;

        if (h > 0)
        {
            return $"{h}h {m}m";
        }

        return m > 0 ? $"{m}m {s}s" : $"{s}s";
    }

    private string? SelectedLocationName(MapOverlayData data)
    {
        if (_selectedLocationId is not { } id)
        {
            return null;
        }

        return data.Locations.Concat(data.RouteLocations).Concat(data.BridgeLocations).FirstOrDefault(l => l.Id == id)?.Name;
    }

    private void CopyRouteToClipboard()
    {
        try
        {
            var data = _data;
            var lines = data.RouteLocations.Select((location, index) => $"{index + 1}){location.Name}");
            Clipboard.SetText(string.Join(Environment.NewLine, lines));
        }
        catch (Exception ex)
        {
            Console.Error.WriteLine($"Could not copy route: {ex.Message}");
        }
    }

    private static string PairKey(int from, int to) => from <= to ? $"{from}:{to}" : $"{to}:{from}";

    private static RectangleF Inflate(RectangleF rect, float amount)
    {
        rect.Inflate(amount, amount);
        return rect;
    }

    private static Color WithAlpha(Color color, int alpha) => Color.FromArgb(alpha, color.R, color.G, color.B);

    private static Color EdgeColor(MapEdge edge)
    {
        if (edge.Source == "traversed")
        {
            return Color.FromArgb(242, 255, 242, 191);
        }

        return Color.FromArgb(166, 20, 184, 166);
    }

    private static Color NodeColor(MapLocation location, string? currentLocation)
    {
        if (string.Equals(location.Name, currentLocation, StringComparison.OrdinalIgnoreCase))
        {
            return Color.FromArgb(34, 197, 94);
        }

        return location.ZoneType switch
        {
            "avalon" => Color.FromArgb(139, 92, 246),
            "blue" or "yellow" => Color.FromArgb(59, 130, 246),
            "red" => Color.FromArgb(239, 68, 68),
            "outlands_black" => Color.FromArgb(25, 31, 42),
            _ => Color.FromArgb(148, 163, 184)
        };
    }

    private static string TrimTo(string value, int max)
    {
        return value.Length <= max ? value : value[..Math.Max(0, max - 1)] + "...";
    }

    private enum RouteField
    {
        From,
        To
    }
}

internal sealed class SelectionForm : Form
{
    private readonly string _mode;
    private Point _start;
    private Point _current;
    private Point _anchor;
    private bool _dragging;

    public SelectionForm(string mode)
    {
        _mode = mode;
        FormBorderStyle = FormBorderStyle.None;
        ShowInTaskbar = false;
        TopMost = true;
        StartPosition = FormStartPosition.Manual;
        Bounds = SystemInformation.VirtualScreen;
        BackColor = Color.Black;
        Opacity = 0.28;
        DoubleBuffered = true;
        Cursor = Cursors.Cross;
        KeyPreview = true;
    }

    protected override CreateParams CreateParams
    {
        get
        {
            var cp = base.CreateParams;
            cp.ExStyle |= NativeMethods.WS_EX_TOOLWINDOW | NativeMethods.WS_EX_TOPMOST;
            return cp;
        }
    }

    protected override void OnShown(EventArgs e)
    {
        base.OnShown(e);
        Activate();
        NativeMethods.SetWindowPos(Handle, NativeMethods.HWND_TOPMOST, Left, Top, Width, Height, NativeMethods.SWP_SHOWWINDOW);
    }

    protected override void OnMouseDown(MouseEventArgs e)
    {
        _dragging = true;
        _start = PointToScreen(e.Location);
        _current = _start;
        _anchor = _start;
        Invalidate();
    }

    protected override void OnMouseMove(MouseEventArgs e)
    {
        if (!_dragging)
        {
            return;
        }

        _current = PointToScreen(e.Location);
        Invalidate();
    }

    protected override void OnMouseUp(MouseEventArgs e)
    {
        _dragging = false;
        _current = PointToScreen(e.Location);
        var rect = Normalized(_start, _current);
        if (rect.Width < 4 || rect.Height < 4)
        {
            Finish(cancelled: true);
            return;
        }

        var screen = Screen.FromPoint(new Point(rect.Left + rect.Width / 2, rect.Top + rect.Height / 2));
        var displayId = (Array.IndexOf(Screen.AllScreens, screen) + 1).ToString();
        var scale = DeviceDpi > 0 ? DeviceDpi / 96.0 : 1.0;
        Program.WriteJson(new OverlaySelectionResult(
            rect.X,
            rect.Y,
            rect.Width,
            rect.Height,
            displayId,
            scale,
            _mode == "portal-size" ? _anchor.X : null,
            _mode == "portal-size" ? _anchor.Y : null,
            false));
        Close();
    }

    protected override void OnKeyDown(KeyEventArgs e)
    {
        if (e.KeyCode == Keys.Escape)
        {
            Finish(cancelled: true);
        }
    }

    protected override void OnPaint(PaintEventArgs e)
    {
        e.Graphics.SmoothingMode = SmoothingMode.AntiAlias;
        using var textBrush = new SolidBrush(Color.White);
        using var font = new Font("Segoe UI", 12, FontStyle.Bold);
        var message = _mode == "portal-size"
            ? "Outline the portal tooltip plaque. Escape to cancel."
            : "Drag to select capture region. Escape to cancel.";
        e.Graphics.DrawString(message, font, textBrush, 20, 20);

        var mouse = PointToClient(Cursor.Position);
        using var crosshair = new Pen(Color.Gold, 1f);
        e.Graphics.DrawLine(crosshair, mouse.X - 12, mouse.Y, mouse.X + 12, mouse.Y);
        e.Graphics.DrawLine(crosshair, mouse.X, mouse.Y - 12, mouse.X, mouse.Y + 12);

        if (!_dragging)
        {
            return;
        }

        var rect = Normalized(PointToClient(_start), PointToClient(_current));
        using var fill = new SolidBrush(Color.FromArgb(55, 20, 184, 166));
        using var pen = new Pen(Color.FromArgb(255, 45, 212, 191), 2f);
        e.Graphics.FillRectangle(fill, rect);
        e.Graphics.DrawRectangle(pen, rect);
        e.Graphics.DrawString($"{rect.Width} x {rect.Height}", font, textBrush, rect.Left + 8, Math.Max(8, rect.Top + 8));
    }

    private void Finish(bool cancelled)
    {
        Program.WriteJson(new OverlaySelectionResult(0, 0, 0, 0, null, null, null, null, cancelled));
        Close();
    }

    private static Rectangle Normalized(Point a, Point b)
    {
        return new Rectangle(Math.Min(a.X, b.X), Math.Min(a.Y, b.Y), Math.Abs(a.X - b.X), Math.Abs(a.Y - b.Y));
    }
}

internal static class CaptureOcr
{
    internal static void Run(string[] args)
    {
        var started = Stopwatch.StartNew();
        var kind = Args.Value(args, "--kind") ?? "capture";
        var outputDir = Args.Value(args, "--output-dir") ?? Path.GetTempPath();
        var width = Math.Max(1, Args.IntValue(args, "--width", 320));
        var height = Math.Max(1, Args.IntValue(args, "--height", 180));

        var rect = CaptureRect(args, width, height);
        Directory.CreateDirectory(outputDir);

        using var bitmap = new Bitmap(Math.Max(1, rect.Width), Math.Max(1, rect.Height));
        using (var graphics = Graphics.FromImage(bitmap))
        {
            graphics.CopyFromScreen(rect.Left, rect.Top, 0, 0, rect.Size, CopyPixelOperation.SourceCopy);
        }

        var imagePath = Path.Combine(outputDir, $"{SanitizeFileName(kind)}-{DateTimeOffset.UtcNow.ToUnixTimeMilliseconds()}.png");
        bitmap.Save(imagePath, System.Drawing.Imaging.ImageFormat.Png);

        Program.WriteJson(new CaptureOcrResult(
            "",
            null,
            "capture_only",
            imagePath,
            bitmap.Width,
            bitmap.Height,
            started.ElapsedMilliseconds,
            true,
            []));
    }

    private static Rectangle CaptureRect(string[] args, int width, int height)
    {
        var portalX = Args.IntValue(args, "--portal-x", int.MinValue);
        var portalY = Args.IntValue(args, "--portal-y", int.MinValue);
        var portalWidth = Args.IntValue(args, "--portal-width", int.MinValue);
        var portalHeight = Args.IntValue(args, "--portal-height", int.MinValue);
        var portalAnchorX = Args.DoubleValue(args, "--portal-anchor-x");
        var portalAnchorY = Args.DoubleValue(args, "--portal-anchor-y");

        if (portalX != int.MinValue && portalY != int.MinValue && portalWidth > 0 && portalHeight > 0 && portalAnchorX.HasValue && portalAnchorY.HasValue)
        {
            var selected = new Rectangle(portalX, portalY, portalWidth, portalHeight);
            var cursor = Cursor.Position;
            var offsetX = selected.Left - (int)Math.Round(portalAnchorX.Value);
            var offsetY = selected.Top - (int)Math.Round(portalAnchorY.Value);
            var sameSide = new Rectangle(cursor.X + offsetX, cursor.Y + offsetY, selected.Width, selected.Height);
            var mirrored = new Rectangle(cursor.X - (selected.Right - (int)Math.Round(portalAnchorX.Value)), cursor.Y + offsetY, selected.Width, selected.Height);
            return Rectangle.Union(sameSide, mirrored);
        }

        if (args.Contains("--center-cursor"))
        {
            var cursor = Cursor.Position;
            return new Rectangle(cursor.X - width / 2, cursor.Y - height / 2, width, height);
        }

        return new Rectangle(
            Args.IntValue(args, "--x", 0),
            Args.IntValue(args, "--y", 0),
            width,
            height);
    }

    private static string SanitizeFileName(string value)
    {
        foreach (var ch in Path.GetInvalidFileNameChars())
        {
            value = value.Replace(ch, '-');
        }

        return string.IsNullOrWhiteSpace(value) ? "capture" : value;
    }
}

internal sealed record OverlayBounds(
    [property: JsonPropertyName("x")] int X,
    [property: JsonPropertyName("y")] int Y,
    [property: JsonPropertyName("width")] int Width,
    [property: JsonPropertyName("height")] int Height);

internal sealed record OverlaySelectionResult(
    int X,
    int Y,
    int Width,
    int Height,
    string? DisplayId,
    double? ScaleFactor,
    int? AnchorX,
    int? AnchorY,
    bool Cancelled);

internal sealed record CaptureOcrResult(
    string Text,
    double? Confidence,
    string Engine,
    string ImagePath,
    int Width,
    int Height,
    long DurationMs,
    bool ScreenRecordingPermission,
    IReadOnlyList<OcrLine> Lines);

internal sealed record OcrLine(string Text, double? Confidence, OcrBbox? Bbox);
internal sealed record OcrBbox(double X, double Y, double Width, double Height);

internal sealed class HotkeyBinding
{
    public HotkeyBinding()
    {
    }

    public HotkeyBinding(int keyCode, uint modifiers, string label)
    {
        KeyCode = keyCode;
        Modifiers = modifiers;
        Label = label;
    }

    [JsonPropertyName("key_code")]
    public int KeyCode { get; set; }

    [JsonPropertyName("modifiers")]
    public uint Modifiers { get; set; }

    [JsonPropertyName("label")]
    public string Label { get; set; } = "";

    internal static HotkeyBinding DefaultToggle => new(46, 2560, "Alt+Shift+M");
    internal static HotkeyBinding DefaultCurrent => new(37, 2560, "Alt+Shift+L");
    internal static HotkeyBinding DefaultPortal => new(35, 2560, "Alt+Shift+P");

    internal static HotkeyBinding FromJson(JsonElement element, HotkeyBinding fallback)
    {
        try
        {
            var parsed = element.Deserialize<HotkeyBinding>(Program.JsonOptions);
            if (parsed is null)
            {
                return fallback;
            }
            if (parsed.KeyCode == 0 && !element.TryGetProperty("key_code", out _))
            {
                return fallback;
            }
            return parsed;
        }
        catch (Exception ex)
        {
            Console.Error.WriteLine($"Could not parse hotkey binding: {ex.Message}; json={element.GetRawText()}");
            return fallback;
        }
    }

    internal Keys ToWindowsKey()
    {
        return KeyCode switch
        {
            0 => Keys.A,
            1 => Keys.S,
            2 => Keys.D,
            3 => Keys.F,
            4 => Keys.H,
            5 => Keys.G,
            6 => Keys.Z,
            7 => Keys.X,
            8 => Keys.C,
            9 => Keys.V,
            11 => Keys.B,
            12 => Keys.Q,
            13 => Keys.W,
            14 => Keys.E,
            15 => Keys.R,
            16 => Keys.Y,
            17 => Keys.T,
            18 => Keys.D1,
            19 => Keys.D2,
            20 => Keys.D3,
            21 => Keys.D4,
            22 => Keys.D6,
            23 => Keys.D5,
            24 => Keys.Oemplus,
            25 => Keys.D9,
            26 => Keys.D7,
            27 => Keys.OemMinus,
            28 => Keys.D8,
            29 => Keys.D0,
            30 => Keys.OemCloseBrackets,
            31 => Keys.O,
            32 => Keys.U,
            33 => Keys.OemOpenBrackets,
            34 => Keys.I,
            35 => Keys.P,
            37 => Keys.L,
            38 => Keys.J,
            39 => Keys.OemQuotes,
            40 => Keys.K,
            41 => Keys.OemSemicolon,
            42 => Keys.OemPipe,
            43 => Keys.Oemcomma,
            44 => Keys.OemQuestion,
            45 => Keys.N,
            46 => Keys.M,
            47 => Keys.OemPeriod,
            49 => Keys.Space,
            50 => Keys.Oemtilde,
            _ when Enum.IsDefined(typeof(Keys), KeyCode) => (Keys)KeyCode,
            _ => Keys.None
        };
    }

    internal uint ToWindowsModifiers()
    {
        uint result = 0;
        if ((Modifiers & 0x0200) != 0 || Label.Contains("Shift", StringComparison.OrdinalIgnoreCase) || Label.Contains('⇧'))
        {
            result |= NativeMethods.MOD_SHIFT;
        }
        if ((Modifiers & 0x0800) != 0 || Label.Contains("Alt", StringComparison.OrdinalIgnoreCase) || Label.Contains('⌥'))
        {
            result |= NativeMethods.MOD_ALT;
        }
        if ((Modifiers & 0x1000) != 0 || Label.Contains("Ctrl", StringComparison.OrdinalIgnoreCase) || Label.Contains('⌃'))
        {
            result |= NativeMethods.MOD_CONTROL;
        }
        if ((Modifiers & 0x0100) != 0 && (Label.Contains("Win", StringComparison.OrdinalIgnoreCase) || Label.Contains('⌘')))
        {
            result |= NativeMethods.MOD_WIN;
        }
        if (Modifiers == 768 && result == NativeMethods.MOD_SHIFT)
        {
            result |= NativeMethods.MOD_ALT;
        }

        return result;
    }
}

internal sealed class MapOverlayData
{
    public string? RouteExpiresAt { get; set; }
    public int? RouteEdgesCount { get; set; }
    public List<MapLocation> BridgeLocations { get; set; } = [];
    public List<RouteOverlayEdge> BridgeEdges { get; set; } = [];
    public string? CurrentLocation { get; set; }
    public string? LastPortalDestination { get; set; }
    public int? LastPortalExpiresInSeconds { get; set; }
    public List<MapLocation> Locations { get; set; } = [];
    public List<MapEdge> Edges { get; set; } = [];
    public List<MapLocation> RouteLocations { get; set; } = [];
    public List<RouteOverlayEdge> RouteEdges { get; set; } = [];
    public string? LastCaptureStatus { get; set; } = "No captures yet";
    public string? CaptureMode { get; set; }
    public string? OcrMode { get; set; }
    public string? DbStatus { get; set; }
    public int? KnownLocationsCount { get; set; }
    public int? KnownEdgesCount { get; set; }

    public static MapOverlayData Empty => new();
}

internal sealed class MapLocation
{
    public int Id { get; set; }
    public string Name { get; set; } = "";
    public string? NormalizedName { get; set; }
    public string? ZoneType { get; set; }
    public double? X { get; set; }
    public double? Y { get; set; }
}

internal sealed class MapEdge
{
    public int Id { get; set; }
    public string? Source { get; set; }
    public int FromLocationId { get; set; }
    public int ToLocationId { get; set; }
    public string FromLocationName { get; set; } = "";
    public string ToLocationName { get; set; } = "";
    public string? LastSeenAt { get; set; }
    public string? Status { get; set; }
}

internal sealed class RouteOverlayEdge
{
    public int Id { get; set; }
    public int FromLocationId { get; set; }
    public int ToLocationId { get; set; }
    public string FromLocationName { get; set; } = "";
    public string ToLocationName { get; set; } = "";
    public string? Source { get; set; }
}

internal static class GraphicsExtensions
{
    internal static void FillRoundedRectangle(this Graphics graphics, Brush brush, RectangleF bounds, float radius)
    {
        using var path = CreateRoundedPath(bounds, radius);
        graphics.FillPath(brush, path);
    }

    internal static void DrawRoundedRectangle(this Graphics graphics, Pen pen, RectangleF bounds, float radius)
    {
        using var path = CreateRoundedPath(bounds, radius);
        graphics.DrawPath(pen, path);
    }

    internal static GraphicsPath CreateRoundedPath(RectangleF bounds, float radius)
    {
        var diameter = radius * 2;
        var path = new GraphicsPath();
        path.AddArc(bounds.Left, bounds.Top, diameter, diameter, 180, 90);
        path.AddArc(bounds.Right - diameter, bounds.Top, diameter, diameter, 270, 90);
        path.AddArc(bounds.Right - diameter, bounds.Bottom - diameter, diameter, diameter, 0, 90);
        path.AddArc(bounds.Left, bounds.Bottom - diameter, diameter, diameter, 90, 90);
        path.CloseFigure();
        return path;
    }
}

internal static class NativeMethods
{
    internal const int GWL_EXSTYLE = -20;
    internal const int WS_EX_TRANSPARENT = 0x00000020;
    internal const int WS_EX_TOOLWINDOW = 0x00000080;
    internal const int WS_EX_TOPMOST = 0x00000008;
    internal const int WS_EX_LAYERED = 0x00080000;
    internal const int WS_EX_NOACTIVATE = 0x08000000;
    internal const uint MOD_ALT = 0x0001;
    internal const uint MOD_CONTROL = 0x0002;
    internal const uint MOD_SHIFT = 0x0004;
    internal const uint MOD_WIN = 0x0008;
    internal const uint MOD_NOREPEAT = 0x4000;
    internal static readonly IntPtr HWND_TOPMOST = new(-1);
    internal const uint SWP_NOACTIVATE = 0x0010;
    internal const uint SWP_SHOWWINDOW = 0x0040;
    internal const uint RDW_INVALIDATE = 0x0001;
    internal const uint RDW_ERASE = 0x0004;
    internal const uint RDW_UPDATENOW = 0x0100;
    internal const uint RDW_ALLCHILDREN = 0x0080;

    [DllImport("user32.dll", SetLastError = true)]
    internal static extern bool RegisterHotKey(IntPtr hWnd, int id, uint fsModifiers, uint vk);

    [DllImport("user32.dll", SetLastError = true)]
    internal static extern bool UnregisterHotKey(IntPtr hWnd, int id);

    [DllImport("user32.dll", SetLastError = true)]
    internal static extern bool SetWindowPos(IntPtr hWnd, IntPtr hWndInsertAfter, int x, int y, int cx, int cy, uint flags);

    [DllImport("user32.dll", SetLastError = true)]
    internal static extern bool RedrawWindow(IntPtr hWnd, IntPtr lprcUpdate, IntPtr hrgnUpdate, uint flags);

    [DllImport("user32.dll", EntryPoint = "GetWindowLongPtrW", SetLastError = true)]
    private static extern IntPtr GetWindowLongPtr64(IntPtr hWnd, int nIndex);

    [DllImport("user32.dll", EntryPoint = "GetWindowLongW", SetLastError = true)]
    private static extern int GetWindowLong32(IntPtr hWnd, int nIndex);

    [DllImport("user32.dll", EntryPoint = "SetWindowLongPtrW", SetLastError = true)]
    private static extern IntPtr SetWindowLongPtr64(IntPtr hWnd, int nIndex, IntPtr dwNewLong);

    [DllImport("user32.dll", EntryPoint = "SetWindowLongW", SetLastError = true)]
    private static extern int SetWindowLong32(IntPtr hWnd, int nIndex, int dwNewLong);

    internal static IntPtr GetWindowLongPtr(IntPtr hWnd, int nIndex)
    {
        return IntPtr.Size == 8 ? GetWindowLongPtr64(hWnd, nIndex) : new IntPtr(GetWindowLong32(hWnd, nIndex));
    }

    internal static IntPtr SetWindowLongPtr(IntPtr hWnd, int nIndex, IntPtr dwNewLong)
    {
        return IntPtr.Size == 8 ? SetWindowLongPtr64(hWnd, nIndex, dwNewLong) : new IntPtr(SetWindowLong32(hWnd, nIndex, dwNewLong.ToInt32()));
    }
}
