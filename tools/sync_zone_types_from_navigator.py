import json
from pathlib import Path
from collections import Counter

from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parents[1]

MY_FILE = PROJECT_ROOT / "data" / "albion_locations_all.json"
NAV_FILE = PROJECT_ROOT / "data" / "albion_navigator_import.json"
BACKUP_FILE = PROJECT_ROOT / "data" / "albion_locations_all.backup.json"

print("PROJECT_ROOT =", PROJECT_ROOT)
print("MY_FILE =", MY_FILE, "exists:", MY_FILE.exists())
print("NAV_FILE =", NAV_FILE, "exists:", NAV_FILE.exists())

def norm(name: str) -> str:
    return " ".join(name.lower().strip().split())


def infer_navigator_zone_type(zone: dict) -> str:
    name = zone.get("name", "")
    normalized = zone.get("normalized_name") or norm(name)

    if "-" in name or "-" in normalized or zone.get("is_avalon") is True:
        return "avalon"

    raw_type = str(zone.get("type", "")).lower()
    layer = str(zone.get("layer", "")).lower()

    text = json.dumps(zone, ensure_ascii=False).lower()

    if "city" in raw_type or "city" in text:
        return "city"

    if "island" in raw_type or "island" in text:
        return "island"

    if "arena" in raw_type or "arena" in text:
        return "arena"

    if "dungeon" in raw_type or "dungeon" in text:
        return "dungeon"

    if "black" in raw_type or "black" in layer or "outland" in raw_type or "outland" in layer:
        return "outlands_black"

    if "red" in raw_type or "red" in layer:
        return "red"

    if "yellow" in raw_type or "yellow" in layer:
        return "yellow"

    if "blue" in raw_type or "blue" in layer or "safe" in raw_type or "safe" in layer:
        return "blue"

    # fallback по типичным данным Albion Navigator:
    # layer часто бывает числом/строкой; оставляем unknown, если уверенно не поняли
    return "unknown"


def main():
    my_data = json.loads(MY_FILE.read_text(encoding="utf-8"))
    nav_data = json.loads(NAV_FILE.read_text(encoding="utf-8"))

    nav_by_norm = {}
    for zone in nav_data.get("zones", []):
        name = zone.get("name")
        if not name:
            continue
        n = zone.get("normalized_name") or norm(name)
        nav_by_norm[n] = zone

    BACKUP_FILE.write_text(json.dumps(my_data, ensure_ascii=False, indent=2), encoding="utf-8")

    stats = Counter()
    changed = []
    not_found = []
    nav_unknown = []

    for item in my_data:
        name = item.get("name", "")
        n = item.get("normalized_name") or norm(name)
        item["normalized_name"] = n

        nav_zone = nav_by_norm.get(n)
        if not nav_zone:
            stats["not_found_in_navigator"] += 1
            not_found.append(name)
            continue

        old_type = item.get("zone_type", "unknown")
        new_type = infer_navigator_zone_type(nav_zone)

        if new_type == "unknown":
            stats["navigator_unknown"] += 1
            nav_unknown.append(name)
            continue

        if old_type != new_type:
            item["zone_type"] = new_type
            stats["fixed"] += 1
            changed.append((name, old_type, new_type))
        else:
            stats["same"] += 1

    MY_FILE.write_text(json.dumps(my_data, ensure_ascii=False, indent=2), encoding="utf-8")

    print("=== Zone type sync report ===")
    print(f"My locations: {len(my_data)}")
    print(f"Navigator zones: {len(nav_by_norm)}")
    print(f"Same: {stats['same']}")
    print(f"Fixed: {stats['fixed']}")
    print(f"Not found in navigator: {stats['not_found_in_navigator']}")
    print(f"Navigator unknown type: {stats['navigator_unknown']}")
    print(f"Backup saved: {BACKUP_FILE}")
    print(f"Updated file: {MY_FILE}")

    if changed:
        print("\n=== Fixed examples ===")
        for name, old, new in changed[:50]:
            print(f"{name}: {old} -> {new}")

    if not_found:
        print("\n=== Not found examples ===")
        for name in not_found[:30]:
            print(name)

    if nav_unknown:
        print("\n=== Navigator unknown examples ===")
        for name in nav_unknown[:30]:
            print(name)

    print("\n=== Raw navigator examples ===")
    for name in nav_unknown[:10]:
        n = norm(name)
        zone = nav_by_norm.get(n)
        print("\n---", name, "---")
        print(json.dumps(zone, ensure_ascii=False, indent=2)[:3000])

if __name__ == "__main__":
    main()