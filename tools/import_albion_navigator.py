import json
import ssl
import urllib.request
from pathlib import Path

import certifi

ZONE_DATA_URL = "https://raw.githubusercontent.com/SugarF0x/albion-navigator/main/Resources/zoneData.json"
ZONE_COMPONENTS_URL = "https://raw.githubusercontent.com/SugarF0x/albion-navigator/main/Resources/zoneComponentsData.json"

OUT = Path("data/albion_navigator_import.json")


def load_json(url: str):
    context = ssl.create_default_context(cafile=certifi.where())
    with urllib.request.urlopen(url, timeout=30, context=context) as r:
        return json.loads(r.read().decode("utf-8"))


def norm(name: str) -> str:
    return " ".join(name.lower().strip().split())


def list_from_any(data):
    if isinstance(data, list):
        return data
    if isinstance(data, dict):
        for key in ["zones", "Zones", "data", "Data", "items", "Items"]:
            if isinstance(data.get(key), list):
                return data[key]
    return []


def component_name(component):
    return component.get("DisplayName") or component.get("displayName") or component.get("name")


def component_id(component):
    return component.get("Id") or component.get("id")


def extract(zone_data, components_data):
    zones_raw = list_from_any(zone_data)
    components_raw = list_from_any(components_data)

    components_by_id = {}
    for c in components_raw:
        cid = component_id(c)
        if cid is not None:
            components_by_id[int(cid)] = {
                "id": int(cid),
                "type": c.get("Type") or c.get("type"),
                "tier": c.get("Tier") or c.get("tier"),
                "properties": c.get("Properties") or c.get("properties") or [],
                "name": component_name(c),
                "raw": c,
            }

    zones_by_id = {}
    zones = []

    for z in zones_raw:
        zid = z.get("id") or z.get("Id")
        name = z.get("displayName") or z.get("DisplayName") or z.get("name")

        if zid is None or not name:
            continue

        zid = int(zid)
        comp_ids = [int(x) for x in (z.get("components") or z.get("Components") or [])]
        comps = [components_by_id[x] for x in comp_ids if x in components_by_id]

        zone = {
            "id": zid,
            "name": name,
            "normalized_name": norm(name),
            "type": z.get("type") or z.get("Type"),
            "layer": z.get("layer") or z.get("Layer"),
            "position": z.get("position") or z.get("Position"),
            "connections": [int(x) for x in (z.get("connections") or z.get("Connections") or [])],
            "component_ids": comp_ids,
            "components": comps,
            "is_avalon": "-" in name,
        }

        zones_by_id[zid] = zone
        zones.append(zone)

    edges = set()

    for z in zones:
        for to_id in z["connections"]:
            if to_id not in zones_by_id:
                continue

            a = z["normalized_name"]
            b = zones_by_id[to_id]["normalized_name"]

            if a != b:
                edges.add(tuple(sorted((a, b))))

    avalon_components = []

    for z in zones:
        if not z["is_avalon"]:
            continue

        avalon_components.append({
            "zone_id": z["id"],
            "name": z["name"],
            "normalized_name": z["normalized_name"],
            "components": z["components"],
            "tiers": sorted({
                c["tier"]
                for c in z["components"]
                if c.get("tier") is not None
            }),
            "component_names": [
                c["name"]
                for c in z["components"]
                if c.get("name")
            ],
        })

    return zones, sorted(edges), avalon_components


def main():
    print("Downloading Albion Navigator data...")

    zone_data = load_json(ZONE_DATA_URL)
    components_data = load_json(ZONE_COMPONENTS_URL)

    zones, edges, avalon_components = extract(zone_data, components_data)

    output = {
        "source": "SugarF0x/albion-navigator",
        "zones_count": len(zones),
        "edges_count": len(edges),
        "avalon_components_count": len(avalon_components),
        "zones": zones,
        "static_edges": [
            {
                "from": a,
                "to": b,
                "source": "albion_navigator_static",
                "weight": 1,
            }
            for a, b in edges
        ],
        "avalon_components": avalon_components,
    }

    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps(output, ensure_ascii=False, indent=2), encoding="utf-8")

    print(f"Saved: {OUT}")
    print(f"Zones: {len(zones)}")
    print(f"Static edges: {len(edges)}")
    print(f"Avalon component records: {len(avalon_components)}")

    if zones:
        print("Example zone:", zones[0]["name"])
    if avalon_components:
        print("Example avalon:", avalon_components[0])


if __name__ == "__main__":
    main()