# Mapsforge Writer Rust Spike

This is a deliberately small benchmark spike, not a replacement writer.

It measures whether a Rust PBF ingest and tile-assignment path is promising enough to justify deeper work against the Java writer baseline. It keeps `geo` in the dependency graph so geometry feasibility is evaluated before any rewrite decision.

Run from this directory:

```bash
cargo run --release -- --input ../../testdata/merged.osm.pbf --mode count
cargo run --release -- --input ../../testdata/merged.osm.pbf \
  --bbox 61.2929618,26.767643,62.5010047,28.6301968 \
  --mode tile-index \
  --output /tmp/mapsforge-rust-sample.map
```

`tile-index` parses the existing `tag-mapping.xml`, indexes nodes, classifies POIs and ways, collects render-relevant multipolygon relation members, emits diagnostic counters, and writes a structural mapsforge `.map` file when `--output` is set. The current writer serializes POIs, simple ways, subtile masks, optional wildcard tag values, Java-style debug strings, Java-style closed-way area and `force-polygon-line` semantics, open-polyline tile clipping with basic JTS-style line noding, simple-polygon tile clipping through `geo::BooleanOps`, simple closed or endpoint-stitched multipolygon rings, and default simplification through `geo::Simplify`, but still does not implement full Java multipolygon parity.

For the small sample dataset, a reader smoke check is available:

```bash
scripts/validate-sample-map.sh
```

The script uses `../../testdata/63240150.osm.pbf`, writes `/tmp/mapsforge-rust-sample.map`, opens it with mapsforge `MapFile`, scans the sample bounding box, and uses the matching peruskartta render-theme fixture at `../../testdata/peruskartta/Peruskartta.xml` for optional render checks. It is not a render-parity check yet.

The fixture theme convention is explicit:

- `../../testdata/merged.osm.pbf` uses the peruskartta theme fixture: `../../testdata/peruskartta/Peruskartta.xml`.
- `../../testdata/finland-260517.osm.pbf` uses the built-in mapsforge default theme: `internal:DEFAULT`.

`scripts/validate-sample-map.sh` chooses `internal:DEFAULT` automatically when `SAMPLE_PBF` ends in `finland-260517.osm.pbf`; otherwise it defaults to the peruskartta fixture. Override with `THEME_SPEC=<path-or-internal-theme>` when needed.

To exercise optional wildcard tag-value serialization through the same reader smoke path:

```bash
TAG_VALUES=true OUTPUT_MAP=/tmp/mapsforge-rust-sample-tag-values.map scripts/validate-sample-map.sh
```

To also render one data-bearing tile with the matching peruskartta theme:

```bash
RENDER=true DOWNLOAD_RENDER_DEPS=true RENDER_PNG=/tmp/mapsforge-rust-render-smoke.png scripts/validate-sample-map.sh
```

To run the same reader/render smoke path against the larger Finland fixture with the mapsforge default style:

```bash
SAMPLE_PBF=../../testdata/finland-260517.osm.pbf \
  THEME_SPEC=internal:DEFAULT \
  RENDER=true DOWNLOAD_RENDER_DEPS=true \
  OUTPUT_MAP=/tmp/mapsforge-rust-finland.map \
  scripts/validate-sample-map.sh
```

Use `SCAN_ZOOMS=10,12,14` when a smoke run should scan more than one zoom level. `RENDER_ZOOM` controls the single zoom used for the optional render tile; it defaults to `SCAN_ZOOM`.

To smoke-check an existing Java reference map without regenerating Rust output:

```bash
GENERATE_MAP=false OUTPUT_MAP=/tmp/mapsforge-java-merged-hd.map \
  RENDER=true DOWNLOAD_RENDER_DEPS=true \
  RENDER_PNG=/tmp/mapsforge-java-render-smoke.png \
  scripts/validate-sample-map.sh
```

To compare two existing maps with the mapsforge reader:

```bash
LEFT_LABEL=rust RIGHT_LABEL=java MAX_TILE_DELTAS=10 \
  scripts/compare-map-files.sh /tmp/mapsforge-rust-merged-polygon-clipped.map \
  /tmp/mapsforge-java-merged-hd.map 14
```

To run the current representative content sweep across multiple zooms:

```bash
MAX_ABS_DELTA_POIS=0 MAX_ABS_DELTA_WAYS=0 SCAN_ZOOMS=10,12,14 \
  scripts/compare-representative-map-files.sh /tmp/mapsforge-java-merged-hd.map \
  /tmp/mapsforge-rust-current.map
```

The current representative content sweep is zero-delta for POIs and ways at z10, z12, and z14.

For `--debug-file true` outputs, compare raw way debug signatures by way ID:

```bash
LEFT_LABEL=java RIGHT_LABEL=rust \
  scripts/compare-debug-way-signatures.sh /tmp/mapsforge-java-current-hd-debug.map \
  /tmp/mapsforge-rust-current-debug.map
```

The current debug-signature comparison is zero-delta. That helper is only a diagnostic for debug maps; the reader comparison above remains the content gate.

To compare rendered pixels for selected tiles with the peruskartta theme:

```bash
LEFT_LABEL=java RIGHT_LABEL=rust RENDER_TILES=9410:4528:14 \
  MAX_DIFFERING_PIXELS=0 \
  scripts/compare-rendered-tiles.sh /tmp/mapsforge-java-merged-hd.map \
  /tmp/mapsforge-rust-current.map ../../testdata/peruskartta/Peruskartta.xml
```

For the larger Finland fixture, use the mapsforge default internal theme:

```bash
LEFT_LABEL=java RIGHT_LABEL=rust RENDER_TILES=<x:y:z> \
  MAX_DIFFERING_PIXELS=65536 \
  scripts/compare-rendered-tiles.sh /tmp/mapsforge-java-finland-hd.map \
  /tmp/mapsforge-rust-finland.map internal:DEFAULT
```

The current known smoke tile `9410,4528,14` renders with `0` differing pixels. This
is still a sampled render check, not a full render-parity proof.

## Current gate result

Primary dataset: `../../testdata/merged.osm.pbf`

Bounds: `61.2929618,26.767643,62.5010047,28.6301968`

Current Rust diagnostic result:

- Entity counts match the Java/Osmium baseline: `31,303,481` nodes, `1,745,863` ways, `6,474` relations.
- `tile-index` resolves all referenced nodes for handled ways: `missing_way_nodes=0`.
- Stable handled-way counters: `ways_needing_handling=19,516`, `ways_with_renderable_tags=17,878`, `way_tile_candidates=26,968`, `way_tile_intersections=25,738`.
- Current writer output run, `--type ram` compatibility alias with clipping, Java-style microdegree truncation, Java-style closed-way area semantics, simple line noding, simple inner-ring/stitching support, default simplification, adaptive referenced-node indexing, and memory-first staged-way/tile-bucket caps: `elapsed_millis=10,783`, with `count_millis=323`, `pass1_collect_pois_relations_millis=2,326`, `pass2_collect_referenced_nodes_millis=2,340`, `pass3_index_referenced_nodes_millis=2,588`, and `pass4_filter_ways_tile_candidates_millis=2,702`.
- Current adaptive referenced-node peak RSS: `86,900,736` bytes on the merged local writer run.
- Current geo polygon-clipped output size: `1,456,597` bytes.
- Merged Rust output opened with mapsforge `MapFile`; z14 bbox scan reported `tiles_with_data=4,675`, `pois=89`, `ways=27,075`, file version `5`.
- The merged sample's current tag mapping reports `render_relevant_multipolygon_relations=0`, so simple inner-ring support is covered by targeted unit tests rather than this sample scan.
- Peruskartta render smoke rendered z14 tile `9410,4528` and wrote a nonblank PNG with `render_sampled_colors=3`.
- Node indexing is adaptive. Sparse extracts index only node coordinates referenced by render-relevant ways; dense extracts fall back to a disk-backed varint/delta block index before the temporary referenced-node list grows beyond one sixty-fourth of the input node count. The writer also favors bounded memory over speed: staged-way cache entries are capped at `4,096`, z14 way buckets are built in chunks of at most `50,000` tiles, and tile serialization groups compact way entries by zoom row before resolving full staged way records one at a time. Progress logs include per-chunk way-bucket fan-out counters (`bucket_entries`, `unique_staged_ways`, and `generated_entries`) and chunk write counters (`tiles`, `nonempty_tiles`, `payload_bytes`, `poi_entries`, and `way_entries`) to make the tile/write locality bottleneck visible without increasing steady-state memory. On the larger Finland fixture, the current dense fallback triggers at `1,450,223` referenced node IDs and indexes all `92,814,328` nodes into `90,639` compressed disk blocks. The full Finland run completed with `elapsed_millis=2,525,275`, `peak_rss_bytes=1,479,540,736`, output size `730,167,579` bytes, `missing_way_nodes=0`, and `write_map_file_millis=2,322,448`. That run confirms memory is bounded on the larger fixture, but also confirms z14 tile serialization is now the throughput bottleneck: interval 2 ended with `cache_hits=1,509,979`, `cache_misses=7,994,615`, and `cache_entries=4,096`.
- A bounded-LRU staged-way cache experiment was stopped at `500,000` z14 tiles because it reduced misses by only about `0.26%` at that checkpoint while raising peak RSS to about `1.51 GB`. The simpler FIFO cache remains in place; future throughput work should change tile/write access locality or bucket representation rather than cache eviction policy.
- The Finland output opened with mapsforge `MapFile` using the built-in default style path. The reader/render smoke with `internal:DEFAULT` scanned z14 and reported `tiles_with_data=325,638`, `pois=57,286`, `ways=6,695,826`, rendered tile `9057,4454,14`, and wrote a nonblank PNG with `render_sampled_colors=2`.

Fresh Java HD comparison on the same dataset:

- `standalone-total=49.6s`, `/usr/bin/time -lp real=49.71s`.
- Peak RSS with `/usr/bin/time -lp`: `508,575,744` bytes, with `469,648,536` bytes peak memory footprint.
- Output size: `1,345,927` bytes.
- Java HD output opened with mapsforge `MapFile`; z14 bbox scan reported `tiles_with_data=4,675`, `pois=89`, `ways=27,075`, file version `3`.
- Peruskartta render smoke rendered z14 tile `9410,4528` and wrote a nonblank PNG with `render_sampled_colors=3`.

Rust-vs-Java HD reader comparison on z14:

- Tile size, projection, zoom range, zoom intervals, start position, start zoom, preferred languages, and comment match.
- Bboxes match after switching Rust microdegree conversion to Java-style truncation: `61.292961,26.767643,62.501004,28.630196`.
- POI and way tag sets match, but optimized tag order differs.
- z14 scan over the overlapping bbox reports equal data-bearing tiles (`4,675`) and equal POIs (`89`).
- Rust and Java HD both read back `27,075` ways at z14.
- No z14 tiles have differing POI or way counts.
- A fresh Java HD debug run and Rust debug run both have `17,878` unique way IDs and `25,738` debug way records; no way IDs differ.
- The representative content sweep is zero-delta for POIs and ways at z10, z12, and z14.
- Peruskartta rendered-pixel comparison is wired up for sampled tiles. The current smoke tile `9410,4528,14` renders identically, with `0` differing pixels between Java HD and Rust.

Java RAM is not a practical oracle for this merged dataset. The default-heap run failed with `java.lang.OutOfMemoryError: Java heap space`, and explicit `-Xmx2g` and `-Xmx4g` RAM runs failed the same way while adding nodes. Use the Java HD output as the correctness oracle for this dataset.

Decision: continue with a Rust writer prototype for this writer-only performance path. The gate is now far enough ahead on runtime and no worse on memory to justify implementing the next Rust slice: mapsforge tile payload serialization for the subset exercised by this dataset, with Java kept as the correctness oracle and fallback. This is not a decision to rewrite the full mapsforge writer yet; relation assembly, clipping, simplification, merged-dataset validation, and reader/render parity still need to pass before replacing the Java path.

See [TODO.md](TODO.md) for the ordered Rust writer rewrite checklist.
