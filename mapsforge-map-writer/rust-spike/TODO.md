# Rust Writer Rewrite TODO

This document is the implementation checklist for turning the Rust spike into a standalone mapsforge writer CLI.

The decision is to continue with Rust for the writer-only path. Java remains the correctness oracle and fallback until the Rust writer produces valid mapsforge `.map` files, passes reader/render checks, and meets the performance target.

## Target

- [x] Produce valid mapsforge `.map` output from `testdata/merged.osm.pbf`.
- [x] Complete the merged dataset conversion in under 45 seconds.
- [x] Keep peak RSS no worse than the current Java baseline.
- [ ] Preserve format compatibility, not byte-identical Java output.
- [x] Keep the first production milestone scoped to the standalone writer CLI, not Osmosis plugin replacement.
- [x] Use pure Rust geometry by default; do not introduce GEOS or other native geometry dependencies for v1.

## Current Baseline

- Rust gate:
  - [x] Entity counts match the Java/Osmium baseline.
  - [x] `tile-index` resolves all referenced nodes for handled ways.
  - [x] Merged writer runs complete in roughly `10.9s` reported Rust elapsed time on the adaptive referenced-node local release run.
  - [x] Peak RSS is roughly `86.9 MB` on the adaptive referenced-node local writer run with memory-first staged-way and tile-bucket caps.
  - [x] Rust geo polygon-clipped output size is `1,456,597` bytes for the current `--type ram` alias run with Java-style microdegree truncation, Java-style closed-way area semantics, line noding, default simplification, and simple multipolygon ring support.
  - [x] Larger Finland fixture completed end-to-end with `elapsed_millis=2,525,275`, `peak_rss_bytes=1,479,540,736`, output size `730,167,579` bytes, and `missing_way_nodes=0`.
  - [x] Finland reader/render smoke with `internal:DEFAULT` passed: z14 scan `tiles_with_data=325,638`, `pois=57,286`, `ways=6,695,826`, render PNG `1,495` bytes with `render_sampled_colors=2`.
  - [ ] Finland z14 write throughput is still the main performance gap: the full memory-first run reported `write_map_file_millis=2,322,448`, with dense z14 bands causing `7,994,615` staged-way cache misses at a `4,096` entry cache cap. A bounded-LRU staged-way cache experiment was stopped at `500,000` z14 tiles because it reduced misses by only about `0.26%` at that checkpoint while raising peak RSS to about `1.51 GB`. Progress logs now report per-chunk way-bucket fan-out (`bucket_entries`, `unique_staged_ways`, `generated_entries`) and write output counters (`tiles`, `nonempty_tiles`, `payload_bytes`, `poi_entries`, `way_entries`) so the next throughput experiment can target tile/write locality without guessing.
- Java HD comparison:
  - [x] `standalone-total=49.6s` on the fresh local HD reference run.
  - [x] `/usr/bin/time -lp real=49.71s`.
  - [x] Peak RSS is `508,575,744` bytes on the fresh local HD reference run.
  - [x] Java HD output size is `1,345,927` bytes.
  - [x] Java HD output opens with mapsforge `MapFile`.
  - [x] Java HD z14 scan reports `tiles_with_data=4,675`, `pois=89`, `ways=27,075`.
  - [x] Java HD peruskartta render smoke passes.
- Java RAM comparison:
  - [x] Decide that HD is the relevant Java oracle for this dataset.
  - [x] Default-heap RAM attempt failed with `java.lang.OutOfMemoryError: Java heap space`.
  - [x] Explicit RAM attempts with `-Xmx2g` and `-Xmx4g` also failed with `java.lang.OutOfMemoryError: Java heap space`.
- Known Rust gaps:
  - [x] Structural `.map` bytes, POI payloads, and simple way payloads are written.
  - [ ] Simplification uses `geo::Simplify` (Douglas-Peucker) instead of Java's `TopologyPreservingSimplifier`. In practice the Java simplifier also falls back to Douglas-Peucker for most geometries; the difference is that `TopologyPreservingSimplifier` preserves topology for self-intersecting geometries. For v1, accept `geo::Simplify` and document the known divergence.
  - [x] Representative render/content parity validation passes for the current sampled z10/z12/z14 content sweep, debug-signature comparison, and sampled rendered tile.

## Performance: Finland z14 Write Throughput

The Finland fixture's z14 `write_map_file` phase dominates wall time at ~39 minutes out of ~42 minutes total. The root cause is the staged-way-to-tile-serialization access pattern: each z14 tile's way bucket resolves ways by random-seeking into the staged-way temp file, producing ~8M cache misses against a 4K-entry FIFO cache. Increasing cache size to match the working set would blow memory; changing eviction policy (LRU) proved ineffective because the access pattern is not temporally local—it is spatially local within each tile-row chunk but each way fans out across many tiles in different chunks.

### Prioritized optimization plan

- [ ] **P0: Bulk-read staged ways per chunk instead of per-tile random seeks.**
  The current flow: `build_way_buckets_for_interval` scans *summaries* (index file) sequentially to build bucket entries by offset, then `write_tile_payload` resolves each offset individually via `StagedWayReader::read_at` with random `seek+read`. The optimization: once way buckets are built for a chunk, collect all unique staged-way offsets referenced by that chunk, sort them, do a single sequential pass over the staged-way data file to bulk-read all needed `WriterWay` records into a temporary `HashMap<u64, WriterWay>`, then serve tile serialization from that in-memory map. This eliminates ~8M random seeks in favor of ~N sequential reads per chunk (where N is the number of unique ways referenced). Memory cost is bounded because chunks are already capped at `WAY_BUCKET_CHUNK_TILE_LIMIT` tiles.
  - Expected impact: **10–50× speedup** on the z14 phase by converting random I/O to sequential I/O.

- [ ] **P1: Use `BufReader` for staged-way data file reads.**
  The `StagedWayReader` currently uses raw `File` without buffering, so every `seek+read` is a syscall pair. Wrapping in `BufReader` with an 8–16 MB buffer reduces syscall count for both the current random-seek path and the proposed bulk-read path.
  - Expected impact: **2–5× syscall reduction** if combined with sorted offset reads.

- [ ] **P2: Use `BufWriter` for staged-way data file writes in pass4.**
  `StagedWayWriter` currently calls `File::write_all` for each way record (4-byte length + variable payload). Adding `BufWriter` reduces write syscalls during pass4.
  - Expected impact: modest, but free.

- [ ] **P3: Pre-serialize way tile payloads during bucket construction.**
  Currently `write_tile_payload` re-reads each way, clips it, simplifies it, and serializes it per-tile. For ways that appear in many tiles (large polygons), the clip/simplify/encode work is repeated. Consider caching the clipped coordinate blocks per-tile during bucket construction, or at least caching the resolved `WriterWay` across tiles within the same chunk.
  - Expected impact: reduces CPU cost for large multipolygon ways that span many tiles.

- [ ] **P4: Parallelize tile serialization within a chunk.**
  Once ways are bulk-loaded into memory for a chunk, individual tile payloads are independent and can be serialized in parallel using `rayon` or scoped threads. The tile index and file writes must remain sequential, but the CPU-heavy clipping/encoding can be parallelized.
  - Expected impact: **2–4× CPU-bound speedup** on multi-core systems, after I/O bottleneck is resolved.
  - Dependency: requires P0 (bulk-read) to be effective.

- [ ] **P5: Compress staged-way records.**
  The staged-way temp file for Finland is large (several GB). Using delta/varint encoding for coordinates within staged records (like the disk node index already does) would reduce file size and I/O volume. LZ4 frame compression is another option but adds a dependency.
  - Expected impact: 30–60% reduction in staged-way I/O volume.

### Non-goals for throughput optimization

- Do not increase `STAGED_WAY_CACHE_LIMIT` beyond 4,096 entries. The access pattern is not cache-friendly; increasing cache size has diminishing returns and raises peak RSS.
- Do not switch to LRU eviction. Measured improvement was 0.26% at the cost of complexity and memory.
- Do not mmap the staged-way file. The OS page cache already provides mmap-like behavior, and the problem is access pattern, not syscall overhead.

## Memory Optimization

Current memory profile:
- Merged dataset: ~87 MB peak RSS (well under Java's ~509 MB).
- Finland dataset: ~1.48 GB peak RSS (2.9× Java's ~509 MB).

### Prioritized memory optimization plan

- [ ] **M0: Reduce `WriterWay` clone overhead in staged-way cache.**
  `StagedWayReader::read_at` clones every cache hit. For ways with many coordinates, this is significant allocation pressure. Consider using `Arc<WriterWay>` in the cache to share ownership without cloning.
  - Expected impact: reduces allocation pressure and peak RSS under heavy cache hit rates.

- [ ] **M1: Compact tag storage in `WriterWay`.**
  Each `WriterWay` stores `Vec<u16>` for tag IDs and `Vec<Option<TagValue>>` for tag values. Most ways have 1–3 tags. Using `SmallVec<[u16; 4]>` and `SmallVec<[Option<TagValue>; 4]>` would eliminate heap allocation for the common case.
  - Expected impact: reduces per-way allocation count.
  - Note: requires adding `smallvec` dependency.

- [ ] **M2: Intern string tag values.**
  `TagValue::String(String)` creates a separate heap allocation per string tag value. Many values repeat (e.g., building colors, surface types). Using a string interner or `Arc<str>` for repeated values would reduce memory.
  - Expected impact: moderate for datasets with many tag-value ways.

- [ ] **M3: Release multipolygon relation data earlier.**
  `relation_way_geometries` holds all way geometries for multipolygon relations until after `multipolygon_assembly`. For Finland, this can be hundreds of MB. Consider processing multipolygon relations incrementally during pass4 instead of collecting all geometries upfront.
  - Expected impact: significant for large datasets with many multipolygon relations.

- [ ] **M4: Shrink `SpecialTags` struct.**
  `SpecialTags` has 6 fields including 4 `Option<String>`. Most elements have no special tags beyond `name`. Consider a two-tier representation: an inline struct for the common case (just name + layer) and a heap-allocated extension for the rare case (housenumber, ref, relation_type, elevation).
  - Expected impact: modest; most memory is in coordinates, not tags.

## Milestone 1: Rust Writer Skeleton

- [x] Add `--output <map>` to the Rust CLI.
- [x] Preserve existing arguments:
  - [x] `--input <osm.pbf>`
  - [x] `--bbox minLat,minLon,maxLat,maxLon`
  - [x] `--tag-conf-file <xml>`
  - [x] `--mode count|tile-index`
- [x] Add writer arguments:
  - [x] `--zoom-interval-conf <base,min,max,...>`
  - [x] `--bbox-enlargement <meters>`
  - [x] `--encoding auto|single|double`
  - [x] `--tag-values true|false`
  - [x] `--preferred-languages <comma-separated languages>`
  - [x] `--map-start-position <lat,lon>`
  - [x] `--map-start-zoom <zoom>`
  - [x] `--comment <text>`
  - [x] `--type hd|ram`
  - [x] `--progress-logs true|false`
  - [x] `--debug-file true|false`
- [x] Treat `--type hd` and `--type ram` as compatibility aliases for the same Rust indexed pipeline.
- [x] Reject unsupported v1 options with clear errors:
  - [x] `--label-position true`
  - [x] `--polylabel true`
- [x] Keep Java writer behavior unchanged.

## Milestone 2: Mapsforge Binary Encoder

- [x] Implement fixed-width big-endian integer writes:
  - [x] `u16`/`i16`
  - [x] `u32`/`i32`
  - [x] `u64`/`i64`
- [x] Implement mapsforge 5-byte unsigned offsets.
- [x] Implement signed variable-byte encoding.
- [x] Implement unsigned variable-byte encoding.
- [x] Implement UTF-8 string encoding compatible with mapsforge reader expectations.
- [x] Write the mapsforge file header:
  - [x] magic bytes
  - [x] header size placeholder
  - [x] file specification version
  - [x] file size placeholder
  - [x] creation date
  - [x] bounding box
  - [x] tile size
  - [x] projection
  - [x] optional header flags
  - [x] map start position
  - [x] map start zoom
  - [x] preferred languages
  - [x] comment
  - [x] created-with string
  - [x] optimized POI tag table
  - [x] optimized way tag table
  - [x] zoom interval metadata placeholders
- [x] Write subfile indexes.
- [x] Write tile blocks.
- [x] Patch header size after header construction.
- [x] Patch final file size after output completion.
- [x] Patch subfile offsets and sizes after subfile streaming.

## Milestone 3: Data Model And Tag Semantics

- [ ] Convert diagnostic structs into writer data structures:
  - [ ] nodes
  - [x] POIs
  - [x] ways
  - [ ] relations
  - [x] tags
  - [ ] zoom intervals
  - [x] sparse tile buckets
  - [x] compact per-tile way row grouping that resolves full staged way records one at a time during serialization
  - [x] Preserve tag mapping XML behavior:
  - [x] POI tags
  - [x] way tags
  - [x] equivalent values
  - [x] renderable flag
  - [x] `zoom-appear`
  - [x] wildcard tag values
  - [x] optimized tag ordering from actual output frequency
  - [x] `force-polygon-line`
- [x] Implement special tag extraction:
  - [x] `name`
  - [x] localized names using `--preferred-languages`
  - [x] `ref`
  - [x] `addr:housenumber`
  - [x] `layer`
  - [x] `ele`
  - [x] relation `type`
- [x] Implement `--tag-values false` by default.
- [x] Implement `--tag-values true` for numeric, string, hex color, CSS named color, and wildcard-backed values.
- [x] Port Java CSS named color table for tag values.
- [x] Enforce mapsforge tag-count limits with clear errors for all serialized elements.
- [x] Match Java `LatLongUtils.degreesToMicrodegrees` truncation semantics for OSM coordinates, clipped geometry, tile origins, bbox metadata, and start-position metadata.

## Milestone 4: Tile Assignment And Geometry

- [x] Use an adaptive node index: referenced-node-only for sparse extracts, with disk-backed block-index fallback when the referenced-node set is dense.
- [x] Assign POIs to all zoom intervals where their minimum zoom is visible.
- [x] Assign ways to sparse tile buckets by zoom interval.
- [x] Implement line-to-tile intersection.
- [x] Implement simple closed polygon-to-tile intersection.
- [x] Match Java closed-way area heuristics for line-vs-polygon clipping.
- [x] Compute subtile masks for ways.
- [x] Implement pure Rust clipping using `geo` where available:
  - [x] open polyline tile clipping
  - [x] simple closed polygon tile clipping using `geo::BooleanOps`
  - [x] simple closed inner-ring tile clipping for single-outer multipolygons
  - [x] multipolygon/inner-ring clipping for supported polygonized relations, including inner rings clipped into tile-boundary notches
- [x] Implement Java-default simplification gate using `geo::Simplify`.
- [ ] Match Java `TopologyPreservingSimplifier` behavior exactly where it matters.
  - Note: for v1, accept `geo::Simplify` (Douglas-Peucker). Java's `TopologyPreservingSimplifier` wraps Douglas-Peucker with additional topology checks for self-intersecting inputs. The practical difference is negligible for valid geometries, and the current content sweep is zero-delta. Document as a known v1 divergence.
- [x] Respect `--bbox-enlargement`.
- [x] Support coastlines as far as current Java HD path requires for the primary dataset.
  - [x] Verified `testdata/merged.osm.pbf` has zero `natural=coastline` ways or relations with `osmium tags-filter`.
- [x] Implement multipolygon support:
  - [x] parse render-relevant multipolygon relations
  - [x] collect outer and inner way members
  - [x] stitch one endpoint-stitchable outer ring and one endpoint-stitchable inner ring
  - [x] stitch multi-segment inner member rings into single virtual holes
  - [x] merge supported relation tags into single closed outer member ways
  - [x] merge supported relation tags into stitched/generated output ways
  - [x] handle simple holes for one closed outer way and closed inner ways
  - [x] suppress standalone inner member ways when their render tags are already covered by the relation/outer tags
  - [x] fail clearly on unsupported or invalid relation geometry
- [x] Do not silently write questionable geometry when relation assembly fails.

## Milestone 5: POI And Way Serialization

- [x] Serialize POIs:
  - [x] latitude and longitude offsets from tile origin
  - [x] layer/tag-count byte
  - [x] optimized tag IDs
  - [x] optional tag values
  - [x] feature byte
  - [x] name
  - [x] housenumber
  - [x] elevation
- [x] Serialize ways:
  - [x] subtile mask
  - [x] layer/tag-count byte
  - [x] optimized tag IDs
  - [x] optional tag values
  - [x] feature byte
  - [x] name
  - [x] housenumber
  - [x] ref
  - [x] coordinate block count
  - [x] outer/simple coordinate block
  - [x] inner coordinate blocks for supported multipolygons
- [x] Implement coordinate encodings:
  - [x] single delta
  - [x] double delta
  - [x] auto selection based on serialized size
- [x] Write zoom-level tables for each tile.
- [x] Write offset to first way in each tile payload.
- [x] Preserve deterministic output ordering where practical.

## Milestone 6: Validation Harness

- [x] Add a sample reader smoke script using `testdata/63240150.osm.pbf`.
- [x] Use the matching peruskartta render-theme fixture path for the small and merged sample checks: `testdata/peruskartta/Peruskartta.xml`.
- [x] Support mapsforge's built-in default render theme in render smoke and pixel-compare helpers for the larger `testdata/finland-260517.osm.pbf` fixture.
- [x] Support multi-zoom reader smoke scans through `SCAN_ZOOMS`.
- [x] Run the larger `testdata/finland-260517.osm.pbf` fixture through the writer and render smoke path with `internal:DEFAULT`.
- [x] Generate Java reference maps for the merged dataset:
  - [x] `type=hd`
  - [x] `type=ram` attempted with explicit heaps and rejected as the oracle for this dataset because it runs out of heap.
- [x] Generate Rust maps for the same commands.
- [x] Open Rust output with mapsforge `MapFile`.
- [x] Compare required metadata:
  - [x] bbox compared; Rust and Java HD now match after Java-style microdegree truncation.
  - [x] zoom intervals
  - [x] tag tables compared; tag sets match, optimized order differs.
  - [x] map start position
  - [x] map start zoom
  - [x] preferred languages
  - [x] comment
- [x] Compare content diagnostics:
  - [x] entity counts
  - [x] output size range for the current local Rust/Java HD outputs
  - [x] sampled tile POI counts; z14 aggregate matches Java HD at `89`.
  - [x] sampled tile way counts; current representative content sweep is exact at z10, z12, and z14.
  - [x] debug way-signature comparison is zero-delta for the current Java HD and Rust debug maps.
  - [x] sampled rendered-pixel comparison is wired up; current smoke tile `9410,4528,14` differs by `0` pixels.
  - [x] key POI tag table set
  - [x] key way tag table set
  - [x] missing-node count
  - [x] unsupported relation count
- [x] Add a render smoke check for a sampled data tile using the peruskartta theme.
- [x] Add broader representative render/content parity checks for sampled tiles.
  - [x] Add representative aggregate content comparison helper for z10/z12/z14.
  - [x] Add debug way-signature comparison helper for `--debug-file true` maps.
  - [x] Add sampled rendered-pixel comparison helper using the peruskartta theme.
  - [x] Drive sampled render/content comparison to zero-delta parity.
- [x] Store benchmark output for:
  - [x] wall time
  - [x] peak RSS
  - [x] output size
  - [x] phase timings
  - [x] way-bucket fan-out and chunk payload counters for throughput diagnosis
- [ ] Keep Java fallback available until all acceptance checks pass.

## Milestone 7: Finland Throughput And Memory

This milestone gates the Rust writer on the Finland-scale dataset.

- [ ] Implement P0 (bulk-read staged ways per chunk) and re-measure Finland `write_map_file_millis`.
  - Target: reduce from ~2,322s to under 120s for the z14 phase.
- [ ] Implement P1 (BufReader for staged-way reads) alongside P0.
- [ ] Implement P2 (BufWriter for staged-way writes).
- [ ] Re-measure Finland peak RSS after P0–P2. Target: under 1 GB.
- [ ] If z14 phase is still over 120s after P0–P2, implement P3 (pre-serialize or cache resolved ways within a chunk).
- [ ] If CPU-bound after I/O optimization, evaluate P4 (parallel tile serialization with `rayon`).
- [ ] Run Finland reader/render smoke with `internal:DEFAULT` after throughput optimization.
- [ ] Run Finland representative content sweep against Java HD reference.
- [ ] Measure and document final Finland wall time, peak RSS, and output size.

## Acceptance Checklist

- [x] `cargo fmt --check`
- [x] `cargo check`
- [x] `cargo test`
- [x] `JAVA_HOME=$(/usr/libexec/java_home -v 17) ./gradlew :mapsforge-map-writer:test :mapsforge-map-writer:fatJar --no-daemon`
- [x] Rust output opens with mapsforge reader.
- [x] Rust merged dataset runtime is under 45 seconds.
- [x] Rust peak RSS is no worse than Java baseline.
- [x] Representative render smoke check passes.
- [x] Representative render/content parity checks pass.
- [x] Unsupported options fail with clear messages.
- [x] Java remains available as fallback.
- [x] `git diff --check`
- [ ] Finland dataset runtime is under 300 seconds (5 minutes).
- [ ] Finland peak RSS is under 1 GB.
- [ ] Finland reader/render smoke passes.

## Non-Goals For V1

- [x] Do not replace the Osmosis plugin in the first Rust milestone.
- [x] Do not require byte-identical Java output.
- [x] Do not add GEOS or other native geometry dependencies by default.
- [x] Do not support `label-position=true` in v1.
- [x] Do not support `polylabel=true` in v1.
- [ ] Do not implement full `TopologyPreservingSimplifier` parity; accept `geo::Simplify` for v1.
- [ ] Do not implement coastline merging; the primary dataset has no coastlines and the Finland fixture does not require it for the current content gate.

## Implementation Notes

- Keep the Java writer as the oracle for every ambiguous mapsforge format behavior.
- Prefer simple data structures until measurement proves they are inadequate.
- Keep diagnostics stable and machine-readable enough for benchmark comparisons.
- Add abstractions only when they remove real duplication or isolate mapsforge binary format details.
- Fail loudly on unsupported format or geometry cases; do not silently produce incomplete maps.
- The staged-way file is the critical I/O bottleneck. Any optimization must preserve the property that peak memory is bounded by chunk size, not by total way count.
- The `StagedWayWriter` writes records with a 4-byte length prefix. The index file stores `(offset, min_zoom, min_lat, min_lon, max_lat, max_lon)` per way (41 bytes each). Both are used for sequential filtering during bucket construction and random access during tile serialization.
- The disk node index uses delta-coded varint blocks of 1024 records with a binary-searchable block directory. This is the memory-efficient fallback for dense datasets where the referenced-node list would exceed 1/64 of the total node count.
