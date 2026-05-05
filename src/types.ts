export interface Region {
  key: string;
  x: number;
  y: number;
  width: number;
  height: number;
  display_id: string | null;
  scale_factor: number | null;
  anchor_x: number | null;
  anchor_y: number | null;
}

export interface HotkeyBinding {
  key_code: number;
  modifiers: number;
  label: string;
}

export interface HotkeySettings {
  toggle_overlay: HotkeyBinding;
  capture_current: HotkeyBinding;
  capture_portal: HotkeyBinding;
}

export interface Location {
  id: number;
  name: string;
  zone_type: string;
  normalized_name: string;
  first_seen_at: string;
  last_seen_at: string;
  visit_count: number;
  x: number | null;
  y: number | null;
}

export interface Edge {
  id: number;
  ttl_seconds: number | null;
  expires_at: string | null;
  source: string;
  status: string;
  from_location_id: number;
  to_location_id: number;
  from_location_name: string;
  to_location_name: string;
  first_seen_at: string;
  last_seen_at: string;
  observations_count: number;
  confidence: number;
}

export interface Observation {
  id: number;
  kind: string;
  from_location_name: string | null;
  to_location_name: string | null;
  raw_ocr_text: string;
  normalized_text: string | null;
  confidence: number | null;
  screenshot_path: string | null;
  metadata_json: string | null;
  created_at: string;
}

export interface DashboardData {
  current_location: string | null;
  last_ocr_result: string | null;
  known_locations_count: number;
  known_edges_count: number;
  last_capture_status: string;
}

export interface GraphData {
  locations: Location[];
  edges: Edge[];
}

export interface OcrResult {
  text: string;
  confidence: number | null;
  engine: string;
  lines: OcrLine[];
}

export interface OcrLine {
  text: string;
  confidence: number | null;
  bbox?: {
    x: number;
    y: number;
    width: number;
    height: number;
  } | null;
}

export interface ParsedCurrentLocation {
  location_name: string | null;
  cleaned_candidate: string;
  matched_location_name: string | null;
  matched_location_score: number;
  used_dictionary_match: boolean;
  match_reason: string;
  confidence: number;
  ignored_lines: string[];
  candidates: string[];
  top_matches: RankedLocation[];
  reason: string;
}

export interface RankedLocation {
  name: string;
  score: number;
}

export interface ParsedPortalTooltip {
  destination_name: string | null;
  slots_used: number | null;
  slots_total: number | null;
  expires_in_seconds: number | null;
  confidence: number;
  ignored_lines: string[];
  candidates: string[];
  reason: string;
}

export interface CaptureOutcome {
  raw_ocr_text: string;
  normalized_text: string;
  matched_name: string;
  match_confidence: number;
  ocr_confidence: number | null;
  parsed_current: ParsedCurrentLocation | null;
  parsed_portal: ParsedPortalTooltip | null;
  image_path: string;
  duration_ms: number;
  engine: string;
}

export interface OverlayDiagnostics {
  platform: string;
  native_overlay_available: boolean;
  helper_strategy: string;
  exclusive_fullscreen_supported: boolean;
  exclusive_fullscreen_note: string;
}

export interface MapOverlayBounds {
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface MapOverlayStatus {
  visible: boolean;
  interactive: boolean;
  bounds: MapOverlayBounds;
  hotkey: string;
  helper_running: boolean;
  exclusive_fullscreen_note: string;
}
