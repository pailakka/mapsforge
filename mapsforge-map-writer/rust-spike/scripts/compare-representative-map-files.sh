#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -ne 2 ]]; then
  echo "usage: $0 <java-reference.map> <rust-output.map>" >&2
  exit 2
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
left_map="$1"
right_map="$2"
scan_zooms="${SCAN_ZOOMS:-10,12,14}"
max_abs_delta_pois="${MAX_ABS_DELTA_POIS:-0}"
max_abs_delta_ways="${MAX_ABS_DELTA_WAYS:-0}"
classes_dir="${CLASSES_DIR:-$(mktemp -d /tmp/mapsforge-map-representative-classes.XXXXXX)}"

abs() {
  local value="$1"
  if (( value < 0 )); then
    echo $(( -value ))
  else
    echo "$value"
  fi
}

status=0
IFS=',' read -r -a zooms <<< "$scan_zooms"
for zoom in "${zooms[@]}"; do
  log_file="$(mktemp "/tmp/mapsforge-map-compare-z${zoom}.XXXXXX")"
  CLASSES_DIR="$classes_dir" "$script_dir/compare-map-files.sh" "$left_map" "$right_map" "$zoom" | tee "$log_file"

  delta_pois="$(awk -F= '$1 == "delta_pois" {print $2}' "$log_file")"
  delta_ways="$(awk -F= '$1 == "delta_ways" {print $2}' "$log_file")"
  if [[ -z "$delta_pois" || -z "$delta_ways" ]]; then
    echo "missing delta output for zoom $zoom" >&2
    status=1
    continue
  fi

  abs_delta_pois="$(abs "$delta_pois")"
  abs_delta_ways="$(abs "$delta_ways")"
  echo "representative_zoom=$zoom abs_delta_pois=$abs_delta_pois abs_delta_ways=$abs_delta_ways"

  if (( abs_delta_pois > max_abs_delta_pois || abs_delta_ways > max_abs_delta_ways )); then
    echo "representative parity failed at zoom $zoom: delta_pois=$delta_pois delta_ways=$delta_ways" >&2
    status=1
  fi
done

exit "$status"
