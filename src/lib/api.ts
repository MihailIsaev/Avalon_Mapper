import { invoke } from "@tauri-apps/api/core";
import type {
  CaptureOutcome,
  DashboardData,
  Edge,
  GraphData,
  Observation,
  OcrResult,
  MapOverlayBounds,
  MapOverlayStatus,
  OverlayDiagnostics,
  Region,
} from "../types";

export const api = {
  getHotkeySettings: () => invoke<HotkeySettings>("get_hotkey_settings"),
  setHotkeyBinding: (action: string, keyCode: number, modifiers: number, label: string) =>
      invoke<HotkeySettings>("set_hotkey_binding", {
        action,
        keyCode,
        modifiers,
        label,
      }),
  rebuildGraphLayout: () => invoke<void>("rebuild_graph_layout"),
  resetGraphDatabase: () => invoke<void>("reset_graph_database"),
  dashboard: () => invoke<DashboardData>("get_dashboard"),
  graph: () => invoke<GraphData>("list_graph"),
  observations: () => invoke<Observation[]>("list_observations"),
  getRegion: (key: string) => invoke<Region | null>("get_region", { key }),
  normalize: (rawText: string) => invoke<string>("normalize_text", { rawText }),
  mockOcr: (text?: string) => invoke<OcrResult>("mock_ocr", { text }),
  acceptCurrentLocation: (rawOcrText: string, correctedName?: string) =>
    invoke("accept_current_location", {
      rawOcrText,
      correctedName: correctedName || null,
    }),
  acceptPortalCapture: (rawOcrText: string, correctedDestination?: string) =>
    invoke("accept_portal_capture", {
      rawOcrText,
      correctedDestination: correctedDestination || null,
    }),
  createManualEdge: (fromLocation: string, toLocation: string) =>
    invoke<Edge>("create_manual_edge", { fromLocation, toLocation }),
  runOverlaySelection: (key: string, mode: "region" | "portal-size" | "diagnostic") =>
    invoke<Region>("run_overlay_selection", { key, mode }),
  overlayDiagnostics: () => invoke<OverlayDiagnostics>("overlay_diagnostics"),
  showMapOverlay: () => invoke("show_map_overlay"),
  hideMapOverlay: () => invoke("hide_map_overlay"),
  toggleMapOverlay: () => invoke("toggle_map_overlay"),
  setOverlayInteractive: (enabled: boolean) => invoke("set_overlay_interactive", { enabled }),
  updateOverlayData: () => invoke("update_overlay_data"),
  setOverlayBounds: (bounds: MapOverlayBounds) => invoke<MapOverlayBounds>("set_overlay_bounds", { bounds }),
  getOverlayBounds: () => invoke<MapOverlayBounds>("get_overlay_bounds"),
  resetOverlayPosition: () => invoke<MapOverlayBounds>("reset_overlay_position"),
  getMapOverlayStatus: () => invoke<MapOverlayStatus>("get_map_overlay_status"),
  captureCurrentLocation: () => invoke<CaptureOutcome>("capture_current_location"),
  capturePortalDestination: () => invoke<CaptureOutcome>("capture_portal_destination"),
};
