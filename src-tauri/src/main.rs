use chrono::{Duration, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex, OnceLock},
    thread,
    time::Instant,
};
use tauri::{AppHandle, Manager};

struct AppState {
    db: Mutex<Connection>,
    db_path: PathBuf,
    capture_dir: PathBuf,
    map_overlay: Arc<Mutex<Option<MapOverlayProcess>>>,
    paddle_ocr: Arc<Mutex<Option<PaddleOcrProcess>>>,
}


#[derive(Debug, Serialize, Deserialize, Clone)]
struct HotkeyBinding {
    key_code: u32,
    modifiers: u32,
    label: String,
}

#[derive(Debug, Serialize)]
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

fn lookup_avalon_info(normalized_name: &str) -> AvalonInfo {
    static CACHE: OnceLock<std::collections::HashMap<String, AvalonInfo>> = OnceLock::new();

    let map = CACHE.get_or_init(|| {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("Could not resolve project root")
            .join("data/albion_navigator_import.json");

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

    map.get(&key).cloned().unwrap_or_else(|| {
        eprintln!("[avalon-info] missing info for {normalized_name:?} normalized={key:?}");
        AvalonInfo {
            tiers: Vec::new(),
            components: Vec::new(),
            chests: Vec::new(),
        }
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

struct PaddleOcrProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
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
            let conn = Connection::open(&db_path)
                .map_err(|err| format!("Could not open SQLite database: {err}"))?;
            initialize_schema(&conn).map_err(|err| format!("Could not initialize SQLite: {err}"))?;
            import_static_route_graph(&conn).map_err(|err| format!("Could not import static route graph: {err}"))?;
            let state = AppState {
                db: Mutex::new(conn),
                db_path,
                capture_dir,
                map_overlay: Arc::new(Mutex::new(None)),
                paddle_ocr: Arc::new(Mutex::new(None)),
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
            app.manage(AppState {
                db: state.db,
                db_path: state.db_path,
                capture_dir: state.capture_dir,
                map_overlay: state.map_overlay,
                paddle_ocr: state.paddle_ocr,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_hotkey_settings,
            set_hotkey_binding,
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
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| "Could not resolve project root".to_string())?
        .join("data/albion_navigator_import.json");

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
        modifiers: 768, // option + shift для Carbon
        label: "⌥⇧M".to_string(),
    }
}

fn default_capture_current_hotkey() -> HotkeyBinding {
    HotkeyBinding {
        key_code: 37, // L
        modifiers: 768,
        label: "⌥⇧L".to_string(),
    }
}

fn default_capture_portal_hotkey() -> HotkeyBinding {
    HotkeyBinding {
        key_code: 35, // P
        modifiers: 768,
        label: "⌥⇧P".to_string(),
    }
}

fn send_hotkeys_to_overlay(app: &AppHandle, state: &AppState) -> Result<(), String> {
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

fn initialize_schema(conn: &Connection) -> rusqlite::Result<()> {
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

    Ok(HotkeySettings {
        toggle_overlay: read_hotkey_binding(&conn, "hotkey_toggle_overlay", default_toggle_overlay_hotkey())?,
        capture_current: read_hotkey_binding(&conn, "hotkey_capture_current", default_capture_current_hotkey())?,
        capture_portal: read_hotkey_binding(&conn, "hotkey_capture_portal", default_capture_portal_hotkey())?,
    })
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

    get_hotkey_settings(state)
}

#[tauri::command]
fn find_shortest_path(
    state: tauri::State<'_, AppState>,
    from_location: String,
    to_location: String,
) -> Result<ShortestPathResult, String> {
    let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;

    delete_expired_edges(&conn)?;

    let from_norm = normalize_location_name(&from_location);
    let to_norm = normalize_location_name(&to_location);

    if from_norm.is_empty() || to_norm.is_empty() {
        return Err("Both locations are required".to_string());
    }

    let graph = build_route_graph(&conn)?;

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
    if region.width <= 0 || region.height <= 0 {
        return Err("Region width and height must be positive".to_string());
    }

    let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
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

    load_edge_by_id(&conn, edge_id)
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

    load_edge_by_id(&conn, edge_id)
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
                WHERE e.status != 'expired'
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
    save_region(
        state,
        key,
        RegionInput {
            x: selection.x,
            y: selection.y,
            width: selection.width,
            height: selection.height,
            display_id: selection.display_id,
            scale_factor: selection.scale_factor,
            anchor_x: selection.anchor_x,
            anchor_y: selection.anchor_y,
        },
    )
}

#[tauri::command]
fn overlay_diagnostics() -> OverlayDiagnostics {
    OverlayDiagnostics {
        platform: std::env::consts::OS.to_string(),
        native_overlay_available: cfg!(target_os = "macos"),
        helper_strategy: if cfg!(target_os = "macos") {
            "Swift/AppKit native helpers: modal selector plus persistent non-activating click-through map overlay"
                .to_string()
        } else {
            "Windows native layered-window helper is planned; current build exposes the interface only"
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
        hotkey: "Cmd+Shift+M".to_string(),
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
    let outcome = capture_current_location_inner(&app, &state)?;
    refresh_map_overlay(&app, &state)?;
    Ok(outcome)
}

#[tauri::command]
fn capture_portal_destination(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<CaptureOutcome, String> {
    let outcome = capture_portal_destination_inner(&app, &state)?;
    refresh_map_overlay(&app, &state)?;
    Ok(outcome)
}

fn run_native_overlay(app: &AppHandle, mode: &str) -> Result<OverlaySelection, String> {
    if !cfg!(target_os = "macos") {
        return Err("Native overlay helper is implemented for macOS in this phase".to_string());
    }
    if mode != "region" && mode != "portal-size" && mode != "diagnostic" {
        return Err("Unknown overlay mode".to_string());
    }

    let helper = ensure_macos_overlay_helper(app)?;
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
    if !cfg!(target_os = "macos") {
        return Err("The persistent map overlay helper is implemented for macOS in this phase".to_string());
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
            *overlay = None;
        }
    }

    let helper = ensure_macos_overlay_helper(app)?;
    let bounds_state_path = app
        .path()
        .app_data_dir()
        .map_err(|err| format!("Could not resolve app data directory: {err}"))?
        .join("map-overlay-bounds.json");
    let mut child = Command::new(&helper)
        .arg("--mode")
        .arg("map-overlay")
        .arg("--bounds-state")
        .arg(&bounds_state_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("Could not launch map overlay helper: {err}"))?;
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

    Ok(())
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
            if let Ok(conn) = Connection::open(&db_path) {
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
                        if let Ok(conn) = Connection::open(&db_path) {
                            let _ = undo_last_graph_action_inner(&conn);
                            if let Ok(data) = build_map_overlay_data_from_conn(&conn) {
                                let _ = send_map_overlay_command_direct(&overlay, json!({ "type": "data", "data": data }));
                            }
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
                    "capture_current" => {
                        let _ = handle_hotkey_capture(
                            &db_path,
                            &helper_path,
                            &capture_dir,
                            &overlay,
                            &paddle_ocr,
                            "current_location",
                        );
                    }
                    "capture_portal" => {
                        let _ = handle_hotkey_capture(
                            &db_path,
                            &helper_path,
                            &capture_dir,
                            &overlay,
                            &paddle_ocr,
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
    debug_avalon_raw_components("Xilos-Osayam");
    debug_avalon_raw_components("Oiritos-Eramtum");
    let locations = load_locations(conn)?;
    let edges = load_edges(conn)?;
    let (bridge_locations, bridge_edges) = build_overlay_shortcuts(conn, &locations)?;
    let known_locations_count = locations.len() as i64;
    let known_edges_count = edges.len() as i64;
    Ok(MapOverlayData {
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
        ocr_mode: "paddleocr:en_PP-OCRv5_mobile_rec".to_string(),
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

fn debug_avalon_raw_components(name: &str) {
    let normalized_target = normalize_location_name(name);

    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("Could not resolve project root")
        .join("data/albion_navigator_import.json");

    let Ok(text) = fs::read_to_string(&path) else {
        eprintln!("[avalon-debug] cannot read {}", path.display());
        return;
    };

    let Ok(root) = serde_json::from_str::<serde_json::Value>(&text) else {
        eprintln!("[avalon-debug] invalid json");
        return;
    };

    let records = root
        .get("avalon_locations")
        .or_else(|| root.get("avalon"))
        .or_else(|| root.get("avalon_components"))
        .or_else(|| root.get("avalon_component_records"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    for record in records {
        let name = record.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let normalized = record
            .get("normalized_name")
            .and_then(|v| v.as_str())
            .map(|v| v.to_string())
            .unwrap_or_else(|| normalize_location_name(name));

        if normalized != normalized_target {
            continue;
        }

        eprintln!("[avalon-debug] LOCATION: {name} / {normalized}");

        if let Some(components) = record.get("components").and_then(|v| v.as_array()) {
            for component in components {
                eprintln!("[avalon-debug] component = {}", component);
            }
        }

        return;
    }

    eprintln!("[avalon-debug] not found: {name} / {normalized_target}");
}

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

fn capture_current_location_inner(app: &AppHandle, state: &AppState) -> Result<CaptureOutcome, String> {
    let region = {
        let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
        load_region_by_key(&conn, "current_location")?
            .ok_or_else(|| "Select the current-location region before capturing".to_string())?
    };
    let started = Instant::now();
    let ocr = run_capture_ocr(app, state, "current", &region, false, None)?;
    let capture_ms = started.elapsed().as_millis() as i64;
    let mut conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
    apply_current_location_capture(&mut conn, &ocr, capture_ms)
}

fn capture_portal_destination_inner(app: &AppHandle, state: &AppState) -> Result<CaptureOutcome, String> {
    let region = {
        let conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
        load_region_by_key(&conn, "portal_tooltip")?
            .ok_or_else(|| "Configure the portal tooltip box before capturing".to_string())?
    };
    let started = Instant::now();
    let portal_anchor = region.anchor_x.zip(region.anchor_y);
    let center_cursor = portal_anchor.is_none();
    let ocr = run_capture_ocr(app, state, "portal", &region, center_cursor, portal_anchor)?;
    let capture_ms = started.elapsed().as_millis() as i64;
    let mut conn = state.db.lock().map_err(|_| "Database lock poisoned".to_string())?;
    apply_portal_capture(&mut conn, &ocr, capture_ms)
}

fn run_capture_ocr(
    app: &AppHandle,
    state: &AppState,
    kind: &str,
    region: &Region,
    center_cursor: bool,
    portal_anchor: Option<(f64, f64)>,
) -> Result<CaptureOcrResult, String> {
    let helper = ensure_macos_overlay_helper(app)?;
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
    let output = command
        .output()
        .map_err(|err| format!("Could not run capture helper: {err}"))?;
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
    if !result.screen_recording_permission {
        return Err(
            "macOS Screen Recording permission is missing. Enable it for Avalon Mapper OCR in System Settings > Privacy & Security > Screen Recording, then restart the app."
                .to_string(),
        );
    }
    let paddle = run_paddle_ocr(&state.paddle_ocr, &result.image_path, kind)?;
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

fn handle_hotkey_capture(
    db_path: &Path,
    helper_path: &Path,
    capture_dir: &Path,
    overlay: &Arc<Mutex<Option<MapOverlayProcess>>>,
    paddle_ocr: &Arc<Mutex<Option<PaddleOcrProcess>>>,
    kind: &str,
) -> Result<(), String> {
    let mut conn = Connection::open(db_path).map_err(db_err)?;
    initialize_schema(&conn).map_err(db_err)?;
    let region_key = if kind == "portal" {
        "portal_tooltip"
    } else {
        "current_location"
    };
    let region = load_region_by_key(&conn, region_key)?
        .ok_or_else(|| format!("Missing {region_key} region"))?;
    let started = Instant::now();
    let portal_anchor = if kind == "portal" { region.anchor_x.zip(region.anchor_y) } else { None };
    let center_cursor = if kind == "portal" { portal_anchor.is_none() } else { false };
    let ocr = match run_capture_ocr_with_helper(
        helper_path,
        capture_dir,
        paddle_ocr,
        kind,
        &region,
        center_cursor,
        portal_anchor,
    ) {
        Ok(ocr) => ocr,
        Err(err) => {
            let _ = set_setting(&conn, "last_capture_status", &format!("{kind} capture failed: {err}"));
            let data = build_map_overlay_data_from_conn(&conn)?;
            let _ = send_map_overlay_command_direct(overlay, json!({ "type": "data", "data": data }));
            return Err(err);
        }
    };
    let measured_ms = started.elapsed().as_millis() as i64;
    let result = if kind == "portal" {
        apply_portal_capture(&mut conn, &ocr, measured_ms)
    } else {
        apply_current_location_capture(&mut conn, &ocr, measured_ms)
    };
    if let Err(err) = result {
        let _ = set_setting(
            &conn,
            "last_capture_status",
            &format!("{kind} capture failed: {err}"),
        );
    }
    let data = build_map_overlay_data_from_conn(&conn)?;
    send_map_overlay_command_direct(overlay, json!({ "type": "data", "data": data }))?;
    Ok(())
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
    let output = command
        .output()
        .map_err(|err| format!("Could not run capture helper: {err}"))?;
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
    if !result.screen_recording_permission {
        return Err(
            "macOS Screen Recording permission is missing. Enable it for Avalon Mapper OCR in System Settings > Privacy & Security > Screen Recording, then restart the app."
                .to_string(),
        );
    }
    let paddle = run_paddle_ocr(paddle_ocr, &result.image_path, kind)?;
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
    let mut guard = paddle_ocr
        .lock()
        .map_err(|_| "PaddleOCR lock poisoned".to_string())?;
    let process = ensure_paddle_ocr_process(&mut guard)?;
    let request = serde_json::to_string(&json!({
        "kind": kind,
        "image_path": image_path,
    }))
    .map_err(|err| err.to_string())?;
    process
        .stdin
        .write_all(request.as_bytes())
        .map_err(|err| format!("Could not write PaddleOCR request: {err}"))?;
    process
        .stdin
        .write_all(b"\n")
        .map_err(|err| format!("Could not write PaddleOCR request newline: {err}"))?;
    process
        .stdin
        .flush()
        .map_err(|err| format!("Could not flush PaddleOCR request: {err}"))?;

    let mut line = String::new();
    process
        .stdout
        .read_line(&mut line)
        .map_err(|err| format!("Could not read PaddleOCR response: {err}"))?;
    if line.trim().is_empty() {
        *guard = None;
        return Err("PaddleOCR worker returned no output".to_string());
    }

    let parsed = serde_json::from_str::<PaddleOcrHelperResult>(line.trim())
        .map_err(|err| format!("Invalid PaddleOCR helper JSON: {err}; output={line}"))?;
    if !parsed.ok {
        return Err(parsed.error.unwrap_or_else(|| "PaddleOCR failed".to_string()));
    }

    let result = OcrResult {
        text: parsed.text.unwrap_or_default(),
        confidence: parsed.confidence,
        engine: parsed
            .engine
            .unwrap_or_else(|| "paddleocr:en_PP-OCRv5_mobile_rec".to_string()),
        lines: parsed.lines.unwrap_or_default(),
    };
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
        return process
            .as_mut()
            .ok_or_else(|| "Could not access PaddleOCR worker".to_string());
    }
    *process = None;

    let project_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| "Could not resolve project root".to_string())?;
    let helper_path = project_root.join("native/ocr/paddle_ocr_helper.py");
    let python_path = project_root.join(".venv/bin/python3");
    let python = if python_path.exists() {
        python_path
    } else {
        PathBuf::from("python3")
    };

    let mut child = Command::new(python)
        .arg(helper_path)
        .arg("--server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("Could not start PaddleOCR worker: {err}"))?;
    if let Some(stderr) = child.stderr.take() {
        spawn_stderr_forwarder("paddleocr", stderr);
    }
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Could not open PaddleOCR stdin".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Could not open PaddleOCR stdout".to_string())?;

    *process = Some(PaddleOcrProcess {
        child,
        stdin,
        stdout: BufReader::new(stdout),
    });
    process
        .as_mut()
        .ok_or_else(|| "Could not initialize PaddleOCR worker".to_string())
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
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("Could not resolve project root")
                .join("data/albion_locations_all.json");

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
            WHERE status != 'expired'
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

        let mut levels = bfs_levels(component, &graph, root);
        reduce_layer_crossings(&mut levels, &graph);

        let local = coordinates_from_levels(&levels);
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

    let all_names = load_route_location_names(conn)?;

    let from_norm = match_route_location_name(&from_query, &all_names)
        .ok_or_else(|| format!("Could not match route start: {from_query}"))?;

    let to_norm = match_route_location_name(&to_query, &all_names)
        .ok_or_else(|| format!("Could not match route destination: {to_query}"))?;

    let graph = build_route_graph(conn)?;
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
            "SELECT id FROM edges WHERE from_location_id = ?1 AND to_location_id = ?2",
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
    lines.extend(raw_text.lines().map(|line| line.to_string()));
    lines
        .into_iter()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
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
    let has_multi_word_candidate = alpha_words.len() >= 2;
    let exact_match = query == normalize_location_name(&best_candidate.name);
    let strong_match = if alpha_words.len() == 1 && !exact_match {
        false
    } else {
        best_candidate.score >= 0.88
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
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("Could not resolve project root")
                .join("data/albion_locations_all.txt");
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
