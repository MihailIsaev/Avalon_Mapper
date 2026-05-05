import { Background, Controls, MiniMap, ReactFlow, type Edge as FlowEdge, type Node } from "@xyflow/react";
import { useCallback, useEffect, useMemo, useState } from "react";
import { api } from "./lib/api";
import { normalizeLocationName } from "./lib/normalization";
import type {
  DashboardData,
  GraphData,
  MapOverlayStatus,
  Observation,
  OverlayDiagnostics,
  ParsedCurrentLocation,
  ParsedPortalTooltip,
  Region,
} from "./types";

type Page = "dashboard" | "capture" | "graph" | "observations" | "settings" | "diagnostics";

type HotkeyBinding = {
  key_code: number;
  modifiers: number;
  label: string;
};

type HotkeySettings = {
  toggle_overlay: HotkeyBinding;
  capture_current: HotkeyBinding;
  capture_portal: HotkeyBinding;
};


const zoneTypeColor: Record<string, string> = {
  avalon: "#8b5cf6",
  blue: "#3b82f6",
  yellow: "#3b82f6",
  red: "#ef4444",
  outlands_black: "#111827",
  city: "#9ca3af",
  island: "#9ca3af",
  arena: "#9ca3af",
  dungeon: "#9ca3af",
  unknown: "#9ca3af",
};

const pageLabels: Record<Page, string> = {
  dashboard: "Dashboard",
  capture: "Capture Setup",
  graph: "Graph",
  observations: "Observations",
  settings: "Settings",
  diagnostics: "Overlay Diagnostics",
};

const emptyDashboard: DashboardData = {
  current_location: null,
  last_ocr_result: null,
  known_locations_count: 0,
  known_edges_count: 0,
  last_capture_status: "No captures yet",
};

export default function App() {
  const [page, setPage] = useState<Page>("dashboard");
  const [hotkeys, setHotkeys] = useState<HotkeySettings | null>(null);
  const [recordingHotkey, setRecordingHotkey] = useState<string | null>(null);
  const [dashboard, setDashboard] = useState<DashboardData>(emptyDashboard);
  const [graph, setGraph] = useState<GraphData>({ locations: [], edges: [] });
  const [observations, setObservations] = useState<Observation[]>([]);
  const [currentRegion, setCurrentRegion] = useState<Region | null>(null);
  const [portalRegion, setPortalRegion] = useState<Region | null>(null);
  const [diagnostics, setDiagnostics] = useState<OverlayDiagnostics | null>(null);
  const [mapOverlayStatus, setMapOverlayStatus] = useState<MapOverlayStatus | null>(null);
  const [rawText, setRawText] = useState("Avalonian Portal\nDeepwood Dell");
  const [manualCorrection, setManualCorrection] = useState("");
  const [portalText, setPortalText] = useState("Avalonian Portal\nEverwinter Crossing");
  const [portalCorrection, setPortalCorrection] = useState("");
  const [lastCaptureSummary, setLastCaptureSummary] = useState("");
  const [currentParsed, setCurrentParsed] = useState<ParsedCurrentLocation | null>(null);
  const [portalParsed, setPortalParsed] = useState<ParsedPortalTooltip | null>(null);
  const [manualFrom, setManualFrom] = useState("Deepwood Dell");
  const [manualTo, setManualTo] = useState("Everwinter Crossing");
  const [status, setStatus] = useState("Ready");
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    const [
      nextDashboard,
      nextGraph,
      nextObservations,
      nextCurrentRegion,
      nextPortalRegion,
      nextDiagnostics,
      nextMapOverlayStatus,
      nextHotkeys,
    ] =
      await Promise.all([
        api.dashboard(),
        api.graph(),
        api.observations(),
        api.getRegion("current_location"),
        api.getRegion("portal_tooltip"),
        api.overlayDiagnostics(),
        api.getMapOverlayStatus(),
        api.getHotkeySettings(),
      ]);
    setDashboard(nextDashboard);
    setGraph(nextGraph);
    setObservations(nextObservations);
    setCurrentRegion(nextCurrentRegion);
    setPortalRegion(nextPortalRegion);
    setDiagnostics(nextDiagnostics);
    setMapOverlayStatus(nextMapOverlayStatus);
    setHotkeys(nextHotkeys);
  }, []);

  useEffect(() => {
    refresh().catch((err) => setError(String(err)));
  }, [refresh]);
  useEffect(() => {
      if (!recordingHotkey) return;

      const handler = async (event: KeyboardEvent) => {
        event.preventDefault();
        event.stopPropagation();

        const keyCode = keyCodeMap[event.code];
        if (keyCode === undefined) {
          setError(`Unsupported key: ${event.code}`);
          return;
        }

        const modifiers = macCarbonModifiersFromKeyboardEvent(event);

        const nextLabel = hotkeyLabelFromKeyboardEvent(event);

        try {
          const updated = await api.setHotkeyBinding(recordingHotkey, keyCode, modifiers, nextLabel);
          setHotkeys(updated);
          setRecordingHotkey(null);
          setError(null);
        } catch (err) {
          setError(String(err));
        }
      };

      window.addEventListener("keydown", handler, true);

      return () => {
        window.removeEventListener("keydown", handler, true);
      };
    }, [recordingHotkey]);
  async function runAction(label: string, action: () => Promise<unknown>) {
    setError(null);
    setStatus(label);
    try {
      await action();
      await refresh();
      setStatus("Ready");
    } catch (err) {
      setError(String(err));
      setStatus("Needs attention");
    }
  }

  useEffect(() => {
    if (!hotkeys || recordingHotkey) return;

    const handler = (event: KeyboardEvent) => {
      const keyCode = keyCodeMap[event.code];
      if (keyCode === undefined) return;

      const modifiers = macCarbonModifiersFromKeyboardEvent(event);
      const matches = (binding: HotkeyBinding) => binding.key_code === keyCode && binding.modifiers === modifiers;

      if (matches(hotkeys.toggle_overlay)) {
        event.preventDefault();
        void runAction("Toggling map overlay", () => api.toggleMapOverlay());
      } else if (matches(hotkeys.capture_current)) {
        event.preventDefault();
        void runAction("Capturing current location", () => api.captureCurrentLocation());
      } else if (matches(hotkeys.capture_portal)) {
        event.preventDefault();
        void runAction("Capturing portal", () => api.capturePortalDestination());
      }
    };

    window.addEventListener("keydown", handler, true);
    return () => window.removeEventListener("keydown", handler, true);
  }, [hotkeys, recordingHotkey]);

  const normalizedCurrent = normalizeLocationName(manualCorrection || rawText);
  const normalizedPortal = normalizeLocationName(portalCorrection || portalText);

  return (
    <div className="app-shell">
      <aside className="sidebar">
        <div className="brand">
          <div className="brand-mark">AM</div>
          <div>
            <h1>Avalon Mapper OCR</h1>
            <p>Screen capture companion</p>
          </div>
        </div>
        <nav>
          {(Object.keys(pageLabels) as Page[]).map((key) => (
            <button key={key} className={page === key ? "active" : ""} onClick={() => setPage(key)}>
              {pageLabels[key]}
            </button>
          ))}
        </nav>
        <div className="safety-note">
          Uses screen capture + OCR only. No memory reads, traffic inspection, injection, gameplay automation, mouse
          clicks, or key presses are implemented.
        </div>
      </aside>

      <main className="main">
        <header className="topbar">
          <div>
            <div className="eyebrow">Phase 1 + Phase 2</div>
            <h2>{pageLabels[page]}</h2>
          </div>
          <div className="status-pill">{status}</div>
        </header>

        {error && <div className="alert">{error}</div>}

        {page === "dashboard" && (
          <Dashboard dashboard={dashboard} currentRegion={currentRegion} portalRegion={portalRegion} />
        )}

        {page === "capture" && (
          <section className="grid two">
            <Panel title="Current Location Region">
              <RegionSummary region={currentRegion} empty="No current-location region selected." />
              <div className="button-row">
                <button onClick={() => runAction("Opening overlay", () => api.runOverlaySelection("current_location", "region"))}>
                  Select over game
                </button>
                <button onClick={() => runAction("Running mock OCR", async () => setRawText((await api.mockOcr()).text))}>
                  Test mock OCR
                </button>
                <button
                  onClick={() =>
                    runAction("Capturing current location", async () => {
                      const result = await api.captureCurrentLocation();
                      setRawText(result.raw_ocr_text);
                      setManualCorrection(result.matched_name);
                      setCurrentParsed(result.parsed_current);
                      setLastCaptureSummary(`${result.matched_name} in ${result.duration_ms} ms`);
                    })
                  }
                >
                  Capture + OCR
                </button>
              </div>
              <TextArea label="Raw OCR text" value={rawText} onChange={setRawText} />
              <Readout label="Cleaned candidate" value={currentParsed?.cleaned_candidate || "None"} />
              <Readout label="Matched suggestion" value={currentParsed?.matched_location_name ?? "None"} />
              <Readout label="Final location" value={currentParsed?.location_name ?? "No parsed capture yet"} />
              <Readout label="Dictionary match" value={currentParsed?.used_dictionary_match ? "yes" : "no"} />
              <Readout label="Match reason" value={currentParsed?.match_reason ?? "None"} />
              <Readout label="Parser confidence" value={formatPercent(currentParsed?.confidence)} />
              <Readout label="Candidates" value={currentParsed?.candidates.join(", ") || "None"} />
              <Readout
                label="Top matches"
                value={
                  currentParsed?.top_matches.map((match) => `${match.name} (${match.score.toFixed(2)})`).join(" | ") ||
                  "None"
                }
              />
              <TextInput label="Manual correction" value={manualCorrection} onChange={setManualCorrection} />
              <Readout label="Normalized" value={normalizedCurrent || "Empty"} />
              <button onClick={() => runAction("Accepting current location", () => api.acceptCurrentLocation(rawText, manualCorrection))}>
                Accept current location
              </button>
            </Panel>

            <Panel title="Portal Tooltip Capture Box">
              <RegionSummary region={portalRegion} empty="No portal tooltip box configured." />
              <div className="safety-note">
                Outline the tooltip plaque on screen. The app stores the selection anchor and mirrors the capture
                rectangle across the current cursor so left/right portal tooltips use the same setup.
              </div>
              <div className="button-row">
                <button onClick={() => runAction("Opening overlay", () => api.runOverlaySelection("portal_tooltip", "portal-size"))}>
                  Configure box over game
                </button>
                <button onClick={() => runAction("Running mock OCR", async () => setPortalText((await api.mockOcr("Avalonian Portal\nEverwinter Crossing")).text))}>
                  Test mock OCR
                </button>
                <button
                  onClick={() =>
                    runAction("Capturing portal", async () => {
                      const result = await api.capturePortalDestination();
                      setPortalText(result.raw_ocr_text);
                      setPortalCorrection(result.matched_name);
                      setPortalParsed(result.parsed_portal);
                      setLastCaptureSummary(`${result.matched_name} in ${result.duration_ms} ms`);
                    })
                  }
                >
                  Capture + OCR
                </button>
              </div>
              <TextArea label="Raw OCR text" value={portalText} onChange={setPortalText} />
              <Readout label="Parsed destination" value={portalParsed?.destination_name ?? "No parsed capture yet"} />
              <Readout label="Slots" value={formatSlots(portalParsed)} />
              <Readout label="Expires in" value={formatExpires(portalParsed?.expires_in_seconds)} />
              <Readout label="Parser confidence" value={formatPercent(portalParsed?.confidence)} />
              <Readout label="Candidates" value={portalParsed?.candidates.join(", ") || "None"} />
              <TextInput label="Manual correction" value={portalCorrection} onChange={setPortalCorrection} />
              <Readout label="Normalized" value={normalizedPortal || "Empty"} />
              <button onClick={() => runAction("Accepting portal", () => api.acceptPortalCapture(portalText, portalCorrection))}>
                Accept portal destination
              </button>
            </Panel>

            <Panel title="Screenshot Preview">
              <div className="preview-box">
                {lastCaptureSummary || "Captured screenshot paths are stored in recent observations after real OCR runs."}
              </div>
            </Panel>

            <Panel title="Manual Graph Input">
              <TextInput label="From" value={manualFrom} onChange={setManualFrom} />
              <TextInput label="To" value={manualTo} onChange={setManualTo} />
              <button onClick={() => runAction("Creating edge", () => api.createManualEdge(manualFrom, manualTo))}>
                Create edge
              </button>
            </Panel>
          </section>
        )}

        {page === "graph" && <GraphPage graph={graph} currentLocation={dashboard.current_location} />}
        {page === "observations" && <Observations observations={observations} />}
        {page === "settings" && (
          <Settings
              diagnostics={diagnostics}
              mapOverlayStatus={mapOverlayStatus}
              hotkeys={hotkeys}
              recordingHotkey={recordingHotkey}
              setRecordingHotkey={setRecordingHotkey}
              setHotkeys={setHotkeys}
              runAction={runAction}
            />
        )}
        {page === "diagnostics" && (
          <Diagnostics
            diagnostics={diagnostics}
            mapOverlayStatus={mapOverlayStatus}
            runAction={runAction}
            onOverlayTest={() => runAction("Opening diagnostic overlay", () => api.runOverlaySelection("overlay_diagnostic", "diagnostic"))}
          />
        )}
      </main>
    </div>
  );
}

function Dashboard({
  dashboard,
  currentRegion,
  portalRegion,
}: {
  dashboard: DashboardData;
  currentRegion: Region | null;
  portalRegion: Region | null;
}) {
  return (
    <section className="grid four">
      <Stat label="Current location" value={dashboard.current_location ?? "Not set"} />
      <Stat label="Known locations" value={String(dashboard.known_locations_count)} />
      <Stat label="Known edges" value={String(dashboard.known_edges_count)} />
      <Stat label="Last capture" value={dashboard.last_capture_status} />
      <Panel title="Last OCR Result">
        <pre className="raw">{dashboard.last_ocr_result ?? "No OCR captures yet."}</pre>
      </Panel>
      <Panel title="Configured Regions">
        <RegionSummary region={currentRegion} empty="Current-location region missing." />
        <RegionSummary region={portalRegion} empty="Portal tooltip box missing." />
      </Panel>
    </section>
  );
}

function edgeRemainingSeconds(edge: { expires_at: string | null }): number | null {
  if (!edge.expires_at) return null;

  const remainingMs = new Date(edge.expires_at).getTime() - Date.now();
  if (!Number.isFinite(remainingMs)) return null;

  return Math.max(0, Math.floor(remainingMs / 1000));
}

function formatRemaining(seconds: number | null): string {
  if (seconds === null) return "";
  if (seconds <= 0) return "expired";

  const h = Math.floor(seconds / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  const s = seconds % 60;

  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m ${s}s`;
  return `${s}s`;
}

function GraphPage({ graph, currentLocation }: { graph: GraphData; currentLocation: string | null }) {

  const nodes = useMemo<Node[]>(

    () =>

      graph.locations.map((location, index) => {

        const x = location.x ?? index * 120;
        const y = location.y ?? 0;

        const isCurrent = currentLocation === location.name;

        const color = isCurrent ? "#22c55e" : zoneTypeColor[location.zone_type] ?? "#9ca3af";

        return {

          id: String(location.id),

          position: { x: x + 360, y: y + 260 },

          data: { label: location.name },

          style: {

            background: color,

            border: "2px solid rgba(255,255,255,0.8)",

            color: "#fff",

          },

          className: isCurrent ? "current-node" : undefined,

        };

      }),

    [graph.locations, currentLocation],

  );

  const edges = useMemo<FlowEdge[]>(
    () =>
      graph.edges.map((edge) => ({
        id: String(edge.id),
        source: String(edge.from_location_id),
        target: String(edge.to_location_id),
        label: formatEdgeTimer(edge),
        className: edge.source === "traversed" ? "edge-traversed-light" : `edge-${edge.status}`,
        style: {
          stroke: edge.source === "traversed" ? "#fff3a7" : undefined,
          strokeWidth: edge.source === "traversed" ? 3 : 1.5,
        },
      })),
    [graph.edges],
  );

    return (
      <div className="graph-layout">
        <div className="graph-canvas">
          <ReactFlow nodes={nodes} edges={edges} fitView>
            <Background />
            <MiniMap pannable zoomable />
            <Controls />
          </ReactFlow>
        </div>

        <Panel title="Connections">
          <div className="list">
            {graph.edges.length === 0 && <p>No edges yet.</p>}

            {graph.edges.map((edge) => (
              <div className="list-row" key={edge.id}>
                <span>{edge.from_location_name}</span>
                <span>{edge.to_location_name}</span>
                <strong>{formatEdgeTimer(edge)}</strong>
              </div>
            ))}
          </div>
        </Panel>
      </div>
    );
}

function Observations({ observations }: { observations: Observation[] }) {
  return (
    <Panel title="Recent OCR Captures">
      <div className="table">
        <div className="table-head">
          <span>Kind</span>
          <span>From</span>
          <span>To</span>
          <span>Raw text</span>
          <span>Confidence</span>
          <span>Parsed</span>
        </div>
        {observations.map((observation) => (
          <div className="table-row" key={observation.id}>
            <span>{observation.kind}</span>
            <span>{observation.from_location_name ?? "-"}</span>
            <span>{observation.to_location_name ?? "-"}</span>
            <span>{observation.raw_ocr_text}</span>
            <span>{observation.confidence ?? "-"}</span>
            <span>{formatObservationMetadata(observation.metadata_json)}</span>
          </div>
        ))}
        {observations.length === 0 && <p>No observations yet.</p>}
      </div>
    </Panel>
  );
}

function Settings({
  diagnostics,
  mapOverlayStatus,
  hotkeys,
  recordingHotkey,
  setRecordingHotkey,
  setHotkeys,
  runAction,
}: {
  diagnostics: OverlayDiagnostics | null;
  mapOverlayStatus: MapOverlayStatus | null;
  hotkeys: HotkeySettings | null;
  recordingHotkey: string | null;
  setRecordingHotkey: (value: string | null) => void;
  setHotkeys: (value: HotkeySettings) => void;
  runAction: (label: string, action: () => Promise<unknown>) => Promise<void>;
}) {
  return (
    <section className="grid two">
      <Panel title="Engine Modes">
        <Readout label="OCR engine" value="paddleocr:en_PP-OCRv5_mobile_rec" />
        <Readout label="Capture engine" value="deferred until overlay is proven over Albion" />
        <Readout label="Overlay engine" value={diagnostics?.helper_strategy ?? "Loading"} />
      </Panel>
      <Panel title="Permissions">
        <Readout label="macOS Screen Recording" value="Required for Phase 3 screenshot capture." />
        <Readout label="Accessibility" value="Only needed later for global hotkeys or cursor APIs." />
        <Readout label="Windows admin" value="Not required by design." />
      </Panel>
      <Panel title="Map Overlay">

      <Panel title="Danger Zone">
          <button
            className="danger"
            onClick={() =>
              runAction("Resetting graph database", async () => {
                await api.resetGraphDatabase();
              })
            }
          >
            Reset graph database
          </button>
      </Panel>
        <div className="button-row">
          <button onClick={() => runAction("Showing map overlay", () => api.showMapOverlay())}>Show overlay</button>
          <button onClick={() => runAction("Hiding map overlay", () => api.hideMapOverlay())}>Hide overlay</button>
          <button onClick={() => runAction("Updating overlay data", () => api.updateOverlayData())}>Update data</button>
          <button onClick={() => runAction("Rebuilding graph layout", () => api.rebuildGraphLayout())}>
              Rebuild graph layout
            </button>
        </div>
        <Readout label="Hotkey" value={mapOverlayStatus?.hotkey ?? "Cmd+Shift+M"} />
        <Readout label="Input mode" value={mapOverlayStatus?.interactive ? "Interactive" : "Click-through"} />
        <Readout label="Bounds" value={formatBounds(mapOverlayStatus?.bounds)} />
      </Panel>
      <Panel title="Hotkeys">
          <div className="list">
            <HotkeyRow
              label="Open overlay"
              action="toggle_overlay"
              value={hotkeys?.toggle_overlay.label ?? "Set"}
              recordingHotkey={recordingHotkey}
              setRecordingHotkey={setRecordingHotkey}
              setHotkeys={setHotkeys}
            />

            <HotkeyRow
              label="Capture location"
              action="capture_current"
              value={hotkeys?.capture_current.label ?? "Set"}
              recordingHotkey={recordingHotkey}
              setRecordingHotkey={setRecordingHotkey}
              setHotkeys={setHotkeys}
            />

            <HotkeyRow
              label="Capture portal"
              action="capture_portal"
              value={hotkeys?.capture_portal.label ?? "Set"}
              recordingHotkey={recordingHotkey}
              setRecordingHotkey={setRecordingHotkey}
              setHotkeys={setHotkeys}
            />
          </div>
        </Panel>
    </section>
  );
}

function HotkeyRow({
  label,
  action,
  value,
  recordingHotkey,
  setRecordingHotkey,
}: {
  label: string;
  action: string;
  value: string;
  recordingHotkey: string | null;
  setRecordingHotkey: (value: string | null) => void;
  setHotkeys: (value: HotkeySettings) => void;
}) {
  return (
    <div className="list-row">
      <span>{label}</span>
      <button
        className="button"
        type="button"
        onClick={() => setRecordingHotkey(action)}
      >
        {recordingHotkey === action ? "Press hotkey..." : value}
      </button>
    </div>
  );
}

function Diagnostics({
  diagnostics,
  mapOverlayStatus,
  runAction,
  onOverlayTest,
}: {
  diagnostics: OverlayDiagnostics | null;
  mapOverlayStatus: MapOverlayStatus | null;
  runAction: (label: string, action: () => Promise<unknown>) => Promise<void>;
  onOverlayTest: () => void;
}) {
  return (
    <section className="grid two">
      <Panel title="Transparent Overlay">
        <button onClick={onOverlayTest}>Test transparent overlay</button>
        <Readout label="Native overlay available" value={diagnostics?.native_overlay_available ? "Yes" : "No"} />
        <Readout label="Platform" value={diagnostics?.platform ?? "Unknown"} />
        <Readout label="Background policy" value="Clear NSWindow, no blur, no backdrop filter, no dimming layer." />
      </Panel>
      <Panel title="Fullscreen Support">
        <Readout
          label="Exclusive fullscreen"
          value={
            diagnostics?.exclusive_fullscreen_supported
              ? "Supported"
              : "Exclusive fullscreen cannot be overlaid reliably. Use Borderless Window / Windowed Fullscreen."
          }
        />
        <Readout label="Borderless/windowed fullscreen" value="Targeted by the native overlay helper." />
        <Readout label="Display bounds" value="Returned with each selected region as display_id and scale_factor." />
      </Panel>
      <Panel title="Map Overlay Controls">
          <div className="button-row">
            <button onClick={() => runAction("Toggling map overlay", () => api.toggleMapOverlay())}>
              Test map overlay
            </button>

            <button onClick={() => runAction("Click-through mode", () => api.setOverlayInteractive(false))}>
              Click-through
            </button>

            <button onClick={() => runAction("Interactive mode", () => api.setOverlayInteractive(true))}>
              Interactive
            </button>

            <button onClick={() => runAction("Resetting overlay", () => api.resetOverlayPosition())}>
              Reset position
            </button>


          </div>

          <Readout label="Hotkey" value={`${mapOverlayStatus?.hotkey ?? "Cmd+Shift+M"} toggles show/hide`} />
          <Readout label="Capture hotkeys" value="Cmd+Shift+L captures current location. Cmd+Shift+P captures portal." />
          <Readout label="Visible" value={mapOverlayStatus?.visible ? "Shown" : "Hidden or controlled by helper hotkey"} />
          <Readout label="Input mode" value={mapOverlayStatus?.interactive ? "Interactive" : "Click-through"} />
          <Readout label="Bounds" value={formatBounds(mapOverlayStatus?.bounds)} />
          <Readout label="Helper" value={mapOverlayStatus?.helper_running ? "Running" : "Not running"} />
      </Panel>
    </section>
  );
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div className="stat">
      <span>{label}</span>
      <strong>{value}</strong>
    </div>
  );
}

function Panel({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section className="panel">
      <h3>{title}</h3>
      {children}
    </section>
  );
}

function RegionSummary({ region, empty }: { region: Region | null; empty: string }) {
  if (!region) return <p className="muted">{empty}</p>;
  const hasAnchor = region.anchor_x !== null && region.anchor_y !== null;
  return (
    <div className="region-summary">
      <span>{region.key}</span>
      <strong>
        x {region.x}, y {region.y}, {region.width} x {region.height}
      </strong>
      <small>
        display {region.display_id ?? "unknown"} / scale {region.scale_factor ?? "unknown"}
        {hasAnchor ? ` / anchor ${Math.round(region.anchor_x as number)}, ${Math.round(region.anchor_y as number)}` : ""}
      </small>
      {region.key === "portal_tooltip" && !hasAnchor && <small className="muted">Reconfigure to enable mirrored portal capture.</small>}
    </div>
  );
}

function TextArea({ label, value, onChange }: { label: string; value: string; onChange: (value: string) => void }) {
  return (
    <label className="field">
      <span>{label}</span>
      <textarea value={value} onChange={(event) => onChange(event.target.value)} />
    </label>
  );
}

function TextInput({ label, value, onChange }: { label: string; value: string; onChange: (value: string) => void }) {
  return (
    <label className="field">
      <span>{label}</span>
      <input value={value} onChange={(event) => onChange(event.target.value)} />
    </label>
  );
}

function Readout({ label, value }: { label: string; value: string }) {
  return (
    <div className="readout">
      <span>{label}</span>
      <strong>{value}</strong>
    </div>
  );
}

function formatBounds(bounds: MapOverlayStatus["bounds"] | undefined): string {
  if (!bounds) return "Unknown";
  return `x ${bounds.x}, y ${bounds.y}, ${bounds.width} x ${bounds.height}`;
}

function formatPercent(value: number | undefined): string {
  return value === undefined ? "Unknown" : `${Math.round(value * 100)}%`;
}

function formatSlots(parsed: ParsedPortalTooltip | null): string {
  if (!parsed || parsed.slots_used === null || parsed.slots_total === null) return "Unknown";
  return `${parsed.slots_used}/${parsed.slots_total}`;
}

function formatExpires(seconds: number | null | undefined): string {
  if (seconds === null || seconds === undefined) return "Unknown";
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const rest = seconds % 60;
  return `${seconds}s (${hours ? `${hours}h ` : ""}${minutes}m ${rest}s)`;
}

function formatObservationMetadata(metadata: string | null): string {
  if (!metadata) return "-";
  try {
    const parsed = JSON.parse(metadata) as {
      location_name?: string;
      destination_name?: string;
      slots_used?: number;
      slots_total?: number;
      expires_in_seconds?: number;
    };
    const name = parsed.location_name ?? parsed.destination_name ?? "-";
    const slots =
      parsed.slots_used !== undefined && parsed.slots_total !== undefined
        ? ` ${parsed.slots_used}/${parsed.slots_total}`
        : "";
    const expires = parsed.expires_in_seconds !== undefined ? ` ${formatExpires(parsed.expires_in_seconds)}` : "";
    return `${name}${slots}${expires}`;
  } catch {
    return "Invalid metadata";
  }
}

function edgeStatus(lastSeenAt: string): "active" | "stale" | "expired" {
  const ageMs = Date.now() - new Date(lastSeenAt).getTime();
  const oneDay = 24 * 60 * 60 * 1000;
  if (ageMs < oneDay) return "active";
  if (ageMs < oneDay * 3) return "stale";
  return "expired";
}

function formatEdgeTimer(edge: { expires_at?: string | null; status: string }): string {
  if (!edge.expires_at) {
    return edge.status;
  }

  const msLeft = new Date(edge.expires_at).getTime() - Date.now();

  if (msLeft <= 0) {
    return "expired";
  }

  const totalSeconds = Math.floor(msLeft / 1000);
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;

  if (hours > 0) {
    return `${hours}h ${minutes}m`;
  }

  return `${minutes}m ${seconds}s`;
}

const keyCodeMap: Record<string, number> = {
  KeyA: 0,
  KeyS: 1,
  KeyD: 2,
  KeyF: 3,
  KeyH: 4,
  KeyG: 5,
  KeyZ: 6,
  KeyX: 7,
  KeyC: 8,
  KeyV: 9,
  KeyB: 11,
  KeyQ: 12,
  KeyW: 13,
  KeyE: 14,
  KeyR: 15,
  KeyY: 16,
  KeyT: 17,
  Digit1: 18,
  Digit2: 19,
  Digit3: 20,
  Digit4: 21,
  Digit6: 22,
  Digit5: 23,
  Equal: 24,
  Digit9: 25,
  Digit7: 26,
  Minus: 27,
  Digit8: 28,
  Digit0: 29,
  BracketRight: 30,
  KeyO: 31,
  KeyU: 32,
  BracketLeft: 33,
  KeyI: 34,
  KeyP: 35,
  KeyL: 37,
  KeyJ: 38,
  Quote: 39,
  KeyK: 40,
  Semicolon: 41,
  Backslash: 42,
  Comma: 43,
  Slash: 44,
  KeyN: 45,
  KeyM: 46,
  Period: 47,
  Space: 49,
  Backquote: 50,
};

function macCarbonModifiers(event: React.KeyboardEvent): number {
  let modifiers = 0;

  if (event.metaKey) modifiers |= 256;   // cmdKey
  if (event.shiftKey) modifiers |= 512;  // shiftKey
  if (event.altKey) modifiers |= 2048;   // optionKey
  if (event.ctrlKey) modifiers |= 4096;  // controlKey

  return modifiers;
}

function hotkeyLabel(event: React.KeyboardEvent): string {
  const parts: string[] = [];

  if (event.metaKey) parts.push("⌘");
  if (event.altKey) parts.push("⌥");
  if (event.ctrlKey) parts.push("⌃");
  if (event.shiftKey) parts.push("⇧");

  parts.push(event.key.length === 1 ? event.key.toUpperCase() : event.key);

  return parts.join("");
}

function macCarbonModifiersFromKeyboardEvent(event: KeyboardEvent): number {
  let modifiers = 0;

  if (event.metaKey) modifiers |= 256;
  if (event.shiftKey) modifiers |= 512;
  if (event.altKey) modifiers |= 2048;
  if (event.ctrlKey) modifiers |= 4096;

  return modifiers;
}

function hotkeyLabelFromKeyboardEvent(event: KeyboardEvent): string {
  const parts: string[] = [];

  if (event.metaKey) parts.push("⌘");
  if (event.altKey) parts.push("⌥");
  if (event.ctrlKey) parts.push("⌃");
  if (event.shiftKey) parts.push("⇧");

  parts.push(event.key.length === 1 ? event.key.toUpperCase() : event.key);

  return parts.join("");
}
