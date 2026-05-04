#!/usr/bin/env python3
import re
import urllib.request
from pathlib import Path

WORLD_TXT_URL = "https://raw.githubusercontent.com/broderickhyman/ao-bin-dumps/master/formatted/world.txt"

OUT_ALL = Path("data/albion_locations_all.txt")
OUT_AVALON = Path("albion_locations_avalon.txt")

LINE_RE = re.compile(r"^\s*([^:#]+):\s*(.+?)\s*$")


import ssl
import certifi
import urllib.request

def download_world_txt() -> str:
    context = ssl.create_default_context(cafile=certifi.where())

    with urllib.request.urlopen(WORLD_TXT_URL, timeout=30, context=context) as response:
        return response.read().decode("utf-8", errors="replace")

def clean_location_name(name: str) -> str:
    name = name.strip()
    name = re.sub(r"\s+", " ", name)
    return name


def main() -> None:
    text = download_world_txt()

    all_locations = set()
    avalon_locations = set()

    for line in text.splitlines():
        match = LINE_RE.match(line)
        if not match:
            continue

        location_id = match.group(1).strip()
        location_name = clean_location_name(match.group(2))

        if not location_name:
            continue

        all_locations.add(location_name)

        # Roads of Avalon / Avalon roads в dump идут как TNL-xxx
        if location_id.startswith("TNL-"):
            avalon_locations.add(location_name)

    OUT_ALL.write_text(
        "\n".join(sorted(all_locations, key=str.lower)) + "\n",
        encoding="utf-8",
    )

    OUT_AVALON.write_text(
        "\n".join(sorted(avalon_locations, key=str.lower)) + "\n",
        encoding="utf-8",
    )

    print(f"Saved {len(all_locations)} locations to {OUT_ALL}")
    print(f"Saved {len(avalon_locations)} Avalon locations to {OUT_AVALON}")

    # Проверка важных примеров
    for name in ["Xebos-Emimsum", "Eldon Hill", "Blackthorn Quarry"]:
        print(f"{name}: {'OK' if name in all_locations else 'MISSING'}")


if __name__ == "__main__":
    main()