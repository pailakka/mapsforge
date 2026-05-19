#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE' >&2
Usage:
  mapsforge-map-writer/scripts/merge-testdata.sh [--output testdata/merged.osm.pbf] [--docker-image IMAGE] [input.osm.pbf ...]

If no inputs are given, all testdata/*.osm.pbf files except the output file are merged.
The script prefers local osmium. If local osmium is unavailable, pass a Docker image
that provides the osmium command.
USAGE
}

output="testdata/merged.osm.pbf"
docker_image=""
inputs=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --output)
      output="${2:-}"
      shift 2
      ;;
    --docker-image)
      docker_image="${2:-}"
      shift 2
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      inputs+=("$1")
      shift
      ;;
  esac
done

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$repo_root"

if [[ "${#inputs[@]}" -eq 0 ]]; then
  while IFS= read -r input; do
    if [[ "$input" != "$output" ]]; then
      inputs+=("$input")
    fi
  done < <(find testdata -maxdepth 1 -type f -name '*.osm.pbf' | sort)
fi

if [[ "${#inputs[@]}" -lt 2 ]]; then
  echo "Need at least two input PBF files to merge." >&2
  exit 2
fi

mkdir -p "$(dirname "$output")"
rm -f "$output"

if command -v osmium >/dev/null 2>&1; then
  osmium merge --overwrite -o "$output" "${inputs[@]}"
  osmium fileinfo -e "$output"
elif [[ -n "$docker_image" ]]; then
  docker run --rm \
    --user "$(id -u):$(id -g)" \
    -v "$repo_root:/workspace" \
    -w /workspace \
    "$docker_image" \
    osmium merge --overwrite -o "$output" "${inputs[@]}"
  docker run --rm \
    --user "$(id -u):$(id -g)" \
    -v "$repo_root:/workspace" \
    -w /workspace \
    "$docker_image" \
    osmium fileinfo -e "$output"
else
  echo "osmium not found. Install osmium-tool or pass --docker-image with an osmium image." >&2
  exit 2
fi
