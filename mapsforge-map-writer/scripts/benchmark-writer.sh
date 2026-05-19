#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE' >&2
Usage:
  mapsforge-map-writer/scripts/benchmark-writer.sh --input input.osm.pbf --output output.map [--runs 3] [--java-version 21] [--docker-image IMAGE] -- [writer options]

Examples:
  mapsforge-map-writer/scripts/benchmark-writer.sh \
    --input testdata/63240150.osm.pbf \
    --output build/bench/63240150.map \
    --runs 3 -- --type ram --threads 1

The benchmark defaults to Java 21. If that JDK is unavailable locally, pass
--docker-image eclipse-temurin:21-jdk or another JDK image to run the build and
writer inside Docker.
USAGE
}

input=""
output=""
runs=3
java_version=21
docker_image=""
writer_args=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --input)
      input="${2:-}"
      shift 2
      ;;
    --output)
      output="${2:-}"
      shift 2
      ;;
    --runs)
      runs="${2:-}"
      shift 2
      ;;
    --java-version)
      java_version="${2:-}"
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
    --)
      shift
      writer_args=("$@")
      break
      ;;
    *)
      echo "Unknown benchmark argument: $1" >&2
      usage
      exit 2
      ;;
  esac
done

if [[ -z "$input" || -z "$output" ]]; then
  usage
  exit 2
fi

if ! [[ "$runs" =~ ^[1-9][0-9]*$ ]]; then
  echo "--runs must be a positive integer" >&2
  exit 2
fi

if [[ ! -f "$input" ]]; then
  echo "Input file not found: $input" >&2
  exit 2
fi

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$repo_root"

docker_run() {
  docker run --rm \
    --user "$(id -u):$(id -g)" \
    -e GRADLE_USER_HOME=/workspace/build/docker-gradle-home \
    -v "$repo_root:/workspace" \
    -w /workspace \
    "$docker_image" \
    "$@"
}

if [[ -n "$docker_image" ]]; then
  if ! command -v docker >/dev/null 2>&1; then
    echo "Docker is required for --docker-image but was not found." >&2
    exit 2
  fi
  docker_run ./gradlew :mapsforge-map-writer:fatJar --no-daemon >/dev/null
  java_cmd=(docker_run java)
  time_cmd=()
elif [[ -z "${JAVA_HOME:-}" ]]; then
  if command -v /usr/libexec/java_home >/dev/null 2>&1; then
    JAVA_HOME="$(/usr/libexec/java_home -v "$java_version" 2>/dev/null || true)"
  fi

  if [[ -z "${JAVA_HOME:-}" || ! -x "$JAVA_HOME/bin/java" ]]; then
    echo "JDK $java_version not found. Set JAVA_HOME or pass --docker-image." >&2
    exit 2
  fi

  detected_java_version="$("$JAVA_HOME/bin/java" -version 2>&1 | awk -F '"' '/version/ {print $2; exit}')"
  if [[ "$detected_java_version" != "$java_version".* ]]; then
    echo "Benchmark requires Java $java_version, but JAVA_HOME reports version $detected_java_version" >&2
    exit 2
  fi

  JAVA_HOME="$JAVA_HOME" ./gradlew :mapsforge-map-writer:fatJar --no-daemon >/dev/null
  java_cmd=("$JAVA_HOME/bin/java")
  time_cmd=(/usr/bin/time -lp)
else
  detected_java_version="$("$JAVA_HOME/bin/java" -version 2>&1 | awk -F '"' '/version/ {print $2; exit}')"
  if [[ "$detected_java_version" != "$java_version".* ]]; then
    echo "Benchmark requires Java $java_version, but JAVA_HOME reports version $detected_java_version" >&2
    exit 2
  fi

  JAVA_HOME="$JAVA_HOME" ./gradlew :mapsforge-map-writer:fatJar --no-daemon >/dev/null
  java_cmd=("$JAVA_HOME/bin/java")
  time_cmd=(/usr/bin/time -lp)
fi

jar_path="mapsforge-map-writer/build/libs/mapsforge-map-writer-master-SNAPSHOT-jar-with-dependencies.jar"
if [[ ! -f "$jar_path" ]]; then
  echo "Writer fat jar not found: $jar_path" >&2
  exit 1
fi

result_dir="$(dirname "$output")/benchmark-$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$result_dir"

summary="$result_dir/summary.csv"
echo "run,status,wall_seconds,peak_rss_bytes,output_bytes" > "$summary"

base_name="$(basename "$output")"
extension="${base_name##*.}"
stem="${base_name%.*}"
if [[ "$stem" == "$extension" ]]; then
  stem="$base_name"
  extension="map"
fi

for run in $(seq 1 "$runs"); do
  run_output="$result_dir/${stem}.run${run}.${extension}"
  stdout_log="$result_dir/run${run}.stdout.log"
  stderr_log="$result_dir/run${run}.stderr.log"
  gc_log="$result_dir/run${run}.gc.log"

  set +e
  started_epoch="$(date +%s)"
  "${time_cmd[@]}" "${java_cmd[@]}" \
    -Xlog:gc*:file="$gc_log":tags,time,uptime,level \
    -jar "$jar_path" \
    --input "$input" \
    --output "$run_output" \
    "${writer_args[@]}" \
    >"$stdout_log" 2>"$stderr_log"
  status=$?
  finished_epoch="$(date +%s)"
  set -e

  wall_seconds="$(awk '/^real / {print $2}' "$stderr_log" | tail -n 1)"
  peak_rss="$(awk '/maximum resident set size/ {print $1}' "$stderr_log" | tail -n 1)"
  if [[ -z "${wall_seconds:-}" ]]; then
    wall_seconds="$((finished_epoch - started_epoch))"
  fi
  output_bytes=0
  if [[ -f "$run_output" ]]; then
    output_bytes="$(wc -c < "$run_output" | tr -d ' ')"
  fi

  echo "$run,$status,${wall_seconds:-},${peak_rss:-},$output_bytes" >> "$summary"
  if [[ "$status" -ne 0 ]]; then
    echo "Run $run failed; see $stderr_log" >&2
    exit "$status"
  fi
done

echo "Benchmark results: $result_dir"
echo "Summary: $summary"
