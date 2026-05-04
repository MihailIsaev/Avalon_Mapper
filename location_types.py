#!/usr/bin/env python3
import json
import re
import ssl
import time
import urllib.parse
import urllib.request
from pathlib import Path
from html import unescape

try:
    import certifi
except ImportError:
    certifi = None


INPUT_NAMES = Path("data/albion_locations_all.txt")
OUT_JSON = Path("data/albion_locations_all.json")
UNKNOWN_TXT = Path("data/albion_locations_unknown.txt")
CACHE_JSON = Path("data/cache/albiondatabase_zone_cache.json")

WORLD_TXT_URL = "https://raw.githubusercontent.com/ao-data/ao-bin-dumps/master/formatted/world.txt"
ALBION_DATABASE_MAP_URL = "https://www.albiondatabase.com/interactive-map"

WORLD_RE = re.compile(r"([A-Za-z0-9#_-]+):\s*(.*?)(?=\s+[A-Za-z0-9#_-]+:\s|$)")

CITY_NAMES = {
    "Bridgewatch",
    "Caerleon",
    "Fort Sterling",
    "Lymhurst",
    "Martlock",
    "Thetford",
    "Brecilien",
}

OUTLAND_RESTS = {
    "Arthur's Rest",
    "Merlyn's Rest",
    "Morgana's Rest",
}


def make_ssl_context():
    if certifi:
        return ssl.create_default_context(cafile=certifi.where())
    return ssl._create_unverified_context()


def http_get(url: str) -> str:
    req = urllib.request.Request(
        url,
        headers={
            "User-Agent": "Mozilla/5.0 AvalonMapper/1.0",
            "Accept": "text/html,application/json,*/*",
        },
    )

    with urllib.request.urlopen(req, timeout=60, context=make_ssl_context()) as response:
        return response.read().decode("utf-8", errors="replace")


def clean_name(name: str) -> str:
    return re.sub(r"\s+", " ", name.strip())


def normalize_name(name: str) -> str:
    return clean_name(name).lower()


def slugify_location(name: str) -> str:
    """
    Wyre Forest -> wyre-forest
    Arthur's Rest -> arthurs-rest
    """
    s = name.lower()
    s = s.replace("'", "")
    s = re.sub(r"[^a-z0-9]+", "-", s)
    s = s.strip("-")
    return s


def load_input_names() -> list[str]:
    if not INPUT_NAMES.exists():
        raise FileNotFoundError(f"Не найден файл: {INPUT_NAMES}")

    names = []

    for line in INPUT_NAMES.read_text(encoding="utf-8").splitlines():
        name = clean_name(line)
        if name:
            names.append(name)

    return names


def load_cache() -> dict:
    if CACHE_JSON.exists():
        return json.loads(CACHE_JSON.read_text(encoding="utf-8"))
    return {}


def save_cache(cache: dict) -> None:
    CACHE_JSON.parent.mkdir(parents=True, exist_ok=True)
    CACHE_JSON.write_text(
        json.dumps(cache, ensure_ascii=False, indent=2),
        encoding="utf-8",
    )


def load_world_mapping() -> dict[str, list[str]]:
    """
    name -> [cluster_id]
    Нужно для Avalon: TNL-*.
    """
    text = http_get(WORLD_TXT_URL)

    mapping = {}

    for cluster_id, raw_name in WORLD_RE.findall(text):
        name = clean_name(raw_name)
        norm = normalize_name(name)

        if not norm:
            continue

        mapping.setdefault(norm, []).append(cluster_id)

    return mapping


def normalize_source_zone_type(raw: str) -> str | None:
    text = unescape(raw)
    text = re.sub(r"<[^>]+>", " ", text)
    text = clean_name(text).lower()

    if re.search(r"\bblue\s+zone\b", text):
        return "blue"

    if re.search(r"\byellow\s+zone\b", text):
        return "yellow"

    if re.search(r"\bred\s+zone\b", text):
        return "red"

    if re.search(r"\bblack\s+zone\b", text):
        return "outlands_black"

    return None


def extract_zone_type_from_html(html: str, name: str) -> str | None:
    """
    Ищет фрагменты типа:
    T6 Red Zone
    T4 Blue Zone
    Yellow Zone
    Black Zone
    """
    html = unescape(html)

    # Сначала ищем рядом с названием локации.
    name_re = re.escape(name)
    near_name_patterns = [
        rf"{name_re}.{{0,800}}?\b(?:T\d+\s+)?(Blue|Yellow|Red|Black)\s+Zone\b",
        rf"\b(?:T\d+\s+)?(Blue|Yellow|Red|Black)\s+Zone\b.{{0,800}}?{name_re}",
    ]

    for pattern in near_name_patterns:
        m = re.search(pattern, html, flags=re.I | re.S)
        if m:
            return normalize_source_zone_type(m.group(0))

    # Потом общий fallback.
    m = re.search(r"\b(?:T\d+\s+)?(Blue|Yellow|Red|Black)\s+Zone\b", html, flags=re.I)
    if m:
        return normalize_source_zone_type(m.group(0))

    return None


def fetch_zone_from_albiondatabase(name: str, cache: dict) -> str | None:
    key = normalize_name(name)

    if key in cache:
        return cache[key]

    candidates = [
        f"{ALBION_DATABASE_MAP_URL}/{slugify_location(name)}",
        f"{ALBION_DATABASE_MAP_URL}/{urllib.parse.quote(name.replace(' ', '-'))}",
        f"{ALBION_DATABASE_MAP_URL}/{urllib.parse.quote(name)}",
    ]

    zone_type = None
    last_error = None

    for url in candidates:
        try:
            html = http_get(url)
            zone_type = extract_zone_type_from_html(html, name)
            if zone_type:
                break
        except Exception as e:
            last_error = str(e)

    cache[key] = zone_type
    save_cache(cache)

    if zone_type is None and last_error:
        print(f"WARNING: {name}: {last_error}")

    time.sleep(0.2)
    return zone_type


def infer_zone_type(name: str, cluster_ids: list[str], cache: dict) -> str:
    if any(cid.startswith("TNL-") for cid in cluster_ids):
        return "avalon"

    if name in CITY_NAMES:
        return "city"

    if name in OUTLAND_RESTS:
        return "outlands_black"

    zone_from_db = fetch_zone_from_albiondatabase(name, cache)
    if zone_from_db:
        return zone_from_db

    return "unknown"


def main() -> None:
    input_names = load_input_names()
    world_mapping = load_world_mapping()
    cache = load_cache()

    result = []
    unknown = []

    for i, name in enumerate(input_names, start=1):
        norm = normalize_name(name)
        cluster_ids = world_mapping.get(norm, [])

        zone_type = infer_zone_type(name, cluster_ids, cache)

        result.append(
            {
                "name": name,
                "normalized_name": norm,
                "zone_type": zone_type,
            }
        )

        if zone_type == "unknown":
            unknown.append(name)

        print(f"[{i}/{len(input_names)}] {name} -> {zone_type}")

    OUT_JSON.parent.mkdir(parents=True, exist_ok=True)

    OUT_JSON.write_text(
        json.dumps(result, ensure_ascii=False, indent=2),
        encoding="utf-8",
    )

    UNKNOWN_TXT.write_text(
        "\n".join(unknown) + ("\n" if unknown else ""),
        encoding="utf-8",
    )

    counts = {}
    for item in result:
        counts[item["zone_type"]] = counts.get(item["zone_type"], 0) + 1

    print()
    print(f"Saved: {OUT_JSON}")
    print(f"Unknown: {len(unknown)}")
    print(f"Unknown list: {UNKNOWN_TXT}")

    print()
    print("Zone counts:")
    for zone_type, count in sorted(counts.items()):
        print(f"  {zone_type}: {count}")


if __name__ == "__main__":
    main()