#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -ne 2 ]]; then
  echo "usage: $0 <left-debug.map> <right-debug.map>" >&2
  exit 2
fi

left_map="$1"
right_map="$2"
left_label="${LEFT_LABEL:-left}"
right_label="${RIGHT_LABEL:-right}"
max_way_deltas="${MAX_WAY_DELTAS:-50}"

if [[ ! -f "$left_map" ]]; then
  echo "missing left debug map file: $left_map" >&2
  exit 1
fi

if [[ ! -f "$right_map" ]]; then
  echo "missing right debug map file: $right_map" >&2
  exit 1
fi

python3 - "$left_map" "$right_map" "$left_label" "$right_label" "$max_way_deltas" <<'PY'
import collections
import re
import sys

left_path, right_path, left_label, right_label, max_way_deltas = sys.argv[1:]
max_way_deltas = int(max_way_deltas)
signature_re = re.compile(rb"(###TileStart(-?\d+),(-?\d+)###|---WayStart([0-9]+)---)")


def parse(path):
    data = open(path, "rb").read()
    current_tile = None
    way_counts = collections.Counter()
    way_tiles = collections.defaultdict(list)

    for match in signature_re.finditer(data):
        if match.group(2) is not None:
            current_tile = (int(match.group(2)), int(match.group(3)))
            continue

        way_id = int(match.group(4))
        way_counts[way_id] += 1
        if current_tile is not None:
            way_tiles[way_id].append(current_tile)

    return way_counts, way_tiles


def format_tiles(tiles):
    return ",".join(f"{x}:{y}" for x, y in tiles)


left_counts, left_tiles = parse(left_path)
right_counts, right_tiles = parse(right_path)
all_way_ids = sorted(set(left_counts) | set(right_counts))
differing_way_ids = [
    way_id for way_id in all_way_ids if left_counts[way_id] != right_counts[way_id]
]

print(f"{left_label}_records={sum(left_counts.values())}")
print(f"{left_label}_unique_ways={len(left_counts)}")
print(f"{right_label}_records={sum(right_counts.values())}")
print(f"{right_label}_unique_ways={len(right_counts)}")
print(f"delta_records={sum(left_counts.values()) - sum(right_counts.values())}")
print(f"differing_way_ids={len(differing_way_ids)}")

for way_id in differing_way_ids[:max_way_deltas]:
    print(
        "way_delta="
        f"{way_id} "
        f"{left_label}_records={left_counts[way_id]} "
        f"{right_label}_records={right_counts[way_id]} "
        f"delta={left_counts[way_id] - right_counts[way_id]} "
        f"{left_label}_tiles={format_tiles(left_tiles.get(way_id, []))} "
        f"{right_label}_tiles={format_tiles(right_tiles.get(way_id, []))}"
    )
PY
