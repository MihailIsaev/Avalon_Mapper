use chrono::{Duration, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration as StdDuration, Instant},
};
use tauri::{AppHandle, Manager};

struct AppState {
    db: Mutex<Connection>,
    db_path: PathBuf,
    capture_dir: PathBuf,
    map_overlay: Arc<Mutex<Option<MapOverlayProcess>>>,
    paddle_ocr: Arc<Mutex<Option<PaddleOcrProcess>>>,
    #[cfg(target_os = "windows")]
    windows_hotkeys: Option<WindowsHotkeyManager>,
}


#[derive(Debug, Serialize, Deserialize, Clone)]
struct HotkeyBinding {
    key_code: u32,
    modifiers: u32,
    label: String,
}

#[derive(Debug, Serialize, Clone)]
struct HotkeySettings {
    toggle_overlay: HotkeyBinding,
    capture_current: HotkeyBinding,
    capture_portal: HotkeyBinding,
}

#[derive(Debug, Serialize, Clone)]
struct AvalonChestInfo {
    color: String,
    size: String,
    count: i64,
}

#[derive(Debug, Clone)]
struct AvalonInfo {
    tiers: Vec<i64>,
    components: Vec<String>,
    chests: Vec<AvalonChestInfo>,
}

fn bundled_or_project_path(relative_path: &str) -> PathBuf {
    let relative = Path::new(relative_path);
    let mut candidates = Vec::new();

    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            candidates.push(exe_dir.join("resources").join(relative));
            candidates.push(exe_dir.join(relative));
        }
    }

    if let Some(project_root) = Path::new(env!("CARGO_MANIFEST_DIR")).parent() {
        candidates.push(project_root.join(relative));
    }

    candidates
        .iter()
        .find(|path| path.exists())
        .cloned()
        .unwrap_or_else(|| candidates.pop().unwrap_or_else(|| relative.to_path_buf()))
}

fn lookup_avalon_info(normalized_name: &str) -> AvalonInfo {
    static CACHE: OnceLock<std::collections::HashMap<String, AvalonInfo>> = OnceLock::new();

    let map = CACHE.get_or_init(|| {
        let path = bundled_or_project_path("data/albion_navigator_import.json");

        let text = fs::read_to_string(&path).unwrap_or_else(|err| {
            eprintln!("[avalon-info] failed to read {}: {err}", path.display());
            "{}".to_string()
        });

        let root: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|err| {
            eprintln!("[avalon-info] failed to parse json: {err}");
            serde_json::Value::Null
        });

        let records = root
            .get("avalon_locations")
            .or_else(|| root.get("avalon"))
            .or_else(|| root.get("avalon_components"))
            .or_else(|| root.get("avalon_component_records"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut map = std::collections::HashMap::<String, AvalonInfo>::new();

        for record in records {
            let name = record
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            let normalized = record
                .get("normalized_name")
                .and_then(|v| v.as_str())
                .map(|v| v.to_string())
                .unwrap_or_else(|| normalize_location_name(name));

            if normalized.is_empty() {
                continue;
            }

            let mut tiers = Vec::<i64>::new();
            let mut components = Vec::<String>::new();
            let mut chest_map = std::collections::HashMap::<(String, String), i64>::new();

            if let Some(raw_tiers) = record.get("tiers").and_then(|v| v.as_array()) {
                for tier in raw_tiers {
                    if let Some(tier) = tier.as_i64() {
                        tiers.push(tier);
                    }
                }
            }

            if let Some(raw_names) = record.get("component_names").and_then(|v| v.as_array()) {
                for component in raw_names {
                    if let Some(name) = component.as_str() {
                        components.push(name.to_string());
                    }
                }
            }

            if let Some(raw_components) = record.get("components").and_then(|v| v.as_array()) {
                for component in raw_components {
                    let display_name = component
                        .get("name")
                        .or_else(|| component.get("DisplayName"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if display_name.eq_ignore_ascii_case("Chest") {
                        let props = component
                            .get("properties")
                            .or_else(|| component.get("Properties"))
                            .and_then(|v| v.as_array())
                            .cloned()
                            .unwrap_or_default();

                        let prop_values = props
                            .iter()
                            .filter_map(|v| v.as_i64())
                            .collect::<Vec<_>>();

                        let color = chest_color_from_properties(&prop_values);
                        let size = chest_size_from_properties(&prop_values);

                        if color != "unknown" {
                            *chest_map.entry((color, size)).or_insert(0) += 1;
                        }
                    }
                    if let Some(tier) = component
                        .get("tier")
                        .or_else(|| component.get("Tier"))
                        .and_then(|v| v.as_i64())
                    {
                        tiers.push(tier);
                    }

                    if let Some(name) = component
                        .get("name")
                        .or_else(|| component.get("DisplayName"))
                        .and_then(|v| v.as_str())
                    {
                        components.push(name.to_string());
                    }
                }
            }

            tiers.sort();
            tiers.dedup();

            components.sort();
            components.dedup();
            let mut chests = chest_map
                .into_iter()
                .map(|((color, size), count)| AvalonChestInfo {
                    color,
                    size,
                    count,
                })
                .collect::<Vec<_>>();

            chests.sort_by_key(|chest| match (chest.color.as_str(), chest.size.as_str()) {
                ("green", "small") => 0,
                ("green", "large") => 1,
                ("blue", "small") => 2,
                ("blue", "large") => 3,
                ("gold", "small") => 4,
                ("gold", "large") => 5,
                _ => 9,
            });

            chests.sort_by_key(|chest| match chest.color.as_str() {
                "green" => 0,
                "blue" => 1,
                "gold" => 2,
                _ => 9,
            });
            map.insert(
                normalized,
                AvalonInfo {
                    tiers,
                    components,
                    chests,
                },
            );
        }

        eprintln!("[avalon-info] loaded {} avalon info records", map.len());

        map
    });

    let key = normalize_location_name(normalized_name);

    map.get(&key).cloned().unwrap_or_else(|| AvalonInfo {
        tiers: Vec::new(),
        components: Vec::new(),
        chests: Vec::new(),
    })
}

#[derive(Debug, Deserialize, Clone)]

struct LocationMetadataEntry {
    name: String,
    normalized_name: String,
    zone_type: String,
}

struct MapOverlayProcess {
    child: Child,
    stdin: ChildStdin,
}

impl Drop for MapOverlayProcess {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            eprintln!("[overlay-helper] stopping orphaned helper pid={}", self.child.id());
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

struct PaddleOcrProcess {
    child: Child,
    stdin: ChildStdin,
    stdout_rx: mpsc::Receiver<String>,
}

impl Drop for PaddleOcrProcess {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            eprintln!("[ocr] stopping worker pid={}", self.child.id());
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(target_os = "windows")]
struct WindowsHotkeyManager {
    sender: mpsc::Sender<WindowsHotkeyCommand>,
    thread_id: Arc<Mutex<Option<u32>>>,
}

#[cfg(target_os = "windows")]
enum WindowsHotkeyCommand {
    Update(HotkeySettings),
    Stop,
}

#[cfg(target_os = "windows")]
impl WindowsHotkeyManager {
    fn start(
        settings: HotkeySettings,
        db_path: PathBuf,
        helper_path: PathBuf,
        capture_dir: PathBuf,
        overlay: Arc<Mutex<Option<MapOverlayProcess>>>,
        paddle_ocr: Arc<Mutex<Option<PaddleOcrProcess>>>,
    ) -> Self {
        let (sender, receiver) = mpsc::channel::<WindowsHotkeyCommand>();
        let thread_id = Arc::new(Mutex::new(None));
        let thread_id_for_thread = Arc::clone(&thread_id);

        thread::spawn(move || {
            let current_thread_id = unsafe { windows_hotkeys::GetCurrentThreadId() };
            let mut bootstrap_message = windows_hotkeys::Msg::default();
            unsafe {
                windows_hotkeys::PeekMessageW(
                    &mut bootstrap_message,
                    std::ptr::null_mut(),
                    0,
                    0,
                    windows_hotkeys::PM_NOREMOVE,
                );
            }
            if let Ok(mut slot) = thread_id_for_thread.lock() {
                *slot = Some(current_thread_id);
            }

            let mut registered_ids = Vec::<i32>::new();
            apply_windows_hotkey_settings(&settings, &mut registered_ids);

            loop {
                let mut message = windows_hotkeys::Msg::default();
                let result = unsafe {
                    windows_hotkeys::GetMessageW(&mut message, std::ptr::null_mut(), 0, 0)
                };

                if result <= 0 {
                    break;
                }

                match message.message {
                    windows_hotkeys::WM_HOTKEY => match message.w_param as i32 {
                        1 => {
                            eprintln!("[windows-hotkey] toggle overlay");
                            let _ = send_map_overlay_command_direct(
                                &overlay,
                                json!({ "type": "toggle" }),
                            );
                        }
                        2 => {
                            eprintln!("[windows-hotkey] capture current location");
                            spawn_hotkey_capture(
                                db_path.clone(),
                                helper_path.clone(),
                                capture_dir.clone(),
                                Arc::clone(&overlay),
                                Arc::clone(&paddle_ocr),
                                "current_location",
                            );
                        }
                        3 => {
                            eprintln!("[windows-hotkey] capture portal");
                            spawn_hotkey_capture(
                                db_path.clone(),
                                helper_path.clone(),
                                capture_dir.clone(),
                                Arc::clone(&overlay),
                                Arc::clone(&paddle_ocr),
                                "portal",
                            );
                        }
                        _ => {}
                    },
                    windows_hotkeys::WM_APP_UPDATE_HOTKEYS => {
                        let mut should_stop = false;
                        while let Ok(command) = receiver.try_recv() {
                            match command {
                                WindowsHotkeyCommand::Update(settings) => {
                                    apply_windows_hotkey_settings(&settings, &mut registered_ids);
                                }
                                WindowsHotkeyCommand::Stop => {
                                    should_stop = true;
                                }
                            }
                        }
                        if should_stop {
                            break;
                        }
                    }
                    _ => {}
                }
            }

            unregister_windows_hotkeys(&mut registered_ids);
        });

        Self { sender, thread_id }
    }

    fn update(&self, settings: HotkeySettings) {
        if self.sender.send(WindowsHotkeyCommand::Update(settings)).is_ok() {
            self.wake();
        }
    }

    fn wake(&self) {
        let thread_id = self.thread_id.lock().ok().and_then(|slot| *slot);
        if let Some(thread_id) = thread_id {
            unsafe {
                windows_hotkeys::PostThreadMessageW(
                    thread_id,
                    windows_hotkeys::WM_APP_UPDATE_HOTKEYS,
                    0,
                    0,
                );
            }
        }
    }
}

#[cfg(target_os = "windows")]
impl Drop for WindowsHotkeyManager {
    fn drop(&mut self) {
        let _ = self.sender.send(WindowsHotkeyCommand::Stop);
        self.wake();
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Region {
    key: String,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    display_id: Option<String>,
    scale_factor: Option<f64>,
    anchor_x: Option<f64>,
    anchor_y: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct RegionInput {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    display_id: Option<String>,
    scale_factor: Option<f64>,
    anchor_x: Option<f64>,
    anchor_y: Option<f64>,
}

#[derive(Debug, Serialize)]
struct Location {
    id: i64,
    name: String,
    x: Option<f64>,
    y: Option<f64>,
    zone_type: String,
    normalized_name: String,
    first_seen_at: String,
    last_seen_at: String,
    visit_count: i64,
    avalon_tiers: Vec<i64>,
    avalon_components: Vec<String>,
    avalon_chests: Vec<AvalonChestInfo>,
}

#[derive(Debug, Serialize)]
struct Edge {
    id: i64,
    from_location_id: i64,
    to_location_id: i64,
    from_location_name: String,
    to_location_name: String,
    first_seen_at: String,
    last_seen_at: String,
    ttl_seconds: Option<i64>,
    expires_at: Option<String>,
    observations_count: i64,
    confidence: f64,
    status: String,
    source: String,
}

#[derive(Debug, Serialize)]
struct Observation {
    id: i64,
    kind: String,
    from_location_name: Option<String>,
    to_location_name: Option<String>,
    raw_ocr_text: String,
    normalized_text: Option<String>,
    confidence: Option<f64>,
    screenshot_path: Option<String>,
    metadata_json: Option<String>,
    created_at: String,
}

#[derive(Debug, Serialize)]
struct DashboardData {
    current_location: Option<String>,
    last_ocr_result: Option<String>,
    known_locations_count: i64,
    known_edges_count: i64,
    last_capture_status: String,
}

#[derive(Debug, Serialize)]
struct GraphData {
    locations: Vec<Location>,
    edges: Vec<Edge>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct SyncSettings {
    enabled: bool,
    server_url: String,
    write_token: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct SyncSnapshot {
    ok: bool,
    edges: Vec<SyncEdgePayload>,
    error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct SyncEdgePayload {
    from_name: String,
    to_name: String,
    from_normalized: Option<String>,
    to_normalized: Option<String>,
    first_seen_at: Option<String>,
    last_seen_at: Option<String>,
    ttl_seconds: Option<i64>,
    expires_at: Option<String>,
    observations_count: Option<i64>,
    source: Option<String>,
    status: Option<String>,
    revive: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SyncPostResponse {
    ok: bool,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct ShortestPathResult {
    from: String,
    to: String,
    found: bool,
    distance_edges: i64,
    path: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AlbionNavigatorImport {
    zones: Vec<NavigatorZone>,
    static_edges: Vec<NavigatorEdge>,
}

#[derive(Debug, Deserialize)]
struct NavigatorZone {
    name: String,
    normalized_name: String,
}

#[derive(Debug, Deserialize)]
struct NavigatorEdge {
    from: String,
    to: String,
}

#[derive(Debug, Serialize)]
struct OcrResult {
    text: String,
    confidence: Option<f64>,
    engine: String,
    lines: Vec<OcrLine>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct OcrLine {
    text: String,
    confidence: Option<f64>,
    bbox: Option<OcrBbox>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct OcrBbox {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Debug, Deserialize)]
struct PaddleOcrHelperResult {
    ok: bool,
    engine: Option<String>,
    text: Option<String>,
    confidence: Option<f64>,
    lines: Option<Vec<OcrLine>>,
    error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CaptureOcrResult {
    text: String,
    confidence: Option<f64>,
    engine: String,
    image_path: String,
    width: i32,
    height: i32,
    duration_ms: i64,
    screen_recording_permission: bool,
    lines: Vec<OcrLine>,
}

#[derive(Debug, Serialize)]
struct CaptureOutcome {
    raw_ocr_text: String,
    normalized_text: String,
    matched_name: String,
    match_confidence: f64,
    ocr_confidence: Option<f64>,
    parsed_current: Option<ParsedCurrentLocation>,
    parsed_portal: Option<ParsedPortalTooltip>,
    image_path: String,
    duration_ms: i64,
    engine: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ParsedCurrentLocation {
    location_name: Option<String>,
    cleaned_candidate: String,
    matched_location_name: Option<String>,
    matched_location_score: f64,
    used_dictionary_match: bool,
    match_reason: String,
    confidence: f64,
    ignored_lines: Vec<String>,
    candidates: Vec<String>,
    top_matches: Vec<RankedLocation>,
    reason: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ParsedPortalTooltip {
    destination_name: Option<String>,
    slots_used: Option<i64>,
    slots_total: Option<i64>,
    expires_in_seconds: Option<i64>,
    confidence: f64,
    ignored_lines: Vec<String>,
    candidates: Vec<String>,
    reason: String,
}

#[derive(Debug, Clone)]
struct LocationMatch {
    name: String,
    score: f64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct RankedLocation {
    name: String,
    score: f64,
}

#[derive(Debug)]
struct LocationMatchResult {
    chosen: LocationMatch,
    cleaned_candidate: String,
    top5: Vec<RankedLocation>,
}

#[derive(Debug)]
struct CurrentLocationSelection {
    final_name: String,
    cleaned_candidate: String,
    matched_location_name: Option<String>,
    matched_location_score: f64,
    used_dictionary_match: bool,
    match_reason: String,
    confidence: f64,
    top5: Vec<RankedLocation>,
}

#[derive(Debug, Serialize)]
struct OverlayDiagnostics {
    platform: String,
    native_overlay_available: bool,
    helper_strategy: String,
    exclusive_fullscreen_supported: bool,
    exclusive_fullscreen_note: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct OverlaySelection {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    display_id: Option<String>,
    scale_factor: Option<f64>,
    anchor_x: Option<f64>,
    anchor_y: Option<f64>,
    destination_x: Option<i32>,
    destination_y: Option<i32>,
    destination_width: Option<i32>,
    destination_height: Option<i32>,
    timer_x: Option<i32>,
    timer_y: Option<i32>,
    timer_width: Option<i32>,
    timer_height: Option<i32>,
    cancelled: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct MapOverlayBounds {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

#[derive(Debug, Serialize)]
struct MapOverlayStatus {
    visible: bool,
    interactive: bool,
    bounds: MapOverlayBounds,
    hotkey: String,
    helper_running: bool,
    exclusive_fullscreen_note: String,
}

#[derive(Debug, Serialize)]
struct MapOverlayData {
    route_expires_at: Option<String>,
    route_edges_count: i64,
    current_location: Option<String>,
    last_portal_destination: Option<String>,
    last_portal_expires_in_seconds: Option<i64>,
    locations: Vec<Location>,
    edges: Vec<Edge>,
    route_locations: Vec<RouteOverlayLocation>,
    route_edges: Vec<RouteOverlayEdge>,
    bridge_locations: Vec<RouteOverlayLocation>,
    bridge_edges: Vec<RouteOverlayEdge>,
    last_capture_status: String,
    capture_mode: String,
    ocr_mode: String,
    db_status: String,
    known_locations_count: i64,
    known_edges_count: i64,
}

#[derive(Debug, Serialize, Clone)]
struct RouteOverlayLocation {
    id: i64,
    name: String,
    normalized_name: String,
    zone_type: String,
    x: Option<f64>,
    y: Option<f64>,
}

#[derive(Debug, Serialize, Clone)]
struct RouteOverlayEdge {
    id: i64,
    from_location_id: i64,
    to_location_id: i64,
    from_location_name: String,
    to_location_name: String,
    source: String,
}

static PRIMARY_LOCATION_DICTIONARY: OnceLock<Vec<String>> = OnceLock::new();
static HOTKEY_CAPTURE_PENDING: AtomicUsize = AtomicUsize::new(0);
const MAX_HOTKEY_CAPTURE_QUEUE: usize = 8;

struct HotkeyCaptureGuard;

impl HotkeyCaptureGuard {
    fn try_acquire() -> Option<Self> {
        let mut current = HOTKEY_CAPTURE_PENDING.load(Ordering::SeqCst);
        loop {
            if current >= MAX_HOTKEY_CAPTURE_QUEUE {
                return None;
            }

            match HOTKEY_CAPTURE_PENDING.compare_exchange(
                current,
                current + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Some(Self),
                Err(actual) => current = actual,
            }
        }
    }

    fn pending_count() -> usize {
        HOTKEY_CAPTURE_PENDING.load(Ordering::SeqCst)
    }
}

impl Drop for HotkeyCaptureGuard {
    fn drop(&mut self) {
        HOTKEY_CAPTURE_PENDING.fetch_sub(1, Ordering::SeqCst);
    }
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let db_path = app
                .path()
                .app_data_dir()
                .map_err(|err| format!("Could not resolve app data directory: {err}"))?
                .join("avalon-mapper-ocr.sqlite3");
            if let Some(parent) = db_path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|err| format!("Could not create app data directory: {err}"))?;
            }
            let capture_dir = app
                .path()
                .app_data_dir()
                .map_err(|err| format!("Could not resolve app data directory: {err}"))?
                .join("captures");
            fs::create_dir_all(&capture_dir)
                .map_err(|err| format!("Could not create capture directory: {err}"))?;
            let conn = open_database(&db_path)
                .map_err(|err| format!("Could not open SQLite database: {err}"))?;
            initialize_schema(&conn).map_err(|err| format!("Could not initialize SQLite: {err}"))?;
            import_static_route_graph(&conn).map_err(|err| format!("Could not import static route graph: {err}"))?;
            let state = AppState {
                db: Mutex::new(conn),
                db_path,
                capture_dir,
                map_overlay: Arc::new(Mutex::new(None)),
                paddle_ocr: Arc::new(Mutex::new(None)),
                #[cfg(target_os = "windows")]
                windows_hotkeys: None,
            };
            if let Err(err) = ensure_map_overlay_running(app.handle(), &state) {
                eprintln!("Could not start map overlay helper: {err}");
            } else if let Ok(data) = build_map_overlay_data(&state) {
                let _ = send_map_overlay_command(
                    app.handle(),
                    &state,
                    json!({ "type": "data", "data": data }),
                );
            }
            #[cfg(target_os = "windows")]
            let mut state = state;
            #[cfg(target_os = "windows")]
            {
                match start_windows_hotkey_manager(app.handle(), &state) {
                    Ok(manager) => {
                        state.windows_hotkeys = Some(manager);
                    }
                    Err(err) => {
                        eprintln!("Could not start Windows global hotkeys: {err}");
                    }
                }
            }
            app.manage(AppState {
                db: state.db,
                db_path: state.db_path,
                capture_dir: state.capture_dir,
                map_overlay: state.map_overlay,
                paddle_ocr: state.paddle_ocr,
                #[cfg(target_os = "windows")]
                windows_hotkeys: state.windows_hotkeys,
            });
            let managed = app.state::<AppState>();
            start_sync_worker(
                managed.db_path.clone(),
                Arc::clone(&managed.map_overlay),
            );
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_hotkey_settings,
            set_hotkey_binding,
            get_sync_settings,
            set_sync_settings,
            sync_now,
            get_dashboard,
            find_shortest_path,
            mark_edge_traversed,
            rebuild_graph_layout,
            undo_last_graph_action,
            reset_graph_database,
            get_region,
            save_region,
            list_graph,
            list_observations,
            normalize_text,
            mock_ocr,
            accept_current_location,
            accept_portal_capture,
            create_manual_edge,
            run_overlay_selection,
            overlay_diagnostics,
            show_map_overlay,
            hide_map_overlay,
            toggle_map_overlay,
            set_overlay_interactive,
            update_overlay_data,
            set_overlay_bounds,
            get_overlay_bounds,
            reset_overlay_position,
            get_map_overlay_status,
            capture_current_location,
            capture_portal_destination
        ])
        .run(tauri::generate_context!())
        .expect("error while running Avalon Mapper OCR");
}

fn import_static_route_graph(conn: &Connection) -> Result<(), String> {
    let path = bundled_or_project_path("data/albion_navigator_import.json");

    if !path.exists() {
        eprintln!("[route-graph] missing {}", path.display());
        return Ok(());
    }

    let text = fs::read_to_string(&path)
        .map_err(|err| format!("Could not read {}: {err}", path.display()))?;

    let imported: AlbionNavigatorImport = serde_json::from_str(&text)
        .map_err(|err| format!("Could not parse {}: {err}", path.display()))?;

    let tx = conn.unchecked_transaction().map_err(db_err)?;

    tx.execute("DELETE FROM route_static_edges", []).map_err(db_err)?;
    tx.execute("DELETE FROM route_static_locations", []).map_err(db_err)?;

    for zone in imported.zones {
        tx.execute(
            r#"
            INSERT INTO route_static_locations (normalized_name, name)
            VALUES (?1, ?2)
            ON CONFLICT(normalized_name) DO UPDATE SET
                name = excluded.name
            "#,
            params![zone.normalized_name, zone.name],
        )
        .map_err(db_err)?;
    }

    for edge in imported.static_edges {
        let a = normalize_location_name(&edge.from);
        let b = normalize_location_name(&edge.to);

        if a.is_empty() || b.is_empty() || a == b {
            continue;
        }

        let (from, to) = if a <= b { (a, b) } else { (b, a) };

        tx.execute(
            r#"
            INSERT OR IGNORE INTO route_static_edges (from_normalized, to_normalized, source)
            VALUES (?1, ?2, 'albion_navigator_static')
            "#,
            params![from, to],
        )
        .map_err(db_err)?;
    }

    for (city, portal) in city_portal_route_pairs() {
        tx.execute(
            r#"
            INSERT INTO route_static_locations (normalized_name, name)
            VALUES (?1, ?2), (?3, ?4)
            ON CONFLICT(normalized_name) DO UPDATE SET
                name = excluded.name
            "#,
            params![
                city,
                title_case_location_name(city),
                portal,
                title_case_location_name(portal)
            ],
        )
        .map_err(db_err)?;

        let (from, to) = if city <= portal { (city, portal) } else { (portal, city) };
        tx.execute(
            r#"
            INSERT OR IGNORE INTO route_static_edges (from_normalized, to_normalized, source)
            VALUES (?1, ?2, 'city_portal_patch')
            "#,
            params![from, to],
        )
        .map_err(db_err)?;
    }
    let manual_royal_city_edges = [
        // Thetford royal continent exits
        ("Thetford", "Swamp Cross"),
        ("Thetford", "Willow Wood"),

        // Fort Sterling royal continent exits
        ("Fort Sterling", "Mountain Cross"),

        // Martlock royal continent exits
        ("Martlock", "Mountain Cross"),

        // Lymhurst royal continent exits
        ("Lymhurst", "Forest Cross"),

        // Bridgewatch royal continent exits
        ("Bridgewatch", "Steppe Cross"),
    ];

    for (a, b) in manual_royal_city_edges {
        insert_manual_route_static_edge(&tx, a, b, "manual_royal_city")?;
    }
    tx.commit().map_err(db_err)?;

    let locations_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM route_static_locations", [], |row| row.get(0))
        .map_err(db_err)?;

    let edges_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM route_static_edges", [], |row| row.get(0))
        .map_err(db_err)?;

    eprintln!(
        "[route-graph] imported static graph: {} locations, {} edges",
        locations_count, edges_count
    );

    Ok(())
}

fn read_hotkey_binding(
    conn: &Connection,
    key: &str,
    fallback: HotkeyBinding,
) -> Result<HotkeyBinding, String> {
    Ok(get_setting(conn, key)?
        .and_then(|raw| serde_json::from_str::<HotkeyBinding>(&raw).ok())
        .unwrap_or(fallback))
}

fn default_toggle_overlay_hotkey() -> HotkeyBinding {
    HotkeyBinding {
        key_code: 46, // M
        modifiers: 2560, // option/alt + shift
        label: "⌥⇧M".to_string(),
    }
}

fn default_capture_current_hotkey() -> HotkeyBinding {
    HotkeyBinding {
        key_code: 37, // L
        modifiers: 2560,
        label: "⌥⇧L".to_string(),
    }
}

fn default_capture_portal_hotkey() -> HotkeyBinding {
    HotkeyBinding {
        key_code: 35, // P
        modifiers: 2560,
        label: "⌥⇧P".to_string(),
    }
}

fn send_hotkeys_to_overlay(app: &AppHandle, state: &AppState) -> Result<(), String> {
    if cfg!(target_os = "windows") {
        return Ok(());
    }

    let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;

    let toggle = read_hotkey_binding(&conn, "hotkey_toggle_overlay", default_toggle_overlay_hotkey())?;
    let current = read_hotkey_binding(&conn, "hotkey_capture_current", default_capture_current_hotkey())?;
    let portal = read_hotkey_binding(&conn, "hotkey_capture_portal", default_capture_portal_hotkey())?;

    drop(conn);

    send_map_overlay_command(
        app,
        state,
        json!({
            "type": "hotkeys",
            "toggle_overlay": toggle,
            "capture_current": current,
            "capture_portal": portal
        }),
    )
}

fn read_hotkey_settings_from_conn(conn: &Connection) -> Result<HotkeySettings, String> {
    Ok(HotkeySettings {
        toggle_overlay: read_hotkey_binding(conn, "hotkey_toggle_overlay", default_toggle_overlay_hotkey())?,
        capture_current: read_hotkey_binding(conn, "hotkey_capture_current", default_capture_current_hotkey())?,
        capture_portal: read_hotkey_binding(conn, "hotkey_capture_portal", default_capture_portal_hotkey())?,
    })
}

#[cfg(target_os = "windows")]
fn start_windows_hotkey_manager(
    app: &AppHandle,
    state: &AppState,
) -> Result<WindowsHotkeyManager, String> {
    let settings = {
        let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
        read_hotkey_settings_from_conn(&conn)?
    };
    let helper_path = ensure_native_overlay_helper(app)?;

    Ok(WindowsHotkeyManager::start(
        settings,
        state.db_path.clone(),
        helper_path,
        state.capture_dir.clone(),
        Arc::clone(&state.map_overlay),
        Arc::clone(&state.paddle_ocr),
    ))
}

#[cfg(target_os = "windows")]
fn apply_windows_hotkey_settings(settings: &HotkeySettings, registered_ids: &mut Vec<i32>) {
    unregister_windows_hotkeys(registered_ids);
    register_windows_hotkey(1, &settings.toggle_overlay, registered_ids);
    register_windows_hotkey(2, &settings.capture_current, registered_ids);
    register_windows_hotkey(3, &settings.capture_portal, registered_ids);
}

#[cfg(target_os = "windows")]
fn unregister_windows_hotkeys(registered_ids: &mut Vec<i32>) {
    for id in registered_ids.drain(..) {
        unsafe {
            windows_hotkeys::UnregisterHotKey(std::ptr::null_mut(), id);
        }
    }
}

#[cfg(target_os = "windows")]
fn register_windows_hotkey(id: i32, binding: &HotkeyBinding, registered_ids: &mut Vec<i32>) {
    let Some(vk) = windows_hotkey_vk(binding.key_code) else {
        eprintln!(
            "[windows-hotkey] skipping unsupported hotkey id={id} key_code={}",
            binding.key_code
        );
        return;
    };
    let modifiers = windows_hotkey_modifiers(binding) | windows_hotkeys::MOD_NOREPEAT;
    let ok = unsafe { windows_hotkeys::RegisterHotKey(std::ptr::null_mut(), id, modifiers, vk) };
    if ok == 0 {
        let error = std::io::Error::last_os_error();
        eprintln!(
            "[windows-hotkey] RegisterHotKey failed id={id} vk={vk} modifiers={modifiers} label={} error={error}",
            binding.label
        );
        return;
    }

    registered_ids.push(id);
    eprintln!(
        "[windows-hotkey] RegisterHotKey ok id={id} vk={vk} modifiers={modifiers} label={}",
        binding.label
    );
}

#[cfg(target_os = "windows")]
fn windows_hotkey_modifiers(binding: &HotkeyBinding) -> u32 {
    let mut result = 0;
    if (binding.modifiers & 0x0200) != 0 || binding.label.contains('⇧') || binding.label.contains("Shift") {
        result |= windows_hotkeys::MOD_SHIFT;
    }
    if (binding.modifiers & 0x0800) != 0 || binding.label.contains('⌥') || binding.label.contains("Alt") {
        result |= windows_hotkeys::MOD_ALT;
    }
    if (binding.modifiers & 0x1000) != 0 || binding.label.contains('⌃') || binding.label.contains("Ctrl") {
        result |= windows_hotkeys::MOD_CONTROL;
    }
    if (binding.modifiers & 0x0100) != 0 && (binding.label.contains('⌘') || binding.label.contains("Win")) {
        result |= windows_hotkeys::MOD_WIN;
    }

    result
}

#[cfg(target_os = "windows")]
fn windows_hotkey_vk(key_code: u32) -> Option<u32> {
    match key_code {
        0 => Some(0x41),  // A
        1 => Some(0x53),  // S
        2 => Some(0x44),  // D
        3 => Some(0x46),  // F
        4 => Some(0x48),  // H
        5 => Some(0x47),  // G
        6 => Some(0x5A),  // Z
        7 => Some(0x58),  // X
        8 => Some(0x43),  // C
        9 => Some(0x56),  // V
        11 => Some(0x42), // B
        12 => Some(0x51), // Q
        13 => Some(0x57), // W
        14 => Some(0x45), // E
        15 => Some(0x52), // R
        16 => Some(0x59), // Y
        17 => Some(0x54), // T
        18 => Some(0x31), // 1
        19 => Some(0x32), // 2
        20 => Some(0x33), // 3
        21 => Some(0x34), // 4
        22 => Some(0x36), // 6
        23 => Some(0x35), // 5
        24 => Some(0xBB), // =
        25 => Some(0x39), // 9
        26 => Some(0x37), // 7
        27 => Some(0xBD), // -
        28 => Some(0x38), // 8
        29 => Some(0x30), // 0
        30 => Some(0xDD), // ]
        31 => Some(0x4F), // O
        32 => Some(0x55), // U
        33 => Some(0xDB), // [
        34 => Some(0x49), // I
        35 => Some(0x50), // P
        37 => Some(0x4C), // L
        38 => Some(0x4A), // J
        39 => Some(0xDE), // '
        40 => Some(0x4B), // K
        41 => Some(0xBA), // ;
        42 => Some(0xDC), // \
        43 => Some(0xBC), // ,
        44 => Some(0xBF), // /
        45 => Some(0x4E), // N
        46 => Some(0x4D), // M
        47 => Some(0xBE), // .
        49 => Some(0x20), // Space
        50 => Some(0xC0), // `
        0x30..=0x5A => Some(key_code),
        _ => None,
    }
}

#[cfg(target_os = "windows")]
mod windows_hotkeys {
    use std::ffi::c_void;

    pub const WM_HOTKEY: u32 = 0x0312;
    pub const WM_APP_UPDATE_HOTKEYS: u32 = 0x8001;
    pub const MOD_ALT: u32 = 0x0001;
    pub const MOD_CONTROL: u32 = 0x0002;
    pub const MOD_SHIFT: u32 = 0x0004;
    pub const MOD_WIN: u32 = 0x0008;
    pub const MOD_NOREPEAT: u32 = 0x4000;
    pub const PM_NOREMOVE: u32 = 0x0000;

    #[repr(C)]
    #[derive(Default)]
    pub struct Point {
        pub x: i32,
        pub y: i32,
    }

    #[repr(C)]
    pub struct Msg {
        pub hwnd: *mut c_void,
        pub message: u32,
        pub w_param: usize,
        pub l_param: isize,
        pub time: u32,
        pub pt: Point,
    }

    impl Default for Msg {
        fn default() -> Self {
            Self {
                hwnd: std::ptr::null_mut(),
                message: 0,
                w_param: 0,
                l_param: 0,
                time: 0,
                pt: Point::default(),
            }
        }
    }

    #[link(name = "user32")]
    extern "system" {
        pub fn RegisterHotKey(hwnd: *mut c_void, id: i32, fs_modifiers: u32, vk: u32) -> i32;
        pub fn UnregisterHotKey(hwnd: *mut c_void, id: i32) -> i32;
        pub fn GetMessageW(msg: *mut Msg, hwnd: *mut c_void, min: u32, max: u32) -> i32;
        pub fn PeekMessageW(msg: *mut Msg, hwnd: *mut c_void, min: u32, max: u32, remove_msg: u32) -> i32;
        pub fn PostThreadMessageW(thread_id: u32, msg: u32, w_param: usize, l_param: isize) -> i32;
        pub fn GetCurrentThreadId() -> u32;
    }
}

fn chest_color_from_properties(properties: &[i64]) -> String {
    match properties.get(1).copied() {
        Some(7) => "green".to_string(),
        Some(8) => "blue".to_string(),
        Some(9) => "gold".to_string(),
        _ => "unknown".to_string(),
    }
}

fn chest_size_from_properties(properties: &[i64]) -> String {
    match properties.first().copied() {
        Some(1) => "large".to_string(),
        Some(0) => "small".to_string(),
        _ => "unknown".to_string(),
    }
}

fn open_database(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    configure_database_connection(&conn)?;
    Ok(conn)
}

fn configure_database_connection(conn: &Connection) -> rusqlite::Result<()> {
    conn.busy_timeout(StdDuration::from_secs(5))?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    Ok(())
}

fn insert_manual_route_static_edge(
    conn: &Connection,
    a: &str,
    b: &str,
    source: &str,
) -> Result<(), String> {
    let a_norm = normalize_location_name(a);
    let b_norm = normalize_location_name(b);

    conn.execute(
        r#"
        INSERT INTO route_static_locations (normalized_name, name)
        VALUES (?1, ?2)
        ON CONFLICT(normalized_name) DO UPDATE SET name = excluded.name
        "#,
        params![a_norm, a],
    )
    .map_err(db_err)?;

    conn.execute(
        r#"
        INSERT INTO route_static_locations (normalized_name, name)
        VALUES (?1, ?2)
        ON CONFLICT(normalized_name) DO UPDATE SET name = excluded.name
        "#,
        params![b_norm, b],
    )
    .map_err(db_err)?;

    let (from, to) = if a_norm <= b_norm {
        (a_norm, b_norm)
    } else {
        (b_norm, a_norm)
    };

    conn.execute(
        r#"
        INSERT OR IGNORE INTO route_static_edges (from_normalized, to_normalized, source)
        VALUES (?1, ?2, ?3)
        "#,
        params![from, to, source],
    )
    .map_err(db_err)?;

    Ok(())
}

fn initialize_schema(conn: &Connection) -> rusqlite::Result<()> {
    configure_database_connection(conn)?;
    conn.execute_batch(
        r#"
        PRAGMA foreign_keys = ON;

        CREATE TABLE IF NOT EXISTS node_positions (
            location_id INTEGER PRIMARY KEY,
            x REAL NOT NULL,
            y REAL NOT NULL,
            updated_at TEXT NOT NULL,
            FOREIGN KEY(location_id) REFERENCES locations(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS route_static_locations (
            normalized_name TEXT PRIMARY KEY,
            name TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS route_static_edges (
            from_normalized TEXT NOT NULL,
            to_normalized TEXT NOT NULL,
            source TEXT NOT NULL DEFAULT 'albion_navigator_static',
            PRIMARY KEY (from_normalized, to_normalized)
        );

        CREATE TABLE IF NOT EXISTS app_settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS regions (
            key TEXT PRIMARY KEY,
            x INTEGER NOT NULL,
            y INTEGER NOT NULL,
            width INTEGER NOT NULL,
            height INTEGER NOT NULL,
            display_id TEXT,
            scale_factor REAL,
            anchor_x REAL,
            anchor_y REAL
        );

        CREATE TABLE IF NOT EXISTS locations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL UNIQUE,
            normalized_name TEXT NOT NULL UNIQUE,
            zone_type TEXT NOT NULL DEFAULT 'unknown',
            first_seen_at TEXT NOT NULL,
            last_seen_at TEXT NOT NULL,
            visit_count INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS edges (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            from_location_id INTEGER NOT NULL,
            to_location_id INTEGER NOT NULL,
            first_seen_at TEXT NOT NULL,
            last_seen_at TEXT NOT NULL,
            ttl_seconds INTEGER,
            expires_at TEXT,
            observations_count INTEGER NOT NULL DEFAULT 1,
            confidence REAL NOT NULL DEFAULT 1.0,
            status TEXT NOT NULL DEFAULT 'observed',
            source TEXT NOT NULL DEFAULT 'ocr',
            UNIQUE(from_location_id, to_location_id),
            FOREIGN KEY(from_location_id) REFERENCES locations(id),
            FOREIGN KEY(to_location_id) REFERENCES locations(id)
        );

        CREATE TABLE IF NOT EXISTS observations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            kind TEXT NOT NULL,
            from_location_name TEXT,
            to_location_name TEXT,
            raw_ocr_text TEXT NOT NULL,
            normalized_text TEXT,
            confidence REAL,
            screenshot_path TEXT,
            metadata_json TEXT,
            created_at TEXT NOT NULL
        );
        "#,
    )?;
    ensure_regions_anchor_columns(conn)?;
    ensure_observations_metadata_column(conn)?;
    ensure_edges_ttl_columns(conn)?;
    ensure_locations_zone_type_column(conn)?;
    Ok(())
}

fn ensure_locations_zone_type_column(conn: &Connection) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(locations)")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    let columns = rows.collect::<rusqlite::Result<Vec<_>>>()?;

    if !columns.iter().any(|column| column == "zone_type") {
        conn.execute(
            "ALTER TABLE locations ADD COLUMN zone_type TEXT NOT NULL DEFAULT 'unknown'",
            [],
        )?;
    }

    Ok(())
}

fn ensure_regions_anchor_columns(conn: &Connection) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(regions)")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    let columns = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    if !columns.iter().any(|column| column == "anchor_x") {
        conn.execute("ALTER TABLE regions ADD COLUMN anchor_x REAL", [])?;
    }
    if !columns.iter().any(|column| column == "anchor_y") {
        conn.execute("ALTER TABLE regions ADD COLUMN anchor_y REAL", [])?;
    }
    Ok(())
}

fn ensure_observations_metadata_column(conn: &Connection) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(observations)")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    let has_column = rows
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .any(|column| column == "metadata_json");
    if !has_column {
        conn.execute("ALTER TABLE observations ADD COLUMN metadata_json TEXT", [])?;
    }
    Ok(())
}

fn ensure_edges_ttl_columns(conn: &Connection) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(edges)")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    let columns = rows.collect::<rusqlite::Result<Vec<_>>>()?;

    if !columns.iter().any(|column| column == "ttl_seconds") {
        conn.execute("ALTER TABLE edges ADD COLUMN ttl_seconds INTEGER", [])?;
    }

    if !columns.iter().any(|column| column == "expires_at") {
        conn.execute("ALTER TABLE edges ADD COLUMN expires_at TEXT", [])?;
    }

    Ok(())
}

#[tauri::command]
fn get_hotkey_settings(state: tauri::State<'_, AppState>) -> Result<HotkeySettings, String> {
    let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;

    read_hotkey_settings_from_conn(&conn)
}

#[tauri::command]
fn set_hotkey_binding(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    action: String,
    key_code: u32,
    modifiers: u32,
    label: String,
) -> Result<HotkeySettings, String> {
    let setting_key = match action.as_str() {
        "toggle_overlay" => "hotkey_toggle_overlay",
        "capture_current" => "hotkey_capture_current",
        "capture_portal" => "hotkey_capture_portal",
        _ => return Err("Unknown hotkey action".to_string()),
    };

    let binding = HotkeyBinding {
        key_code,
        modifiers,
        label,
    };

    {
        let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
        set_setting(
            &conn,
            setting_key,
            &serde_json::to_string(&binding).map_err(|err| err.to_string())?,
        )?;
    }

    send_hotkeys_to_overlay(&app, &state)?;

    let settings = {
        let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
        read_hotkey_settings_from_conn(&conn)?
    };

    #[cfg(target_os = "windows")]
    if let Some(manager) = &state.windows_hotkeys {
        manager.update(settings.clone());
    }

    Ok(settings)
}

#[tauri::command]
fn get_sync_settings(state: tauri::State<'_, AppState>) -> Result<SyncSettings, String> {
    let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
    read_sync_settings(&conn)
}

#[tauri::command]
fn set_sync_settings(
    state: tauri::State<'_, AppState>,
    enabled: bool,
    server_url: String,
    write_token: String,
) -> Result<SyncSettings, String> {
    let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
    set_setting(&conn, "sync_enabled", if enabled { "true" } else { "false" })?;
    set_setting(&conn, "sync_server_url", server_url.trim())?;
    set_setting(&conn, "sync_write_token", write_token.trim())?;
    read_sync_settings(&conn)
}

#[tauri::command]
fn sync_now(state: tauri::State<'_, AppState>) -> Result<(), String> {
    run_sync_once(&state.db_path, &state.map_overlay)
}

#[tauri::command]
fn find_shortest_path(
    state: tauri::State<'_, AppState>,
    from_location: String,
    to_location: String,
) -> Result<ShortestPathResult, String> {
    let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;

    delete_expired_edges(&conn)?;

    let from_query = from_location.trim();
    let to_query = to_location.trim();

    if from_query.is_empty() || to_query.is_empty() {
        return Err("Both locations are required".to_string());
    }

    let graph = build_route_graph(&conn)?;
    let all_names = load_route_location_names(&conn)?;

    let to_is_safe = is_safe_route_query(to_query);
    let from_is_city = is_city_route_query(from_query);

    let matched_to_norm = if to_is_safe {
        None
    } else {
        Some(
            match_route_location_name(to_query, &all_names)
                .ok_or_else(|| format!("Could not match route destination: {to_query}"))?,
        )
    };

    let from_norm = if from_is_city {
        let destination = matched_to_norm
            .as_deref()
            .ok_or_else(|| "Route start 'city' requires a concrete destination".to_string())?;
        nearest_city_route_target(&graph, destination)
            .ok_or_else(|| format!("Could not find nearest city to {to_query}"))?
    } else {
        match_route_location_name(from_query, &all_names)
            .ok_or_else(|| format!("Could not match route start: {from_query}"))?
    };

    let to_norm = if to_is_safe {
        nearest_safe_route_target(&graph, &from_norm)
            .ok_or_else(|| format!("Could not find nearest safe zone from {from_query}"))?
    } else {
        matched_to_norm.expect("matched_to_norm exists for non-safe route")
    };

    let path = shortest_path_bfs(&graph, &from_norm, &to_norm);

    Ok(match path {
        Some(path_norms) => {
            let names = path_norms
                .iter()
                .map(|normalized| resolve_route_location_name(&conn, normalized).unwrap_or_else(|_| normalized.clone()))
                .collect::<Vec<_>>();

            ShortestPathResult {
                from: from_location,
                to: to_location,
                found: true,
                distance_edges: names.len().saturating_sub(1) as i64,
                path: names,
            }
        }
        None => ShortestPathResult {
            from: from_location,
            to: to_location,
            found: false,
            distance_edges: -1,
            path: Vec::new(),
        },
    })
}

#[tauri::command]
fn get_dashboard(state: tauri::State<'_, AppState>) -> Result<DashboardData, String> {
    let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
    let current_location = get_setting(&conn, "current_location_name")?;
    let last_ocr_result = conn
        .query_row(
            "SELECT raw_ocr_text FROM observations ORDER BY created_at DESC, id DESC LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(db_err)?;
    let known_locations_count = conn
        .query_row("SELECT COUNT(*) FROM locations", [], |row| row.get(0))
        .map_err(db_err)?;
    let known_edges_count = conn
        .query_row("SELECT COUNT(*) FROM edges", [], |row| row.get(0))
        .map_err(db_err)?;
    let last_capture_status =
        get_setting(&conn, "last_capture_status")?.unwrap_or_else(|| "No captures yet".to_string());

    Ok(DashboardData {
        current_location,
        last_ocr_result,
        known_locations_count,
        known_edges_count,
        last_capture_status,
    })
}

#[tauri::command]
fn get_region(state: tauri::State<'_, AppState>, key: String) -> Result<Option<Region>, String> {
    let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
    conn.query_row(
        "SELECT key, x, y, width, height, display_id, scale_factor, anchor_x, anchor_y FROM regions WHERE key = ?1",
        params![key],
        region_from_row,
    )
    .optional()
    .map_err(db_err)
}

#[tauri::command]
fn save_region(
    state: tauri::State<'_, AppState>,
    key: String,
    region: RegionInput,
) -> Result<Region, String> {
    let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
    save_region_with_conn(&conn, key, region)
}

fn save_region_with_conn(conn: &Connection, key: String, region: RegionInput) -> Result<Region, String> {
    if region.width <= 0 || region.height <= 0 {
        return Err("Region width and height must be positive".to_string());
    }

    conn.execute(
        r#"
        INSERT INTO regions (key, x, y, width, height, display_id, scale_factor, anchor_x, anchor_y)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
        ON CONFLICT(key) DO UPDATE SET
            x = excluded.x,
            y = excluded.y,
            width = excluded.width,
            height = excluded.height,
            display_id = excluded.display_id,
            scale_factor = excluded.scale_factor,
            anchor_x = excluded.anchor_x,
            anchor_y = excluded.anchor_y
        "#,
        params![
            key,
            region.x,
            region.y,
            region.width,
            region.height,
            region.display_id,
            region.scale_factor,
            region.anchor_x,
            region.anchor_y
        ],
    )
    .map_err(db_err)?;

    Ok(Region {
        key,
        x: region.x,
        y: region.y,
        width: region.width,
        height: region.height,
        display_id: region.display_id,
        scale_factor: region.scale_factor,
        anchor_x: region.anchor_x,
        anchor_y: region.anchor_y,
    })
}

#[tauri::command]
fn list_graph(state: tauri::State<'_, AppState>) -> Result<GraphData, String> {
    let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
    Ok(GraphData {
        locations: load_locations(&conn)?,
        edges: load_edges(&conn)?,
    })
}

#[tauri::command]
fn list_observations(state: tauri::State<'_, AppState>) -> Result<Vec<Observation>, String> {
    let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
    let mut stmt = conn
        .prepare(
            r#"
            SELECT id, kind, from_location_name, to_location_name, raw_ocr_text, normalized_text,
                   confidence, screenshot_path, metadata_json, created_at
            FROM observations
            ORDER BY created_at DESC, id DESC
            LIMIT 100
            "#,
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map([], |row| {
            Ok(Observation {
                id: row.get(0)?,
                kind: row.get(1)?,
                from_location_name: row.get(2)?,
                to_location_name: row.get(3)?,
                raw_ocr_text: row.get(4)?,
                normalized_text: row.get(5)?,
                confidence: row.get(6)?,
                screenshot_path: row.get(7)?,
                metadata_json: row.get(8)?,
                created_at: row.get(9)?,
            })
        })
        .map_err(db_err)?;

    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_err)
}

#[tauri::command]
fn normalize_text(raw_text: String) -> String {
    normalize_location_name(&raw_text)
}

#[tauri::command]
fn mock_ocr(text: Option<String>) -> OcrResult {
    let text = text.unwrap_or_else(|| "Avalonian Portal\nDeepwood Dell".to_string());
    OcrResult {
        lines: text
            .lines()
            .map(|line| OcrLine {
                text: line.to_string(),
                confidence: Some(1.0),
                bbox: None,
            })
            .collect(),
        text,
        confidence: Some(1.0),
        engine: "manual/mock".to_string(),
    }
}

#[tauri::command]
fn accept_current_location(
    state: tauri::State<'_, AppState>,
    raw_ocr_text: String,
    corrected_name: Option<String>,
) -> Result<Location, String> {
    let display_name = corrected_name
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| raw_ocr_text.clone());
    let normalized = normalize_location_name(&display_name);
    if normalized.is_empty() {
        return Err("Current location is empty after normalization".to_string());
    }

    let mut conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
    let tx = conn.transaction().map_err(db_err)?;
    let zone_type = infer_zone_type_from_name(display_name.trim());
    let location_id = upsert_location(&tx, display_name.trim(), &normalized, &zone_type, true)?;
    insert_observation(
        &tx,
        "current_location",
        Some(display_name.trim()),
        None,
        &raw_ocr_text,
        Some(&normalized),
        Some(1.0),
        None,
        None,
    )?;
    set_setting(&tx, "current_location_id", &location_id.to_string())?;
    set_setting(&tx, "current_location_name", display_name.trim())?;
    set_setting(&tx, "last_capture_status", "Accepted current location")?;
    tx.commit().map_err(db_err)?;

    load_location_by_id(&conn, location_id)
}

#[tauri::command]
fn accept_portal_capture(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    raw_ocr_text: String,
    corrected_destination: Option<String>,
) -> Result<Edge, String> {
    let display_name = corrected_destination
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| raw_ocr_text.clone());
    let normalized = normalize_location_name(&display_name);
    if normalized.is_empty() {
        return Err("Portal destination is empty after normalization".to_string());
    }

    let mut conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
    let current_location_id = get_setting(&conn, "current_location_id")?
        .ok_or_else(|| "Set the current location before accepting a portal capture".to_string())?
        .parse::<i64>()
        .map_err(|_| "Stored current location id is invalid".to_string())?;
    let current_location_name = get_setting(&conn, "current_location_name")?
        .unwrap_or_else(|| "Unknown current location".to_string());

    let tx = conn.transaction().map_err(db_err)?;
    let zone_type = infer_zone_type_from_name(display_name.trim());
    if !allowed_graph_zone_type(&zone_type) {
        return Err(format!(
            "Ignored non-graph portal destination: {} ({})",
            display_name.trim(), zone_type
        ));
    }
    let destination_id = upsert_location(&tx, display_name.trim(), &normalized, &zone_type, false)?;

    let edge_id = upsert_edge(&tx, current_location_id, destination_id, None)?;
    recompute_graph_layout(&tx)?;
    insert_observation(
        &tx,
        "portal",
        Some(&current_location_name),
        Some(display_name.trim()),
        &raw_ocr_text,
        Some(&normalized),
        Some(1.0),
        None,
        None,
    )?;
    set_setting(&tx, "last_capture_status", "Accepted portal destination")?;
    tx.commit().map_err(db_err)?;

    let edge = load_edge_by_id(&conn, edge_id)?;
    drop(conn);
    refresh_map_overlay(&app, &state)?;
    trigger_sync_after_local_change(state.db_path.clone(), Arc::clone(&state.map_overlay));
    Ok(edge)
}

#[tauri::command]
fn rebuild_graph_layout(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;

    recompute_graph_layout(&conn)?;
    set_setting(&conn, "last_capture_status", "Graph layout rebuilt")?;

    drop(conn);
    refresh_map_overlay(&app, &state)?;
    Ok(())
}


#[tauri::command]
fn create_manual_edge(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    from_location: String,
    to_location: String,
) -> Result<Edge, String> {
    let from_normalized = normalize_location_name(&from_location);
    let to_normalized = normalize_location_name(&to_location);
    if from_normalized.is_empty() || to_normalized.is_empty() {
        return Err("Both locations are required".to_string());
    }

    let mut conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
    let tx = conn.transaction().map_err(db_err)?;

    let zone_type_from = infer_zone_type_from_name(from_location.trim());
    let from_id = upsert_location(&tx, from_location.trim(), &from_normalized, &zone_type_from, false)?;

    let zone_type_to = infer_zone_type_from_name(to_location.trim());
    let to_id = upsert_location(&tx, to_location.trim(), &to_normalized, &zone_type_to, false)?;


    let edge_id = upsert_edge(&tx, from_id, to_id, None)?;
    recompute_graph_layout(&tx)?;
    insert_observation(
        &tx,
        "manual_edge",
        Some(from_location.trim()),
        Some(to_location.trim()),
        &format!("{} -> {}", from_location.trim(), to_location.trim()),
        Some(&to_normalized),
        Some(1.0),
        None,
        None,
    )?;
    set_setting(&tx, "last_capture_status", "Created manual edge")?;
    tx.commit().map_err(db_err)?;

    let edge = load_edge_by_id(&conn, edge_id)?;
    drop(conn);
    refresh_map_overlay(&app, &state)?;
    trigger_sync_after_local_change(state.db_path.clone(), Arc::clone(&state.map_overlay));
    Ok(edge)
}

#[tauri::command]
fn undo_last_graph_action(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
    undo_last_graph_action_inner(&conn)?;
    drop(conn);

    refresh_map_overlay(&app, &state)?;
    Ok(())
}

fn build_route_graph(conn: &Connection) -> Result<std::collections::HashMap<String, Vec<String>>, String> {
    let mut graph = std::collections::HashMap::<String, Vec<String>>::new();

    {
        let mut stmt = conn
            .prepare("SELECT from_normalized, to_normalized FROM route_static_edges")
            .map_err(db_err)?;

        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(db_err)?;

        for row in rows {
            let (a, b) = row.map_err(db_err)?;
            add_undirected_route_edge(&mut graph, a, b);
        }
    }

    {
        let mut stmt = conn
            .prepare(
                r#"
                SELECT lf.normalized_name, lt.normalized_name
                FROM edges e
                JOIN locations lf ON lf.id = e.from_location_id
                JOIN locations lt ON lt.id = e.to_location_id
                WHERE e.status NOT IN ('expired', 'deleted')
                  AND (
                    e.expires_at IS NULL
                    OR datetime(e.expires_at) > datetime('now')
                  )
                "#,
            )
            .map_err(db_err)?;

        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(db_err)?;

        for row in rows {
            let (a, b) = row.map_err(db_err)?;
            add_undirected_route_edge(&mut graph, a, b);
        }
    }

    add_city_portal_route_edges(&mut graph);

    for neighbors in graph.values_mut() {
        neighbors.sort();
        neighbors.dedup();
    }

    Ok(graph)
}

fn add_undirected_route_edge(
    graph: &mut std::collections::HashMap<String, Vec<String>>,
    a: String,
    b: String,
) {
    if a == b {
        return;
    }

    graph.entry(a.clone()).or_default().push(b.clone());
    graph.entry(b).or_default().push(a);
}

fn add_city_portal_route_edges(graph: &mut std::collections::HashMap<String, Vec<String>>) {
    for (city, portal) in city_portal_route_pairs() {
        add_undirected_route_edge(graph, city.to_string(), portal.to_string());
    }
}

fn city_portal_route_pairs() -> &'static [(&'static str, &'static str)] {
    &[
        ("bridgewatch", "bridgewatch portal"),
        ("fort sterling", "fort sterling portal"),
        ("lymhurst", "lymhurst portal"),
        ("martlock", "martlock portal"),
        ("thetford", "thetford portal"),
    ]
}

fn route_city_names() -> &'static [&'static str] {
    &[
        "bridgewatch",
        "caerleon",
        "fort sterling",
        "lymhurst",
        "martlock",
        "thetford",
    ]
}

fn shortest_path_bfs(
    graph: &std::collections::HashMap<String, Vec<String>>,
    from: &str,
    to: &str,
) -> Option<Vec<String>> {
    if from == to {
        return Some(vec![from.to_string()]);
    }

    let mut queue = std::collections::VecDeque::<String>::new();
    let mut visited = std::collections::HashSet::<String>::new();
    let mut parent = std::collections::HashMap::<String, String>::new();

    visited.insert(from.to_string());
    queue.push_back(from.to_string());

    while let Some(current) = queue.pop_front() {
        let Some(neighbors) = graph.get(&current) else {
            continue;
        };

        for neighbor in neighbors {
            if visited.contains(neighbor) {
                continue;
            }

            visited.insert(neighbor.clone());
            parent.insert(neighbor.clone(), current.clone());

            if neighbor == to {
                let mut path = vec![to.to_string()];
                let mut cursor = to.to_string();

                while let Some(prev) = parent.get(&cursor) {
                    path.push(prev.clone());
                    cursor = prev.clone();

                    if cursor == from {
                        break;
                    }
                }

                path.reverse();
                return Some(path);
            }

            queue.push_back(neighbor.clone());
        }
    }

    None
}

fn nearest_route_target<F>(
    graph: &std::collections::HashMap<String, Vec<String>>,
    from: &str,
    mut is_target: F,
) -> Option<String>
where
    F: FnMut(&str) -> bool,
{
    let mut queue = std::collections::VecDeque::<String>::new();
    let mut visited = std::collections::HashSet::<String>::new();

    visited.insert(from.to_string());
    queue.push_back(from.to_string());

    while let Some(current) = queue.pop_front() {
        if current != from && is_target(&current) {
            return Some(current);
        }

        let mut neighbors = graph.get(&current).cloned().unwrap_or_default();
        neighbors.sort();

        for neighbor in neighbors {
            if visited.insert(neighbor.clone()) {
                queue.push_back(neighbor);
            }
        }
    }

    None
}

fn nearest_safe_route_target(
    graph: &std::collections::HashMap<String, Vec<String>>,
    from: &str,
) -> Option<String> {
    if matches!(infer_zone_type_from_name(from).as_str(), "blue" | "yellow") {
        return Some(from.to_string());
    }

    nearest_route_target(graph, from, |normalized| {
        matches!(infer_zone_type_from_name(normalized).as_str(), "blue" | "yellow")
    })
}

fn nearest_city_route_target(
    graph: &std::collections::HashMap<String, Vec<String>>,
    from: &str,
) -> Option<String> {
    let cities = route_city_names();

    if cities.contains(&from) {
        return Some(from.to_string());
    }

    nearest_route_target(graph, from, |normalized| cities.contains(&normalized))
}

fn is_safe_route_query(query: &str) -> bool {
    normalize_location_name(query) == "safe"
}

fn is_city_route_query(query: &str) -> bool {
    normalize_location_name(query) == "city"
}

fn resolve_route_location_name(conn: &Connection, normalized: &str) -> Result<String, String> {
    if let Some(name) = conn
        .query_row(
            "SELECT name FROM locations WHERE normalized_name = ?1",
            params![normalized],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(db_err)?
    {
        return Ok(name);
    }

    if let Some(name) = conn
        .query_row(
            "SELECT name FROM route_static_locations WHERE normalized_name = ?1",
            params![normalized],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(db_err)?
    {
        return Ok(name);
    }

    Ok(normalized.to_string())
}

fn undo_last_graph_action_inner(conn: &Connection) -> Result<(), String> {
    let last = conn
        .query_row(
            r#"
            SELECT id, kind, from_location_name, to_location_name
            FROM observations
            WHERE kind IN ('portal', 'manual_edge', 'current_location')
            ORDER BY created_at DESC, id DESC
            LIMIT 1
            "#,
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()
        .map_err(db_err)?;

    let Some((observation_id, kind, from_name, to_name)) = last else {
        return Ok(());
    };

    if matches!(kind.as_str(), "portal" | "manual_edge") {
        if let (Some(from_name), Some(to_name)) = (from_name, to_name) {
            let from_norm = normalize_location_name(&from_name);
            let to_norm = normalize_location_name(&to_name);

            conn.execute(
                r#"
                DELETE FROM edges
                WHERE (
                    from_location_id = (SELECT id FROM locations WHERE normalized_name = ?1)
                    AND to_location_id = (SELECT id FROM locations WHERE normalized_name = ?2)
                )
                OR (
                    from_location_id = (SELECT id FROM locations WHERE normalized_name = ?2)
                    AND to_location_id = (SELECT id FROM locations WHERE normalized_name = ?1)
                )
                "#,
                params![from_norm, to_norm],
            )
            .map_err(db_err)?;
        }
    }

    conn.execute("DELETE FROM observations WHERE id = ?1", params![observation_id])
        .map_err(db_err)?;

    conn.execute_batch(
        r#"
        DELETE FROM node_positions
        WHERE location_id NOT IN (SELECT id FROM locations);

        DELETE FROM locations
        WHERE id NOT IN (
            SELECT from_location_id FROM edges
            UNION
            SELECT to_location_id FROM edges
        )
        AND normalized_name NOT IN (
            SELECT value FROM app_settings WHERE key = 'current_location_name'
        );
        "#,
    )
    .map_err(db_err)?;

    set_setting(conn, "last_capture_status", "Undid last graph action")?;
    Ok(())
}

#[tauri::command]
fn reset_graph_database(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;

    conn.execute_batch(
        r#"
        DELETE FROM edges;
        DELETE FROM node_positions;
        DELETE FROM locations;
        DELETE FROM observations;

        DELETE FROM app_settings
        WHERE key IN (
            'current_location_id',
            'current_location_name',
            'last_portal_destination',
            'last_capture_status'
        );
        "#,
    )
    .map_err(db_err)?;

    set_setting(&conn, "last_capture_status", "Graph database reset")?;
    drop(conn);

    refresh_map_overlay(&app, &state)?;
    Ok(())
}

#[tauri::command]
fn run_overlay_selection(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    key: String,
    mode: String,
) -> Result<Region, String> {
    let selection = run_native_overlay(&app, &mode)?;
    if selection.cancelled {
        return Err("Overlay selection cancelled".to_string());
    }
    let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
    let base = save_region_with_conn(
        &conn,
        key.clone(),
        RegionInput {
            x: selection.x,
            y: selection.y,
            width: selection.width,
            height: selection.height,
            display_id: selection.display_id.clone(),
            scale_factor: selection.scale_factor,
            anchor_x: selection.anchor_x,
            anchor_y: selection.anchor_y,
        },
    )?;

    if key == "portal_tooltip" && mode == "portal-strips" {
        if let (Some(x), Some(y), Some(width), Some(height)) = (
            selection.destination_x,
            selection.destination_y,
            selection.destination_width,
            selection.destination_height,
        ) {
            save_region_with_conn(
                &conn,
                "portal_tooltip_destination".to_string(),
                RegionInput {
                    x,
                    y,
                    width,
                    height,
                    display_id: selection.display_id.clone(),
                    scale_factor: selection.scale_factor,
                    anchor_x: selection.anchor_x,
                    anchor_y: selection.anchor_y,
                },
            )?;
        }
        if let (Some(x), Some(y), Some(width), Some(height)) = (
            selection.timer_x,
            selection.timer_y,
            selection.timer_width,
            selection.timer_height,
        ) {
            save_region_with_conn(
                &conn,
                "portal_tooltip_timer".to_string(),
                RegionInput {
                    x,
                    y,
                    width,
                    height,
                    display_id: selection.display_id.clone(),
                    scale_factor: selection.scale_factor,
                    anchor_x: selection.anchor_x,
                    anchor_y: selection.anchor_y,
                },
            )?;
        }
    }

    Ok(base)
}

#[tauri::command]
fn overlay_diagnostics() -> OverlayDiagnostics {
    OverlayDiagnostics {
        platform: std::env::consts::OS.to_string(),
        native_overlay_available: cfg!(any(target_os = "macos", target_os = "windows")),
        helper_strategy: if cfg!(target_os = "macos") {
            "Swift/AppKit native helpers: modal selector plus persistent non-activating click-through map overlay"
                .to_string()
        } else if cfg!(target_os = "windows") {
            "C#/.NET WinForms native helper: modal selector plus persistent layered click-through map overlay"
                .to_string()
        } else {
            "Native overlay helper is not implemented for this platform"
                .to_string()
        },
        exclusive_fullscreen_supported: false,
        exclusive_fullscreen_note:
            "Exclusive fullscreen cannot be overlaid reliably. Use Borderless Window / Windowed Fullscreen."
                .to_string(),
    }
}

#[tauri::command]
fn show_map_overlay(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let data = build_map_overlay_data(&state)?;
    send_map_overlay_command(&app, &state, json!({ "type": "data", "data": data }))?;
    send_map_overlay_command(&app, &state, json!({ "type": "show" }))?;
    set_setting_locked(&state, "map_overlay_visible", "true")?;
    Ok(())
}

#[tauri::command]
fn hide_map_overlay(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    send_map_overlay_command(&app, &state, json!({ "type": "hide" }))?;
    set_setting_locked(&state, "map_overlay_visible", "false")?;
    Ok(())
}

#[tauri::command]
fn toggle_map_overlay(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let data = build_map_overlay_data(&state)?;
    send_map_overlay_command(&app, &state, json!({ "type": "data", "data": data }))?;
    send_map_overlay_command(&app, &state, json!({ "type": "toggle" }))?;
    Ok(())
}

#[tauri::command]
fn set_overlay_interactive(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<(), String> {
    send_map_overlay_command(
        &app,
        &state,
        json!({ "type": "interactive", "enabled": enabled }),
    )?;
    set_setting_locked(
        &state,
        "map_overlay_interactive",
        if enabled { "true" } else { "false" },
    )?;
    Ok(())
}

#[tauri::command]
fn update_overlay_data(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let data = build_map_overlay_data(&state)?;
    send_map_overlay_command(&app, &state, json!({ "type": "data", "data": data }))
}

#[tauri::command]
fn set_overlay_bounds(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    bounds: MapOverlayBounds,
) -> Result<MapOverlayBounds, String> {
    let bounds = sanitize_overlay_bounds(bounds);
    save_overlay_bounds(&state, &bounds)?;
    send_map_overlay_command(&app, &state, json!({ "type": "bounds", "bounds": bounds }))?;
    Ok(bounds)
}

#[tauri::command]
fn get_overlay_bounds(state: tauri::State<'_, AppState>) -> Result<MapOverlayBounds, String> {
    read_overlay_bounds(&state)
}

#[tauri::command]
fn reset_overlay_position(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<MapOverlayBounds, String> {
    let bounds = default_overlay_bounds();
    save_overlay_bounds(&state, &bounds)?;
    send_map_overlay_command(&app, &state, json!({ "type": "bounds", "bounds": bounds }))?;
    Ok(bounds)
}

#[tauri::command]
fn get_map_overlay_status(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<MapOverlayStatus, String> {
    ensure_map_overlay_running(&app, &state)?;
    let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
    let visible = get_setting(&conn, "map_overlay_visible")?
        .map(|value| value == "true")
        .unwrap_or(false);
    let interactive = get_setting(&conn, "map_overlay_interactive")?
        .map(|value| value == "true")
        .unwrap_or(false);
    drop(conn);
    Ok(MapOverlayStatus {
        visible,
        interactive,
        bounds: read_overlay_bounds(&state)?,
        hotkey: if cfg!(target_os = "windows") {
            "Alt+Shift+M".to_string()
        } else {
            "Cmd+Shift+M".to_string()
        },
        helper_running: true,
        exclusive_fullscreen_note:
            "Exclusive fullscreen cannot be overlaid reliably. Use Borderless Window / Windowed Fullscreen."
                .to_string(),
    })
}

#[tauri::command]
fn capture_current_location(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<CaptureOutcome, String> {
    eprintln!("[capture] current-location capture requested from UI");
    let outcome = capture_current_location_inner(&app, &state)?;
    refresh_map_overlay(&app, &state)?;
    Ok(outcome)
}

#[tauri::command]
fn capture_portal_destination(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<CaptureOutcome, String> {
    eprintln!("[capture] portal capture requested from UI");
    let outcome = capture_portal_destination_inner(&app, &state)?;
    refresh_map_overlay(&app, &state)?;
    trigger_sync_after_local_change(state.db_path.clone(), Arc::clone(&state.map_overlay));
    Ok(outcome)
}

fn run_native_overlay(app: &AppHandle, mode: &str) -> Result<OverlaySelection, String> {
    if !cfg!(any(target_os = "macos", target_os = "windows")) {
        return Err("Native overlay helper is implemented for macOS and Windows".to_string());
    }
    if mode != "region" && mode != "portal-size" && mode != "portal-strips" && mode != "diagnostic" {
        return Err("Unknown overlay mode".to_string());
    }

    let helper = ensure_native_overlay_helper(app)?;
    eprintln!("[overlay-helper] launching persistent map overlay {}", helper.display());
    eprintln!("[overlay-helper] launching {}", helper.display());
    let output = Command::new(helper)
        .arg("--mode")
        .arg(mode)
        .output()
        .map_err(|err| format!("Could not launch overlay helper: {err}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(if stderr.trim().is_empty() {
            "Overlay helper exited without a selection".to_string()
        } else {
            stderr.trim().to_string()
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json_line = stdout
        .lines()
        .last()
        .ok_or_else(|| "Overlay helper returned no data".to_string())?;
    serde_json::from_str::<OverlaySelection>(json_line)
        .map_err(|err| format!("Overlay helper returned invalid data: {err}"))
}

fn send_map_overlay_command(
    app: &AppHandle,
    state: &AppState,
    value: serde_json::Value,
) -> Result<(), String> {
    ensure_map_overlay_running(app, state)?;
    let mut overlay = state
        .map_overlay
        .lock()
        .map_err(|_| "Map overlay lock poisoned".to_string())?;
    let process = overlay
        .as_mut()
        .ok_or_else(|| "Map overlay helper is not running".to_string())?;
    let line = serde_json::to_string(&value).map_err(|err| err.to_string())?;
    process
        .stdin
        .write_all(line.as_bytes())
        .and_then(|_| process.stdin.write_all(b"\n"))
        .and_then(|_| process.stdin.flush())
        .map_err(|err| {
            *overlay = None;
            format!("Could not send command to map overlay helper: {err}")
        })
}

fn ensure_map_overlay_running(app: &AppHandle, state: &AppState) -> Result<(), String> {
    if !cfg!(any(target_os = "macos", target_os = "windows")) {
        return Err("The persistent map overlay helper is implemented for macOS and Windows".to_string());
    }

    {
        let mut overlay = state
            .map_overlay
            .lock()
            .map_err(|_| "Map overlay lock poisoned".to_string())?;
        if let Some(process) = overlay.as_mut() {
            if process.child.try_wait().map_err(|err| err.to_string())?.is_none() {
                return Ok(());
            }
            eprintln!("[overlay-helper] previous helper pid={} exited; starting a new one", process.child.id());
            *overlay = None;
        }
    }

    let helper = ensure_native_overlay_helper(app)?;
    let bounds_state_path = app
        .path()
        .app_data_dir()
        .map_err(|err| format!("Could not resolve app data directory: {err}"))?
        .join("map-overlay-bounds.json");
    kill_stale_windows_overlay_helpers();
    let mut command = Command::new(&helper);
    command
        .arg("--mode")
        .arg("map-overlay")
        .arg("--bounds-state")
        .arg(&bounds_state_path);
    if cfg!(target_os = "windows") {
        command.arg("--disable-hotkeys");
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("Could not launch map overlay helper: {err}"))?;
    eprintln!("[overlay-helper] persistent map overlay pid={}", child.id());
    if let Some(stderr) = child.stderr.take() {
        spawn_stderr_forwarder("overlay-helper", stderr);
    }
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Could not open map overlay stdin".to_string())?;
    if let Some(stdout) = child.stdout.take() {
        spawn_map_overlay_stdout_reader(
            stdout,
            state.db_path.clone(),
            helper.clone(),
            state.capture_dir.clone(),
            Arc::clone(&state.map_overlay),
            Arc::clone(&state.paddle_ocr),
        );
    }

    let mut overlay = state
        .map_overlay
        .lock()
        .map_err(|_| "Map overlay lock poisoned".to_string())?;
    *overlay = Some(MapOverlayProcess { child, stdin });
    drop(overlay);

    let bounds = read_overlay_bounds(state)?;
    send_map_overlay_command(app, state, json!({ "type": "bounds", "bounds": bounds }))?;
    let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
    let interactive = get_setting(&conn, "map_overlay_interactive")?
        .map(|value| value == "true")
        .unwrap_or(false);
    drop(conn);
    send_map_overlay_command(
        app,
        state,
        json!({ "type": "interactive", "enabled": interactive }),
    )?;

    let _ = send_hotkeys_to_overlay(app, state);

    match build_map_overlay_data(state) {
        Ok(data) => {
            send_map_overlay_command(app, state, json!({ "type": "data", "data": data }))?;
        }
        Err(err) => {
            eprintln!("[overlay-helper] could not build initial map data: {err}");
        }
    }

    Ok(())
}

fn kill_stale_windows_overlay_helpers() {
    if !cfg!(target_os = "windows") {
        return;
    }

    let output = Command::new("taskkill")
        .arg("/IM")
        .arg("AvalonOverlayHelper.exe")
        .arg("/F")
        .output();

    match output {
        Ok(output) if output.status.success() => {
            eprintln!("[overlay-helper] stopped stale AvalonOverlayHelper.exe processes");
        }
        Ok(_) => {}
        Err(err) => {
            eprintln!("[overlay-helper] could not run taskkill for stale helpers: {err}");
        }
    }
}

fn spawn_map_overlay_stdout_reader(
    stdout: std::process::ChildStdout,
    db_path: PathBuf,
    helper_path: PathBuf,
    capture_dir: PathBuf,
    overlay: Arc<Mutex<Option<MapOverlayProcess>>>,
    paddle_ocr: Arc<Mutex<Option<PaddleOcrProcess>>>,
) {
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let Some(event) = value.get("event").and_then(|event| event.as_str()) else {
                continue;
            };
            if let Ok(mut conn) = open_database(&db_path) {
                match event {
                    "set_shortcut_depth" => {
                        let value = value
                            .get("value")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(3)
                            .clamp(1, 6);

                        let _ = set_setting(&conn, "overlay_shortcut_depth", &value.to_string());

                        let Ok(data) = build_map_overlay_data_from_conn(&conn) else {
                            continue;
                        };

                        let _ = send_map_overlay_command_direct(
                            &overlay,
                            json!({ "type": "data", "data": data }),
                        );
                    }
                    "find_route" => {
                        let from = value
                            .get("from")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();

                        let to = value
                            .get("to")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();

                        eprintln!("[route] find_route event received: from={from:?}, to={to:?}");

                        let Ok(mut data) = build_map_overlay_data_from_conn(&conn) else {
                            eprintln!("[route] failed to build overlay data before route search");
                            continue;
                        };

                        match build_overlay_route(&conn, &from, &to) {
                            Ok((route_locations, route_edges)) => {
                                eprintln!(
                                    "[route] found: locations={}, edges={}, path={}",
                                    route_locations.len(),
                                    route_edges.len(),
                                    route_locations
                                        .iter()
                                        .map(|l| l.name.as_str())
                                        .collect::<Vec<_>>()
                                        .join(" -> ")
                                );

                                data.route_edges_count = route_edges.len() as i64;
                                data.route_expires_at = route_min_expires_at(&conn, &route_locations).ok().flatten();
                                data.route_locations = route_locations;
                                data.route_edges = route_edges;
                                data.last_capture_status = "Route found".to_string();

                                let _ = set_setting(&conn, "last_capture_status", "Route found");
                            }
                            Err(err) => {
                                eprintln!("[route] failed: {err}");

                                data.last_capture_status = format!("Route failed: {err}");
                                let _ = set_setting(&conn, "last_capture_status", &data.last_capture_status);
                            }
                        }

                        let result = send_map_overlay_command_direct(
                            &overlay,
                            json!({ "type": "data", "data": data }),
                        );

                        if let Err(err) = result {
                            eprintln!("[route] failed to send route result to overlay: {err}");
                        } else {
                            eprintln!("[route] route result sent to overlay");
                        }
                    }

                    "clear_route" => {
                        let Ok(mut data) = build_map_overlay_data_from_conn(&conn) else {
                            continue;
                        };

                        data.route_locations = Vec::new();
                        data.route_edges = Vec::new();

                        let _ = send_map_overlay_command_direct(
                            &overlay,
                            json!({ "type": "data", "data": data }),
                        );
                    }
                    "bounds" => {
                        let Some(bounds_value) = value.get("bounds") else {
                            continue;
                        };
                        let Ok(bounds) =
                            serde_json::from_value::<MapOverlayBounds>(bounds_value.clone())
                        else {
                            continue;
                        };
                        let _ = set_setting(
                            &conn,
                            "map_overlay_bounds",
                            &serde_json::to_string(&sanitize_overlay_bounds(bounds))
                                .unwrap_or_default(),
                        );
                    }
                    "undo_last_action" => {
                        if let Ok(conn) = open_database(&db_path) {
                            let _ = undo_last_graph_action_inner(&conn);
                            if let Ok(data) = build_map_overlay_data_from_conn(&conn) {
                                let _ = send_map_overlay_command_direct(&overlay, json!({ "type": "data", "data": data }));
                            }
                        }
                    }
                    "delete_edge" => {
                        let Some(edge_id) = value.get("edge_id").and_then(|edge_id| edge_id.as_i64()) else {
                            continue;
                        };

                        let Ok(edge) = load_edge_by_id(&conn, edge_id) else {
                            eprintln!("[delete-edge] edge id {edge_id} not found");
                            continue;
                        };

                        if let Err(err) = delete_edge_by_id(&mut conn, edge_id) {
                            eprintln!("[delete-edge] local delete failed: {err}");
                            continue;
                        }

                        if let Ok(sync_settings) = read_sync_settings(&conn) {
                            if sync_settings.enabled && !sync_settings.server_url.trim().is_empty() {
                                let delete_url = match sync_url(&sync_settings.server_url, "edges") {
                                    Ok(url) => url,
                                    Err(err) => {
                                        eprintln!("[delete-edge] invalid sync url: {err}");
                                        String::new()
                                    }
                                };

                                if !delete_url.is_empty() {
                                    let from_normalized = normalize_location_name(&edge.from_location_name);
                                    let to_normalized = normalize_location_name(&edge.to_location_name);
                                    let (from_normalized, to_normalized) =
                                        canonicalize_normalized_edge_pair(&from_normalized, &to_normalized);
                                    let payload = json!({
                                        "action": "delete",
                                        "from_name": edge.from_location_name,
                                        "to_name": edge.to_location_name,
                                        "from_normalized": from_normalized,
                                        "to_normalized": to_normalized,
                                        "source": "deleted",
                                    });
                                    let body = payload.to_string();
                                    if let Err(err) = http_json_request(
                                        "DELETE",
                                        &delete_url,
                                        Some(&body),
                                        Some(&sync_settings.write_token),
                                    ) {
                                        if err.contains("HTTP 501")
                                            || err.contains("HTTP 405")
                                            || err.contains("Unsupported method")
                                        {
                                            eprintln!(
                                                "[delete-edge] sync DELETE unsupported, retrying delete via POST"
                                            );
                                            if let Err(post_err) = http_json_request(
                                                "POST",
                                                &delete_url,
                                                Some(&body),
                                                Some(&sync_settings.write_token),
                                            ) {
                                                eprintln!("[delete-edge] sync delete POST fallback failed: {post_err}");
                                            }
                                        } else {
                                            eprintln!("[delete-edge] sync delete failed: {err}");
                                        }
                                    }
                                }
                            }
                        }

                        if let Err(err) = recompute_graph_layout(&conn) {
                            eprintln!("[delete-edge] layout recompute failed: {err}");
                        }
                        if let Ok(data) = build_map_overlay_data_from_conn(&conn) {
                            let _ = send_map_overlay_command_direct(
                                &overlay,
                                json!({ "type": "data", "data": data }),
                            );
                        }
                    }
                    "visible" => {
                        if let Some(visible) = value.get("visible").and_then(|visible| visible.as_bool()) {
                            let _ = set_setting(
                                &conn,
                                "map_overlay_visible",
                                if visible { "true" } else { "false" },
                            );
                        }
                    }
                    "interactive" => {
                        if let Some(enabled) = value.get("enabled").and_then(|enabled| enabled.as_bool()) {
                            let _ = set_setting(
                                &conn,
                                "map_overlay_interactive",
                                if enabled { "true" } else { "false" },
                            );
                        }
                    }
                    "hotkey_error" => {
                        eprintln!("[overlay-hotkey] registration failed: {value}");
                    }
                    "capture_current" => {
                        spawn_hotkey_capture(
                            db_path.clone(),
                            helper_path.clone(),
                            capture_dir.clone(),
                            Arc::clone(&overlay),
                            Arc::clone(&paddle_ocr),
                            "current_location",
                        );
                    }
                    "capture_portal" => {
                        spawn_hotkey_capture(
                            db_path.clone(),
                            helper_path.clone(),
                            capture_dir.clone(),
                            Arc::clone(&overlay),
                            Arc::clone(&paddle_ocr),
                            "portal",
                        );
                    }
                    _ => {}
                }
            }
        }
    });
}

fn build_map_overlay_data(state: &AppState) -> Result<MapOverlayData, String> {
    let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
    build_map_overlay_data_from_conn(&conn)
}

fn build_map_overlay_data_from_conn(conn: &Connection) -> Result<MapOverlayData, String> {
    delete_expired_edges(conn)?;

//     debug_avalon_raw_components("Xilos-Osayam");
//     debug_avalon_raw_components("Oiritos-Eramtum");
    let locations = load_locations(conn)?;
    let edges = load_edges(conn)?;
    let (bridge_locations, bridge_edges) = build_overlay_shortcuts(conn, &locations)?;
    let known_locations_count = locations.len() as i64;
    let known_edges_count = edges.len() as i64;
    Ok(MapOverlayData {
        route_expires_at: None,
        route_edges_count: 0,
        route_locations: Vec::new(),
        route_edges: Vec::new(),
        bridge_locations,
        bridge_edges,
        current_location: get_setting(conn, "current_location_name")?,
        last_portal_expires_in_seconds: get_setting(conn, "last_portal_expires_in_seconds")?
            .and_then(|value| value.parse::<i64>().ok()),
        last_portal_destination: get_setting(conn, "last_portal_destination")?,
        locations,
        edges,
        last_capture_status: get_setting(&conn, "last_capture_status")?
            .unwrap_or_else(|| "No captures yet".to_string()),
        capture_mode: "macOS CGWindowList capture".to_string(),
        ocr_mode: "ocr:auto".to_string(),
        db_status: "SQLite connected".to_string(),
        known_locations_count,
        known_edges_count,
    })
}

fn refresh_map_overlay(app: &AppHandle, state: &AppState) -> Result<(), String> {
    let data = build_map_overlay_data(state)?;
    send_map_overlay_command(app, state, json!({ "type": "data", "data": data }))
}

fn read_overlay_bounds(state: &AppState) -> Result<MapOverlayBounds, String> {
    let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
    let raw = get_setting(&conn, "map_overlay_bounds")?;
    Ok(raw
        .and_then(|value| serde_json::from_str::<MapOverlayBounds>(&value).ok())
        .map(sanitize_overlay_bounds)
        .unwrap_or_else(default_overlay_bounds))
}

fn route_min_expires_at(
    conn: &Connection,
    route_locations: &[RouteOverlayLocation],
) -> Result<Option<String>, String> {
    let mut min_expires: Option<chrono::DateTime<Utc>> = None;

    for pair in route_locations.windows(2) {
        let a = &pair[0];
        let b = &pair[1];

        eprintln!(
            "[route-copy-debug] checking edge: {}({}) -> {}({})",
            a.name, a.normalized_name, b.name, b.normalized_name
        );

        let expires_at = conn
            .query_row(
                r#"
                SELECT e.expires_at
                FROM edges e
                JOIN locations lf ON lf.id = e.from_location_id
                JOIN locations lt ON lt.id = e.to_location_id
                WHERE e.status NOT IN ('expired', 'deleted')
                  AND (
                    (lf.normalized_name = ?1 AND lt.normalized_name = ?2)
                    OR (lf.normalized_name = ?2 AND lt.normalized_name = ?1)
                  )
                LIMIT 1
                "#,
                params![a.normalized_name, b.normalized_name],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(db_err)?
            .flatten();

        eprintln!("[route-copy-debug] expires_at={expires_at:?}");

        let Some(raw) = expires_at else {
            continue;
        };

        let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(&raw) else {
            eprintln!("[route-copy-debug] invalid expires_at={raw:?}");
            continue;
        };

        let parsed = parsed.with_timezone(&Utc);

        min_expires = Some(match min_expires {
            Some(current) => current.min(parsed),
            None => parsed,
        });
    }

    let result = min_expires.map(|dt| dt.to_rfc3339());
    eprintln!("[route-copy-debug] final route_expires_at={result:?}");

    Ok(result)
}

fn save_overlay_bounds(state: &AppState, bounds: &MapOverlayBounds) -> Result<(), String> {
    let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
    set_setting(
        &conn,
        "map_overlay_bounds",
        &serde_json::to_string(bounds).map_err(|err| err.to_string())?,
    )
}

fn set_setting_locked(state: &AppState, key: &str, value: &str) -> Result<(), String> {
    let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
    set_setting(&conn, key, value)
}

fn default_overlay_bounds() -> MapOverlayBounds {
    MapOverlayBounds {
        x: 80,
        y: 120,
        width: 360,
        height: 300,
    }
}

fn sanitize_overlay_bounds(bounds: MapOverlayBounds) -> MapOverlayBounds {
    MapOverlayBounds {
        x: bounds.x,
        y: bounds.y,
        width: bounds.width.clamp(260, 800),
        height: bounds.height.clamp(220, 700),
    }
}

fn build_overlay_shortcuts(
    conn: &Connection,
    visible_locations: &[Location],
) -> Result<(Vec<RouteOverlayLocation>, Vec<RouteOverlayEdge>), String> {
    if visible_locations.len() < 2 {
        return Ok((Vec::new(), Vec::new()));
    }
    let max_edges = get_setting(conn, "overlay_shortcut_depth")?
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(3)
        .clamp(1, 6);
    let graph = build_route_graph(conn)?;

    let mut visible_by_norm = std::collections::HashMap::<String, &Location>::new();

    for location in visible_locations {
        visible_by_norm.insert(location.normalized_name.clone(), location);
    }

    let visible = visible_locations
        .iter()
        .filter(|location| {
            location.zone_type == "avalon"
                && graph.contains_key(&location.normalized_name)
        })
        .collect::<Vec<_>>();

    let mut hidden_id_by_norm = std::collections::HashMap::<String, i64>::new();
    let mut bridge_locations_by_id = std::collections::HashMap::<i64, RouteOverlayLocation>::new();
    let mut bridge_edges_by_key = std::collections::HashMap::<(i64, i64), RouteOverlayEdge>::new();

    let mut next_hidden_id = -10_000_i64;
    let mut next_edge_id = -20_000_i64;

    for i in 0..visible.len() {
        for j in (i + 1)..visible.len() {
            let a = visible[i];
            let b = visible[j];

            let Some(path) = shortest_path_bfs_limited(
                &graph,
                &a.normalized_name,
                &b.normalized_name,
                max_edges,
            )else {
                continue;
            };

            if path.len() < 3 || path.len() > max_edges + 1 {
                continue;
            }

            let ax = a.x.unwrap_or(0.0);
            let ay = a.y.unwrap_or(0.0);
            let bx = b.x.unwrap_or(ax + 160.0);
            let by = b.y.unwrap_or(ay);

            let mut ids = Vec::<i64>::new();

            for (path_index, normalized) in path.iter().enumerate() {
                if let Some(existing) = visible_by_norm.get(normalized) {
                    ids.push(existing.id);
                    continue;
                }

                let id = if let Some(id) = hidden_id_by_norm.get(normalized) {
                    *id
                } else {
                    let id = next_hidden_id;
                    next_hidden_id -= 1;
                    hidden_id_by_norm.insert(normalized.clone(), id);

                    let t = path_index as f64 / (path.len().saturating_sub(1).max(1) as f64);
                    let x = ax + (bx - ax) * t;
                    let y = ay + (by - ay) * t;

                    let name = resolve_route_location_name(conn, normalized)
                        .unwrap_or_else(|_| title_case_location_name(normalized));

                    let zone_type = infer_zone_type_from_name(&name);

                    bridge_locations_by_id.insert(
                        id,
                        RouteOverlayLocation {
                            id,
                            name,
                            normalized_name: normalized.clone(),
                            zone_type,
                            x: Some(x),
                            y: Some(y),
                        },
                    );

                    id
                };

                ids.push(id);
            }

            for k in 0..ids.len().saturating_sub(1) {
                let from_id = ids[k];
                let to_id = ids[k + 1];

                let key = if from_id <= to_id {
                    (from_id, to_id)
                } else {
                    (to_id, from_id)
                };

                if bridge_edges_by_key.contains_key(&key) {
                    continue;
                }

                let from_name = overlay_name_for_bridge_id(
                    conn,
                    from_id,
                    &visible_by_norm,
                    &bridge_locations_by_id,
                );

                let to_name = overlay_name_for_bridge_id(
                    conn,
                    to_id,
                    &visible_by_norm,
                    &bridge_locations_by_id,
                );

                bridge_edges_by_key.insert(
                    key,
                    RouteOverlayEdge {
                        id: next_edge_id,
                        from_location_id: from_id,
                        to_location_id: to_id,
                        from_location_name: from_name,
                        to_location_name: to_name,
                        source: "bridge_shortcut".to_string(),
                    },
                );

                next_edge_id -= 1;
            }
        }
    }

    let mut bridge_locations = bridge_locations_by_id
        .into_values()
        .collect::<Vec<_>>();

    bridge_locations.sort_by(|a, b| a.id.cmp(&b.id));

    let mut bridge_edges = bridge_edges_by_key
        .into_values()
        .collect::<Vec<_>>();

    bridge_edges.sort_by(|a, b| a.id.cmp(&b.id));

    Ok((bridge_locations, bridge_edges))
}

fn shortest_path_bfs_limited(
    graph: &std::collections::HashMap<String, Vec<String>>,
    from: &str,
    to: &str,
    max_edges: usize,
) -> Option<Vec<String>> {
    if from == to {
        return Some(vec![from.to_string()]);
    }

    let mut queue = std::collections::VecDeque::<(String, usize)>::new();
    let mut visited = std::collections::HashSet::<String>::new();
    let mut parent = std::collections::HashMap::<String, String>::new();

    visited.insert(from.to_string());
    queue.push_back((from.to_string(), 0));

    while let Some((current, depth)) = queue.pop_front() {
        if depth >= max_edges {
            continue;
        }

        let Some(neighbors) = graph.get(&current) else {
            continue;
        };

        for neighbor in neighbors {
            if visited.contains(neighbor) {
                continue;
            }

            visited.insert(neighbor.clone());
            parent.insert(neighbor.clone(), current.clone());

            if neighbor == to {
                let mut path = vec![to.to_string()];
                let mut cursor = to.to_string();

                while let Some(prev) = parent.get(&cursor) {
                    path.push(prev.clone());
                    cursor = prev.clone();

                    if cursor == from {
                        break;
                    }
                }

                path.reverse();
                return Some(path);
            }

            queue.push_back((neighbor.clone(), depth + 1));
        }
    }

    None
}

// fn debug_avalon_raw_components(name: &str) {
//     let normalized_target = normalize_location_name(name);
//
//     let path = Path::new(env!("CARGO_MANIFEST_DIR"))
//         .parent()
//         .expect("Could not resolve project root")
//         .join("data/albion_navigator_import.json");
//
//     let Ok(text) = fs::read_to_string(&path) else {
//         eprintln!("[avalon-debug] cannot read {}", path.display());
//         return;
//     };
//
//     let Ok(root) = serde_json::from_str::<serde_json::Value>(&text) else {
//         eprintln!("[avalon-debug] invalid json");
//         return;
//     };
//
//     let records = root
//         .get("avalon_locations")
//         .or_else(|| root.get("avalon"))
//         .or_else(|| root.get("avalon_components"))
//         .or_else(|| root.get("avalon_component_records"))
//         .and_then(|v| v.as_array())
//         .cloned()
//         .unwrap_or_default();
//
//     for record in records {
//         let name = record.get("name").and_then(|v| v.as_str()).unwrap_or("");
//         let normalized = record
//             .get("normalized_name")
//             .and_then(|v| v.as_str())
//             .map(|v| v.to_string())
//             .unwrap_or_else(|| normalize_location_name(name));
//
//         if normalized != normalized_target {
//             continue;
//         }
//
//         eprintln!("[avalon-debug] LOCATION: {name} / {normalized}");
//
//         if let Some(components) = record.get("components").and_then(|v| v.as_array()) {
//             for component in components {
//                 eprintln!("[avalon-debug] component = {}", component);
//             }
//         }
//
//         return;
//     }
//
//     eprintln!("[avalon-debug] not found: {name} / {normalized_target}");
// }

fn overlay_name_for_bridge_id(
    _conn: &Connection,
    id: i64,
    visible_by_norm: &std::collections::HashMap<String, &Location>,
    hidden_by_id: &std::collections::HashMap<i64, RouteOverlayLocation>,
) -> String {
    if let Some(hidden) = hidden_by_id.get(&id) {
        return hidden.name.clone();
    }

    visible_by_norm
        .values()
        .find(|location| location.id == id)
        .map(|location| location.name.clone())
        .unwrap_or_else(|| format!("node:{id}"))
}

fn ensure_native_overlay_helper(app: &AppHandle) -> Result<PathBuf, String> {
    if cfg!(target_os = "macos") {
        return ensure_macos_overlay_helper(app);
    }
    if cfg!(target_os = "windows") {
        return ensure_windows_overlay_helper(app);
    }
    Err("Native overlay helper is not implemented for this platform".to_string())
}

fn ensure_macos_overlay_helper(app: &AppHandle) -> Result<PathBuf, String> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| "Could not resolve project root".to_string())?
        .join("native/macos/AvalonOverlayHelper.swift");
    if !source.exists() {
        return Err(format!("Missing overlay helper source: {}", source.display()));
    }

    let helper_dir = app
        .path()
        .app_cache_dir()
        .map_err(|err| format!("Could not resolve app cache directory: {err}"))?
        .join("helpers");
    fs::create_dir_all(&helper_dir)
        .map_err(|err| format!("Could not create helper cache directory: {err}"))?;
    let helper = helper_dir.join("avalon-overlay-helper");

    let needs_build = match (fs::metadata(&helper), fs::metadata(&source)) {
        (Ok(helper_meta), Ok(source_meta)) => {
            let helper_modified = helper_meta.modified().ok();
            let source_modified = source_meta.modified().ok();
            helper_modified < source_modified
        }
        _ => true,
    };

    if needs_build {
        let output = Command::new("swiftc")
            .arg(&source)
            .arg("-o")
            .arg(&helper)
            .output()
            .map_err(|err| format!("Could not compile overlay helper with swiftc: {err}"))?;
        if !output.status.success() {
            return Err(format!(
                "Overlay helper failed to compile: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }

    Ok(helper)
}

fn ensure_windows_overlay_helper(app: &AppHandle) -> Result<PathBuf, String> {
    let exe_name = "AvalonOverlayHelper.exe";
    let project_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| "Could not resolve project root".to_string())?;

    let mut candidates = Vec::new();

    if let Ok(resource_dir) = app.path().resource_dir() {
        candidates.push(resource_dir.join(exe_name));
        candidates.push(resource_dir.join("AvalonOverlayHelper").join(exe_name));
        candidates.push(resource_dir.join("native").join("windows").join(exe_name));
    }

    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(dir) = current_exe.parent() {
            candidates.push(dir.join(exe_name));
            candidates.push(dir.join("resources").join(exe_name));
        }
    }

    let publish_rids = if cfg!(target_arch = "aarch64") {
        ["win-arm64", "win-x64"]
    } else {
        ["win-x64", "win-arm64"]
    };
    for rid in publish_rids {
        candidates.push(
            project_root
                .join("native")
                .join("windows")
                .join("AvalonOverlayHelper")
                .join("bin")
                .join("Release")
                .join("net8.0-windows")
                .join(rid)
                .join("publish")
                .join(exe_name),
        );
    }
    candidates.push(
        project_root
            .join("native")
            .join("windows")
            .join("AvalonOverlayHelper")
            .join(exe_name),
    );

    for candidate in &candidates {
        if candidate.exists() {
            return Ok(candidate.clone());
        }
    }

    Err(format!(
        "Missing Windows overlay helper. Build it with `dotnet publish -c Release -r win-x64 --self-contained` on x64 Windows or `dotnet publish -c Release -r win-arm64 --self-contained` on ARM64 Windows, then copy AvalonOverlayHelper.exe into a Tauri resource path. Checked: {}",
        candidates
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

fn capture_current_location_inner(app: &AppHandle, state: &AppState) -> Result<CaptureOutcome, String> {
    let total_started = Instant::now();
    let region_started = Instant::now();
    eprintln!("[capture-timing] kind=current phase=load_region start");
    let region = {
        let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
        load_region_by_key(&conn, "current_location")?
            .ok_or_else(|| "Select the current-location region before capturing".to_string())?
    };
    eprintln!(
        "[capture-timing] kind=current phase=load_region ms={} x={} y={} width={} height={}",
        region_started.elapsed().as_millis(), region.x, region.y, region.width, region.height
    );
    let started = Instant::now();
    let ocr = run_capture_ocr(app, state, "current", &region, false, None)?;
    let capture_ms = started.elapsed().as_millis() as i64;
    eprintln!("[capture-timing] kind=current phase=capture_ocr ms={capture_ms}");
    let mut conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
    let apply_started = Instant::now();
    let outcome = apply_current_location_capture(&mut conn, &ocr, capture_ms);
    eprintln!(
        "[capture-timing] kind=current phase=apply_result ms={} total_ms={}",
        apply_started.elapsed().as_millis(),
        total_started.elapsed().as_millis()
    );
    outcome
}

fn capture_portal_destination_inner(app: &AppHandle, state: &AppState) -> Result<CaptureOutcome, String> {
    let total_started = Instant::now();
    let region_started = Instant::now();
    eprintln!("[capture-timing] kind=portal phase=load_region start");
    let (region, strip_regions) = {
        let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
        let region = load_region_by_key(&conn, "portal_tooltip")?
            .ok_or_else(|| "Configure the portal tooltip box before capturing".to_string())?;
        let destination = load_region_by_key(&conn, "portal_tooltip_destination")?;
        let timer = load_region_by_key(&conn, "portal_tooltip_timer")?;
        let strip_regions = destination.zip(timer);
        (region, strip_regions)
    };
    eprintln!(
        "[capture-timing] kind=portal phase=load_region ms={} x={} y={} width={} height={} anchor=({:?},{:?})",
        region_started.elapsed().as_millis(), region.x, region.y, region.width, region.height, region.anchor_x, region.anchor_y
    );
    let started = Instant::now();
    let ocr = if let Some((destination_region, timer_region)) = strip_regions {
        run_portal_strip_capture_ocr(app, state, &destination_region, &timer_region)?
    } else {
        let portal_anchor = region.anchor_x.zip(region.anchor_y);
        let center_cursor = portal_anchor.is_none();
        run_capture_ocr(app, state, "portal", &region, center_cursor, portal_anchor)?
    };
    let capture_ms = started.elapsed().as_millis() as i64;
    eprintln!("[capture-timing] kind=portal phase=capture_ocr ms={capture_ms}");
    let mut conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
    let apply_started = Instant::now();
    let outcome = apply_portal_capture(&mut conn, &ocr, capture_ms);
    eprintln!(
        "[capture-timing] kind=portal phase=apply_result ms={} total_ms={}",
        apply_started.elapsed().as_millis(),
        total_started.elapsed().as_millis()
    );
    outcome
}

fn run_portal_strip_capture_ocr(
    app: &AppHandle,
    state: &AppState,
    destination_region: &Region,
    timer_region: &Region,
) -> Result<CaptureOcrResult, String> {
    eprintln!(
        "[capture-timing] kind=portal phase=strip_regions destination={}x{} timer={}x{}",
        destination_region.width, destination_region.height, timer_region.width, timer_region.height
    );
    let destination = run_capture_ocr(
        app,
        state,
        "portal",
        destination_region,
        false,
        None,
    )?;
    let timer = run_capture_ocr(
        app,
        state,
        "portal",
        timer_region,
        false,
        None,
    )?;
    Ok(combine_portal_strip_ocr(destination, timer))
}

fn combine_portal_strip_ocr(destination: CaptureOcrResult, timer: CaptureOcrResult) -> CaptureOcrResult {
    let text = format!(
        "Road of Avalon to\n{}\nCloses in {}",
        destination.text.trim(),
        timer.text.trim()
    );
    let mut lines = destination.lines;
    lines.extend(timer.lines);
    CaptureOcrResult {
        text,
        confidence: destination.confidence.or(timer.confidence),
        engine: format!("{}/strip", destination.engine),
        image_path: format!("{}|{}", destination.image_path, timer.image_path),
        width: destination.width.max(timer.width),
        height: destination.height + timer.height,
        duration_ms: destination.duration_ms + timer.duration_ms,
        screen_recording_permission: destination.screen_recording_permission && timer.screen_recording_permission,
        lines,
    }
}

fn run_capture_ocr(
    app: &AppHandle,
    state: &AppState,
    kind: &str,
    region: &Region,
    center_cursor: bool,
    portal_anchor: Option<(f64, f64)>,
) -> Result<CaptureOcrResult, String> {
    let total_started = Instant::now();
    let helper_resolve_started = Instant::now();
    let helper = ensure_native_overlay_helper(app)?;
    eprintln!(
        "[capture-timing] kind={kind} phase=resolve_helper ms={} helper={}",
        helper_resolve_started.elapsed().as_millis(),
        helper.display()
    );
    fs::create_dir_all(&state.capture_dir)
        .map_err(|err| format!("Could not create capture directory: {err}"))?;
    let mut command = Command::new(helper);
    command
        .arg("--mode")
        .arg("capture-ocr")
        .arg("--kind")
        .arg(kind)
        .arg("--width")
        .arg(region.width.to_string())
        .arg("--height")
        .arg(region.height.to_string())
        .arg("--output-dir")
        .arg(&state.capture_dir);
    if center_cursor {
        command.arg("--center-cursor");
    } else {
        command
            .arg("--x")
            .arg(region.x.to_string())
            .arg("--y")
            .arg(region.y.to_string());
    }
    if let Some((anchor_x, anchor_y)) = portal_anchor {
        command
            .arg("--portal-anchor-x")
            .arg(anchor_x.to_string())
            .arg("--portal-anchor-y")
            .arg(anchor_y.to_string());
        command
            .arg("--portal-x")
            .arg(region.x.to_string())
            .arg("--portal-y")
            .arg(region.y.to_string())
            .arg("--portal-width")
            .arg(region.width.to_string())
            .arg("--portal-height")
            .arg(region.height.to_string());
    }
    if let Some(display_id) = &region.display_id {
        command.arg("--display-id").arg(display_id);
    }
    let helper_started = Instant::now();
    let output = command
        .output()
        .map_err(|err| format!("Could not run capture helper: {err}"))?;
    let helper_ms = helper_started.elapsed().as_millis();
    forward_child_stderr("capture-helper", &output.stderr);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(if stderr.trim().is_empty() {
            "Capture helper failed".to_string()
        } else {
            stderr.trim().to_string()
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json_line = stdout
        .lines()
        .last()
        .ok_or_else(|| "Capture helper returned no data".to_string())?;
    let result = serde_json::from_str::<CaptureOcrResult>(json_line)
        .map_err(|err| format!("Capture helper returned invalid data: {err}"))?;
    eprintln!(
        "[capture-timing] kind={kind} phase=helper_process ms={helper_ms} helper_capture_ms={} image={} size={}x{}",
        result.duration_ms, result.image_path, result.width, result.height
    );
    if !result.screen_recording_permission {
        return Err(
            "macOS Screen Recording permission is missing. Enable it for Avalon Mapper OCR in System Settings > Privacy & Security > Screen Recording, then restart the app."
                .to_string(),
        );
    }
    let ocr_started = Instant::now();
    let paddle = run_paddle_ocr(&state.paddle_ocr, &result.image_path, kind)?;
    let ocr_ms = ocr_started.elapsed().as_millis();
    eprintln!(
        "[capture-timing] kind={kind} phase=ocr_worker ms={ocr_ms} total_ms={} engine={} confidence={:?}",
        total_started.elapsed().as_millis(),
        paddle.engine,
        paddle.confidence
    );
    eprintln!(
        "[capture] kind={kind} image={} size={}x{} capture_ms={} ocr_engine={} ocr_confidence={:?}",
        result.image_path,
        result.width,
        result.height,
        result.duration_ms,
        paddle.engine,
        paddle.confidence
    );
    Ok(CaptureOcrResult {
        text: paddle.text,
        confidence: paddle.confidence,
        engine: paddle.engine,
        image_path: result.image_path,
        width: result.width,
        height: result.height,
        duration_ms: result.duration_ms,
        screen_recording_permission: result.screen_recording_permission,
        lines: paddle.lines,
    })
}

fn apply_current_location_capture(
    conn: &mut Connection,
    ocr: &CaptureOcrResult,
    measured_ms: i64,
) -> Result<CaptureOutcome, String> {
    let known = known_location_names(conn)?;
    let parsed = parse_current_location_ocr(&ocr.text, &ocr.lines, &known);
    let parsed_name = parsed.location_name.clone().ok_or_else(|| {
        format!(
            "Could not parse current location: {} | raw OCR: {:?} | lines: {:?}",
            parsed.reason,
            ocr.text,
            ocr.lines
                .iter()
                .map(|line| line.text.clone())
                .collect::<Vec<_>>()
        )
    })?;
    let normalized = normalize_location_name(&parsed_name);
    let metadata = serde_json::to_string(&json!({
        "parser": "current_location_v1",
        "location_name": parsed.location_name,
        "cleaned_candidate": parsed.cleaned_candidate,
        "matched_location_name": parsed.matched_location_name,
        "matched_location_score": parsed.matched_location_score,
        "used_dictionary_match": parsed.used_dictionary_match,
        "match_reason": parsed.match_reason,
        "confidence": parsed.confidence,
        "candidates": parsed.candidates,
        "ignored_lines": parsed.ignored_lines,
        "top_matches": parsed.top_matches,
        "reason": parsed.reason,
        "ocr_lines": ocr.lines
    }))
    .map_err(|err| err.to_string())?;
    let tx = conn.transaction().map_err(db_err)?;

    let zone_type = infer_zone_type_from_name(parsed_name.trim());
    let location_id = upsert_location(&tx, &parsed_name, &normalized, &zone_type, true)?;
    insert_observation(
        &tx,
        "current_location",
        Some(&parsed_name),
        None,
        &ocr.text,
        Some(&normalized),
        ocr.confidence,
        Some(&ocr.image_path),
        Some(&metadata),
    )?;
    set_setting(&tx, "current_location_id", &location_id.to_string())?;
    if let Some(previous_location_name) = get_setting(&tx, "current_location_name")? {
        let previous_norm = normalize_location_name(&previous_location_name);
        let current_norm = normalize_location_name(&parsed_name);

        if previous_norm != current_norm {
            tx.execute(
                r#"
                UPDATE edges
                SET source = 'traversed',
                    status = 'active',
                    last_seen_at = ?1
                WHERE (
                    from_location_id = (SELECT id FROM locations WHERE normalized_name = ?2)
                    AND to_location_id = (SELECT id FROM locations WHERE normalized_name = ?3)
                )
                OR (
                    from_location_id = (SELECT id FROM locations WHERE normalized_name = ?3)
                    AND to_location_id = (SELECT id FROM locations WHERE normalized_name = ?2)
                )
                "#,
                params![now(), previous_norm, current_norm],
            )
            .map_err(db_err)?;
        }
    }
    set_setting(&tx, "current_location_name", &parsed_name)?;
    set_setting(
        &tx,
        "last_capture_status",
        &format!(
            "Current location OCR: {} ({:.0}%) | {}",
            parsed_name,
            parsed.confidence * 100.0,
            parsed.match_reason
        ),
    )?;
    tx.commit().map_err(db_err)?;
    Ok(CaptureOutcome {
        raw_ocr_text: ocr.text.clone(),
        normalized_text: normalized,
        matched_name: parsed_name,
        match_confidence: parsed.confidence,
        ocr_confidence: ocr.confidence,
        parsed_current: Some(parsed),
        parsed_portal: None,
        image_path: ocr.image_path.clone(),
        duration_ms: measured_ms.max(ocr.duration_ms),
        engine: ocr.engine.clone(),
    })
}


#[tauri::command]
fn mark_edge_traversed(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    from_location: String,
    to_location: String,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;

    conn.execute(
        r#"
        UPDATE edges
        SET source = 'traversed',
            status = 'active',
            last_seen_at = ?1
        WHERE from_location_id = (
            SELECT id FROM locations WHERE normalized_name = ?2
        )
        AND to_location_id = (
            SELECT id FROM locations WHERE normalized_name = ?3
        )
        "#,
        params![
            now(),
            normalize_location_name(&from_location),
            normalize_location_name(&to_location),
        ],
    )
    .map_err(db_err)?;

    drop(conn);
    refresh_map_overlay(&app, &state)?;
    Ok(())
}

fn apply_portal_capture(
    conn: &mut Connection,
    ocr: &CaptureOcrResult,
    measured_ms: i64,
) -> Result<CaptureOutcome, String> {
    let current_location_id = get_setting(conn, "current_location_id")?
        .ok_or_else(|| "Set the current location before capturing a portal".to_string())?
        .parse::<i64>()
        .map_err(|_| "Stored current location id is invalid".to_string())?;
    let current_location_name =
        get_setting(conn, "current_location_name")?.unwrap_or_else(|| "Unknown current location".to_string());
    let known = known_location_names(conn)?;
    let parsed = parse_portal_tooltip_ocr(&ocr.text, &ocr.lines, &known);
    let parsed_name = parsed
        .destination_name
        .clone()
        .ok_or_else(|| format!("Could not parse portal destination: {}", parsed.reason))?;
    let match_result = match_location_name(conn, &ocr.text, &parsed_name, true)?;
    let location_match = match_result.chosen.clone();
    let normalized = normalize_location_name(&location_match.name);
    let metadata = serde_json::to_string(&json!({
        "parser": "portal_tooltip_v1",
        "destination_name": parsed.destination_name,
        "slots_used": parsed.slots_used,
        "slots_total": parsed.slots_total,
        "expires_in_seconds": parsed.expires_in_seconds,
        "confidence": parsed.confidence,
        "candidates": parsed.candidates,
        "ignored_lines": parsed.ignored_lines,
        "reason": parsed.reason,
        "match": {
            "cleaned_candidate": match_result.cleaned_candidate,
            "top5": match_result.top5,
            "score": match_result.chosen.score,
            "chosen": match_result.chosen.name,
        },
        "ocr_lines": ocr.lines
    }))
    .map_err(|err| err.to_string())?;
    let tx = conn.transaction().map_err(db_err)?;

    let zone_type = infer_zone_type_from_name(&location_match.name);
    if !allowed_graph_zone_type(&zone_type) {
        return Err(format!(
            "Ignored non-graph portal destination: {} ({})",
            location_match.name, zone_type
        ));
    }
    let destination_id = upsert_location(&tx, &location_match.name, &normalized, &zone_type, false)?;

    let edge_id = upsert_edge(
        &tx,
        current_location_id,
        destination_id,
        parsed.expires_in_seconds,
    )?;
    recompute_graph_layout(&tx)?;
    insert_observation(
        &tx,
        "portal",
        Some(&current_location_name),
        Some(&location_match.name),
        &ocr.text,
        Some(&normalized),
        ocr.confidence,
        Some(&ocr.image_path),
        Some(&metadata),
    )?;
    set_setting(&tx, "last_portal_destination", &location_match.name)?;
    if let Some(seconds) = parsed.expires_in_seconds {
        set_setting(&tx, "last_portal_expires_in_seconds", &seconds.to_string())?;
    }
    set_setting(
        &tx,
        "last_capture_status",
        &format!("Portal OCR: {} ({:.0}%)", location_match.name, location_match.score * 100.0),
    )?;
    tx.commit().map_err(db_err)?;
    let _ = load_edge_by_id(conn, edge_id)?;
    Ok(CaptureOutcome {
        raw_ocr_text: ocr.text.clone(),
        normalized_text: normalized,
        matched_name: location_match.name,
        match_confidence: location_match.score,
        ocr_confidence: ocr.confidence,
        parsed_current: None,
        parsed_portal: Some(parsed),
        image_path: ocr.image_path.clone(),
        duration_ms: measured_ms.max(ocr.duration_ms),
        engine: ocr.engine.clone(),
    })
}

fn spawn_hotkey_capture(
    db_path: PathBuf,
    helper_path: PathBuf,
    capture_dir: PathBuf,
    overlay: Arc<Mutex<Option<MapOverlayProcess>>>,
    paddle_ocr: Arc<Mutex<Option<PaddleOcrProcess>>>,
    kind: &'static str,
) {
    thread::spawn(move || {
        if let Err(err) = handle_hotkey_capture(
            &db_path,
            &helper_path,
            &capture_dir,
            &overlay,
            &paddle_ocr,
            kind,
        ) {
            eprintln!("[capture-hotkey] {kind} capture failed: {err}");
        }
    });
}

fn handle_hotkey_capture(
    db_path: &Path,
    helper_path: &Path,
    capture_dir: &Path,
    overlay: &Arc<Mutex<Option<MapOverlayProcess>>>,
    paddle_ocr: &Arc<Mutex<Option<PaddleOcrProcess>>>,
    kind: &str,
) -> Result<(), String> {
    let total_started = Instant::now();
    let Some(_capture_guard) = HotkeyCaptureGuard::try_acquire() else {
        eprintln!(
            "[capture-hotkey] ignoring {kind} capture because capture queue is full ({MAX_HOTKEY_CAPTURE_QUEUE})"
        );
        return Ok(());
    };
    eprintln!(
        "[capture-hotkey] {kind} capture requested from overlay hotkey pending={}",
        HotkeyCaptureGuard::pending_count()
    );
    let mut conn = open_database(db_path).map_err(db_err)?;
    initialize_schema(&conn).map_err(db_err)?;
    let region_started = Instant::now();
    let region_key = if kind == "portal" {
        "portal_tooltip"
    } else {
        "current_location"
    };
    let region = load_region_by_key(&conn, region_key)?
        .ok_or_else(|| format!("Missing {region_key} region"))?;
    let strip_regions = if kind == "portal" {
        load_region_by_key(&conn, "portal_tooltip_destination")?
            .zip(load_region_by_key(&conn, "portal_tooltip_timer")?)
    } else {
        None
    };
    eprintln!(
        "[capture-timing] kind={kind} phase=load_region ms={} key={region_key} x={} y={} width={} height={}",
        region_started.elapsed().as_millis(), region.x, region.y, region.width, region.height
    );
    let _ = set_setting(&conn, "last_capture_status", &format!("{kind} capture running"));
    if let Ok(data) = build_map_overlay_data_from_conn(&conn) {
        let _ = send_map_overlay_command_direct(overlay, json!({ "type": "data", "data": data }));
    }
    let started = Instant::now();
    let ocr_result = if let Some((destination_region, timer_region)) = strip_regions {
        run_portal_strip_capture_ocr_with_helper(
            helper_path,
            capture_dir,
            paddle_ocr,
            &destination_region,
            &timer_region,
        )
    } else {
        let portal_anchor = if kind == "portal" { region.anchor_x.zip(region.anchor_y) } else { None };
        let center_cursor = if kind == "portal" { portal_anchor.is_none() } else { false };
        run_capture_ocr_with_helper(
            helper_path,
            capture_dir,
            paddle_ocr,
            kind,
            &region,
            center_cursor,
            portal_anchor,
        )
    };
    let ocr = match ocr_result {
        Ok(ocr) => ocr,
        Err(err) => {
            let _ = set_setting(&conn, "last_capture_status", &format!("{kind} capture failed: {err}"));
            let data = build_map_overlay_data_from_conn(&conn)?;
            let _ = send_map_overlay_command_direct(overlay, json!({ "type": "data", "data": data }));
            return Err(err);
        }
    };
    let measured_ms = started.elapsed().as_millis() as i64;
    eprintln!("[capture-timing] kind={kind} phase=capture_ocr ms={measured_ms}");
    let apply_started = Instant::now();
    let result = if kind == "portal" {
        apply_portal_capture(&mut conn, &ocr, measured_ms)
    } else {
        apply_current_location_capture(&mut conn, &ocr, measured_ms)
    };
    let should_sync = result.is_ok();
    if let Err(err) = result {
        let _ = set_setting(
            &conn,
            "last_capture_status",
            &format!("{kind} capture failed: {err}"),
        );
    }
    eprintln!(
        "[capture-timing] kind={kind} phase=apply_result ms={} ok={}",
        apply_started.elapsed().as_millis(),
        should_sync
    );
    let overlay_started = Instant::now();
    let data = build_map_overlay_data_from_conn(&conn)?;
    send_map_overlay_command_direct(overlay, json!({ "type": "data", "data": data }))?;
    eprintln!(
        "[capture-timing] kind={kind} phase=overlay_update ms={} total_ms={}",
        overlay_started.elapsed().as_millis(),
        total_started.elapsed().as_millis()
    );
    drop(conn);
    if should_sync {
        trigger_sync_after_local_change(db_path.to_path_buf(), Arc::clone(overlay));
    }
    Ok(())
}

fn run_portal_strip_capture_ocr_with_helper(
    helper_path: &Path,
    capture_dir: &Path,
    paddle_ocr: &Arc<Mutex<Option<PaddleOcrProcess>>>,
    destination_region: &Region,
    timer_region: &Region,
) -> Result<CaptureOcrResult, String> {
    eprintln!(
        "[capture-timing] kind=portal phase=strip_regions destination={}x{} timer={}x{}",
        destination_region.width, destination_region.height, timer_region.width, timer_region.height
    );
    let destination = run_capture_ocr_with_helper(
        helper_path,
        capture_dir,
        paddle_ocr,
        "portal",
        destination_region,
        false,
        None,
    )?;
    let timer = run_capture_ocr_with_helper(
        helper_path,
        capture_dir,
        paddle_ocr,
        "portal",
        timer_region,
        false,
        None,
    )?;
    Ok(combine_portal_strip_ocr(destination, timer))
}

fn run_capture_ocr_with_helper(
    helper_path: &Path,
    capture_dir: &Path,
    paddle_ocr: &Arc<Mutex<Option<PaddleOcrProcess>>>,
    kind: &str,
    region: &Region,
    center_cursor: bool,
    portal_anchor: Option<(f64, f64)>,
) -> Result<CaptureOcrResult, String> {
    let total_started = Instant::now();
    fs::create_dir_all(capture_dir)
        .map_err(|err| format!("Could not create capture directory: {err}"))?;
    let mut command = Command::new(helper_path);
    command
        .arg("--mode")
        .arg("capture-ocr")
        .arg("--kind")
        .arg(kind)
        .arg("--width")
        .arg(region.width.to_string())
        .arg("--height")
        .arg(region.height.to_string())
        .arg("--output-dir")
        .arg(capture_dir);
    if center_cursor {
        command.arg("--center-cursor");
    } else {
        command
            .arg("--x")
            .arg(region.x.to_string())
            .arg("--y")
            .arg(region.y.to_string());
    }
    if let Some((anchor_x, anchor_y)) = portal_anchor {
        command
            .arg("--portal-anchor-x")
            .arg(anchor_x.to_string())
            .arg("--portal-anchor-y")
            .arg(anchor_y.to_string());
        command
            .arg("--portal-x")
            .arg(region.x.to_string())
            .arg("--portal-y")
            .arg(region.y.to_string())
            .arg("--portal-width")
            .arg(region.width.to_string())
            .arg("--portal-height")
            .arg(region.height.to_string());
    }
    if let Some(display_id) = &region.display_id {
        command.arg("--display-id").arg(display_id);
    }
    eprintln!(
        "[capture] running helper kind={kind} region={}x{} at {},{}",
        region.width, region.height, region.x, region.y
    );
    let helper_started = Instant::now();
    let output = command
        .output()
        .map_err(|err| format!("Could not run capture helper: {err}"))?;
    let helper_ms = helper_started.elapsed().as_millis();
    forward_child_stderr("capture-helper", &output.stderr);
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json_line = stdout
        .lines()
        .last()
        .ok_or_else(|| "Capture helper returned no data".to_string())?;
    let result = serde_json::from_str::<CaptureOcrResult>(json_line)
        .map_err(|err| format!("Capture helper returned invalid data: {err}"))?;
    eprintln!(
        "[capture-timing] kind={kind} phase=helper_process ms={helper_ms} helper_capture_ms={} image={} size={}x{}",
        result.duration_ms, result.image_path, result.width, result.height
    );
    if !result.screen_recording_permission {
        return Err(
            "macOS Screen Recording permission is missing. Enable it for Avalon Mapper OCR in System Settings > Privacy & Security > Screen Recording, then restart the app."
                .to_string(),
        );
    }
    let paddle_kind = if kind == "current_location" { "current" } else { kind };
    eprintln!(
        "[capture] running ocr kind={paddle_kind} image={}",
        result.image_path
    );
    let ocr_started = Instant::now();
    let paddle = run_paddle_ocr(paddle_ocr, &result.image_path, paddle_kind)?;
    let ocr_ms = ocr_started.elapsed().as_millis();
    eprintln!(
        "[capture-timing] kind={kind} phase=ocr_worker ms={ocr_ms} total_ms={} engine={} confidence={:?}",
        total_started.elapsed().as_millis(),
        paddle.engine,
        paddle.confidence
    );
    eprintln!(
        "[capture] kind={kind} image={} size={}x{} capture_ms={} ocr_engine={} ocr_confidence={:?}",
        result.image_path,
        result.width,
        result.height,
        result.duration_ms,
        paddle.engine,
        paddle.confidence
    );
    Ok(CaptureOcrResult {
        text: paddle.text,
        confidence: paddle.confidence,
        engine: paddle.engine,
        image_path: result.image_path,
        width: result.width,
        height: result.height,
        duration_ms: result.duration_ms,
        screen_recording_permission: result.screen_recording_permission,
        lines: paddle.lines,
    })
}

fn run_paddle_ocr(
    paddle_ocr: &Arc<Mutex<Option<PaddleOcrProcess>>>,
    image_path: &str,
    kind: &str,
) -> Result<OcrResult, String> {
    let total_started = Instant::now();
    let lock_started = Instant::now();
    let mut guard = paddle_ocr
        .lock()
        .map_err(|_| "Python OCR lock poisoned".to_string())?;
    let lock_ms = lock_started.elapsed().as_millis();
    let ensure_started = Instant::now();
    let process = ensure_paddle_ocr_process(&mut guard)?;
    let ensure_ms = ensure_started.elapsed().as_millis();
    let request = serde_json::to_string(&json!({
        "kind": kind,
        "image_path": image_path,
    }))
    .map_err(|err| err.to_string())?;
    let write_started = Instant::now();
    process
        .stdin
        .write_all(request.as_bytes())
        .map_err(|err| format!("Could not write Python OCR request: {err}"))?;
    process
        .stdin
        .write_all(b"\n")
        .map_err(|err| format!("Could not write Python OCR request newline: {err}"))?;
    process
        .stdin
        .flush()
        .map_err(|err| format!("Could not flush Python OCR request: {err}"))?;
    let write_ms = write_started.elapsed().as_millis();

    let wait_started = Instant::now();
    let line = match process.stdout_rx.recv_timeout(StdDuration::from_secs(120)) {
        Ok(line) => line,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let _ = process.child.kill();
            let _ = process.child.wait();
            *guard = None;
            return Err(
                "Python OCR worker timed out after 120 seconds. First run may be downloading OCR models; check internet access or run tools\\windows\\run-dev.cmd again."
                    .to_string(),
            );
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let status = process.child.try_wait().ok().flatten();
            *guard = None;
            return Err(match status {
                Some(status) => format!("Python OCR worker exited before responding: {status}"),
                None => "Python OCR worker stdout closed before responding".to_string(),
            });
        }
    };
    let wait_ms = wait_started.elapsed().as_millis();
    if line.trim().is_empty() {
        let status = process.child.try_wait().ok().flatten();
        *guard = None;
        return Err(match status {
            Some(status) => format!("Python OCR worker exited without output: {status}"),
            None => "Python OCR worker returned no output".to_string(),
        });
    }

    let parsed = serde_json::from_str::<PaddleOcrHelperResult>(line.trim())
        .map_err(|err| format!("Invalid Python OCR helper JSON: {err}; output={line}"))?;
    if !parsed.ok {
        return Err(parsed.error.unwrap_or_else(|| "Python OCR failed".to_string()));
    }

    let result = OcrResult {
        text: parsed.text.unwrap_or_default(),
        confidence: parsed.confidence,
        engine: parsed
            .engine
            .unwrap_or_else(|| "ocr:auto".to_string()),
        lines: parsed.lines.unwrap_or_default(),
    };
    eprintln!(
        "[ocr-timing] kind={kind} lock_ms={lock_ms} ensure_ms={ensure_ms} write_ms={write_ms} wait_ms={wait_ms} total_ms={}",
        total_started.elapsed().as_millis()
    );
    eprintln!(
        "[ocr] engine={} kind={kind} text={:?} confidence={:?} lines={:?}",
        result.engine,
        result.text,
        result.confidence,
        result
            .lines
            .iter()
            .map(|line| (&line.text, line.confidence))
            .collect::<Vec<_>>()
    );
    Ok(result)
}

fn ensure_paddle_ocr_process(
    process: &mut Option<PaddleOcrProcess>,
) -> Result<&mut PaddleOcrProcess, String> {
    let existing_alive = if let Some(existing) = process.as_mut() {
        existing.child.try_wait().map_err(|err| err.to_string())?.is_none()
    } else {
        false
    };
    if existing_alive {
        eprintln!("[ocr-timing] phase=ensure_worker reused=true");
        return process
            .as_mut()
            .ok_or_else(|| "Could not access Python OCR worker".to_string());
    }
    *process = None;

    let bundled_ocr_exe = bundled_or_project_path("AvalonOcrHelper.exe");
    let mut command = if cfg!(target_os = "windows") && bundled_ocr_exe.exists() {
        eprintln!("[ocr] starting bundled worker exe={}", bundled_ocr_exe.display());
        let mut command = Command::new(bundled_ocr_exe);
        command.arg("--server");
        command
    } else {
        let helper_path = bundled_or_project_path("native/ocr/paddle_ocr_helper.py");
        let windows_python_path = bundled_or_project_path(".venv/Scripts/python.exe");
        let unix_python_path = bundled_or_project_path(".venv/bin/python3");
        let python = if cfg!(target_os = "windows") && windows_python_path.exists() {
            windows_python_path
        } else if unix_python_path.exists() {
            unix_python_path
        } else if cfg!(target_os = "windows") {
            return Err(format!(
                "Python OCR environment is missing: {}. Install Python OCR runtime or run tools\\windows\\run-dev.cmd on a development checkout so it creates .venv and installs OCR dependencies.",
                windows_python_path.display()
            ));
        } else {
            PathBuf::from("python3")
        };

        eprintln!("[ocr] starting worker python={} helper={}", python.display(), helper_path.display());
        let mut command = Command::new(python);
        command.arg(helper_path).arg("--server");
        command
    };

    let mut child = command
        .env("PADDLE_PDX_MODEL_SOURCE", "BOS")
        .env("PYTHONIOENCODING", "utf-8")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("Could not start Python OCR worker: {err}"))?;
    let startup_started = Instant::now();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    if let Some(stderr) = child.stderr.take() {
        spawn_paddle_stderr_forwarder(stderr, ready_tx);
    }
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Could not open Python OCR stdin".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Could not open Python OCR stdout".to_string())?;
    let (stdout_tx, stdout_rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut buffer = Vec::new();
        loop {
            buffer.clear();
            match reader.read_until(b'\n', &mut buffer) {
                Ok(0) => break,
                Ok(_) => {
                    while matches!(buffer.last(), Some(b'\n' | b'\r')) {
                        buffer.pop();
                    }
                    let line = String::from_utf8_lossy(&buffer).to_string();
                    if stdout_tx.send(line).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    let _ = stdout_tx.send(json!({
                        "ok": false,
                        "error": format!("Could not read Python OCR stdout: {err}")
                    }).to_string());
                    break;
                }
            }
        }
    });

    match ready_rx.recv_timeout(StdDuration::from_secs(60)) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            let status = child.try_wait().ok().flatten();
            if status.is_none() {
                let _ = child.kill();
            }
            let status = child.wait().ok().or(status);
            return Err(match status {
                Some(status) => format!("{err}; exit_status={status}"),
                None => err,
            });
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(
                "Python OCR initialization timed out after 60 seconds. It is stuck while loading/downloading OCR models. Check internet access, delete .venv, then run npm run windows:dev again."
                    .to_string(),
            );
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let status = child.try_wait().ok().flatten();
            return Err(match status {
                Some(status) => format!("Python OCR worker exited during initialization: {status}"),
                None => "Python OCR worker stderr closed during initialization".to_string(),
            });
        }
    }
    eprintln!(
        "[ocr-timing] phase=ensure_worker reused=false startup_ms={}",
        startup_started.elapsed().as_millis()
    );

    *process = Some(PaddleOcrProcess {
        child,
        stdin,
        stdout_rx,
    });
    process
        .as_mut()
        .ok_or_else(|| "Could not initialize Python OCR worker".to_string())
}

fn forward_child_stderr(label: &str, stderr: &[u8]) {
    let text = String::from_utf8_lossy(stderr);
    let trimmed = text.trim();
    if !trimmed.is_empty() {
        for line in trimmed.lines() {
            eprintln!("[{label}] {line}");
        }
    }
}

fn spawn_stderr_forwarder(label: &'static str, stderr: std::process::ChildStderr) {
    thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            if !line.trim().is_empty() {
                eprintln!("[{label}] {line}");
            }
        }
    });
}

fn spawn_paddle_stderr_forwarder(
    stderr: std::process::ChildStderr,
    ready_tx: mpsc::Sender<Result<(), String>>,
) {
    thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut ready_sent = false;
        let mut recent_lines = Vec::<String>::new();
        let mut buffer = Vec::new();
        loop {
            buffer.clear();
            let read = match reader.read_until(b'\n', &mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(_) => break,
            };
            if read == 0 {
                break;
            }
            while matches!(buffer.last(), Some(b'\n' | b'\r')) {
                buffer.pop();
            }
            let line = String::from_utf8_lossy(&buffer);
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            eprintln!("[ocr] {trimmed}");
            recent_lines.push(trimmed.to_string());
            if recent_lines.len() > 12 {
                recent_lines.remove(0);
            }

            if !ready_sent && trimmed.contains("server_ready") {
                ready_sent = true;
                let _ = ready_tx.send(Ok(()));
            }

            if !ready_sent
                && (trimmed.contains("Traceback")
                    || trimmed.contains("init_failed")
                    || trimmed.contains("Fatal Python error"))
            {
                ready_sent = true;
                let _ = ready_tx.send(Err(format!(
                    "Python OCR failed during initialization: {}",
                    recent_lines.join(" | ")
                )));
            }
        }

        if !ready_sent {
            let _ = ready_tx.send(Err(format!(
                "Python OCR worker stopped before becoming ready: {}",
                recent_lines.join(" | ")
            )));
        }
    });
}

fn send_map_overlay_command_direct(
    overlay: &Arc<Mutex<Option<MapOverlayProcess>>>,
    value: serde_json::Value,
) -> Result<(), String> {
    let mut overlay = overlay.lock().map_err(|_| "Map overlay lock poisoned".to_string())?;
    let process = overlay
        .as_mut()
        .ok_or_else(|| "Map overlay helper is not running".to_string())?;
    let line = serde_json::to_string(&value).map_err(|err| err.to_string())?;
    process
        .stdin
        .write_all(line.as_bytes())
        .and_then(|_| process.stdin.write_all(b"\n"))
        .and_then(|_| process.stdin.flush())
        .map_err(|err| format!("Could not send command to map overlay helper: {err}"))
}

fn load_region_by_key(conn: &Connection, key: &str) -> Result<Option<Region>, String> {
    conn.query_row(
        "SELECT key, x, y, width, height, display_id, scale_factor, anchor_x, anchor_y FROM regions WHERE key = ?1",
        params![key],
        region_from_row,
    )
    .optional()
    .map_err(db_err)
}

fn region_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Region> {
    Ok(Region {
        key: row.get(0)?,
        x: row.get(1)?,
        y: row.get(2)?,
        width: row.get(3)?,
        height: row.get(4)?,
        display_id: row.get(5)?,
        scale_factor: row.get(6)?,
        anchor_x: row.get(7)?,
        anchor_y: row.get(8)?,
    })
}

fn load_locations(conn: &Connection) -> Result<Vec<Location>, String> {
    let mut stmt = conn
        .prepare(
            r#"
            SELECT l.id, l.name, l.normalized_name, l.zone_type,
                   l.first_seen_at, l.last_seen_at, l.visit_count,
                   p.x, p.y
            FROM locations l
            LEFT JOIN node_positions p ON p.location_id = l.id
            WHERE EXISTS (
                SELECT 1
                FROM edges e
                WHERE e.status NOT IN ('expired', 'deleted')
                  AND (
                    e.expires_at IS NULL
                    OR datetime(e.expires_at) > datetime('now')
                  )
                  AND (
                    e.from_location_id = l.id
                    OR e.to_location_id = l.id
                  )
            )
            OR l.id = CAST(COALESCE(
                (SELECT value FROM app_settings WHERE key = 'current_location_id'),
                '-1'
            ) AS INTEGER)
            OR l.name = (
                SELECT value FROM app_settings WHERE key = 'current_location_name'
            )
            ORDER BY l.name
            "#,
        )
        .map_err(db_err)?;

    let rows = stmt
        .query_map([], |row| {
            let name: String = row.get(1)?;
            let normalized_name: String = row.get(2)?;
            let avalon = lookup_avalon_info(&normalized_name);

            Ok(Location {
                id: row.get(0)?,
                name,
                normalized_name,
                zone_type: row.get(3)?,
                first_seen_at: row.get(4)?,
                last_seen_at: row.get(5)?,
                visit_count: row.get(6)?,
                x: row.get(7)?,
                y: row.get(8)?,
                avalon_tiers: avalon.tiers,
                avalon_components: avalon.components,
                avalon_chests: avalon.chests,
            })
        })
        .map_err(db_err)?;

    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_err)
}

fn load_edges(conn: &Connection) -> Result<Vec<Edge>, String> {
    let mut stmt = conn
         .prepare(
            r#"
            SELECT e.id, e.from_location_id, e.to_location_id, lf.name, lt.name, e.first_seen_at,
                   e.last_seen_at, e.ttl_seconds, e.expires_at, e.observations_count,
                   e.confidence, e.status, e.source
            FROM edges e
            JOIN locations lf ON lf.id = e.from_location_id
            JOIN locations lt ON lt.id = e.to_location_id
            WHERE e.status NOT IN ('expired', 'deleted')
              AND (
                e.expires_at IS NULL
                OR datetime(e.expires_at) > datetime('now')
              )
            ORDER BY e.last_seen_at DESC
            "#,
        )
        .map_err(db_err)?;
    let rows = stmt.query_map([], edge_from_row).map_err(db_err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_err)
}

fn load_location_by_id(conn: &Connection, id: i64) -> Result<Location, String> {
    conn.query_row(
        "SELECT id, name, normalized_name, zone_type, first_seen_at, last_seen_at, visit_count FROM locations WHERE id = ?1",
        params![id],
        |row| {
            let normalized_name: String = row.get(2)?;
            let avalon = lookup_avalon_info(&normalized_name);

            Ok(Location {
                id: row.get(0)?,
                name: row.get(1)?,
                normalized_name,
                zone_type: row.get(3)?,
                first_seen_at: row.get(4)?,
                last_seen_at: row.get(5)?,
                visit_count: row.get(6)?,
                x: None,
                y: None,
                avalon_tiers: avalon.tiers,
                avalon_components: avalon.components,
                avalon_chests: avalon.chests,
            })
        },
    )
    .map_err(db_err)
}

fn load_edge_by_id(conn: &Connection, id: i64) -> Result<Edge, String> {
    conn.query_row(
        r#"
        SELECT e.id, e.from_location_id, e.to_location_id, lf.name, lt.name, e.first_seen_at,
               e.last_seen_at, e.ttl_seconds, e.expires_at, e.observations_count,
               e.confidence, e.status, e.source
        FROM edges e
        JOIN locations lf ON lf.id = e.from_location_id
        JOIN locations lt ON lt.id = e.to_location_id
        WHERE e.id = ?1
        "#,
        params![id],
        edge_from_row,
    )
    .map_err(db_err)
}

fn canonicalize_normalized_edge_pair(from: &str, to: &str) -> (String, String) {
    if from <= to {
        (from.to_string(), to.to_string())
    } else {
        (to.to_string(), from.to_string())
    }
}

fn delete_edge_by_id(conn: &mut Connection, id: i64) -> Result<bool, String> {
    let edge = load_edge_by_id(conn, id)?;
    delete_edge_by_locations(conn, &edge.from_location_name, &edge.to_location_name)
}

fn delete_edge_by_locations(conn: &mut Connection, from_name: &str, to_name: &str) -> Result<bool, String> {
    let from_normalized = normalize_location_name(from_name);
    let to_normalized = normalize_location_name(to_name);
    if from_normalized.is_empty() || to_normalized.is_empty() {
        return Ok(false);
    }

    let (from_normalized, to_normalized) =
        canonicalize_normalized_edge_pair(&from_normalized, &to_normalized);

    let tx = conn.transaction().map_err(db_err)?;
    let deleted = tx
        .execute(
            r#"
            UPDATE edges
            SET status = 'deleted',
                last_seen_at = ?3,
                source = 'deleted'
            WHERE status != 'deleted'
              AND (
                (
                    from_location_id = (SELECT id FROM locations WHERE normalized_name = ?1)
                    AND to_location_id = (SELECT id FROM locations WHERE normalized_name = ?2)
                )
                OR (
                    from_location_id = (SELECT id FROM locations WHERE normalized_name = ?2)
                    AND to_location_id = (SELECT id FROM locations WHERE normalized_name = ?1)
                )
              )
            "#,
            params![from_normalized, to_normalized, now()],
        )
        .map_err(db_err)?;

    if deleted == 0 {
        tx.rollback().map_err(db_err)?;
        return Ok(false);
    }

    tx.commit().map_err(db_err)?;
    Ok(true)
}

fn edge_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Edge> {
    let stored_status: String = row.get(11)?;
    let expires_at: Option<String> = row.get(8)?;
    let status = compute_edge_status(expires_at.as_deref(), &stored_status);

    Ok(Edge {
        id: row.get(0)?,
        from_location_id: row.get(1)?,
        to_location_id: row.get(2)?,
        from_location_name: row.get(3)?,
        to_location_name: row.get(4)?,
        first_seen_at: row.get(5)?,
        last_seen_at: row.get(6)?,
        ttl_seconds: row.get(7)?,
        expires_at,
        observations_count: row.get(9)?,
        confidence: row.get(10)?,
        status,
        source: row.get(12)?,
    })
}

fn delete_expired_edges(conn: &Connection) -> Result<(), String> {
    conn.execute(
        r#"
        DELETE FROM edges
        WHERE expires_at IS NOT NULL
          AND datetime(expires_at) <= datetime('now')
        "#,
        [],
    )
    .map_err(db_err)?;

    delete_isolated_locations(conn)?;

    Ok(())
}

fn delete_isolated_locations(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        r#"
        DELETE FROM node_positions
        WHERE location_id NOT IN (
            SELECT from_location_id FROM edges
            UNION
            SELECT to_location_id FROM edges
        );

        DELETE FROM locations
        WHERE id NOT IN (
            SELECT from_location_id FROM edges
            UNION
            SELECT to_location_id FROM edges
        )
        AND id NOT IN (
            SELECT CAST(value AS INTEGER)
            FROM app_settings
            WHERE key = 'current_location_id'
        );
        "#,
    )
    .map_err(db_err)?;

    Ok(())
}

fn compute_edge_status(expires_at: Option<&str>, fallback_status: &str) -> String {
    if fallback_status == "deleted" {
        return "deleted".to_string();
    }

    let Some(expires_at) = expires_at else {
        return fallback_status.to_string();
    };

    let Ok(expires_dt) = chrono::DateTime::parse_from_rfc3339(expires_at) else {
        return fallback_status.to_string();
    };

    if expires_dt.with_timezone(&Utc) <= Utc::now() {
        "expired".to_string()
    } else {
        "active".to_string()
    }
}

fn upsert_location(
    conn: &Connection,
    name: &str,
    normalized_name: &str,
    zone_type: &str,
    increment_visit: bool,
) -> Result<i64, String> {
    let now = now();
    let existing_id = conn
        .query_row(
            "SELECT id FROM locations WHERE normalized_name = ?1",
            params![normalized_name],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(db_err)?;

    if let Some(id) = existing_id {
        conn.execute(
            r#"
            UPDATE locations
            SET name = ?1,
                last_seen_at = ?2,
                zone_type = ?3,
                visit_count = visit_count + ?4
            WHERE id = ?5
            "#,
            params![name, now, zone_type, if increment_visit { 1 } else { 0 }, id],
        )
        .map_err(db_err)?;
        Ok(id)
    } else {
        conn.execute(
            r#"
            INSERT INTO locations (name, normalized_name, zone_type, first_seen_at, last_seen_at, visit_count)
            VALUES (?1, ?2, ?3, ?4, ?4, ?5)
            "#,
            params![name, normalized_name, zone_type, now, if increment_visit { 1 } else { 0 }],
        )
        .map_err(db_err)?;
        Ok(conn.last_insert_rowid())
    }
}

fn allowed_graph_zone_type(zone_type: &str) -> bool {
    matches!(zone_type, "avalon" | "blue" | "yellow" | "red" | "outlands_black")
}

static LOCATION_METADATA: OnceLock<Vec<LocationMetadataEntry>> = OnceLock::new();

fn load_location_metadata() -> Vec<LocationMetadataEntry> {
    LOCATION_METADATA
        .get_or_init(|| {
            let path = bundled_or_project_path("data/albion_locations_all.json");

            let text = fs::read_to_string(&path).unwrap_or_else(|err| {
                eprintln!("[location-metadata] failed to read {}: {err}", path.display());
                "[]".to_string()
            });

            let mut entries: Vec<LocationMetadataEntry> = serde_json::from_str(&text).unwrap_or_else(|err| {
                eprintln!("[location-metadata] failed to parse JSON: {err}");
                Vec::new()
            });

            for entry in &mut entries {
                if entry.normalized_name.trim().is_empty() {
                    entry.normalized_name = normalize_location_name(&entry.name);
                }
            }

            eprintln!("[location-metadata] loaded {} entries", entries.len());
            entries
        })
        .clone()
}

fn infer_zone_type_from_name(name: &str) -> String {
    let normalized = normalize_location_name(name);

    load_location_metadata()
        .iter()
        .find(|entry| entry.normalized_name == normalized)
        .map(|entry| entry.zone_type.clone())
        .unwrap_or_else(|| {
            if name.contains('-') {
                "avalon".to_string()
            } else {
                "unknown".to_string()
            }
        })
}

fn ensure_node_position(
    conn: &Connection,
    location_id: i64,
    parent_location_id: Option<i64>,
) -> Result<(), String> {
    let exists = conn
        .query_row(
            "SELECT 1 FROM node_positions WHERE location_id = ?1",
            params![location_id],
            |_| Ok(()),
        )
        .optional()
        .map_err(db_err)?
        .is_some();

    if exists {
        return Ok(());
    }

    let (x, y) = choose_node_position(conn, parent_location_id)?;

    conn.execute(
        r#"
        INSERT INTO node_positions (location_id, x, y, updated_at)
        VALUES (?1, ?2, ?3, ?4)
        ON CONFLICT(location_id) DO NOTHING
        "#,
        params![location_id, x, y, now()],
    )
    .map_err(db_err)?;

    Ok(())
}

fn choose_node_position(
    conn: &Connection,
    parent_location_id: Option<i64>,
) -> Result<(f64, f64), String> {
    let existing = load_node_positions(conn)?;

    if existing.is_empty() {
        return Ok((0.0, 0.0));
    }

    let parent = parent_location_id
        .and_then(|id| existing.iter().find(|(loc_id, _, _)| *loc_id == id).cloned())
        .map(|(_, x, y)| (x, y))
        .unwrap_or((0.0, 0.0));

    let step = 140.0;
    let candidates = [
        (step, 0.0),
        (-step, 0.0),
        (0.0, step),
        (0.0, -step),
        (step, step),
        (step, -step),
        (-step, step),
        (-step, -step),
        (step * 2.0, 0.0),
        (-step * 2.0, 0.0),
        (0.0, step * 2.0),
        (0.0, -step * 2.0),
    ];

    let mut best = (parent.0 + step, parent.1);
    let mut best_score = f64::MAX;

    for (dx, dy) in candidates {
        let candidate = (parent.0 + dx, parent.1 + dy);
        let score = position_score(candidate, &existing);
        if score < best_score {
            best_score = score;
            best = candidate;
        }
    }

    Ok(best)
}

fn load_node_positions(conn: &Connection) -> Result<Vec<(i64, f64, f64)>, String> {
    let mut stmt = conn
        .prepare("SELECT location_id, x, y FROM node_positions")
        .map_err(db_err)?;

    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, f64>(2)?,
            ))
        })
        .map_err(db_err)?;

    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_err)
}

fn position_score(candidate: (f64, f64), existing: &[(i64, f64, f64)]) -> f64 {
    let mut score = 0.0;

    for (_, x, y) in existing {
        let dx = candidate.0 - x;
        let dy = candidate.1 - y;
        let dist = (dx * dx + dy * dy).sqrt();

        if dist < 80.0 {
            score += 10_000.0;
        }

        score += 1.0 / dist.max(1.0);
    }

    score
}

fn recompute_graph_layout(conn: &Connection) -> Result<(), String> {
    let mut location_ids = Vec::<i64>::new();

    let mut stmt = conn
        .prepare("SELECT id FROM locations ORDER BY id")
        .map_err(db_err)?;

    let rows = stmt
        .query_map([], |row| row.get::<_, i64>(0))
        .map_err(db_err)?;

    for row in rows {
        location_ids.push(row.map_err(db_err)?);
    }

    if location_ids.is_empty() {
        conn.execute("DELETE FROM node_positions", []).map_err(db_err)?;
        return Ok(());
    }

    let mut edges = Vec::<(i64, i64)>::new();

    let mut stmt = conn
        .prepare(
            r#"
            SELECT from_location_id, to_location_id
            FROM edges
            WHERE status NOT IN ('expired', 'deleted')
            ORDER BY id
            "#,
        )
        .map_err(db_err)?;

    let rows = stmt
        .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
        .map_err(db_err)?;

    for row in rows {
        edges.push(row.map_err(db_err)?);
    }

    let mut graph = std::collections::HashMap::<i64, Vec<i64>>::new();

    for id in &location_ids {
        graph.insert(*id, Vec::new());
    }

    for (a, b) in &edges {
        graph.entry(*a).or_default().push(*b);
        graph.entry(*b).or_default().push(*a);
    }

    for neighbors in graph.values_mut() {
        neighbors.sort();
        neighbors.dedup();
    }

    let root_id = get_setting(conn, "current_location_id")?
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|id| graph.contains_key(id))
        .unwrap_or_else(|| {
            location_ids
                .iter()
                .max_by_key(|id| graph.get(id).map(|v| v.len()).unwrap_or(0))
                .copied()
                .unwrap_or(location_ids[0])
        });

    let components = graph_components(&location_ids, &graph, root_id);

    let mut final_positions = std::collections::HashMap::<i64, (f64, f64)>::new();

    let component_padding_x = 260.0;
    let component_padding_y = 260.0;
    let max_row_width = 1500.0;

    let mut cursor_x = 0.0;
    let mut cursor_y = 0.0;
    let mut row_height = 0.0;

    for component in components.iter() {
        let root = if component.contains(&root_id) {
            root_id
        } else {
            component
                .iter()
                .max_by_key(|id| graph.get(id).map(|v| v.len()).unwrap_or(0))
                .copied()
                .unwrap_or(component[0])
        };

        let local = coordinates_from_spanning_tree(component, &graph, root);
        let (min_x, max_x, min_y, max_y) = bounds_of_positions(&local);

        let width = (max_x - min_x).max(1.0);
        let height = (max_y - min_y).abs().max(1.0);

        if cursor_x > 0.0 && cursor_x + width > max_row_width {
            cursor_x = 0.0;
            cursor_y -= row_height + component_padding_y;
            row_height = 0.0;
        }

        for (id, (x, y)) in local {
            final_positions.insert(
                id,
                (
                    cursor_x + (x - min_x),
                    cursor_y + (y - max_y),
                ),
            );
        }

        cursor_x += width + component_padding_x;
        row_height = row_height.max(height);
    }

    conn.execute("DELETE FROM node_positions", []).map_err(db_err)?;

    for id in location_ids {
        let (x, y) = final_positions.get(&id).copied().unwrap_or((0.0, 0.0));

        conn.execute(
            r#"
            INSERT INTO node_positions (location_id, x, y, updated_at)
            VALUES (?1, ?2, ?3, ?4)
            "#,
            params![id, x, y, now()],
        )
        .map_err(db_err)?;
    }

    Ok(())
}

fn graph_components(
    location_ids: &[i64],
    graph: &std::collections::HashMap<i64, Vec<i64>>,
    preferred_root: i64,
) -> Vec<Vec<i64>> {
    let mut visited = std::collections::HashSet::<i64>::new();
    let mut components = Vec::<Vec<i64>>::new();

    let mut starts = Vec::<i64>::new();
    if location_ids.contains(&preferred_root) {
        starts.push(preferred_root);
    }
    starts.extend(location_ids.iter().copied().filter(|id| *id != preferred_root));

    for start in starts {
        if visited.contains(&start) {
            continue;
        }

        let mut queue = std::collections::VecDeque::new();
        let mut component = Vec::<i64>::new();

        visited.insert(start);
        queue.push_back(start);

        while let Some(id) = queue.pop_front() {
            component.push(id);

            if let Some(neighbors) = graph.get(&id) {
                for neighbor in neighbors {
                    if visited.insert(*neighbor) {
                        queue.push_back(*neighbor);
                    }
                }
            }
        }

        component.sort_by_key(|id| {
            if *id == preferred_root {
                (0, 0_i64)
            } else {
                (1, *id)
            }
        });

        components.push(component);
    }

    components.sort_by(|a, b| {
        let a_has_root = a.contains(&preferred_root);
        let b_has_root = b.contains(&preferred_root);

        match (a_has_root, b_has_root) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => b.len().cmp(&a.len()),
        }
    });

    components
}

fn build_overlay_route(
    conn: &Connection,
    from_raw: &str,
    to_raw: &str,
) -> Result<(Vec<RouteOverlayLocation>, Vec<RouteOverlayEdge>), String> {
    delete_expired_edges(conn)?;

    let current_location = get_setting(conn, "current_location_name")?;

    let from_query = if from_raw.trim().is_empty() {
        current_location.as_deref().unwrap_or("").to_string()
    } else {
        from_raw.trim().to_string()
    };

    let to_query = to_raw.trim().to_string();

    if from_query.trim().is_empty() || to_query.trim().is_empty() {
        return Err("Route start and destination are required".to_string());
    }

    let graph = build_route_graph(conn)?;
    let all_names = load_route_location_names(conn)?;

    let to_is_safe = is_safe_route_query(&to_query);
    let from_is_city = is_city_route_query(&from_query);

    let matched_to_norm = if to_is_safe {
        None
    } else {
        Some(
            match_route_location_name(&to_query, &all_names)
                .ok_or_else(|| format!("Could not match route destination: {to_query}"))?,
        )
    };

    let from_norm = if from_is_city {
        let destination = matched_to_norm
            .as_deref()
            .ok_or_else(|| "Route start 'city' requires a concrete destination".to_string())?;
        nearest_city_route_target(&graph, destination)
            .ok_or_else(|| format!("Could not find nearest city to {to_query}"))?
    } else {
        match_route_location_name(&from_query, &all_names)
            .ok_or_else(|| format!("Could not match route start: {from_query}"))?
    };

    let to_norm = if to_is_safe {
        nearest_safe_route_target(&graph, &from_norm)
            .ok_or_else(|| format!("Could not find nearest safe zone from {from_query}"))?
    } else {
        matched_to_norm.expect("matched_to_norm exists for non-safe route")
    };

    let _ = debug_route_static_edges_for(conn, "martlock");
    let _ = debug_route_static_edges_for(conn, "thetford");
    let _ = debug_route_static_edges_for(conn, "portal");
    eprintln!("[route-debug] matched from={from_query:?} -> {from_norm:?}");
    eprintln!("[route-debug] matched to={to_query:?} -> {to_norm:?}");

    let from_neighbors = graph.get(&from_norm).cloned().unwrap_or_default();
    let to_neighbors = graph.get(&to_norm).cloned().unwrap_or_default();

    eprintln!("[route-debug] from neighbors count={}", from_neighbors.len());
    eprintln!("[route-debug] from neighbors={:?}", from_neighbors.iter().take(20).collect::<Vec<_>>());

    eprintln!("[route-debug] to neighbors count={}", to_neighbors.len());
    eprintln!("[route-debug] to neighbors={:?}", to_neighbors.iter().take(20).collect::<Vec<_>>());
    let path = shortest_path_bfs(&graph, &from_norm, &to_norm)
        .ok_or_else(|| format!("No route from {from_query} to {to_query}"))?;

    let mut route_locations = Vec::new();

    for (index, normalized) in path.iter().enumerate() {
        route_locations.push(resolve_overlay_route_location(
            conn,
            normalized,
            index,
        )?);
    }

    let mut route_edges = Vec::new();

    for i in 0..route_locations.len().saturating_sub(1) {
        let from = &route_locations[i];
        let to = &route_locations[i + 1];

        route_edges.push(RouteOverlayEdge {
            id: -1 - i as i64,
            from_location_id: from.id,
            to_location_id: to.id,
            from_location_name: from.name.clone(),
            to_location_name: to.name.clone(),
            source: "route".to_string(),
        });
    }

    Ok((route_locations, route_edges))
}

fn resolve_overlay_route_location(
    conn: &Connection,
    normalized: &str,
    index: usize,
) -> Result<RouteOverlayLocation, String> {
    if let Some(location) = conn
        .query_row(
            r#"
            SELECT l.id, l.name, l.normalized_name, l.zone_type, p.x, p.y
            FROM locations l
            LEFT JOIN node_positions p ON p.location_id = l.id
            WHERE l.normalized_name = ?1
            "#,
            params![normalized],
            |row| {
                Ok(RouteOverlayLocation {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    normalized_name: row.get(2)?,
                    zone_type: row.get(3)?,
                    x: row.get(4)?,
                    y: row.get(5)?,
                })
            },
        )
        .optional()
        .map_err(db_err)?
    {
        return Ok(location);
    }

    let name = resolve_route_location_name(conn, normalized)
        .unwrap_or_else(|_| normalized.to_string());

    Ok(RouteOverlayLocation {
        id: -1 - index as i64,
        name,
        normalized_name: normalized.to_string(),
        zone_type: infer_zone_type_from_name(normalized),
        x: Some(index as f64 * 160.0),
        y: Some(0.0),
    })
}

fn load_route_location_names(conn: &Connection) -> Result<Vec<(String, String)>, String> {
    let mut result = Vec::<(String, String)>::new();

    {
        let mut stmt = conn
            .prepare("SELECT normalized_name, name FROM route_static_locations")
            .map_err(db_err)?;

        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(db_err)?;

        for row in rows {
            result.push(row.map_err(db_err)?);
        }
    }

    {
        let mut stmt = conn
            .prepare("SELECT normalized_name, name FROM locations")
            .map_err(db_err)?;

        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(db_err)?;

        for row in rows {
            result.push(row.map_err(db_err)?);
        }
    }

    for city in route_city_names() {
        result.push((city.to_string(), title_case_location_name(city)));
    }

    for (_, portal) in city_portal_route_pairs() {
        result.push((portal.to_string(), title_case_location_name(portal)));
    }

    // ВАЖНО: добавляем вершины, которые существуют только в route_static_edges
    {
        let mut stmt = conn
            .prepare(
                r#"
                SELECT from_normalized FROM route_static_edges
                UNION
                SELECT to_normalized FROM route_static_edges
                "#,
            )
            .map_err(db_err)?;

        let rows = stmt
            .query_map([], |row| {
                let normalized: String = row.get(0)?;
                Ok((normalized.clone(), title_case_location_name(&normalized)))
            })
            .map_err(db_err)?;

        for row in rows {
            result.push(row.map_err(db_err)?);
        }
    }

    result.sort_by(|a, b| a.0.cmp(&b.0));
    result.dedup_by(|a, b| a.0 == b.0);

    Ok(result)
}

fn title_case_location_name(normalized: &str) -> String {
    normalized
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    let first = first.to_uppercase().collect::<String>();
                    let rest = chars.collect::<String>();
                    format!("{first}{rest}")
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn match_route_location_name(
    query: &str,
    names: &[(String, String)],
) -> Option<String> {
    let normalized_query = normalize_location_name(query);

    if normalized_query.is_empty() {
        return None;
    }
    let city_names = [
        "bridgewatch",
        "fort sterling",
        "lymhurst",
        "martlock",
        "thetford",
        "caerleon",
        "brecilien",
    ];

    if city_names.contains(&normalized_query.as_str()) {
        if names.iter().any(|(normalized, _)| normalized == &normalized_query) {
            return Some(normalized_query);
        }
    }
    // 1. Сначала точное совпадение normalized_name
    if let Some((normalized, _)) = names.iter().find(|(normalized, _)| normalized == &normalized_query) {
        return Some(normalized.clone());
    }

    // 2. Потом точное совпадение display name
    if let Some((normalized, _)) = names
        .iter()
        .find(|(_, display)| normalize_location_name(display) == normalized_query)
    {
        return Some(normalized.clone());
    }

    // 3. Только потом Levenshtein
    names
        .iter()
        .map(|(normalized, display)| {
            let score = levenshtein_distance_score(&normalized_query, normalized)
                .max(levenshtein_distance_score(&normalized_query, &normalize_location_name(display)));
            (normalized.clone(), score)
        })
        .max_by(|a, b| {
            a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal)
        })
        .and_then(|(normalized, score)| {
            if score >= 0.55 {
                Some(normalized)
            } else {
                None
            }
        })
}

fn levenshtein_distance_score(a: &str, b: &str) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }

    if a == b {
        return 1.0;
    }

    let distance = levenshtein(a, b) as f64;
    let max_len = a.chars().count().max(b.chars().count()).max(1) as f64;

    (1.0 - distance / max_len).clamp(0.0, 1.0)
}


fn debug_route_static_edges_for(conn: &Connection, needle: &str) -> Result<(), String> {
    let needle_norm = normalize_location_name(needle);
    let like = format!("%{}%", needle_norm);

    eprintln!("[route-debug-db] searching static edges like {:?}", like);

    let mut stmt = conn
        .prepare(
            r#"
            SELECT from_normalized, to_normalized, source
            FROM route_static_edges
            WHERE from_normalized LIKE ?1 OR to_normalized LIKE ?1
            LIMIT 30
            "#,
        )
        .map_err(db_err)?;

    let rows = stmt
        .query_map(params![like], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(db_err)?;

    for row in rows {
        let (from, to, source) = row.map_err(db_err)?;
        eprintln!("[route-debug-db] edge: {from} <-> {to} / {source}");
    }

    Ok(())
}

fn bfs_levels(
    component: &[i64],
    graph: &std::collections::HashMap<i64, Vec<i64>>,
    root: i64,
) -> Vec<Vec<i64>> {
    let component_set = component.iter().copied().collect::<std::collections::HashSet<_>>();
    let mut visited = std::collections::HashSet::<i64>::new();
    let mut queue = std::collections::VecDeque::<(i64, usize)>::new();
    let mut levels = Vec::<Vec<i64>>::new();

    visited.insert(root);
    queue.push_back((root, 0));

    while let Some((id, depth)) = queue.pop_front() {
        while levels.len() <= depth {
            levels.push(Vec::new());
        }

        levels[depth].push(id);

        let mut neighbors = graph.get(&id).cloned().unwrap_or_default();
        neighbors.sort_by_key(|neighbor| {
            let degree = graph.get(neighbor).map(|v| v.len()).unwrap_or(0);
            (std::cmp::Reverse(degree), *neighbor)
        });

        for neighbor in neighbors {
            if component_set.contains(&neighbor) && visited.insert(neighbor) {
                queue.push_back((neighbor, depth + 1));
            }
        }
    }

    for id in component {
        if !visited.contains(id) {
            levels.push(vec![*id]);
        }
    }

    levels
}

fn reduce_layer_crossings(
    levels: &mut [Vec<i64>],
    graph: &std::collections::HashMap<i64, Vec<i64>>,
) {
    for _ in 0..6 {
        for layer_index in 1..levels.len() {
            let previous_positions = levels[layer_index - 1]
                .iter()
                .enumerate()
                .map(|(index, id)| (*id, index as f64))
                .collect::<std::collections::HashMap<_, _>>();

            levels[layer_index].sort_by(|a, b| {
                let ba = barycenter(*a, graph, &previous_positions);
                let bb = barycenter(*b, graph, &previous_positions);

                ba.partial_cmp(&bb)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.cmp(b))
            });
        }

        if levels.len() >= 2 {
            for layer_index in (0..levels.len() - 1).rev() {
                let next_positions = levels[layer_index + 1]
                    .iter()
                    .enumerate()
                    .map(|(index, id)| (*id, index as f64))
                    .collect::<std::collections::HashMap<_, _>>();

                levels[layer_index].sort_by(|a, b| {
                    let ba = barycenter(*a, graph, &next_positions);
                    let bb = barycenter(*b, graph, &next_positions);

                    ba.partial_cmp(&bb)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.cmp(b))
                });
            }
        }
    }
}

fn barycenter(
    id: i64,
    graph: &std::collections::HashMap<i64, Vec<i64>>,
    neighbor_positions: &std::collections::HashMap<i64, f64>,
) -> f64 {
    let positions = graph
        .get(&id)
        .into_iter()
        .flatten()
        .filter_map(|neighbor| neighbor_positions.get(neighbor))
        .copied()
        .collect::<Vec<_>>();

    if positions.is_empty() {
        return id as f64;
    }

    positions.iter().sum::<f64>() / positions.len() as f64
}

fn coordinates_from_spanning_tree(
    component: &[i64],
    graph: &std::collections::HashMap<i64, Vec<i64>>,
    root: i64,
) -> std::collections::HashMap<i64, (f64, f64)> {
    let component_set = component.iter().copied().collect::<std::collections::HashSet<_>>();
    let mut visited = std::collections::HashSet::<i64>::new();
    let mut children = std::collections::HashMap::<i64, Vec<i64>>::new();
    let mut queue = std::collections::VecDeque::<i64>::new();

    visited.insert(root);
    queue.push_back(root);

    while let Some(id) = queue.pop_front() {
        let mut neighbors = graph.get(&id).cloned().unwrap_or_default();
        neighbors.retain(|neighbor| component_set.contains(neighbor));
        neighbors.sort_by_key(|neighbor| {
            let degree = graph.get(neighbor).map(|items| items.len()).unwrap_or(0);
            (std::cmp::Reverse(degree), *neighbor)
        });

        for neighbor in neighbors {
            if visited.insert(neighbor) {
                children.entry(id).or_default().push(neighbor);
                queue.push_back(neighbor);
            }
        }
    }

    let mut positions = std::collections::HashMap::<i64, (f64, f64)>::new();
    let mut next_leaf_x = 0.0;
    assign_tree_coordinates(root, 0, &children, &mut next_leaf_x, &mut positions);

    for id in component {
        if !positions.contains_key(id) {
            assign_tree_coordinates(*id, 0, &children, &mut next_leaf_x, &mut positions);
        }
    }

    positions
}

fn assign_tree_coordinates(
    id: i64,
    depth: usize,
    children: &std::collections::HashMap<i64, Vec<i64>>,
    next_leaf_x: &mut f64,
    positions: &mut std::collections::HashMap<i64, (f64, f64)>,
) -> f64 {
    let layer_spacing = 210.0;
    let leaf_spacing = 140.0;

    let child_ids = children.get(&id).cloned().unwrap_or_default();
    let x = if child_ids.is_empty() {
        let x = *next_leaf_x;
        *next_leaf_x += leaf_spacing;
        x
    } else {
        let mut child_xs = Vec::<f64>::new();
        for child in child_ids {
            child_xs.push(assign_tree_coordinates(
                child,
                depth + 1,
                children,
                next_leaf_x,
                positions,
            ));
        }

        let first = child_xs.first().copied().unwrap_or(*next_leaf_x);
        let last = child_xs.last().copied().unwrap_or(first);
        (first + last) / 2.0
    };

    positions.insert(id, (x, -(depth as f64) * layer_spacing));
    x
}

fn bounds_of_positions(
    positions: &std::collections::HashMap<i64, (f64, f64)>,
) -> (f64, f64, f64, f64) {
    let mut min_x = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_y = f64::NEG_INFINITY;

    for (_, (x, y)) in positions {
        min_x = min_x.min(*x);
        max_x = max_x.max(*x);
        min_y = min_y.min(*y);
        max_y = max_y.max(*y);
    }

    if positions.is_empty() {
        (0.0, 0.0, 0.0, 0.0)
    } else {
        (min_x, max_x, min_y, max_y)
    }
}

fn coordinates_from_levels(levels: &[Vec<i64>]) -> std::collections::HashMap<i64, (f64, f64)> {
    let layer_spacing = 210.0;
    let node_spacing = 120.0;

    let mut positions = std::collections::HashMap::<i64, (f64, f64)>::new();

    for (layer_index, layer) in levels.iter().enumerate() {
        let width = (layer.len().saturating_sub(1)) as f64 * node_spacing;
        let start_x = -width / 2.0;
        let y = -(layer_index as f64) * layer_spacing;

        for (index, id) in layer.iter().enumerate() {
            let x = start_x + index as f64 * node_spacing;
            positions.insert(*id, (x, y));
        }
    }

    positions
}

fn add_layout_delta(
    delta: &mut std::collections::HashMap<i64, (f64, f64)>,
    id: i64,
    dx: f64,
    dy: f64,
) {
    let entry = delta.entry(id).or_insert((0.0, 0.0));
    entry.0 += dx;
    entry.1 += dy;
}

fn segments_intersect(
    a: (f64, f64),
    b: (f64, f64),
    c: (f64, f64),
    d: (f64, f64),
) -> bool {
    let o1 = orientation(a, b, c);
    let o2 = orientation(a, b, d);
    let o3 = orientation(c, d, a);
    let o4 = orientation(c, d, b);

    o1 * o2 < 0.0 && o3 * o4 < 0.0
}

fn orientation(a: (f64, f64), b: (f64, f64), c: (f64, f64)) -> f64 {
    (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)
}

fn closest_point_on_segment(
    px: f64,
    py: f64,
    ax: f64,
    ay: f64,
    bx: f64,
    by: f64,
) -> (f64, f64, f64) {
    let abx = bx - ax;
    let aby = by - ay;
    let apx = px - ax;
    let apy = py - ay;

    let ab_len2 = (abx * abx + aby * aby).max(1.0);
    let t = ((apx * abx + apy * aby) / ab_len2).clamp(0.0, 1.0);

    let cx = ax + abx * t;
    let cy = ay + aby * t;

    let dx = px - cx;
    let dy = py - cy;
    let dist = (dx * dx + dy * dy).sqrt();

    (cx, cy, dist)
}

fn upsert_edge(
    conn: &Connection,
    from_location_id: i64,
    to_location_id: i64,
    ttl_seconds: Option<i64>,
) -> Result<i64, String> {
    let now_dt = Utc::now();
    let now = now_dt.to_rfc3339();
    let expires_at = ttl_seconds.map(|ttl| (now_dt + Duration::seconds(ttl)).to_rfc3339());

    let existing_id = conn
        .query_row(
            r#"
            SELECT id FROM edges
            WHERE (from_location_id = ?1 AND to_location_id = ?2)
               OR (from_location_id = ?2 AND to_location_id = ?1)
            "#,
            params![from_location_id, to_location_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(db_err)?;

    if let Some(id) = existing_id {
        conn.execute(
            r#"
            UPDATE edges
            SET last_seen_at = ?1,
                ttl_seconds = COALESCE(?2, ttl_seconds),
                expires_at = COALESCE(?3, expires_at),
                observations_count = observations_count + 1,
                confidence = 1.0,
                source = CASE
                    WHEN status = 'deleted' THEN 'local_readd'
                    ELSE source
                END,
                status = 'active'
            WHERE id = ?4
            "#,
            params![now, ttl_seconds, expires_at, id],
        )
        .map_err(db_err)?;
        Ok(id)
    } else {
        conn.execute(
            r#"
            INSERT INTO edges (
                from_location_id, to_location_id, first_seen_at, last_seen_at,
                ttl_seconds, expires_at, confidence, status, source
            )
            VALUES (?1, ?2, ?3, ?3, ?4, ?5, 1.0, 'active', 'ocr')
            "#,
            params![from_location_id, to_location_id, now, ttl_seconds, expires_at],
        )
        .map_err(db_err)?;
        Ok(conn.last_insert_rowid())
    }
}

fn insert_observation(
    conn: &Connection,
    kind: &str,
    from_location_name: Option<&str>,
    to_location_name: Option<&str>,
    raw_ocr_text: &str,
    normalized_text: Option<&str>,
    confidence: Option<f64>,
    screenshot_path: Option<&str>,
    metadata_json: Option<&str>,
) -> Result<(), String> {
    let confidence = confidence.map(|v| (v * 100.0).round() / 100.0);

    conn.execute(
        r#"
        INSERT INTO observations (
            kind, from_location_name, to_location_name, raw_ocr_text,
            normalized_text, confidence, screenshot_path, metadata_json, created_at
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
        "#,
        params![
            kind,
            from_location_name,
            to_location_name,
            raw_ocr_text,
            normalized_text,
            confidence,
            screenshot_path,
            metadata_json,
            now()
        ],
    )
    .map_err(db_err)?;

    Ok(())
}

fn get_setting(conn: &Connection, key: &str) -> Result<Option<String>, String> {
    conn.query_row(
        "SELECT value FROM app_settings WHERE key = ?1",
        params![key],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(db_err)
}

fn set_setting(conn: &Connection, key: &str, value: &str) -> Result<(), String> {
    conn.execute(
        r#"
        INSERT INTO app_settings (key, value)
        VALUES (?1, ?2)
        ON CONFLICT(key) DO UPDATE SET value = excluded.value
        "#,
        params![key, value],
    )
    .map_err(db_err)?;
    Ok(())
}

fn read_sync_settings(conn: &Connection) -> Result<SyncSettings, String> {
    Ok(SyncSettings {
        enabled: get_setting(conn, "sync_enabled")?
            .map(|value| value == "true")
            .unwrap_or(false),
        server_url: get_setting(conn, "sync_server_url")?.unwrap_or_default(),
        write_token: get_setting(conn, "sync_write_token")?.unwrap_or_default(),
    })
}

fn start_sync_worker(db_path: PathBuf, overlay: Arc<Mutex<Option<MapOverlayProcess>>>) {
    thread::spawn(move || loop {
        if let Err(err) = run_sync_once(&db_path, &overlay) {
            eprintln!("[sync] {err}");
        }
        thread::sleep(StdDuration::from_secs(3));
    });
}

fn trigger_sync_after_local_change(
    db_path: PathBuf,
    overlay: Arc<Mutex<Option<MapOverlayProcess>>>,
) {
    thread::spawn(move || {
        if let Err(err) = run_sync_once(&db_path, &overlay) {
            eprintln!("[sync] local change sync failed: {err}");
        }
    });
}

fn run_sync_once(
    db_path: &Path,
    overlay: &Arc<Mutex<Option<MapOverlayProcess>>>,
) -> Result<(), String> {
    let mut conn = open_database(db_path).map_err(db_err)?;
    initialize_schema(&conn).map_err(db_err)?;
    let settings = read_sync_settings(&conn)?;
    if !settings.enabled || settings.server_url.trim().is_empty() {
        return Ok(());
    }

    let mut changed = 0usize;
    let edges = load_edges(&conn)?;
    let post_url = sync_url(&settings.server_url, "edges")?;
    for edge in edges {
        if edge.status == "expired" {
            continue;
        }
        let from_normalized = normalize_location_name(&edge.from_location_name);
        let to_normalized = normalize_location_name(&edge.to_location_name);
        let revive = edge.source == "local_readd";
        let payload = SyncEdgePayload {
            from_name: edge.from_location_name,
            to_name: edge.to_location_name,
            from_normalized: Some(from_normalized),
            to_normalized: Some(to_normalized),
            first_seen_at: Some(edge.first_seen_at),
            last_seen_at: Some(edge.last_seen_at),
            ttl_seconds: edge.ttl_seconds,
            expires_at: edge.expires_at,
            observations_count: Some(edge.observations_count),
            source: Some(edge.source),
            status: Some(edge.status),
            revive: Some(revive),
        };
        let body = serde_json::to_string(&payload).map_err(|err| err.to_string())?;
        let response_body = http_json_request("POST", &post_url, Some(&body), Some(&settings.write_token))?;
        let response: SyncPostResponse = serde_json::from_str(&response_body)
            .map_err(|err| format!("Could not parse sync POST response: {err}; body={response_body}"))?;
        if !response.ok {
            return Err(response.error.unwrap_or_else(|| "Sync POST failed".to_string()));
        }
    }

    let snapshot_url = sync_url(&settings.server_url, "snapshot")?;
    let snapshot_body = http_json_request("GET", &snapshot_url, None, None)?;
    let snapshot: SyncSnapshot = serde_json::from_str(&snapshot_body)
        .map_err(|err| format!("Could not parse sync snapshot: {err}; body={snapshot_body}"))?;
    if !snapshot.ok {
        return Err(snapshot.error.unwrap_or_else(|| "Sync snapshot failed".to_string()));
    }

    for edge in snapshot.edges {
        if apply_sync_edge(&mut conn, &edge)? {
            changed += 1;
        }
    }

    if changed > 0 {
        recompute_graph_layout(&conn)?;
        let data = build_map_overlay_data_from_conn(&conn)?;
        let _ = send_map_overlay_command_direct(overlay, json!({ "type": "data", "data": data }));
        eprintln!("[sync] applied {changed} remote edges");
    }

    Ok(())
}

fn apply_sync_edge(conn: &mut Connection, edge: &SyncEdgePayload) -> Result<bool, String> {
    let from_name = edge.from_name.trim();
    let to_name = edge.to_name.trim();
    if from_name.is_empty() || to_name.is_empty() {
        return Ok(false);
    }

    if edge.status.as_deref() == Some("deleted") {
        return delete_edge_by_locations(conn, from_name, to_name);
    }

    let from_normalized = edge
        .from_normalized
        .as_deref()
        .map(normalize_location_name)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| normalize_location_name(from_name));
    let to_normalized = edge
        .to_normalized
        .as_deref()
        .map(normalize_location_name)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| normalize_location_name(to_name));
    if from_normalized.is_empty() || to_normalized.is_empty() {
        return Ok(false);
    }

    let tx = conn.transaction().map_err(db_err)?;
    let from_zone = infer_zone_type_from_name(from_name);
    let to_zone = infer_zone_type_from_name(to_name);
    if !allowed_graph_zone_type(&from_zone) || !allowed_graph_zone_type(&to_zone) {
        return Ok(false);
    }
    let from_id = upsert_location(&tx, from_name, &from_normalized, &from_zone, false)?;
    let to_id = upsert_location(&tx, to_name, &to_normalized, &to_zone, false)?;
    let changed = upsert_synced_edge(
        &tx,
        from_id,
        to_id,
        edge.first_seen_at.as_deref(),
        edge.last_seen_at.as_deref(),
        edge.ttl_seconds,
        edge.expires_at.as_deref(),
        edge.observations_count,
    )?;
    tx.commit().map_err(db_err)?;
    Ok(changed)
}

fn upsert_synced_edge(
    conn: &Connection,
    from_location_id: i64,
    to_location_id: i64,
    first_seen_at: Option<&str>,
    last_seen_at: Option<&str>,
    ttl_seconds: Option<i64>,
    expires_at: Option<&str>,
    observations_count: Option<i64>,
) -> Result<bool, String> {
    let now = now();
    let first_seen_at = first_seen_at.unwrap_or(&now);
    let last_seen_at = last_seen_at.unwrap_or(&now);
    let existing = conn
        .query_row(
            r#"
            SELECT id, last_seen_at FROM edges
            WHERE (from_location_id = ?1 AND to_location_id = ?2)
               OR (from_location_id = ?2 AND to_location_id = ?1)
            "#,
            params![from_location_id, to_location_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(db_err)?;

    if let Some((id, existing_last_seen_at)) = existing {
        if existing_last_seen_at.as_str() >= last_seen_at {
            return Ok(false);
        }
        conn.execute(
            r#"
            UPDATE edges
            SET last_seen_at = ?1,
                ttl_seconds = COALESCE(?2, ttl_seconds),
                expires_at = COALESCE(?3, expires_at),
                observations_count = MAX(observations_count, COALESCE(?4, observations_count)),
                confidence = 1.0,
                status = 'active',
                source = 'sync'
            WHERE id = ?5
            "#,
            params![last_seen_at, ttl_seconds, expires_at, observations_count, id],
        )
        .map_err(db_err)?;
        Ok(true)
    } else {
        conn.execute(
            r#"
            INSERT INTO edges (
                from_location_id, to_location_id, first_seen_at, last_seen_at,
                ttl_seconds, expires_at, observations_count, confidence, status, source
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, COALESCE(?7, 1), 1.0, 'active', 'sync')
            "#,
            params![
                from_location_id,
                to_location_id,
                first_seen_at,
                last_seen_at,
                ttl_seconds,
                expires_at,
                observations_count
            ],
        )
        .map_err(db_err)?;
        Ok(true)
    }
}

fn sync_url(base: &str, endpoint: &str) -> Result<String, String> {
    let base = base.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err("Sync server URL is empty".to_string());
    }
    Ok(format!("{base}/{endpoint}"))
}

struct ParsedHttpUrl {
    host: String,
    port: u16,
    path: String,
}

fn parse_http_url(url: &str) -> Result<ParsedHttpUrl, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| "Only http:// sync URLs are supported in this MVP".to_string())?;
    let (authority, path) = rest
        .split_once('/')
        .map(|(authority, path)| (authority, format!("/{path}")))
        .unwrap_or((rest, "/".to_string()));
    let (host, port) = if let Some((host, port)) = authority.rsplit_once(':') {
        let port = port
            .parse::<u16>()
            .map_err(|_| format!("Invalid sync URL port: {port}"))?;
        (host.to_string(), port)
    } else {
        (authority.to_string(), 80)
    };
    if host.is_empty() {
        return Err("Sync URL host is empty".to_string());
    }
    Ok(ParsedHttpUrl { host, port, path })
}

fn http_json_request(
    method: &str,
    url: &str,
    body: Option<&str>,
    token: Option<&str>,
) -> Result<String, String> {
    let parsed = parse_http_url(url)?;
    let mut stream = TcpStream::connect((parsed.host.as_str(), parsed.port))
        .map_err(|err| format!("Could not connect to sync server {}:{}: {err}", parsed.host, parsed.port))?;
    stream
        .set_read_timeout(Some(StdDuration::from_secs(10)))
        .map_err(|err| err.to_string())?;
    stream
        .set_write_timeout(Some(StdDuration::from_secs(10)))
        .map_err(|err| err.to_string())?;

    let body = body.unwrap_or("");
    let mut request = format!(
        "{method} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: application/json\r\nContent-Length: {}\r\n",
        parsed.path,
        parsed.host,
        body.as_bytes().len()
    );
    if body.is_empty() {
        request.push_str("Content-Type: application/json\r\n");
    } else {
        request.push_str("Content-Type: application/json; charset=utf-8\r\n");
    }
    if let Some(token) = token.filter(|token| !token.trim().is_empty()) {
        request.push_str(&format!("X-Avalon-Token: {}\r\n", token.trim()));
    }
    request.push_str("\r\n");
    request.push_str(body);

    stream
        .write_all(request.as_bytes())
        .map_err(|err| format!("Could not write sync request: {err}"))?;
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|err| format!("Could not read sync response: {err}"))?;
    let response = String::from_utf8_lossy(&response);
    let (head, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| "Invalid HTTP response from sync server".to_string())?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse::<u16>().ok())
        .ok_or_else(|| "Invalid HTTP status from sync server".to_string())?;
    if !(200..300).contains(&status) {
        return Err(format!("Sync server returned HTTP {status}: {body}"));
    }
    Ok(body.to_string())
}

fn deterministic_jitter(id: i64) -> (f64, f64) {
    let mut x = id as u64;
    x = x.wrapping_mul(6364136223846793005).wrapping_add(1);

    let jx = ((x >> 16) & 0xff) as f64 / 255.0 - 0.5;

    x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
    let jy = ((x >> 16) & 0xff) as f64 / 255.0 - 0.5;

    (jx * 40.0, jy * 40.0)
}

fn parse_current_location_ocr(
    raw_text: &str,
    ocr_lines: &[OcrLine],
    known_locations: &[String],
) -> ParsedCurrentLocation {
    let lines = ocr_candidate_lines(raw_text, ocr_lines);
    let mut ignored_lines = Vec::new();
    let mut candidates = Vec::new();

    for line in lines {
        let cleaned = clean_current_location_candidate(&line);

        if cleaned.is_empty() || !is_current_location_candidate_like(&cleaned, known_locations) {
            ignored_lines.push(format!("{line} -> {cleaned}"));
            continue;
        }

        candidates.push(cleaned);
    }

    // Paddle can split the plaque into fragments or return noisy combined text.
    // Try the whole raw OCR text as one candidate before failing.
    if candidates.is_empty() {
        let raw_joined = raw_text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join(" ");

        let cleaned = clean_current_location_candidate(&raw_joined);

        if !cleaned.is_empty() && is_current_location_candidate_like(&cleaned, known_locations) {
            candidates.push(cleaned);
        } else {
            ignored_lines.push(format!("RAW_FALLBACK: {raw_joined} -> {cleaned}"));
        }
    }

    candidates.sort_by(|left, right| {
        current_location_candidate_rank(right, known_locations)
            .partial_cmp(&current_location_candidate_rank(left, known_locations))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates.dedup();

    let Some(best_raw) = candidates.first().cloned() else {
        return ParsedCurrentLocation {
            location_name: None,
            cleaned_candidate: String::new(),
            matched_location_name: None,
            matched_location_score: 0.0,
            used_dictionary_match: false,
            match_reason: format!(
                "No name-like current-location line survived plaque cleanup. raw={:?}",
                raw_text
            ),
            confidence: 0.0,
            ignored_lines,
            candidates,
            top_matches: Vec::new(),
            reason: format!(
                "No name-like current-location line survived plaque cleanup. raw={:?}",
                raw_text
            ),
        };
    };

    let best = match_current_location(&best_raw, known_locations);
    log_current_location_match(raw_text, &best, &best_raw);
    ParsedCurrentLocation {
        location_name: Some(best.final_name.clone()),
        cleaned_candidate: best.cleaned_candidate,
        matched_location_name: best.matched_location_name,
        matched_location_score: best.matched_location_score,
        used_dictionary_match: best.used_dictionary_match,
        match_reason: best.match_reason,
        confidence: best.confidence,
        ignored_lines,
        candidates,
        top_matches: best.top5,
        reason: "Removed tier, numbers, timers, and icon-like plaque fragments".to_string(),
    }
}

fn log_current_location_match(raw_text: &str, selection: &CurrentLocationSelection, candidate: &str) {
    eprintln!("[ocr-match] raw={raw_text:?}");
    eprintln!("[ocr-match] cleaned={candidate:?}");
    eprintln!(
        "[ocr-match] top5={}",
        selection
            .top5
            .iter()
            .map(|match_item| format!("{}:{:.3}", match_item.name, match_item.score))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    eprintln!(
        "[ocr-match] chosen={} score={:.3} dictionary_match={} reason={}",
        selection.final_name,
        selection.matched_location_score,
        selection.used_dictionary_match,
        selection.match_reason
    );
}

fn current_location_candidate_rank(candidate: &str, known_locations: &[String]) -> f64 {
    let normalized_candidate = normalize_location_name(candidate);
    if normalized_candidate.is_empty() {
        return 0.0;
    }
    let words = tokens(&normalized_candidate).len() as f64;
    let has_dictionary_hit = known_locations
        .iter()
        .any(|name| normalize_location_name(name) == normalized_candidate);
    let substring_hit = known_locations.iter().any(|name| {
        let normalized_name = normalize_location_name(name);
        normalized_name.contains(&normalized_candidate)
            || (normalized_candidate.len() >= 4 && normalized_candidate.contains(&normalized_name))
    });
    let hyphen_penalty = if normalized_candidate.contains('-') { 0.08 } else { 0.0 };
    let len_bonus = (normalized_candidate.len() as f64 / 48.0).min(0.18);
    words * 0.12 + len_bonus + if has_dictionary_hit { 1.0 } else { 0.0 } + if substring_hit { 0.35 } else { 0.0 } - hyphen_penalty
}

fn parse_portal_tooltip_ocr(
    raw_text: &str,
    ocr_lines: &[OcrLine],
    known_locations: &[String],
) -> ParsedPortalTooltip {
    let lines = ocr_candidate_lines(raw_text, ocr_lines);
    let mut ignored_lines = Vec::new();
    let mut candidates = Vec::new();
    let mut slots = None;
    let mut duration = None;
    let mut fallback_colon_duration = None;

    for (index, line) in lines.iter().enumerate() {
        let next_line = lines.get(index + 1).map(String::as_str);
        if slots.is_none() {
            slots = parse_slots(line);
        }
        if duration.is_none() {
            duration = parse_reasonable_duration_seconds(line)
                .or_else(|| parse_noisy_portal_duration_seconds(line))
                .or_else(|| parse_noisy_portal_duration_with_next(line, next_line));
        }
        if fallback_colon_duration.is_none() {
            fallback_colon_duration = parse_colon_timer_seconds(line);
        }

        if is_portal_title_line(line)
            || parse_slots(line).is_some()
            || parse_reasonable_duration_seconds(line).is_some()
            || parse_noisy_portal_duration_seconds(line).is_some()
            || is_plain_timer_line(line)
            || is_icon_garbage(line)
        {
            ignored_lines.push(line.clone());
            continue;
        }
        let cleaned = clean_portal_destination_candidate(line);
        if !cleaned.is_empty() && is_portal_destination_candidate_like(&cleaned) {
            candidates.push(cleaned);
        } else {
            ignored_lines.push(line.clone());
        }
    }

    candidates.sort_by(|a, b| {
        let a_score = portal_candidate_score(a, ocr_lines);
        let b_score = portal_candidate_score(b, ocr_lines);
        b_score.partial_cmp(&a_score).unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates.dedup();

    let destination = candidates
        .first()
        .cloned()
        .map(|candidate| choose_location_match(&candidate, known_locations, &[], true).name);
    let confidence = destination
        .as_ref()
        .map(|_| candidates.first().map(|c| portal_candidate_score(c, ocr_lines)).unwrap_or(0.55).min(0.95))
        .unwrap_or(0.0);
    let (slots_used, slots_total) = slots.unwrap_or((None, None));

    ParsedPortalTooltip {
        destination_name: destination,
        slots_used,
        slots_total,
        expires_in_seconds: duration.or(fallback_colon_duration),
        confidence,
        ignored_lines,
        candidates,
        reason: "Ignored title, slots, timers, duration text, and icon-like tooltip fragments".to_string(),
    }
}

fn ocr_candidate_lines(raw_text: &str, ocr_lines: &[OcrLine]) -> Vec<String> {
    let mut lines = if ocr_lines.is_empty() {
        raw_text.lines().map(|line| line.to_string()).collect::<Vec<_>>()
    } else {
        let mut lines = ocr_lines.to_vec();
        lines.sort_by(|a, b| {
            let ay = a.bbox.as_ref().map(|bbox| bbox.y).unwrap_or(0.0);
            let by = b.bbox.as_ref().map(|bbox| bbox.y).unwrap_or(0.0);
            by.partial_cmp(&ay).unwrap_or(std::cmp::Ordering::Equal)
        });
        lines.into_iter().map(|line| line.text).collect()
    };
    if !ocr_lines.is_empty() {
        let joined = joined_current_location_ocr_lines(ocr_lines);
        if !joined.is_empty() {
            lines.push(joined);
        }
    }
    lines.extend(raw_text.lines().map(|line| line.to_string()));
    lines
        .into_iter()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

fn joined_current_location_ocr_lines(ocr_lines: &[OcrLine]) -> String {
    let mut lines = ocr_lines.to_vec();
    lines.sort_by(|left, right| {
        let left_y = left.bbox.as_ref().map(|bbox| bbox.y).unwrap_or(0.0);
        let right_y = right.bbox.as_ref().map(|bbox| bbox.y).unwrap_or(0.0);
        let left_x = left.bbox.as_ref().map(|bbox| bbox.x).unwrap_or(0.0);
        let right_x = right.bbox.as_ref().map(|bbox| bbox.x).unwrap_or(0.0);
        left_y
            .partial_cmp(&right_y)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                left_x
                    .partial_cmp(&right_x)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });

    lines
        .into_iter()
        .map(|line| line.text)
        .collect::<Vec<_>>()
        .join(" ")
}

fn clean_current_location_candidate(line: &str) -> String {
    line.split_whitespace()
        .filter_map(|token| {
            let cleaned = clean_name_token(token);
            if cleaned.is_empty()
                || is_roman_tier(&cleaned)
                || cleaned.chars().all(|ch| ch.is_ascii_digit())
                || is_timer_token(&cleaned)
            {
                None
            } else {
                Some(cleaned)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

fn clean_portal_destination_candidate(line: &str) -> String {
    line.split_whitespace()
        .filter_map(|token| {
            let cleaned = clean_name_token(token);
            if cleaned.is_empty() || is_timer_token(&cleaned) || cleaned.chars().all(|ch| ch.is_ascii_digit()) {
                None
            } else {
                Some(cleaned)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn clean_name_token(token: &str) -> String {
    token
        .chars()
        .map(|ch| {
            if ch.is_alphabetic() || matches!(ch, '-' | '\'') {
                ch
            } else {
                ' '
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_string()
}

fn is_roman_tier(value: &str) -> bool {
    matches!(
        value.to_ascii_uppercase().as_str(),
        "I" | "II" | "III" | "IV" | "V" | "VI" | "VII" | "VIII"
    )
}

fn is_timer_token(value: &str) -> bool {
    let parts = value.split(':').collect::<Vec<_>>();
    parts.len() == 2
        && (1..=2).contains(&parts[0].len())
        && parts[1].len() == 2
        && parts.iter().all(|part| part.chars().all(|ch| ch.is_ascii_digit()))
}

fn is_plain_timer_line(line: &str) -> bool {
    line.split_whitespace().any(is_timer_token)
}

fn is_portal_title_line(line: &str) -> bool {
    let lower = line.to_lowercase();
    ["путь авалона", "авалона", "avalon", "road", "roads", "portal", "портал"]
        .iter()
        .any(|needle| lower.contains(needle))
}

fn is_icon_garbage(line: &str) -> bool {
    let letters = line.chars().filter(|ch| ch.is_alphabetic()).count();
    letters <= 1 || line.len() <= 2
}

fn has_name_letters(value: &str) -> bool {
    value.chars().filter(|ch| ch.is_alphabetic()).count() >= 3
}

fn is_current_location_candidate_like(value: &str, known_locations: &[String]) -> bool {
    let value = value.trim();

    if value.is_empty() {
        return false;
    }

    let normalized = normalize_location_name(value);

    if normalized.is_empty() {
        return false;
    }

    // Exact dictionary hit.
    if known_locations
        .iter()
        .any(|name| normalize_location_name(name) == normalized)
    {
        return true;
    }

    // One-word hyphenated Avalon road names, e.g. Hures-Ugumtum.
    if is_hyphenated_location_like(value) {
        return true;
    }

    let alpha_tokens = tokens(&normalized)
        .into_iter()
        .filter(|token| token.chars().any(|ch| ch.is_alphabetic()))
        .collect::<Vec<_>>();
    let alpha_count = normalized.chars().filter(|ch| ch.is_alphabetic()).count();

    // Normal two-word biome names, e.g. Shaleheath Hills.
    if alpha_tokens.len() >= 2 {
        return alpha_count >= 6 && alpha_tokens.iter().all(|token| token.len() >= 2);
    }

    // One-token non-hyphenated names are risky, accept only if dictionary-related.
    if alpha_tokens.len() == 1 {
        if alpha_count < 5 {
            return false;
        }
        return known_locations.iter().any(|name| {
            let normalized_name = normalize_location_name(name);
            normalized_name == normalized
                || normalized_name.starts_with(&(normalized.clone() + " "))
                || is_strong_levenshtein_location_match(&normalized, name)
        });
    }

    false
}

fn is_hyphenated_location_like(value: &str) -> bool {
    let value = value.trim();

    if value.len() < 5 || !value.contains('-') {
        return false;
    }

    let parts = value.split('-').collect::<Vec<_>>();

    if parts.len() != 2 {
        return false;
    }

    parts.iter().all(|part| {
        let letters = part.chars().filter(|ch| ch.is_alphabetic()).count();
        letters >= 3 && part.chars().all(|ch| ch.is_alphabetic() || ch == '\'')
    })
}

fn is_portal_destination_candidate_like(value: &str) -> bool {
    let normalized = normalize_location_name(value);
    if normalized.is_empty() || !has_name_letters(&normalized) {
        return false;
    }

    if is_hyphenated_location_like(value) {
        return true;
    }

    let alpha_tokens = tokens(&normalized)
        .into_iter()
        .filter(|token| token.chars().any(|ch| ch.is_alphabetic()))
        .collect::<Vec<_>>();

    if alpha_tokens.len() >= 2 {
        return alpha_tokens.iter().all(|token| token.len() >= 2);
    }

    // One-word Royal locations, e.g. Aspenwood, Lymhurst, Martlock.
    if alpha_tokens.len() == 1 {
        let token = &alpha_tokens[0];

        if token.len() < 5 {
            return false;
        }

        let lower = normalized.to_lowercase();

        let forbidden = [
            "road", "roads", "avalon", "portal", "closes", "close", "expires",
            "enter", "exit",
        ];

        return !forbidden.iter().any(|word| lower.contains(word));
    }

    false
}

fn parse_slots(line: &str) -> Option<(Option<i64>, Option<i64>)> {
    for token in line.split_whitespace() {
        let compact = token.chars().filter(|ch| ch.is_ascii_digit() || *ch == '/').collect::<String>();
        let parts = compact.split('/').collect::<Vec<_>>();
        if parts.len() == 2 {
            if let (Ok(used), Ok(total)) = (parts[0].parse::<i64>(), parts[1].parse::<i64>()) {
                return Some((Some(used), Some(total)));
            }
        }
    }
    let compact = line.chars().filter(|ch| ch.is_ascii_digit() || *ch == '/').collect::<String>();
    let parts = compact.split('/').collect::<Vec<_>>();
    if parts.len() == 2 {
        if let (Ok(used), Ok(total)) = (parts[0].parse::<i64>(), parts[1].parse::<i64>()) {
            return Some((Some(used), Some(total)));
        }
    }
    None
}

fn parse_duration_seconds(line: &str) -> Option<i64> {
    let lower = line
        .to_lowercase()
        .replace("hours", "h")
        .replace("hour", "h")
        .replace("hrs", "h")
        .replace("hr", "h")
        .replace("minutes", "m")
        .replace("minute", "m")
        .replace("mins", "m")
        .replace("min", "m")
        .replace("seconds", "s")
        .replace("second", "s")
        .replace("secs", "s")
        .replace("sec", "s");
    let lower = merge_split_hour_digits(&lower);
    let chars = lower.chars().collect::<Vec<_>>();
    let mut index = 0;
    let mut seconds = 0;
    let mut found_unit = false;
    while index < chars.len() {
        if !chars[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        let start = index;
        while index < chars.len() && chars[index].is_ascii_digit() {
            index += 1;
        }
        let number = chars[start..index].iter().collect::<String>().parse::<i64>().ok()?;
        while index < chars.len() && chars[index].is_whitespace() {
            index += 1;
        }
        let unit = chars.get(index).copied();
        match unit {
            Some('h') | Some('ч') => {
                seconds += number * 3600;
                found_unit = true;
            }
            Some('m') | Some('м') => {
                seconds += number * 60;
                found_unit = true;
            }
            Some('s') | Some('с') => {
                seconds += number;
                found_unit = true;
            }
            _ => {}
        }
        index += 1;
    }
    found_unit.then_some(seconds)
}

fn merge_split_hour_digits(line: &str) -> String {
    let tokens = line.split_whitespace().collect::<Vec<_>>();
    if tokens.len() < 3 {
        return line.to_string();
    }

    let mut merged = Vec::<String>::new();
    let mut index = 0;

    while index < tokens.len() {
        if index + 2 < tokens.len()
            && is_single_digit_token(tokens[index])
            && is_single_digit_token(tokens[index + 1])
            && is_hour_unit_token(tokens[index + 2])
        {
            merged.push(format!("{}{}", tokens[index], tokens[index + 1]));
            merged.push(tokens[index + 2].to_string());
            index += 3;
            continue;
        }

        merged.push(tokens[index].to_string());
        index += 1;
    }

    merged.join(" ")
}

fn is_single_digit_token(token: &str) -> bool {
    token.len() == 1 && token.chars().all(|ch| ch.is_ascii_digit())
}

fn is_hour_unit_token(token: &str) -> bool {
    matches!(token, "h" | "ч")
}

fn parse_reasonable_duration_seconds(line: &str) -> Option<i64> {
    parse_duration_seconds(line).filter(|seconds| (0..=24 * 60 * 60).contains(seconds))
}

fn parse_noisy_portal_duration_seconds(line: &str) -> Option<i64> {
    if !looks_like_portal_close_line(line) {
        return None;
    }
    parse_noisy_duration_value(line, true)
}

fn parse_noisy_portal_duration_with_next(line: &str, next_line: Option<&str>) -> Option<i64> {
    if !looks_like_portal_close_line(line) {
        return None;
    }
    let next_line = next_line?;
    parse_noisy_duration_value(next_line, true).or_else(|| parse_reasonable_duration_seconds(next_line))
}

fn parse_noisy_duration_value(line: &str, allow_hour_minute: bool) -> Option<i64> {
    if allow_hour_minute {
        if let Some(seconds) = parse_compact_noisy_hour_minute(line) {
            return Some(seconds);
        }
    }
    let numbers = standalone_number_groups(line);
    if numbers.len() < 2 {
        return None;
    }
    if allow_hour_minute {
        if let Some(seconds) = parse_split_noisy_hour_minute(&numbers) {
            return Some(seconds);
        }
    }
    let first = numbers[numbers.len() - 2];
    let second = normalize_ocr_seconds(numbers[numbers.len() - 1])?;
    if allow_hour_minute && looks_like_hour_minute_duration(line) && first <= 24 && second <= 59 {
        return Some(first * 3600 + second * 60);
    }
    if first > 180 || second > 59 {
        return None;
    }
    Some(first * 60 + second)
}

fn parse_compact_noisy_hour_minute(line: &str) -> Option<i64> {
    let compact = line
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase();
    if !compact.ends_with('m') {
        return None;
    }
    let body = compact.trim_end_matches('m');
    if body.len() == 5 {
        let chars = body.chars().collect::<Vec<_>>();
        if chars.get(2) == Some(&'4')
            && chars.iter().enumerate().all(|(index, ch)| index == 2 || ch.is_ascii_digit())
        {
            let hours = chars[0..2].iter().collect::<String>().parse::<i64>().ok()?;
            let minutes = chars[3..5].iter().collect::<String>().parse::<i64>().ok()?;
            if hours <= 24 && minutes <= 59 {
                return Some(hours * 3600 + minutes * 60);
            }
        }
    }
    if body.len() == 4 && body.chars().all(|ch| ch.is_ascii_digit()) {
        let hours = body[0..2].parse::<i64>().ok()?;
        let minutes = body[2..4].parse::<i64>().ok()?;
        if hours <= 24 && minutes <= 59 {
            return Some(hours * 3600 + minutes * 60);
        }
    }
    None
}

fn parse_split_noisy_hour_minute(numbers: &[i64]) -> Option<i64> {
    if numbers.len() < 3 {
        return None;
    }
    let marker = numbers[numbers.len() - 2];
    if marker != 4 {
        return None;
    }
    let hours = numbers[numbers.len() - 3];
    let minutes = numbers[numbers.len() - 1];
    if hours <= 24 && minutes <= 59 {
        return Some(hours * 3600 + minutes * 60);
    }
    None
}

fn looks_like_hour_minute_duration(line: &str) -> bool {
    let lower = line.to_lowercase();
    lower.contains('h') || lower.contains('ч') || lower.contains("hour") || lower.contains("hr")
}

fn looks_like_portal_close_line(line: &str) -> bool {
    let lower = line.to_lowercase();
    let letters = lower
        .chars()
        .filter(|ch| ch.is_alphabetic())
        .collect::<String>();
    lower.contains("закро")
        || lower.contains("через")
        || lower.contains("closes")
        || lower.contains("close")
        || lower.contains("expires")
        || lower.contains("3ok")
        || lower.contains("wepe")
        || levenshtein(&letters, "zakroetsacherez") <= 8
}

fn standalone_number_groups(line: &str) -> Vec<i64> {
    let chars = line.chars().collect::<Vec<_>>();
    let mut numbers = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if !chars[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        let start = index;
        while index < chars.len() && chars[index].is_ascii_digit() {
            index += 1;
        }
        let before_alpha = start > 0 && chars[start - 1].is_alphabetic();
        let after_alpha = index < chars.len() && chars[index].is_alphabetic();
        if !before_alpha && !after_alpha {
            if let Ok(value) = chars[start..index].iter().collect::<String>().parse::<i64>() {
                numbers.push(value);
            }
        }
    }
    numbers
}

fn normalize_ocr_seconds(value: i64) -> Option<i64> {
    if (0..=59).contains(&value) {
        return Some(value);
    }
    if (100..=599).contains(&value) {
        let truncated = value / 10;
        if truncated <= 59 {
            return Some(truncated);
        }
    }
    None
}

fn parse_colon_timer_seconds(line: &str) -> Option<i64> {
    for token in line.split_whitespace() {
        if is_timer_token(token) {
            let parts = token.split(':').collect::<Vec<_>>();
            let minutes = parts[0].parse::<i64>().ok()?;
            let seconds = parts[1].parse::<i64>().ok()?;
            return Some(minutes * 60 + seconds);
        }
    }
    None
}

fn portal_candidate_score(candidate: &str, lines: &[OcrLine]) -> f64 {
    let mut score: f64 = 0.55;
    if candidate.contains('-') {
        score += 0.25;
    }
    if candidate.split_whitespace().count() > 1 {
        score += 0.08;
    }
    if let Some(line) = lines.iter().find(|line| line.text.contains(candidate)) {
        score += line.confidence.unwrap_or(0.0) * 0.12;
        score += line.bbox.as_ref().map(|bbox| bbox.height.min(0.2) * 0.5).unwrap_or(0.0);
    }
    score.min(0.98)
}

fn known_location_names(conn: &Connection) -> Result<Vec<String>, String> {
    let mut candidates = load_primary_location_dictionary()?;
    candidates.extend(load_location_names(conn)?);
    candidates.sort();
    candidates.dedup_by(|a, b| normalize_location_name(a) == normalize_location_name(b));
    Ok(candidates)
}

fn match_current_location(cleaned_candidate: &str, primary: &[String]) -> CurrentLocationSelection {
    let query = normalize_location_name(cleaned_candidate);
    let top5 = rank_current_locations(&query, primary)
        .into_iter()
        .take(5)
        .collect::<Vec<_>>();
    let exact_or_substring = extract_dictionary_location_candidates(&query, primary);
    let extracted_best = exact_or_substring
        .iter()
        .map(|name| LocationMatch {
            name: name.clone(),
            score: score_current_location_match(&query, &normalize_location_name(name)),
        })
        .max_by(|left, right| {
            left.score
                .partial_cmp(&right.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    let fuzzy_best = top5.first().map(|ranked| LocationMatch {
        name: ranked.name.clone(),
        score: ranked.score,
    });
    let best_candidate = extracted_best
        .clone()
        .or(fuzzy_best.clone())
        .unwrap_or(LocationMatch {
            name: cleaned_candidate.to_string(),
            score: 0.0,
        });

    let alpha_words = tokens(&query)
        .into_iter()
        .filter(|token| token.chars().any(|ch| ch.is_alphabetic()))
        .collect::<Vec<_>>();
    let candidate_tokens = tokens(&normalize_location_name(&best_candidate.name))
        .into_iter()
        .filter(|token| token.chars().any(|ch| ch.is_alphabetic()))
        .collect::<Vec<_>>();
    let has_multi_word_candidate = alpha_words.len() >= 2;
    let exact_match = query == normalize_location_name(&best_candidate.name);
    let levenshtein_match = is_strong_levenshtein_location_match(&query, &best_candidate.name);
    let avalon_compound_match = has_multi_word_candidate
        && best_candidate.name.contains('-')
        && alpha_words.len() == candidate_tokens.len()
        && same_token_initials_score(&alpha_words, &candidate_tokens) >= 1.0
        && token_prefix_similarity(&alpha_words, &candidate_tokens) >= 0.30
        && best_candidate.score >= 0.45;
    let strong_match = if alpha_words.len() == 1 && !exact_match {
        levenshtein_match
    } else {
        levenshtein_match
            ||
        avalon_compound_match
            || best_candidate.score >= 0.88
            && (!has_multi_word_candidate
                || !best_candidate.name.contains('-')
                || query.contains('-')
                || exact_match)
    };
    let medium_match = best_candidate.score >= 0.75;
    let used_dictionary_match = strong_match;
    let final_name = if strong_match {
        best_candidate.name.clone()
    } else {
        cleaned_candidate.to_string()
    };
    let match_reason = if exact_match {
        "exact cleaned candidate".to_string()
    } else if strong_match && levenshtein_match {
        "levenshtein dictionary match".to_string()
    } else if strong_match {
        "high-confidence dictionary match".to_string()
    } else if extracted_best.is_some() {
        "substring dictionary candidate".to_string()
    } else if medium_match {
        "medium-confidence needs confirmation".to_string()
    } else {
        "low-confidence fallback".to_string()
    };

    CurrentLocationSelection {
        final_name,
        cleaned_candidate: cleaned_candidate.to_string(),
        matched_location_name: Some(best_candidate.name),
        matched_location_score: best_candidate.score,
        used_dictionary_match,
        match_reason,
        confidence: if used_dictionary_match { best_candidate.score } else { best_candidate.score.min(0.84) },
        top5,
    }
}

fn match_location_name(
    conn: &Connection,
    raw_text: &str,
    cleaned_candidate: &str,
    portal_name: bool,
) -> Result<LocationMatchResult, String> {
    let primary = load_primary_location_dictionary()?;
    let secondary = load_location_names(conn)?;
    let chosen = choose_location_match(cleaned_candidate, &primary, &secondary, portal_name);
    let top5 = rank_locations(cleaned_candidate, &primary)
        .into_iter()
        .take(5)
        .collect::<Vec<_>>();
    log_location_match(raw_text, cleaned_candidate, &top5, &chosen);
    Ok(LocationMatchResult {
        chosen,
        cleaned_candidate: cleaned_candidate.to_string(),
        top5,
    })
}

fn load_location_names(conn: &Connection) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare("SELECT name FROM locations")
        .map_err(db_err)?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(db_err)?;
    let mut names = rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_err)?;
    names = normalize_location_list(names);
    Ok(names)
}

fn log_location_dictionary_summary(locations: &[String]) {
    let targets = [
        "Xebos-Emimsum",
        "Eldon Hill",
        "Blackthorn Quarry",
        "Shaleheath Hills",
    ];
    let mut summary = vec![format!("total={}", locations.len())];
    for target in targets {
        let exists = locations.iter().any(|name| normalize_location_name(name) == normalize_location_name(target));
        summary.push(format!("{target}={}", if exists { "yes" } else { "missing" }));
    }
    eprintln!("[location-dict] {}", summary.join(" "));
}

fn load_primary_location_dictionary() -> Result<Vec<String>, String> {
    Ok(PRIMARY_LOCATION_DICTIONARY
        .get_or_init(|| {
            let path = bundled_or_project_path("data/albion_locations_all.txt");
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|_| include_str!("../../data/albion_locations_all.txt").to_string());
            let locations = normalize_location_list(
                text.lines().map(|line| line.to_string()).collect::<Vec<_>>(),
            );
            log_location_dictionary_summary(&locations);
            locations
        })
        .clone())
}

fn extract_dictionary_location_candidates(candidate: &str, dictionary: &[String]) -> Vec<String> {
    let query = normalize_location_name(candidate);
    if query.is_empty() {
        return Vec::new();
    }
    let query_tokens = tokens(&query);
    let mut matches = dictionary
        .iter()
        .filter_map(|name| {
            let normalized_name = normalize_location_name(name);
            if normalized_name == query {
                return Some(name.clone());
            }
            if normalized_name.contains(&query) {
                return Some(name.clone());
            }
            if query_tokens.len() == 1 && normalized_name.starts_with(&query) {
                return Some(name.clone());
            }
            if query_tokens.len() == 1 && normalized_name.split_whitespace().any(|token| token == query) {
                return Some(name.clone());
            }
            None
        })
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| {
        let left_norm = normalize_location_name(left);
        let right_norm = normalize_location_name(right);
        right_norm
            .len()
            .cmp(&left_norm.len())
            .then_with(|| left.cmp(right))
    });
    matches.dedup();
    matches
}

fn rank_current_locations(query: &str, candidates: &[String]) -> Vec<RankedLocation> {
    let normalized_query = normalize_location_name(query);
    let mut ranked = candidates
        .iter()
        .map(|candidate| RankedLocation {
            name: candidate.clone(),
            score: score_current_location_match(&normalized_query, &normalize_location_name(candidate)),
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    ranked
}

fn score_current_location_match(query: &str, candidate: &str) -> f64 {
    let query = normalize_location_name(query);
    let candidate = normalize_location_name(candidate);

    if query.is_empty() || candidate.is_empty() {
        return 0.0;
    }

    if query == candidate {
        return 1.0;
    }

    let distance = levenshtein(&query, &candidate) as f64;
    let max_len = query.chars().count().max(candidate.chars().count()) as f64;

    if max_len == 0.0 {
        return 0.0;
    }

    (1.0 - distance / max_len).clamp(0.0, 1.0)
}

fn is_strong_levenshtein_location_match(query: &str, candidate_name: &str) -> bool {
    let candidate = normalize_location_name(candidate_name);
    if query.is_empty() || candidate.is_empty() {
        return false;
    }
    if query == candidate {
        return true;
    }

    let query_tokens = tokens(query)
        .into_iter()
        .filter(|token| token.chars().any(|ch| ch.is_alphabetic()))
        .collect::<Vec<_>>();
    let candidate_tokens = tokens(&candidate)
        .into_iter()
        .filter(|token| token.chars().any(|ch| ch.is_alphabetic()))
        .collect::<Vec<_>>();
    if query_tokens.is_empty() || query_tokens.len() != candidate_tokens.len() {
        return false;
    }

    let distance = levenshtein(query, &candidate);
    let max_len = query.chars().count().max(candidate.chars().count());
    let allowed_distance = if max_len <= 6 {
        1
    } else if max_len <= 12 {
        2
    } else {
        3
    };

    distance <= allowed_distance && score_current_location_match(query, &candidate) >= 0.78
}

fn same_token_initials_score(query_tokens: &[String], candidate_tokens: &[String]) -> f64 {
    if query_tokens.is_empty() || candidate_tokens.is_empty() {
        return 0.0;
    }
    let shared = query_tokens
        .iter()
        .zip(candidate_tokens.iter())
        .filter(|(query_token, candidate_token)| {
            query_token.chars().next() == candidate_token.chars().next()
        })
        .count();
    shared as f64 / query_tokens.len().max(candidate_tokens.len()) as f64
}

fn token_prefix_similarity(query_tokens: &[String], candidate_tokens: &[String]) -> f64 {
    let pairs = query_tokens.iter().zip(candidate_tokens.iter()).collect::<Vec<_>>();
    if pairs.is_empty() {
        return 0.0;
    }
    let total = pairs
        .iter()
        .map(|(query_token, candidate_token)| {
            let query_token = normalize_location_name(query_token);
            let candidate_token = normalize_location_name(candidate_token);
            let prefix = query_token
                .chars()
                .zip(candidate_token.chars())
                .take_while(|(left, right)| left == right)
                .count();
            prefix as f64 / query_token.len().max(candidate_token.len()).max(1) as f64
        })
        .sum::<f64>();
    total / pairs.len() as f64
}

fn longest_common_substring_score(a: &str, b: &str) -> f64 {
    let a_chars = a.chars().collect::<Vec<_>>();
    let b_chars = b.chars().collect::<Vec<_>>();
    if a_chars.is_empty() || b_chars.is_empty() {
        return 0.0;
    }
    let mut dp = vec![vec![0usize; b_chars.len() + 1]; a_chars.len() + 1];
    let mut best = 0usize;
    for i in 1..=a_chars.len() {
        for j in 1..=b_chars.len() {
            if a_chars[i - 1] == b_chars[j - 1] {
                dp[i][j] = dp[i - 1][j - 1] + 1;
                best = best.max(dp[i][j]);
            }
        }
    }
    best as f64 / a_chars.len().max(b_chars.len()) as f64
}

fn normalize_location_list(names: Vec<String>) -> Vec<String> {
    let mut deduped = Vec::<String>::new();
    let mut seen = std::collections::HashSet::<String>::new();
    for name in names {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            continue;
        }
        let normalized = normalize_location_name(trimmed);
        if normalized.is_empty() || !seen.insert(normalized) {
            continue;
        }
        deduped.push(trimmed.to_string());
    }
    deduped
}

fn choose_location_match(
    cleaned_candidate: &str,
    primary: &[String],
    secondary: &[String],
    portal_name: bool,
) -> LocationMatch {
    let query = normalize_location_name(cleaned_candidate);
    let ranked_primary = rank_locations(cleaned_candidate, primary);
    let best_primary = ranked_primary
        .first()
        .cloned()
        .map(|entry| LocationMatch {
            name: entry.name,
            score: entry.score,
        })
        .unwrap_or(LocationMatch {
            name: cleaned_candidate.to_string(),
            score: 0.0,
        });
    let primary_threshold = if portal_name && cleaned_candidate.contains('-') { 0.90 } else { 0.70 };
    let fallback_threshold = if portal_name && cleaned_candidate.contains('-') { 0.92 } else { 0.80 };

    let secondary_best = rank_locations(cleaned_candidate, secondary)
        .first()
        .cloned()
        .map(|entry| LocationMatch {
            name: entry.name,
            score: entry.score,
        });
    let chosen = if best_primary.score >= primary_threshold {
        best_primary
    } else if let Some(secondary_best) = secondary_best {
        if secondary_best.score >= fallback_threshold {
            secondary_best
        } else if portal_name && cleaned_candidate.contains('-') {
            LocationMatch {
                name: cleaned_candidate.to_string(),
                score: best_primary.score.max(secondary_best.score),
            }
        } else {
            best_primary
        }
    } else {
        best_primary
    };

    if query.is_empty() {
        return LocationMatch {
            name: cleaned_candidate.to_string(),
            score: 0.0,
        };
    }
    chosen
}

fn rank_locations(query: &str, candidates: &[String]) -> Vec<RankedLocation> {
    let normalized_query = normalize_location_name(query);
    let mut ranked = candidates
        .iter()
        .map(|candidate| RankedLocation {
            name: candidate.clone(),
            score: match_score(&normalized_query, &normalize_location_name(candidate)),
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    ranked
}

fn log_location_match(raw_text: &str, cleaned_candidate: &str, top5: &[RankedLocation], chosen: &LocationMatch) {
    eprintln!("[ocr-match] raw={raw_text:?}");
    eprintln!("[ocr-match] cleaned={cleaned_candidate:?}");
    eprintln!(
        "[ocr-match] top5={}",
        top5.iter()
            .map(|match_item| format!("{}:{:.3}", match_item.name, match_item.score))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    eprintln!("[ocr-match] chosen={} score={:.3}", chosen.name, chosen.score);
}

fn match_score(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    let lev = 1.0 - (levenshtein(a, b) as f64 / a.len().max(b.len()).max(1) as f64);
    let token = token_similarity(a, b);
    let jw = jaro_winkler(a, b);
    let prefix = prefix_similarity(a, b);
    let skeleton = skeleton_similarity(a, b);
    (lev * 0.30 + token * 0.16 + jw * 0.24 + prefix * 0.14 + skeleton * 0.16).clamp(0.0, 1.0)
}

fn token_similarity(a: &str, b: &str) -> f64 {
    let a_tokens = tokens(a);
    let b_tokens = tokens(b);
    if a_tokens.is_empty() || b_tokens.is_empty() {
        return 0.0;
    }
    let hits = a_tokens
        .iter()
        .filter(|token| b_tokens.iter().any(|other| other == *token))
        .count();
    hits as f64 / a_tokens.len().max(b_tokens.len()) as f64
}

fn prefix_similarity(a: &str, b: &str) -> f64 {
    let min_len = a.len().min(b.len());
    if min_len == 0 {
        return 0.0;
    }
    let prefix = a.chars().zip(b.chars()).take_while(|(left, right)| left == right).count();
    prefix as f64 / min_len as f64
}

fn skeleton_similarity(a: &str, b: &str) -> f64 {
    let left = consonant_skeleton(a);
    let right = consonant_skeleton(b);
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let lev = 1.0 - (levenshtein(&left, &right) as f64 / left.len().max(right.len()).max(1) as f64);
    lev.clamp(0.0, 1.0)
}

fn consonant_skeleton(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_alphabetic())
        .flat_map(|ch| ch.to_lowercase())
        .filter(|ch| !matches!(ch, 'a' | 'e' | 'i' | 'o' | 'u' | 'y'))
        .collect()
}

fn tokens(value: &str) -> Vec<String> {
    value
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(|token| token.to_string())
        .collect()
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a = a.chars().collect::<Vec<_>>();
    let b = b.chars().collect::<Vec<_>>();
    let mut costs = (0..=b.len()).collect::<Vec<_>>();
    for (i, ca) in a.iter().enumerate() {
        let mut last = i;
        costs[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let old = costs[j + 1];
            costs[j + 1] = if ca == cb {
                last
            } else {
                1 + last.min(costs[j]).min(costs[j + 1])
            };
            last = old;
        }
    }
    costs[b.len()]
}

fn jaro_winkler(a: &str, b: &str) -> f64 {
    let jaro = jaro(a, b);
    let prefix = a
        .chars()
        .zip(b.chars())
        .take_while(|(left, right)| left == right)
        .take(4)
        .count() as f64;
    jaro + prefix * 0.1 * (1.0 - jaro)
}

fn jaro(a: &str, b: &str) -> f64 {
    let a = a.chars().collect::<Vec<_>>();
    let b = b.chars().collect::<Vec<_>>();
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let distance = (a.len().max(b.len()) / 2).saturating_sub(1);
    let mut a_match = vec![false; a.len()];
    let mut b_match = vec![false; b.len()];
    let mut matches = 0.0;
    for i in 0..a.len() {
        let start = i.saturating_sub(distance);
        let end = (i + distance + 1).min(b.len());
        for j in start..end {
            if !b_match[j] && a[i] == b[j] {
                a_match[i] = true;
                b_match[j] = true;
                matches += 1.0;
                break;
            }
        }
    }
    if matches == 0.0 {
        return 0.0;
    }
    let mut t = 0.0;
    let mut j = 0;
    for i in 0..a.len() {
        if !a_match[i] {
            continue;
        }
        while j < b.len() && !b_match[j] {
            j += 1;
        }
        if j < b.len() && a[i] != b[j] {
            t += 1.0;
        }
        j += 1;
    }
    ((matches / a.len() as f64) + (matches / b.len() as f64) + ((matches - t / 2.0) / matches)) / 3.0
}

fn normalize_location_name(raw_text: &str) -> String {
    let common_words = [
        "avalonian portal",
        "roads of avalon",
        "portal",
        "enter",
        "exit",
    ];
    let mut cleaned = raw_text
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch.is_ascii_whitespace() || matches!(ch, '-' | '\'' ) {
                ch
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    let lower = cleaned.to_lowercase();
    for word in common_words {
        cleaned = remove_phrase_case_insensitive(&cleaned, &lower, word);
    }

    cleaned
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_lowercase()
}

fn remove_phrase_case_insensitive(text: &str, _lower_text: &str, phrase: &str) -> String {
    let mut current = text.to_string();
    loop {
        let lower = current.to_lowercase();
        if let Some(index) = lower.find(phrase) {
            let end = index + phrase.len();
            current.replace_range(index..end, " ");
        } else {
            return current;
        }
    }
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

fn db_err(err: rusqlite::Error) -> String {
    err.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known() -> Vec<String> {
        normalize_location_list(
            include_str!("../tests/fixtures/albion_locations_subset.txt")
                .lines()
                .map(|line| line.to_string())
                .collect(),
        )
    }

    #[test]
    fn current_location_dictionary_fixture_contains_requested_names() {
        let names = known();
        assert!(names.contains(&"Shaleheath Hills".to_string()));
        assert!(names.contains(&"Souos-Availos".to_string()));
        assert!(names.contains(&"Xebos-Emimsum".to_string()));
        assert!(names.contains(&"Eldon Hill".to_string()));
        assert!(names.contains(&"Blackthorn Quarry".to_string()));
        assert!(names.contains(&"Curlew Fen".to_string()));
    }

    #[test]
    fn parses_current_location_shaleheath_with_noise() {
        let parsed = parse_current_location_ocr("0-5 VI Shaleheath Hills 02:29", &[], &known());
        assert_eq!(parsed.location_name.as_deref(), Some("Shaleheath Hills"));
        assert_ne!(parsed.location_name.as_deref(), Some("Souos-Availos"));
        assert_eq!(parsed.used_dictionary_match, true);
    }

    #[test]
    fn parses_current_location_combined_plaque() {
        let parsed = parse_current_location_ocr("6 IV Blackthorn Quarry 01:31", &[], &known());
        assert_eq!(parsed.location_name.as_deref(), Some("Blackthorn Quarry"));
    }

    #[test]
    fn parses_current_location_split_rapidocr_words() {
        let lines = vec![
            OcrLine {
                text: "&0".to_string(),
                confidence: Some(0.66),
                bbox: Some(OcrBbox { x: 17.0, y: 9.0, width: 57.0, height: 29.0 }),
            },
            OcrLine {
                text: "V".to_string(),
                confidence: Some(0.89),
                bbox: Some(OcrBbox { x: 158.0, y: 12.0, width: 23.0, height: 25.0 }),
            },
            OcrLine {
                text: "Eldon".to_string(),
                confidence: Some(0.98),
                bbox: Some(OcrBbox { x: 233.0, y: 12.0, width: 77.0, height: 26.0 }),
            },
            OcrLine {
                text: "Hill".to_string(),
                confidence: Some(0.95),
                bbox: Some(OcrBbox { x: 313.0, y: 13.0, width: 47.0, height: 24.0 }),
            },
            OcrLine {
                text: "21:49".to_string(),
                confidence: Some(0.99),
                bbox: Some(OcrBbox { x: 395.0, y: 13.0, width: 75.0, height: 24.0 }),
            },
        ];
        let parsed = parse_current_location_ocr("Eldon", &lines, &known());
        assert_eq!(parsed.location_name.as_deref(), Some("Eldon Hill"));
        assert_eq!(parsed.used_dictionary_match, true);
    }

    #[test]
    fn parses_current_location_with_roman_tier() {
        let parsed = parse_current_location_ocr("IV Blackthorn Quarry", &[], &known());
        assert_eq!(parsed.location_name.as_deref(), Some("Blackthorn Quarry"));
    }

    #[test]
    fn parses_current_location_with_timer() {
        let parsed = parse_current_location_ocr("Blackthorn Quarry 01:31", &[], &known());
        assert_eq!(parsed.location_name.as_deref(), Some("Blackthorn Quarry"));
    }

    #[test]
    fn parses_current_location_with_comma_between_words() {
        let parsed = parse_current_location_ocr("21\nIV\nCurlew,Fen\n16:17", &[], &known());
        assert_eq!(parsed.cleaned_candidate, "Curlew Fen");
        assert_eq!(parsed.location_name.as_deref(), Some("Curlew Fen"));
    }

    #[test]
    fn short_elldon_candidate_keeps_medium_suggestion() {
        let parsed = parse_current_location_ocr("Eldon", &[], &known());
        assert_eq!(parsed.matched_location_name.as_deref(), Some("Eldon Hill"));
        assert_eq!(parsed.used_dictionary_match, false);
        assert_eq!(parsed.location_name.as_deref(), Some("Eldon"));
    }

    #[test]
    fn parses_current_location_city_with_levenshtein_typo() {
        let mut names = known();
        names.push("Lymhurst".to_string());
        let names = normalize_location_list(names);
        let parsed = parse_current_location_ocr("61\nLymhyrst\n05:04", &[], &names);
        assert_eq!(parsed.location_name.as_deref(), Some("Lymhurst"));
        assert_eq!(parsed.used_dictionary_match, true);
        assert_eq!(parsed.match_reason, "levenshtein dictionary match");
    }

    #[test]
    fn parses_russian_portal_tooltip() {
        let parsed = parse_portal_tooltip_ocr(
            "Путь Авалона\nOieos-Umiutum\n5/7\nЗакроется через 46 м 42 с",
            &[],
            &[],
        );
        assert_eq!(parsed.destination_name.as_deref(), Some("Oieos-Umiutum"));
        assert_eq!(parsed.slots_used, Some(5));
        assert_eq!(parsed.slots_total, Some(7));
        assert_eq!(parsed.expires_in_seconds, Some(2802));
    }

    #[test]
    fn parses_english_portal_tooltip() {
        let parsed = parse_portal_tooltip_ocr(
            "Avalon Road\nOieos-Umiutum\n5 / 7\nCloses in 46m 42s",
            &[],
            &[],
        );
        assert_eq!(parsed.destination_name.as_deref(), Some("Oieos-Umiutum"));
        assert_eq!(parsed.slots_used, Some(5));
        assert_eq!(parsed.slots_total, Some(7));
        assert_eq!(parsed.expires_in_seconds, Some(2802));
    }

    #[test]
    fn parses_spaced_english_portal_duration() {
        let parsed = parse_portal_tooltip_ocr(
            "Avalon Road\nFynitos-Agosaum\n7/7\nCloses in 5 h 02 m",
            &[],
            &[],
        );
        assert_eq!(parsed.destination_name.as_deref(), Some("Fynitos-Agosaum"));
        assert_eq!(parsed.expires_in_seconds, Some(18120));
    }

    #[test]
    fn parses_split_two_digit_hour_portal_duration() {
        let parsed = parse_portal_tooltip_ocr(
            "Road of Avalon to\nSetent-Al-Duosas\n± 6/7\n+02:25\nClo ses in 1 1 h 39 m",
            &[],
            &[],
        );
        assert_eq!(parsed.destination_name.as_deref(), Some("Setent-Al-Duosas"));
        assert_eq!(parsed.expires_in_seconds, Some(41940));
    }

    #[test]
    fn prefers_duration_over_small_timer() {
        let parsed = parse_portal_tooltip_ocr(
            "Путь Авалона\nOieos-Umiutum\n03:29\nЗакроется через 46 м 42 с",
            &[],
            &[],
        );
        assert_eq!(parsed.destination_name.as_deref(), Some("Oieos-Umiutum"));
        assert_eq!(parsed.expires_in_seconds, Some(2802));
    }

    #[test]
    fn parses_portal_tooltip_like_screenshot() {
        let parsed = parse_portal_tooltip_ocr(
            "Путь Авалона\nOieos-Umiutum\n5/7\n+ 03:29\nЗакроется через 46 м 42 с",
            &[],
            &[],
        );
        assert_eq!(parsed.destination_name.as_deref(), Some("Oieos-Umiutum"));
        assert_eq!(parsed.slots_used, Some(5));
        assert_eq!(parsed.slots_total, Some(7));
        assert_eq!(parsed.expires_in_seconds, Some(2802));
        assert!(!parsed.candidates.iter().any(|candidate| candidate == "03"));
    }

    #[test]
    fn parses_compact_portal_duration_without_spaces() {
        let parsed = parse_portal_tooltip_ocr(
            "Путь Авалона\nOieos-Umiutum\n5/7\nЗакроется через 46м42с",
            &[],
            &[],
        );
        assert_eq!(parsed.destination_name.as_deref(), Some("Oieos-Umiutum"));
        assert_eq!(parsed.expires_in_seconds, Some(2802));
    }

    #[test]
    fn parses_noisy_portal_duration_from_paddle_ocr() {
        let parsed = parse_portal_tooltip_ocr(
            "Tb ABaNOHa B\nE\nOieos-Umiutum\n3okpoercs wepes 13 414",
            &[],
            &[],
        );
        assert_eq!(parsed.destination_name.as_deref(), Some("Oieos-Umiutum"));
        assert_eq!(parsed.expires_in_seconds, Some(821));
    }

    #[test]
    fn parses_noisy_hour_minute_portal_duration_from_english_ocr_model() {
        let parsed = parse_portal_tooltip_ocr(
            "yTb ABQJOHO B\nQiient-Sa-Odetis\n20/20\n+ HET\n3akpoeTca 4epea\n12448M\nE",
            &[],
            &[],
        );
        assert_eq!(parsed.destination_name.as_deref(), Some("Qiient-Sa-Odetis"));
        assert_eq!(parsed.slots_used, Some(20));
        assert_eq!(parsed.slots_total, Some(20));
        assert_eq!(parsed.expires_in_seconds, Some(46080));
    }

    #[test]
    fn parses_noisy_split_hour_marker_portal_duration() {
        let parsed = parse_portal_tooltip_ocr(
            "yTb ABaJHQB\nSectun-Oc-Odesis\n+03:15\n3akpoeTcR 4epe3 4 4 02\nW",
            &[],
            &[],
        );
        assert_eq!(parsed.destination_name.as_deref(), Some("Sectun-Oc-Odesis"));
        assert_eq!(parsed.expires_in_seconds, Some(14520));
    }

    #[test]
    fn observation_inserts_store_metadata_json() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        insert_observation(
            &conn,
            "current_location",
            Some("Blackthorn Quarry"),
            None,
            "Blackthorn Quarry 01:31",
            Some("blackthorn quarry"),
            Some(0.91),
            Some("/tmp/current.png"),
            Some(r#"{"parser":"current_location_v1"}"#),
        )
        .unwrap();

        insert_observation(
            &conn,
            "portal",
            Some("Blackthorn Quarry"),
            Some("Oieos-Umiutum"),
            "Путь Авалона\nOieos-Umiutum\n5/7\nЗакроется через 46 м 42 с",
            Some("oieos-umiutum"),
            Some(0.88),
            Some("/tmp/portal.png"),
            Some(r#"{"parser":"portal_tooltip_v1","slots_used":5}"#),
        )
        .unwrap();

        let mut stmt = conn
            .prepare(
                "SELECT kind, screenshot_path, metadata_json FROM observations ORDER BY id ASC",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "current_location");
        assert_eq!(rows[0].1.as_deref(), Some("/tmp/current.png"));
        assert!(rows[0].2.as_deref().unwrap().contains("current_location_v1"));
        assert_eq!(rows[1].0, "portal");
        assert_eq!(rows[1].1.as_deref(), Some("/tmp/portal.png"));
        assert!(rows[1].2.as_deref().unwrap().contains("portal_tooltip_v1"));
    }

    #[test]
    fn remote_delete_removes_active_local_edge_everywhere() {
        let mut conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let from_id = upsert_location(&conn, "Test From", "test from", "avalon", false).unwrap();
        let to_id = upsert_location(&conn, "Test To", "test to", "avalon", false).unwrap();
        upsert_edge(&conn, from_id, to_id, None).unwrap();

        let remote_delete = SyncEdgePayload {
            from_name: "Test From".to_string(),
            to_name: "Test To".to_string(),
            from_normalized: Some("test from".to_string()),
            to_normalized: Some("test to".to_string()),
            first_seen_at: None,
            last_seen_at: Some("2000-01-01T00:00:00Z".to_string()),
            ttl_seconds: None,
            expires_at: None,
            observations_count: None,
            source: Some("deleted".to_string()),
            status: Some("deleted".to_string()),
            revive: None,
        };

        assert!(apply_sync_edge(&mut conn, &remote_delete).unwrap());
        assert!(load_edges(&conn).unwrap().is_empty());

        let readded_id = upsert_edge(&conn, to_id, from_id, None).unwrap();
        let edge = load_edge_by_id(&conn, readded_id).unwrap();
        assert_eq!(edge.status, "active");
        assert_eq!(edge.source, "local_readd");
    }

    #[test]
    fn matcher_uses_fixture_dictionary_for_current_locations() {
        let primary = known();
        let parsed = parse_current_location_ocr("VI Xeb-EniirnU 02:03", &[], &primary);
        assert_eq!(parsed.location_name.as_deref(), Some("Xebos-Emimsum"));

        let parsed = parse_current_location_ocr("Eldon", &[], &primary);
        assert_eq!(parsed.matched_location_name.as_deref(), Some("Eldon Hill"));
        assert_eq!(parsed.used_dictionary_match, false);

        let parsed = parse_current_location_ocr("6 IV Blackthom Quarry 01:31", &[], &primary);
        assert_eq!(parsed.location_name.as_deref(), Some("Blackthorn Quarry"));
    }
}
