use geo::{coord, BooleanOps, Coord, LineString, Polygon, Rect, Simplify};
use osmpbf::{Element, ElementReader, RelMemberType};
use roxmltree::Document;
use rstar::{RTree, RTreeObject, AABB};
use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_ZOOM_INTERVALS: [ZoomInterval; 3] = [
    ZoomInterval {
        base: 5,
        min: 0,
        max: 7,
    },
    ZoomInterval {
        base: 10,
        min: 8,
        max: 11,
    },
    ZoomInterval {
        base: 14,
        min: 12,
        max: 21,
    },
];
const DEFAULT_BBOX_ENLARGEMENT_METERS: f64 = 20.0;
const DEFAULT_SIMPLIFICATION_FACTOR: f64 = 2.5;
const DEFAULT_SIMPLIFICATION_MAX_ZOOM: u8 = 12;
const DEBUG_INDEX_SIGNATURE: &str = "+++IndexStart+++";
const DEBUG_BLOCK_SIZE: usize = 32;
const DEBUG_TILE_HEAD: &str = "###TileStart";
const DEBUG_TILE_TAIL: &str = "###";
const DEBUG_POI_HEAD: &str = "***POIStart";
const DEBUG_POI_TAIL: &str = "***";
const DEBUG_WAY_HEAD: &str = "---WayStart";
const DEBUG_WAY_TAIL: &str = "---";
const EARTH_EQUATORIAL_RADIUS_METERS: f64 = 6_378_137.0;
const MAX_MERCATOR_LATITUDE: f64 = 85.05112878;
#[allow(dead_code)]
const FEATURE_ENCODING: u8 = 0x04;
#[allow(dead_code)]
const FEATURE_HOUSENUMBER: u8 = 0x40;
#[allow(dead_code)]
const FEATURE_LABEL: u8 = 0x10;
#[allow(dead_code)]
const FEATURE_MULTIPLE_WAY_BLOCKS: u8 = 0x08;
#[allow(dead_code)]
const FEATURE_NAME: u8 = 0x80;
#[allow(dead_code)]
const FEATURE_ELEVATION: u8 = 0x20;
#[allow(dead_code)]
const FEATURE_REF: u8 = 0x20;
#[allow(dead_code)]
const MAX_TAGS_PER_ELEMENT: usize = 15;
const PROGRESS_INTERVAL: Duration = Duration::from_secs(5);
const STAGED_WAY_CACHE_LIMIT: usize = 4_096;
const WAY_BUCKET_CHUNK_TILE_LIMIT: u64 = 50_000;
const REFERENCED_NODE_SPARSE_LIMIT_DIVISOR: u64 = 64;
const DISK_NODE_INDEX_BLOCK_SIZE: u64 = 1024;
const DISK_NODE_INDEX_CACHE_BLOCK_LIMIT: usize = 4096;

#[derive(Clone, Copy, Debug, Default)]
struct Counts {
    nodes: u64,
    ways: u64,
    relations: u64,
    way_refs: u64,
    relation_members: u64,
    tagged_elements: u64,
}

impl Counts {
    fn add(self, other: Counts) -> Counts {
        Counts {
            nodes: self.nodes + other.nodes,
            ways: self.ways + other.ways,
            relations: self.relations + other.relations,
            way_refs: self.way_refs + other.way_refs,
            relation_members: self.relation_members + other.relation_members,
            tagged_elements: self.tagged_elements + other.tagged_elements,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ZoomInterval {
    base: u8,
    min: u8,
    max: u8,
}

#[derive(Clone, Copy, Debug)]
struct BBox {
    min_lat: f64,
    min_lon: f64,
    max_lat: f64,
    max_lon: f64,
}

impl BBox {
    fn contains(&self, lat: f64, lon: f64) -> bool {
        lat >= self.min_lat && lat <= self.max_lat && lon >= self.min_lon && lon <= self.max_lon
    }

    fn overlaps(&self, min_lat: f64, min_lon: f64, max_lat: f64, max_lon: f64) -> bool {
        max_lat >= self.min_lat
            && min_lat <= self.max_lat
            && max_lon >= self.min_lon
            && min_lon <= self.max_lon
    }
}

#[derive(Clone, Copy, Debug)]
struct TileRange {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl TileRange {
    fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x <= self.right && y >= self.top && y <= self.bottom
    }

    fn clipped(&self, other: TileRange) -> Option<TileRange> {
        let clipped = TileRange {
            left: self.left.max(other.left),
            right: self.right.min(other.right),
            top: self.top.max(other.top),
            bottom: self.bottom.min(other.bottom),
        };
        if clipped.left > clipped.right || clipped.top > clipped.bottom {
            return None;
        }
        Some(clipped)
    }

    fn count(&self) -> u64 {
        ((self.right - self.left + 1) as u64) * ((self.bottom - self.top + 1) as u64)
    }

    fn iter(&self) -> TileRangeIter {
        TileRangeIter {
            range: *self,
            next_x: self.left,
            next_y: self.top,
            done: false,
        }
    }
}

struct TileRangeIter {
    range: TileRange,
    next_x: i32,
    next_y: i32,
    done: bool,
}

impl Iterator for TileRangeIter {
    type Item = (i32, i32);

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let result = (self.next_x, self.next_y);
        if self.next_x == self.range.right {
            self.next_x = self.range.left;
            if self.next_y == self.range.bottom {
                self.done = true;
            } else {
                self.next_y += 1;
            }
        } else {
            self.next_x += 1;
        }
        Some(result)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct NodeCoord {
    lat_micro: i32,
    lon_micro: i32,
}

#[derive(Clone, Copy, Debug)]
struct Point {
    x: f64,
    y: f64,
}

impl From<NodeCoord> for Point {
    fn from(value: NodeCoord) -> Self {
        Point {
            x: value.lon(),
            y: value.lat(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct RectBounds {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

impl RectBounds {
    fn contains(self, point: Point) -> bool {
        point.x >= self.min_x
            && point.x <= self.max_x
            && point.y >= self.min_y
            && point.y <= self.max_y
    }

    fn corners(self) -> [Point; 4] {
        [
            Point {
                x: self.min_x,
                y: self.min_y,
            },
            Point {
                x: self.max_x,
                y: self.min_y,
            },
            Point {
                x: self.max_x,
                y: self.max_y,
            },
            Point {
                x: self.min_x,
                y: self.max_y,
            },
        ]
    }
}

impl NodeCoord {
    fn from_degrees(lat: f64, lon: f64) -> Self {
        NodeCoord {
            lat_micro: (lat * 1_000_000.0) as i32,
            lon_micro: (lon * 1_000_000.0) as i32,
        }
    }

    fn lat(self) -> f64 {
        self.lat_micro as f64 / 1_000_000.0
    }

    fn lon(self) -> f64 {
        self.lon_micro as f64 / 1_000_000.0
    }
}

#[derive(Clone, Copy, Debug)]
struct TagInfo {
    id: u16,
    zoom_appear: u8,
    renderable: bool,
    force_polygon_line: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum TagValue {
    Byte(i8),
    Short(i16),
    Int(i32),
    Float(f32),
    String(String),
}

impl TagValue {
    fn from_wildcard(wildcard: &str, value: &str) -> Option<Self> {
        match wildcard {
            "%b" => parse_double_unit(value).map(|number| TagValue::Byte(number as i8)),
            "%h" => parse_double_unit(value).map(|number| TagValue::Short(number as i16)),
            "%i" => parse_tag_int(value).map(TagValue::Int),
            "%f" => parse_double_unit(value).map(|number| TagValue::Float(number as f32)),
            "%s" => Some(TagValue::String(value.to_string())),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
struct TagDef {
    key: String,
    value: String,
}

impl TagDef {
    fn tag_key(&self) -> String {
        tag_key(&self.key, &self.value)
    }
}

#[derive(Debug, Default)]
struct TagMapping {
    poi_tags: HashMap<String, TagInfo>,
    way_tags: HashMap<String, TagInfo>,
    poi_wildcards: Vec<(String, String, TagInfo)>,
    way_wildcards: Vec<(String, String, TagInfo)>,
    poi_defs: Vec<TagDef>,
    way_defs: Vec<TagDef>,
}

impl TagMapping {
    fn poi_match<'a, I>(&self, tags: I, tag_values: bool) -> TagMatch
    where
        I: Iterator<Item = (&'a str, &'a str)>,
    {
        self.match_tags(tags, &self.poi_tags, &self.poi_wildcards, tag_values)
    }

    fn way_match<'a, I>(&self, tags: I, tag_values: bool) -> TagMatch
    where
        I: Iterator<Item = (&'a str, &'a str)>,
    {
        self.match_tags(tags, &self.way_tags, &self.way_wildcards, tag_values)
    }

    fn optimized_poi_tags(&self, frequencies: &HashMap<u16, u64>) -> Vec<String> {
        optimized_tag_keys(&self.poi_defs, frequencies)
    }

    fn optimized_poi_id_map(&self, frequencies: &HashMap<u16, u64>) -> HashMap<u16, u16> {
        optimized_tag_id_map(&self.poi_defs, frequencies)
    }

    fn optimized_way_tags(&self, frequencies: &HashMap<u16, u64>) -> Vec<String> {
        optimized_tag_keys(&self.way_defs, frequencies)
    }

    fn optimized_way_id_map(&self, frequencies: &HashMap<u16, u64>) -> HashMap<u16, u16> {
        optimized_tag_id_map(&self.way_defs, frequencies)
    }

    fn match_tags<'a, I>(
        &self,
        tags: I,
        exact: &HashMap<String, TagInfo>,
        wildcards: &[(String, String, TagInfo)],
        tag_values: bool,
    ) -> TagMatch
    where
        I: Iterator<Item = (&'a str, &'a str)>,
    {
        let mut result = TagMatch::default();
        for (key, value) in tags {
            if let Some(info) = exact.get(&tag_key(key, value)) {
                result.has_known = true;
                result.tag_ids.push(info.id);
                result.tag_values.push(None);
                result.force_polygon_line |= info.force_polygon_line;
                if info.renderable {
                    result.min_renderable_zoom = Some(
                        result
                            .min_renderable_zoom
                            .map_or(info.zoom_appear, |current| current.min(info.zoom_appear)),
                    );
                }
                continue;
            }
            if !tag_values {
                continue;
            }
            let value_type = tag_value_type(key, value);
            for (wildcard_key, wildcard_value, info) in wildcards {
                if wildcard_key == key && wildcard_value == &value_type {
                    result.has_known = true;
                    result.tag_ids.push(info.id);
                    result.force_polygon_line |= info.force_polygon_line;
                    result
                        .tag_values
                        .push(TagValue::from_wildcard(wildcard_value, value));
                    if info.renderable {
                        result.min_renderable_zoom = Some(
                            result
                                .min_renderable_zoom
                                .map_or(info.zoom_appear, |current| current.min(info.zoom_appear)),
                        );
                    }
                }
            }
        }
        result
    }
}

#[derive(Clone, Debug, Default)]
struct TagMatch {
    has_known: bool,
    min_renderable_zoom: Option<u8>,
    force_polygon_line: bool,
    tag_ids: Vec<u16>,
    tag_values: Vec<Option<TagValue>>,
}

#[derive(Debug)]
struct Args {
    input: PathBuf,
    bbox: Option<BBox>,
    tag_conf_file: PathBuf,
    mode: Mode,
    writer: WriterArgs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Count,
    TileIndex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EncodingChoice {
    Auto,
    Single,
    Double,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CoordinateEncoding {
    SingleDelta,
    DoubleDelta,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WriterType {
    Hd,
    Ram,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct MapStartPosition {
    lat: f64,
    lon: f64,
}

#[derive(Debug)]
struct WriterArgs {
    output: Option<PathBuf>,
    zoom_intervals: Vec<ZoomInterval>,
    bbox_enlargement_meters: f64,
    encoding: EncodingChoice,
    tag_values: bool,
    preferred_languages: Vec<String>,
    map_start_position: Option<MapStartPosition>,
    map_start_zoom: Option<u8>,
    comment: Option<String>,
    debug_file: bool,
    writer_type: WriterType,
    progress_logs: bool,
}

impl Default for WriterArgs {
    fn default() -> Self {
        WriterArgs {
            output: None,
            zoom_intervals: DEFAULT_ZOOM_INTERVALS.to_vec(),
            bbox_enlargement_meters: DEFAULT_BBOX_ENLARGEMENT_METERS,
            encoding: EncodingChoice::Auto,
            tag_values: false,
            preferred_languages: Vec::new(),
            map_start_position: None,
            map_start_zoom: None,
            comment: None,
            debug_file: false,
            writer_type: WriterType::Hd,
            progress_logs: true,
        }
    }
}

#[derive(Debug, Default)]
struct TileIndexStats {
    nodes_indexed: u64,
    poi_nodes: u64,
    poi_tile_assignments: u64,
    multipolygon_relations: u64,
    render_relevant_multipolygon_relations: u64,
    relation_way_members: u64,
    multipolygon_member_refs: u64,
    simple_multipolygon_relations_with_inner_rings: u64,
    multipolygon_inner_rings_attached: u64,
    inner_ways_without_additional_tags: u64,
    partial_multipolygon_relations: u64,
    unsupported_multipolygon_relations: u64,
    unsupported_multipolygon_no_valid_rings: u64,
    unsupported_multipolygon_relation_failures: u64,
    unsupported_multipolygon_empty_relations: u64,
    unsupported_multipolygon_missing_min_zoom: u64,
    unsupported_multipolygon_empty_bounds: u64,
    ways_needing_handling: u64,
    ways_with_renderable_tags: u64,
    ways_overlapping_bbox: u64,
    way_tile_candidates: u64,
    way_tile_intersection_tests: u64,
    way_tile_intersections: u64,
    missing_way_nodes: u64,
}

#[derive(Debug, Default)]
struct TagFrequencies {
    poi: HashMap<u16, u64>,
    way: HashMap<u16, u64>,
}

struct ProgressLog {
    enabled: bool,
    total_started: Instant,
    phase_started: Instant,
    last_log: Instant,
}

impl ProgressLog {
    fn new(enabled: bool, total_started: Instant) -> Self {
        ProgressLog {
            enabled,
            total_started,
            phase_started: total_started,
            last_log: total_started,
        }
    }

    fn phase_start(&mut self, phase: &str) {
        let now = Instant::now();
        self.phase_started = now;
        self.last_log = now;
        self.log(phase, "start");
    }

    fn phase_done(&mut self, phase: &str) {
        self.log(phase, "done");
    }

    fn tick(&mut self, phase: &str, detail: impl AsRef<str>) {
        if !self.enabled || self.last_log.elapsed() < PROGRESS_INTERVAL {
            return;
        }
        self.last_log = Instant::now();
        self.log(phase, detail.as_ref());
    }

    fn event(&mut self, phase: &str, detail: impl AsRef<str>) {
        self.last_log = Instant::now();
        self.log(phase, detail.as_ref());
    }

    fn log(&self, phase: &str, detail: &str) {
        if !self.enabled {
            return;
        }
        eprintln!(
            "progress phase={} total_millis={} phase_millis={} peak_rss_bytes={} {}",
            phase,
            self.total_started.elapsed().as_millis(),
            self.phase_started.elapsed().as_millis(),
            peak_rss_bytes()
                .map(|bytes| bytes.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            detail
        );
    }
}

fn peak_rss_bytes() -> Option<u64> {
    #[cfg(unix)]
    {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
        if result != 0 {
            return None;
        }
        let raw = unsafe { usage.assume_init().ru_maxrss };
        if raw < 0 {
            return None;
        }
        let raw = raw as u64;
        #[cfg(target_os = "macos")]
        {
            Some(raw)
        }
        #[cfg(not(target_os = "macos"))]
        {
            Some(raw.saturating_mul(1024))
        }
    }
    #[cfg(not(unix))]
    {
        None
    }
}

impl TagFrequencies {
    fn record_poi(&mut self, tag_ids: &[u16]) {
        record_tag_ids(&mut self.poi, tag_ids);
    }

    fn record_way(&mut self, tag_ids: &[u16]) {
        record_tag_ids(&mut self.way, tag_ids);
    }
}

#[allow(dead_code)]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SpecialTags {
    name: Option<String>,
    ref_value: Option<String>,
    housenumber: Option<String>,
    layer: i8,
    elevation: i16,
    relation_type: Option<String>,
}

#[derive(Clone, Debug)]
struct WriterPoi {
    id: i64,
    coord: NodeCoord,
    min_zoom: u8,
    tag_ids: Vec<u16>,
    tag_values: Vec<Option<TagValue>>,
    special: SpecialTags,
}

#[derive(Clone, Debug)]
struct WriterWay {
    id: i64,
    min_zoom: u8,
    area: bool,
    coords: Vec<NodeCoord>,
    inner_coords: Vec<Vec<NodeCoord>>,
    min_lat: f64,
    min_lon: f64,
    max_lat: f64,
    max_lon: f64,
    tag_ids: Vec<u16>,
    tag_values: Vec<Option<TagValue>>,
    special: SpecialTags,
}

#[derive(Clone, Debug)]
struct RelationMemberInfo {
    is_inner: bool,
    tag_ids: Vec<u16>,
    tag_values: Vec<Option<TagValue>>,
    min_renderable_zoom: Option<u8>,
    force_polygon_line: bool,
    special: SpecialTags,
}

#[derive(Clone, Debug)]
struct MultipolygonRelationInfo {
    id: i64,
    members: Vec<RelationMemberRef>,
    tag_ids: Vec<u16>,
    tag_values: Vec<Option<TagValue>>,
    min_renderable_zoom: Option<u8>,
    force_polygon_line: bool,
    special: SpecialTags,
}

#[derive(Clone, Debug)]
struct RelationMemberRef {
    way_id: i64,
}

#[derive(Clone, Debug)]
struct RelationWayGeometry {
    coords: Vec<NodeCoord>,
}

#[derive(Clone, Debug)]
struct RelationPolygonRing {
    coords: Vec<NodeCoord>,
    member_ids: Vec<i64>,
    min_lat: f64,
    min_lon: f64,
    max_lat: f64,
    max_lon: f64,
}

#[derive(Clone, Copy, Debug)]
struct RingEnvelope {
    index: usize,
    envelope: AABB<[f64; 2]>,
}

impl RTreeObject for RingEnvelope {
    type Envelope = AABB<[f64; 2]>;

    fn envelope(&self) -> Self::Envelope {
        self.envelope
    }
}

struct TempSubfile {
    index: Vec<u8>,
    tile_data_path: PathBuf,
    size: u64,
}

#[derive(Clone, Copy, Debug)]
enum WaySource {
    Staged { offset: u64 },
    Generated { index: usize },
}

#[derive(Clone, Copy, Debug)]
struct WayBucketEntry {
    id: i64,
    min_zoom: u8,
    source: WaySource,
}

#[derive(Clone, Copy, Debug, Default)]
struct WayBucketEntryStats {
    bucket_entries: usize,
    unique_staged_ways: usize,
    generated_entries: usize,
}

#[derive(Clone, Debug)]
struct StagedWayStore {
    path: PathBuf,
    index_path: PathBuf,
    count: u64,
}

struct StagedWayWriter {
    file: BufWriter<File>,
    offset: u64,
    index_file: File,
    path: PathBuf,
    index_path: PathBuf,
    count: u64,
}

impl StagedWayWriter {
    fn create(path: PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        let index_path = temp_staged_way_index_path(&path);
        Ok(Self {
            file: BufWriter::with_capacity(8 * 1024 * 1024, File::create(&path)?),
            offset: 0,
            index_file: File::create(&index_path)?,
            path,
            index_path,
            count: 0,
        })
    }

    fn push(&mut self, way: &WriterWay) -> Result<u64, Box<dyn std::error::Error>> {
        let offset = self.offset;
        let record = encode_staged_way(way)?;
        let len_bytes = u32::try_from(record.len())
            .map_err(|_| "staged way record is too large")?
            .to_be_bytes();
        self.file.write_all(&len_bytes)?;
        self.file.write_all(&record)?;
        self.offset += 4 + record.len() as u64;
        write_staged_way_summary(&mut self.index_file, offset, way).map_err(io::Error::other)?;
        self.count += 1;
        Ok(offset)
    }

    fn finish(mut self) -> Result<StagedWayStore, Box<dyn std::error::Error>> {
        self.file.flush()?;
        self.index_file.flush()?;
        Ok(StagedWayStore {
            path: self.path,
            index_path: self.index_path,
            count: self.count,
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct StagedWaySummary {
    offset: u64,
    min_zoom: u8,
    min_lat: f64,
    min_lon: f64,
    max_lat: f64,
    max_lon: f64,
}

struct StagedWayReader {
    file: BufReader<File>,
    cache: HashMap<u64, WriterWay>,
    cache_order: VecDeque<u64>,
    cache_hits: u64,
    cache_misses: u64,
}

#[derive(Clone, Copy, Debug)]
struct NodeIndexRecord {
    id_delta: u32,
    coord: NodeCoord,
}

#[derive(Clone, Copy, Debug)]
struct DiskNodeBlock {
    first_id: i64,
    byte_offset: u64,
    byte_len: u32,
    record_count: u32,
}

struct DiskNodeIndexBuilder {
    path: PathBuf,
    file: BufWriter<File>,
    blocks: Vec<DiskNodeBlock>,
    count: u64,
    last_id: Option<i64>,
    current_block_first_id: Option<i64>,
    current_block_records: Vec<NodeIndexRecord>,
    byte_offset: u64,
}

struct DiskNodeIndex {
    path: PathBuf,
    file: File,
    blocks: Vec<DiskNodeBlock>,
    count: u64,
    cache: HashMap<u64, Vec<NodeIndexRecord>>,
    cache_order: VecDeque<u64>,
    cache_hits: u64,
    cache_misses: u64,
}

enum NodeLookupIndex {
    Memory(Vec<(i64, NodeCoord)>),
    Disk(DiskNodeIndex),
}

impl DiskNodeIndexBuilder {
    fn create(path: PathBuf) -> Result<Self, String> {
        Ok(Self {
            file: BufWriter::with_capacity(
                8 * 1024 * 1024,
                File::create(&path).map_err(|error| {
                    format!(
                        "cannot create temporary node index {}: {error}",
                        path.display()
                    )
                })?,
            ),
            path,
            blocks: Vec::new(),
            count: 0,
            last_id: None,
            current_block_first_id: None,
            current_block_records: Vec::with_capacity(DISK_NODE_INDEX_BLOCK_SIZE as usize),
            byte_offset: 0,
        })
    }

    fn push(&mut self, id: i64, coord: NodeCoord) -> Result<(), String> {
        if let Some(last_id) = self.last_id {
            if id <= last_id {
                return Err(format!(
                    "dense disk node index requires sorted unique node ids: previous={last_id} current={id}"
                ));
            }
        }
        let starts_new_block = match self.current_block_first_id {
            None => true,
            Some(first_id) => {
                self.current_block_records.len() >= DISK_NODE_INDEX_BLOCK_SIZE as usize
                    || u64::try_from(id - first_id).unwrap_or(u64::MAX) > u32::MAX as u64
            }
        };
        if starts_new_block {
            self.flush_current_block()?;
            self.current_block_first_id = Some(id);
        }
        let first_id = self
            .current_block_first_id
            .ok_or_else(|| "dense disk node index block is missing".to_string())?;
        let id_delta = u32::try_from(id - first_id).map_err(|_| {
            format!("dense disk node index block id delta overflow: first={first_id} current={id}")
        })?;
        self.current_block_records
            .push(NodeIndexRecord { id_delta, coord });
        self.count += 1;
        self.last_id = Some(id);
        Ok(())
    }

    fn finish(mut self) -> Result<DiskNodeIndex, String> {
        self.flush_current_block()?;
        self.file
            .flush()
            .map_err(|error| format!("cannot flush temporary node index: {error}"))?;
        Ok(DiskNodeIndex {
            file: File::open(&self.path).map_err(|error| {
                format!(
                    "cannot open temporary node index {}: {error}",
                    self.path.display()
                )
            })?,
            path: self.path,
            blocks: self.blocks,
            count: self.count,
            cache: HashMap::new(),
            cache_order: VecDeque::new(),
            cache_hits: 0,
            cache_misses: 0,
        })
    }

    fn flush_current_block(&mut self) -> Result<(), String> {
        let Some(first_id) = self.current_block_first_id else {
            return Ok(());
        };
        if self.current_block_records.is_empty() {
            return Ok(());
        }
        let encoded = encode_disk_node_block(&self.current_block_records);
        let byte_len = u32::try_from(encoded.len())
            .map_err(|_| "encoded node index block is too large".to_string())?;
        self.file
            .write_all(&encoded)
            .map_err(|error| format!("cannot write node index block: {error}"))?;
        self.blocks.push(DiskNodeBlock {
            first_id,
            byte_offset: self.byte_offset,
            byte_len,
            record_count: self.current_block_records.len() as u32,
        });
        self.byte_offset = self
            .byte_offset
            .checked_add(encoded.len() as u64)
            .ok_or_else(|| "node index byte offset overflow".to_string())?;
        self.current_block_records.clear();
        Ok(())
    }
}

fn encode_disk_node_block(records: &[NodeIndexRecord]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(records.len() * 10);
    let mut previous_lat = 0_i32;
    let mut previous_lon = 0_i32;
    for record in records {
        bytes.extend_from_slice(&binary::var_uint(record.id_delta));
        bytes.extend_from_slice(&binary::var_int(record.coord.lat_micro - previous_lat));
        bytes.extend_from_slice(&binary::var_int(record.coord.lon_micro - previous_lon));
        previous_lat = record.coord.lat_micro;
        previous_lon = record.coord.lon_micro;
    }
    bytes
}

fn decode_disk_node_block(
    bytes: &[u8],
    record_count: usize,
) -> Result<Vec<NodeIndexRecord>, String> {
    let mut decoder = DiskNodeBlockDecoder { bytes, offset: 0 };
    let mut records = Vec::with_capacity(record_count);
    let mut previous_lat = 0_i32;
    let mut previous_lon = 0_i32;
    for _ in 0..record_count {
        let id_delta = decoder.read_var_uint()?;
        let lat_micro = previous_lat
            .checked_add(decoder.read_var_int()?)
            .ok_or_else(|| "node index latitude delta overflow".to_string())?;
        let lon_micro = previous_lon
            .checked_add(decoder.read_var_int()?)
            .ok_or_else(|| "node index longitude delta overflow".to_string())?;
        records.push(NodeIndexRecord {
            id_delta,
            coord: NodeCoord {
                lat_micro,
                lon_micro,
            },
        });
        previous_lat = lat_micro;
        previous_lon = lon_micro;
    }
    if decoder.offset != bytes.len() {
        return Err("trailing bytes in node index block".to_string());
    }
    Ok(records)
}

struct DiskNodeBlockDecoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl DiskNodeBlockDecoder<'_> {
    fn read_u8(&mut self) -> Result<u8, String> {
        let byte = *self
            .bytes
            .get(self.offset)
            .ok_or_else(|| "truncated node index block".to_string())?;
        self.offset += 1;
        Ok(byte)
    }

    fn read_var_uint(&mut self) -> Result<u32, String> {
        let mut shift = 0_u32;
        let mut value = 0_u32;
        loop {
            let byte = self.read_u8()?;
            value |= ((byte & 0x7f) as u32)
                .checked_shl(shift)
                .ok_or_else(|| "node index varuint shift overflow".to_string())?;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
            if shift > 28 {
                return Err("node index varuint is too large".to_string());
            }
        }
    }

    fn read_var_int(&mut self) -> Result<i32, String> {
        let mut shift = 0_u32;
        let mut magnitude = 0_u32;
        loop {
            let byte = self.read_u8()?;
            if byte & 0x80 == 0 {
                magnitude |= ((byte & 0x3f) as u32)
                    .checked_shl(shift)
                    .ok_or_else(|| "node index varint shift overflow".to_string())?;
                let value = i32::try_from(magnitude)
                    .map_err(|_| "node index varint magnitude is too large".to_string())?;
                return if byte & 0x40 != 0 {
                    value
                        .checked_neg()
                        .ok_or_else(|| "node index varint negation overflow".to_string())
                } else {
                    Ok(value)
                };
            }
            magnitude |= ((byte & 0x7f) as u32)
                .checked_shl(shift)
                .ok_or_else(|| "node index varint shift overflow".to_string())?;
            shift += 7;
            if shift > 28 {
                return Err("node index varint is too large".to_string());
            }
        }
    }
}

impl DiskNodeIndex {
    fn lookup(&mut self, id: i64) -> Result<Option<NodeCoord>, String> {
        let Some(block_index) = self.block_index_for_id(id) else {
            return Ok(None);
        };
        let first_id = self.blocks[block_index].first_id;
        let block = self.read_block(block_index)?;
        Ok(block
            .binary_search_by_key(&id, |record| first_id + record.id_delta as i64)
            .ok()
            .map(|index| block[index].coord))
    }

    fn block_index_for_id(&self, id: i64) -> Option<usize> {
        match self
            .blocks
            .binary_search_by_key(&id, |block| block.first_id)
        {
            Ok(index) => Some(index),
            Err(0) => None,
            Err(index) => Some(index - 1),
        }
    }

    fn read_block(&mut self, block_index: usize) -> Result<&[NodeIndexRecord], String> {
        let cache_key = block_index as u64;
        if self.cache.contains_key(&cache_key) {
            self.cache_hits += 1;
            return Ok(self
                .cache
                .get(&cache_key)
                .expect("node index cache entry should exist"));
        }
        self.cache_misses += 1;
        let block_info = self.blocks[block_index];
        self.file
            .seek(SeekFrom::Start(block_info.byte_offset))
            .map_err(|error| format!("cannot seek temporary node index: {error}"))?;
        let mut bytes = vec![0_u8; block_info.byte_len as usize];
        self.file
            .read_exact(&mut bytes)
            .map_err(|error| format!("cannot read temporary node index block: {error}"))?;
        let records = decode_disk_node_block(&bytes, block_info.record_count as usize)?;
        self.remember_block(cache_key, records);
        Ok(self
            .cache
            .get(&cache_key)
            .expect("node index cache entry should exist"))
    }

    fn remember_block(&mut self, start_record: u64, records: Vec<NodeIndexRecord>) {
        if DISK_NODE_INDEX_CACHE_BLOCK_LIMIT == 0 {
            return;
        }
        if self.cache.len() >= DISK_NODE_INDEX_CACHE_BLOCK_LIMIT {
            if let Some(oldest) = self.cache_order.pop_front() {
                self.cache.remove(&oldest);
            }
        }
        self.cache.insert(start_record, records);
        self.cache_order.push_back(start_record);
    }
}

impl NodeLookupIndex {
    fn len(&self) -> usize {
        match self {
            NodeLookupIndex::Memory(nodes) => nodes.len(),
            NodeLookupIndex::Disk(nodes) => nodes.count as usize,
        }
    }

    fn lookup(&mut self, id: i64) -> Result<Option<NodeCoord>, String> {
        match self {
            NodeLookupIndex::Memory(nodes) => Ok(lookup_node(nodes, id)),
            NodeLookupIndex::Disk(nodes) => nodes.lookup(id),
        }
    }

    fn progress_detail(&self) -> Option<String> {
        match self {
            NodeLookupIndex::Memory(_) => None,
            NodeLookupIndex::Disk(nodes) => Some(format!(
                "node_index_cache_hits={} node_index_cache_misses={} node_index_cache_entries={}",
                nodes.cache_hits,
                nodes.cache_misses,
                nodes.cache.len()
            )),
        }
    }

    fn cleanup(self) -> Result<(), String> {
        match self {
            NodeLookupIndex::Memory(_) => Ok(()),
            NodeLookupIndex::Disk(nodes) => fs::remove_file(&nodes.path).map_err(|error| {
                format!(
                    "cannot remove temporary node index {}: {error}",
                    nodes.path.display()
                )
            }),
        }
    }
}

impl StagedWayReader {
    fn open(store: &StagedWayStore) -> Result<Self, String> {
        Ok(Self {
            file: BufReader::with_capacity(
                8 * 1024 * 1024,
                File::open(&store.path).map_err(|error| {
                    format!(
                        "cannot open staged way file {}: {error}",
                        store.path.display()
                    )
                })?,
            ),
            cache: HashMap::new(),
            cache_order: VecDeque::new(),
            cache_hits: 0,
            cache_misses: 0,
        })
    }

    fn read_at(&mut self, offset: u64) -> Result<WriterWay, String> {
        if let Some(way) = self.cache.get(&offset) {
            self.cache_hits += 1;
            return Ok(way.clone());
        }
        self.cache_misses += 1;
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|error| format!("cannot seek staged way file: {error}"))?;
        let way = read_staged_way(&mut self.file)?;
        self.remember(offset, way.clone());
        Ok(way)
    }

    /// Bulk-read all staged ways at the given offsets in a single sequential pass.
    /// Offsets are sorted internally so the file is read front-to-back once.
    /// Returns a map from offset to WriterWay, suitable for serving an entire
    /// chunk's tile serialization without further file I/O.
    fn bulk_read_offsets(&mut self, offsets: &HashSet<u64>) -> Result<HashMap<u64, WriterWay>, String> {
        let mut sorted_offsets: Vec<u64> = offsets.iter().copied().collect();
        sorted_offsets.sort_unstable();
        let mut result = HashMap::with_capacity(sorted_offsets.len());
        for &offset in &sorted_offsets {
            self.file
                .seek(SeekFrom::Start(offset))
                .map_err(|error| format!("cannot seek staged way file for bulk read: {error}"))?;
            let way = read_staged_way(&mut self.file)?;
            result.insert(offset, way);
        }
        Ok(result)
    }

    fn remember(&mut self, offset: u64, way: WriterWay) {
        if STAGED_WAY_CACHE_LIMIT == 0 {
            return;
        }
        if self.cache.len() >= STAGED_WAY_CACHE_LIMIT {
            if let Some(oldest) = self.cache_order.pop_front() {
                self.cache.remove(&oldest);
            }
        }
        self.cache.insert(offset, way);
        self.cache_order.push_back(offset);
    }
}

#[derive(Clone, Debug, PartialEq)]
struct WayDataBlock {
    outer: Vec<NodeCoord>,
    inners: Vec<Vec<NodeCoord>>,
}

#[derive(Clone, Debug)]
struct EncodedWayDataBlock {
    outer: Vec<i32>,
    inners: Vec<Vec<i32>>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct TileKey {
    interval_index: usize,
    x: i32,
    y: i32,
}

#[allow(dead_code)]
fn extract_special_tags<'a, I>(tags: I, preferred_languages: &[String]) -> SpecialTags
where
    I: Iterator<Item = (&'a str, &'a str)>,
{
    let tags = tags.collect::<Vec<_>>();
    let mut result = SpecialTags {
        layer: 5,
        ..SpecialTags::default()
    };

    result.name = extract_name(&tags, preferred_languages);

    for (key, value) in &tags {
        let key = key.to_ascii_lowercase();
        match key.as_str() {
            "piste:name" if result.name.is_none() => result.name = Some((*value).to_string()),
            "addr:housenumber" => result.housenumber = Some((*value).to_string()),
            "ref" => result.ref_value = Some((*value).to_string()),
            "layer" => {
                if let Ok(layer) = value.parse::<i8>() {
                    result.layer = if (-5..=5).contains(&layer) {
                        layer + 5
                    } else {
                        layer
                    };
                }
            }
            "ele" => {
                if let Some(elevation) = parse_double_unit(value) {
                    if elevation < 9000.0 {
                        result.elevation = elevation as i16;
                    }
                }
            }
            "type" => result.relation_type = Some((*value).to_string()),
            _ => {}
        }
    }

    result
}

#[allow(dead_code)]
fn extract_name(tags: &[(&str, &str)], preferred_languages: &[String]) -> Option<String> {
    if preferred_languages.len() > 1 {
        return extract_multilingual_name(tags, preferred_languages);
    }

    let mut name = None;
    let mut found_preferred_language_name = false;
    for (key, value) in tags {
        let key = key.to_ascii_lowercase();
        if key == "name" && !found_preferred_language_name {
            name = Some((*value).to_string());
        } else if !preferred_languages.is_empty() && !found_preferred_language_name {
            if let Some(language) = name_language(&key) {
                if language.eq_ignore_ascii_case(&preferred_languages[0]) {
                    name = Some((*value).to_string());
                    found_preferred_language_name = true;
                }
            }
        }
    }
    name
}

#[allow(dead_code)]
fn extract_multilingual_name(
    tags: &[(&str, &str)],
    preferred_languages: &[String],
) -> Option<String> {
    let mut sorted_tags = tags.to_vec();
    sorted_tags.sort_by(|left, right| {
        let left_name_rank = if left.0.eq_ignore_ascii_case("name") {
            0
        } else {
            1
        };
        let right_name_rank = if right.0.eq_ignore_ascii_case("name") {
            0
        } else {
            1
        };
        left_name_rank
            .cmp(&right_name_rank)
            .then_with(|| left.0.cmp(&right.0))
    });

    let normalized_preferred = preferred_languages
        .iter()
        .map(|language| language.to_ascii_lowercase().replace('_', "-"))
        .collect::<Vec<_>>();
    let mut rest = normalized_preferred.clone();
    let mut default_name = None;
    let mut name = None;

    for (key, value) in &sorted_tags {
        let key = key.to_ascii_lowercase();
        if key == "name" {
            default_name = Some((*value).to_string());
            name = Some((*value).to_string());
            continue;
        }
        let Some(language) = name_language(&key) else {
            continue;
        };
        if default_name.as_deref() == Some(*value) {
            continue;
        }
        if normalized_preferred.contains(&language) {
            rest.retain(|preferred| preferred != &language);
            append_localized_name(&mut name, &language, value);
        }
    }

    if !rest.is_empty() {
        let mut fallbacks: HashMap<String, String> = HashMap::new();
        for preferred_language in &rest {
            for (key, value) in &sorted_tags {
                let Some(language) = name_language(&key.to_ascii_lowercase()) else {
                    continue;
                };
                if default_name.as_deref() == Some(*value) {
                    continue;
                }
                if !fallbacks.contains_key(&language)
                    && !language.contains('-')
                    && preferred_language.contains('-')
                    && preferred_language.starts_with(&language)
                {
                    fallbacks.insert(language, (*value).to_string());
                }
            }
        }
        let mut fallback_languages = fallbacks.keys().cloned().collect::<Vec<_>>();
        fallback_languages.sort();
        for language in fallback_languages {
            if let Some(value) = fallbacks.get(&language) {
                append_localized_name(&mut name, &language, value);
            }
        }
    }

    name
}

#[allow(dead_code)]
fn append_localized_name(name: &mut Option<String>, language: &str, value: &str) {
    let suffix = format!("{language}\u{0008}{value}");
    if let Some(name) = name {
        name.push('\r');
        name.push_str(&suffix);
    } else {
        *name = Some(suffix);
    }
}

#[allow(dead_code)]
fn name_language(key: &str) -> Option<String> {
    let language = key.strip_prefix("name:")?;
    if language.is_empty()
        || !language
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return None;
    }
    Some(language.to_ascii_lowercase().replace('_', "-"))
}

#[allow(dead_code)]
fn parse_double_unit(value: &str) -> Option<f64> {
    let normalized = value.replace('m', "").replace(',', ".");
    if normalized.is_empty() {
        return None;
    }
    let has_only_number_chars = normalized
        .chars()
        .all(|ch| ch.is_ascii_digit() || ch == '-' || ch == '.');
    if !has_only_number_chars {
        return None;
    }
    normalized.parse::<f64>().ok()
}

fn tag_value_type(key: &str, value: &str) -> String {
    if let Some(number) = parse_double_unit(value) {
        if number.round() == number {
            if number >= i8::MIN as f64 && number <= i8::MAX as f64 {
                return "%b".to_string();
            }
            if number >= i16::MIN as f64 && number <= i16::MAX as f64 {
                return "%h".to_string();
            }
            return "%i".to_string();
        }
        return "%f".to_string();
    }
    if key.contains("colour") && (is_hex_color(value) || css_named_color(value).is_some()) {
        return "%i".to_string();
    }
    "%s".to_string()
}

fn parse_tag_int(value: &str) -> Option<i32> {
    if is_hex_color(value) {
        let rgb = u32::from_str_radix(&value[1..], 16).ok()?;
        return Some((0xff00_0000_u32 | rgb) as i32);
    }
    if let Some(color) = css_named_color(value) {
        return Some(color);
    }
    parse_double_unit(value).map(|number| number as i32)
}

#[allow(dead_code)]
fn delta_encode_coordinates(coordinates: &[i32]) -> Result<Vec<i32>, String> {
    validate_coordinate_pairs(coordinates)?;
    if coordinates.is_empty() {
        return Ok(Vec::new());
    }

    let mut encoded = Vec::with_capacity(coordinates.len());
    let mut previous_lat = coordinates[0];
    let mut previous_lon = coordinates[1];
    encoded.push(previous_lat);
    encoded.push(previous_lon);

    for pair in coordinates[2..].chunks_exact(2) {
        let current_lat = pair[0];
        let current_lon = pair[1];
        encoded.push(current_lat - previous_lat);
        encoded.push(current_lon - previous_lon);
        previous_lat = current_lat;
        previous_lon = current_lon;
    }

    Ok(encoded)
}

#[allow(dead_code)]
fn double_delta_encode_coordinates(coordinates: &[i32]) -> Result<Vec<i32>, String> {
    validate_coordinate_pairs(coordinates)?;
    if coordinates.is_empty() {
        return Ok(Vec::new());
    }

    let mut encoded = Vec::with_capacity(coordinates.len());
    let mut previous_lat = coordinates[0];
    let mut previous_lon = coordinates[1];
    let mut previous_lat_delta = 0;
    let mut previous_lon_delta = 0;
    encoded.push(previous_lat);
    encoded.push(previous_lon);

    for pair in coordinates[2..].chunks_exact(2) {
        let current_lat = pair[0];
        let current_lon = pair[1];
        let lat_delta = current_lat - previous_lat;
        let lon_delta = current_lon - previous_lon;
        encoded.push(lat_delta - previous_lat_delta);
        encoded.push(lon_delta - previous_lon_delta);
        previous_lat = current_lat;
        previous_lon = current_lon;
        previous_lat_delta = lat_delta;
        previous_lon_delta = lon_delta;
    }

    Ok(encoded)
}

#[allow(dead_code)]
fn choose_coordinate_encoding(coordinates: &[i32]) -> Result<CoordinateEncoding, String> {
    let single = delta_encode_coordinates(coordinates)?;
    let double = double_delta_encode_coordinates(coordinates)?;
    let single_size = serialized_signed_varint_size(&single);
    let double_size = serialized_signed_varint_size(&double);
    if single_size <= double_size {
        Ok(CoordinateEncoding::SingleDelta)
    } else {
        Ok(CoordinateEncoding::DoubleDelta)
    }
}

#[allow(dead_code)]
fn encode_coordinates(
    coordinates: &[i32],
    encoding: EncodingChoice,
) -> Result<(CoordinateEncoding, Vec<i32>), String> {
    match encoding {
        EncodingChoice::Single => Ok((
            CoordinateEncoding::SingleDelta,
            delta_encode_coordinates(coordinates)?,
        )),
        EncodingChoice::Double => Ok((
            CoordinateEncoding::DoubleDelta,
            double_delta_encode_coordinates(coordinates)?,
        )),
        EncodingChoice::Auto => match choose_coordinate_encoding(coordinates)? {
            CoordinateEncoding::SingleDelta => Ok((
                CoordinateEncoding::SingleDelta,
                delta_encode_coordinates(coordinates)?,
            )),
            CoordinateEncoding::DoubleDelta => Ok((
                CoordinateEncoding::DoubleDelta,
                double_delta_encode_coordinates(coordinates)?,
            )),
        },
    }
}

#[allow(dead_code)]
fn serialized_signed_varint_size(coordinates: &[i32]) -> usize {
    coordinates
        .iter()
        .map(|coordinate| binary::var_int(*coordinate).len())
        .sum()
}

#[allow(dead_code)]
fn validate_coordinate_pairs(coordinates: &[i32]) -> Result<(), String> {
    if coordinates.len() % 2 != 0 {
        return Err("coordinate list must contain lat/lon pairs".to_string());
    }
    Ok(())
}

#[allow(dead_code)]
fn layer_and_tag_count_byte(layer: i8, tag_count: usize) -> Result<u8, String> {
    if tag_count > MAX_TAGS_PER_ELEMENT {
        return Err("more than 15 tags are not supported".to_string());
    }
    let layer = layer.clamp(0, 10) as u8;
    Ok((layer << 4) | tag_count as u8)
}

#[allow(dead_code)]
fn poi_feature_byte(name: Option<&str>, elevation: i16, housenumber: Option<&str>) -> u8 {
    let mut feature = 0;
    if has_text(name) {
        feature |= FEATURE_NAME;
    }
    if has_text(housenumber) {
        feature |= FEATURE_HOUSENUMBER;
    }
    if elevation != 0 {
        feature |= FEATURE_ELEVATION;
    }
    feature
}

#[allow(dead_code)]
fn way_feature_byte(
    name: Option<&str>,
    housenumber: Option<&str>,
    ref_value: Option<&str>,
    has_label_position: bool,
    way_data_block_count: usize,
    encoding: CoordinateEncoding,
) -> u8 {
    let mut feature = 0;
    if has_text(name) {
        feature |= FEATURE_NAME;
    }
    if has_text(housenumber) {
        feature |= FEATURE_HOUSENUMBER;
    }
    if has_text(ref_value) {
        feature |= FEATURE_REF;
    }
    if has_label_position {
        feature |= FEATURE_LABEL;
    }
    if way_data_block_count > 1 {
        feature |= FEATURE_MULTIPLE_WAY_BLOCKS;
    }
    if encoding == CoordinateEncoding::DoubleDelta {
        feature |= FEATURE_ENCODING;
    }
    feature
}

#[allow(dead_code)]
fn has_text(value: Option<&str>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

#[allow(dead_code)]
mod binary {
    use super::{BBox, MapStartPosition, ZoomInterval};

    const BITMAP_COMMENT: u8 = 0x08;
    const BITMAP_CREATED_WITH: u8 = 0x04;
    const BITMAP_DEBUG: u8 = 0x80;
    const BITMAP_MAP_START_POSITION: u8 = 0x40;
    const BITMAP_MAP_START_ZOOM: u8 = 0x20;
    const BITMAP_PREFERRED_LANGUAGES: u8 = 0x10;
    const FILE_SPECIFICATION_VERSION: u32 = 5;
    const MAGIC_BYTE: &[u8] = b"mapsforge binary OSM";
    const MAX_FIVE_BYTE_OFFSET: u64 = (1_u64 << 40) - 1;
    const PROJECTION: &str = "Mercator";
    const TILE_SIZE: u16 = 256;

    #[derive(Debug, Default)]
    pub(super) struct BinaryEncoder {
        bytes: Vec<u8>,
    }

    impl BinaryEncoder {
        pub(super) fn new() -> Self {
            Self::default()
        }

        pub(super) fn into_bytes(self) -> Vec<u8> {
            self.bytes
        }

        pub(super) fn write_u16(&mut self, value: u16) {
            self.bytes.extend_from_slice(&value.to_be_bytes());
        }

        pub(super) fn write_i16(&mut self, value: i16) {
            self.bytes.extend_from_slice(&value.to_be_bytes());
        }

        pub(super) fn write_u32(&mut self, value: u32) {
            self.bytes.extend_from_slice(&value.to_be_bytes());
        }

        pub(super) fn write_i32(&mut self, value: i32) {
            self.bytes.extend_from_slice(&value.to_be_bytes());
        }

        pub(super) fn write_u64(&mut self, value: u64) {
            self.bytes.extend_from_slice(&value.to_be_bytes());
        }

        pub(super) fn write_i64(&mut self, value: i64) {
            self.bytes.extend_from_slice(&value.to_be_bytes());
        }

        pub(super) fn write_u8(&mut self, value: u8) {
            self.bytes.push(value);
        }

        pub(super) fn write_bytes(&mut self, value: &[u8]) {
            self.bytes.extend_from_slice(value);
        }

        pub(super) fn write_five_byte_offset(&mut self, value: u64) -> Result<(), String> {
            if value > MAX_FIVE_BYTE_OFFSET {
                return Err(format!("5-byte offset out of range: {value}"));
            }
            self.bytes.extend_from_slice(&[
                (value >> 32) as u8,
                (value >> 24) as u8,
                (value >> 16) as u8,
                (value >> 8) as u8,
                value as u8,
            ]);
            Ok(())
        }

        pub(super) fn write_var_uint(&mut self, value: u32) {
            self.bytes.extend_from_slice(&var_uint(value));
        }

        pub(super) fn write_var_int(&mut self, value: i32) {
            self.bytes.extend_from_slice(&var_int(value));
        }

        pub(super) fn write_utf8(&mut self, value: &str) -> Result<(), String> {
            let length = u32::try_from(value.len())
                .map_err(|_| "UTF-8 string is too large to encode".to_string())?;
            self.write_var_uint(length);
            self.bytes.extend_from_slice(value.as_bytes());
            Ok(())
        }
    }

    pub(super) struct HeaderOptions<'a> {
        pub(super) bbox: BBox,
        pub(super) file_size: u64,
        pub(super) creation_date_millis: u64,
        pub(super) zoom_intervals: &'a [ZoomInterval],
        pub(super) subfiles: &'a [SubfileMetadata],
        pub(super) poi_tags: &'a [String],
        pub(super) way_tags: &'a [String],
        pub(super) map_start_position: Option<MapStartPosition>,
        pub(super) map_start_zoom: Option<u8>,
        pub(super) preferred_languages: &'a [String],
        pub(super) comment: Option<&'a str>,
        pub(super) debug_file: bool,
        pub(super) created_with: &'a str,
    }

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub(super) struct SubfileMetadata {
        pub(super) start_address: u64,
        pub(super) size: u64,
    }

    pub(super) fn write_header(options: HeaderOptions<'_>) -> Result<Vec<u8>, String> {
        if options.poi_tags.len() > u16::MAX as usize {
            return Err(format!("too many POI tags: {}", options.poi_tags.len()));
        }
        if options.way_tags.len() > u16::MAX as usize {
            return Err(format!("too many way tags: {}", options.way_tags.len()));
        }
        if options.zoom_intervals.len() != options.subfiles.len() {
            return Err("zoom interval and subfile metadata counts must match".to_string());
        }

        let mut encoder = BinaryEncoder::new();
        encoder.bytes.extend_from_slice(MAGIC_BYTE);
        let header_size_position = encoder.bytes.len();
        encoder.write_i32(0);
        encoder.write_u32(FILE_SPECIFICATION_VERSION);
        encoder.write_u64(options.file_size);
        encoder.write_u64(options.creation_date_millis);
        encoder.write_i32(degrees_to_microdegrees(options.bbox.min_lat)?);
        encoder.write_i32(degrees_to_microdegrees(options.bbox.min_lon)?);
        encoder.write_i32(degrees_to_microdegrees(options.bbox.max_lat)?);
        encoder.write_i32(degrees_to_microdegrees(options.bbox.max_lon)?);
        encoder.write_u16(TILE_SIZE);
        encoder.write_utf8(PROJECTION)?;

        encoder.write_u8(header_flags(&options));
        if let Some(position) = options.map_start_position {
            encoder.write_i32(degrees_to_microdegrees(position.lat)?);
            encoder.write_i32(degrees_to_microdegrees(position.lon)?);
        }
        if let Some(zoom) = options.map_start_zoom {
            encoder.write_u8(zoom);
        }
        if !options.preferred_languages.is_empty() {
            encoder.write_utf8(&options.preferred_languages.join(","))?;
        }
        if let Some(comment) = options.comment {
            encoder.write_utf8(comment)?;
        }
        encoder.write_utf8(options.created_with)?;

        encoder.write_u16(options.poi_tags.len() as u16);
        for tag in options.poi_tags {
            encoder.write_utf8(tag)?;
        }
        encoder.write_u16(options.way_tags.len() as u16);
        for tag in options.way_tags {
            encoder.write_utf8(tag)?;
        }

        if options.zoom_intervals.len() > u8::MAX as usize {
            return Err(format!(
                "too many zoom intervals: {}",
                options.zoom_intervals.len()
            ));
        }
        encoder.write_u8(options.zoom_intervals.len() as u8);
        for (interval, subfile) in options.zoom_intervals.iter().zip(options.subfiles.iter()) {
            encoder.write_u8(interval.base);
            encoder.write_u8(interval.min);
            encoder.write_u8(interval.max);
            encoder.write_u64(subfile.start_address);
            encoder.write_u64(subfile.size);
        }

        let header_size = i32::try_from(encoder.bytes.len() - header_size_position - 4)
            .map_err(|_| "header too large".to_string())?;
        encoder.bytes[header_size_position..header_size_position + 4]
            .copy_from_slice(&header_size.to_be_bytes());

        Ok(encoder.into_bytes())
    }

    fn header_flags(options: &HeaderOptions<'_>) -> u8 {
        let mut flags = BITMAP_CREATED_WITH;
        if options.map_start_position.is_some() {
            flags |= BITMAP_MAP_START_POSITION;
        }
        if options.map_start_zoom.is_some() {
            flags |= BITMAP_MAP_START_ZOOM;
        }
        if !options.preferred_languages.is_empty() {
            flags |= BITMAP_PREFERRED_LANGUAGES;
        }
        if options.comment.is_some() {
            flags |= BITMAP_COMMENT;
        }
        if options.debug_file {
            flags |= BITMAP_DEBUG;
        }
        flags
    }

    fn degrees_to_microdegrees(value: f64) -> Result<i32, String> {
        let microdegrees = value * 1_000_000.0;
        if microdegrees < i32::MIN as f64 || microdegrees > i32::MAX as f64 {
            return Err(format!("coordinate out of microdegree range: {value}"));
        }
        Ok(microdegrees as i32)
    }

    pub(super) fn var_uint(value: u32) -> Vec<u8> {
        let mut value = value;
        let mut bytes = Vec::with_capacity(5);
        while value >= 0x80 {
            bytes.push((value as u8 & 0x7f) | 0x80);
            value >>= 7;
        }
        bytes.push(value as u8);
        bytes
    }

    pub(super) fn var_int(value: i32) -> Vec<u8> {
        let negative = value < 0;
        let mut magnitude = (value as i64).abs() as u32;
        let mut bytes = Vec::with_capacity(5);

        while magnitude >= 0x40 {
            bytes.push((magnitude as u8 & 0x7f) | 0x80);
            magnitude >>= 7;
        }

        let mut last = magnitude as u8;
        if negative {
            last |= 0x40;
        }
        bytes.push(last);
        bytes
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args(env::args().skip(1))?;

    match args.mode {
        Mode::Count => run_count(&args.input),
        Mode::TileIndex => {
            let bbox = args
                .bbox
                .ok_or("--bbox minLat,minLon,maxLat,maxLon is required for --mode tile-index")?;
            run_tile_index(&args.input, bbox, &args.tag_conf_file, &args.writer)
        }
    }
}

fn run_count(input: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let started = Instant::now();
    let reader = ElementReader::from_path(input)?;
    let counts = reader.par_map_reduce(count_element, Counts::default, Counts::add)?;

    // Keep geo in the spike dependency graph from day one. The full rewrite decision
    // depends on replacing JTS clipping/simplification, not just faster PBF decoding.
    let world: Rect = Rect::new(
        coord! { x: -180.0, y: -MAX_MERCATOR_LATITUDE },
        coord! { x: 180.0, y: MAX_MERCATOR_LATITUDE },
    );

    println!("mode=count");
    println!("input={}", input.display());
    println!("elapsed_millis={}", started.elapsed().as_millis());
    print_counts(counts);
    println!(
        "world_bbox=minLon:{},minLat:{},maxLon:{},maxLat:{}",
        world.min().x,
        world.min().y,
        world.max().x,
        world.max().y
    );

    Ok(())
}

fn run_tile_index(
    input: &PathBuf,
    bbox: BBox,
    tag_conf_file: &PathBuf,
    writer: &WriterArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let total_started = Instant::now();
    let mut progress = ProgressLog::new(writer.progress_logs, total_started);

    progress.phase_start("count");
    let count_started = Instant::now();
    let reader = ElementReader::from_path(input)?;
    let counts = reader.par_map_reduce(count_element, Counts::default, Counts::add)?;
    let count_millis = count_started.elapsed().as_millis();
    progress.phase_done("count");

    progress.phase_start("prepare");
    let mapping = load_tag_mapping(tag_conf_file)?;
    let tile_ranges = bbox_tile_ranges(bbox, &writer.zoom_intervals);
    let mut node_index: Vec<(i64, NodeCoord)>;
    let mut relation_way_members: HashMap<i64, Vec<RelationMemberInfo>> = HashMap::new();
    let mut multipolygon_relations = Vec::new();
    let mut relation_way_geometries: HashMap<i64, RelationWayGeometry> = HashMap::new();
    let mut staged_way_writer = StagedWayWriter::create(temp_staged_way_path())?;
    let mut staged_way_ids = HashSet::new();
    let mut generated_ways = Vec::new();
    let mut inner_attachments: HashMap<i64, Vec<Vec<NodeCoord>>> = HashMap::new();
    let mut stats = TileIndexStats::default();
    let mut tag_frequencies = TagFrequencies::default();
    let mut pois = Vec::new();
    progress.phase_done("prepare");

    progress.phase_start("pass1_collect_pois_relations");
    let pass1_started = Instant::now();
    let reader = ElementReader::from_path(input)?;
    let mut pass1_elements = 0_u64;
    reader.for_each(|element| match element {
        Element::Node(node) => {
            pass1_elements += 1;
            handle_node(
                node.id(),
                node.lat(),
                node.lon(),
                node.tags(),
                &mapping,
                bbox,
                &tile_ranges,
                &writer.zoom_intervals,
                writer.tag_values,
                &writer.preferred_languages,
                &mut stats,
                &mut tag_frequencies,
                &mut pois,
            );
            if pass1_elements % 1_000_000 == 0 {
                progress.tick(
                    "pass1_collect_pois_relations",
                    format!(
                        "elements={} poi_nodes={} render_relevant_multipolygons={} relation_way_members={} multipolygon_member_refs={}",
                        pass1_elements,
                        stats.poi_nodes,
                        stats.render_relevant_multipolygon_relations,
                        stats.relation_way_members,
                        stats.multipolygon_member_refs
                    ),
                );
            }
        }
        Element::DenseNode(node) => {
            pass1_elements += 1;
            handle_node(
                node.id(),
                node.lat(),
                node.lon(),
                node.tags(),
                &mapping,
                bbox,
                &tile_ranges,
                &writer.zoom_intervals,
                writer.tag_values,
                &writer.preferred_languages,
                &mut stats,
                &mut tag_frequencies,
                &mut pois,
            );
            if pass1_elements % 1_000_000 == 0 {
                progress.tick(
                    "pass1_collect_pois_relations",
                    format!(
                        "elements={} poi_nodes={} render_relevant_multipolygons={} relation_way_members={} multipolygon_member_refs={}",
                        pass1_elements,
                        stats.poi_nodes,
                        stats.render_relevant_multipolygon_relations,
                        stats.relation_way_members,
                        stats.multipolygon_member_refs
                    ),
                );
            }
        }
        Element::Relation(relation) => {
            pass1_elements += 1;
            let is_multipolygon = relation
                .tags()
                .any(|(key, value)| key == "type" && value == "multipolygon");
            if !is_multipolygon {
                if pass1_elements % 10_000 == 0 {
                    progress.tick(
                        "pass1_collect_pois_relations",
                        format!(
                            "elements={} poi_nodes={} render_relevant_multipolygons={} relation_way_members={} multipolygon_member_refs={}",
                            pass1_elements,
                            stats.poi_nodes,
                            stats.render_relevant_multipolygon_relations,
                            stats.relation_way_members,
                            stats.multipolygon_member_refs
                        ),
                    );
                }
                return;
            }
            stats.multipolygon_relations += 1;
            let relation_tag_match = mapping.way_match(relation.tags(), writer.tag_values);
            let is_render_relevant = relation_tag_match.has_known
                || relation.tags().any(|(key, value)| {
                    (key == "name" || key == "ref") && !value.trim().is_empty()
                });
            if is_render_relevant {
                stats.render_relevant_multipolygon_relations += 1;
                let special = extract_special_tags(relation.tags(), &writer.preferred_languages);
                let way_members = relation
                    .members()
                    .filter(|member| matches!(member.member_type, RelMemberType::Way))
                    .map(|member| (member.member_id, member.role().unwrap_or("") == "inner"))
                    .collect::<Vec<_>>();
                stats.multipolygon_member_refs += way_members.len() as u64;
                for (way_id, is_inner) in &way_members {
                    let entry = relation_way_members.entry(*way_id).or_default();
                    if entry.is_empty() {
                        stats.relation_way_members += 1;
                    }
                    entry.push(RelationMemberInfo {
                        is_inner: *is_inner,
                        tag_ids: relation_tag_match.tag_ids.clone(),
                        tag_values: relation_tag_match.tag_values.clone(),
                        min_renderable_zoom: relation_tag_match.min_renderable_zoom,
                        force_polygon_line: relation_tag_match.force_polygon_line,
                        special: special.clone(),
                    });
                }
                multipolygon_relations.push(MultipolygonRelationInfo {
                    id: relation.id(),
                    members: way_members
                        .iter()
                        .map(|(way_id, _)| RelationMemberRef { way_id: *way_id })
                        .collect(),
                    tag_ids: relation_tag_match.tag_ids.clone(),
                    tag_values: relation_tag_match.tag_values.clone(),
                    min_renderable_zoom: relation_tag_match.min_renderable_zoom,
                    force_polygon_line: relation_tag_match.force_polygon_line,
                    special,
                });
            }
            if pass1_elements % 10_000 == 0 {
                progress.tick(
                    "pass1_collect_pois_relations",
                    format!(
                        "elements={} poi_nodes={} render_relevant_multipolygons={} relation_way_members={} multipolygon_member_refs={}",
                        pass1_elements,
                        stats.poi_nodes,
                        stats.render_relevant_multipolygon_relations,
                        stats.relation_way_members,
                        stats.multipolygon_member_refs
                    ),
                );
            }
        }
        Element::Way(_) => {}
    })?;
    progress.phase_done("pass1_collect_pois_relations");
    let pass1_millis = pass1_started.elapsed().as_millis();

    progress.phase_start("pass2_collect_referenced_nodes");
    let collect_refs_started = Instant::now();
    let reader = ElementReader::from_path(input)?;
    let mut pass2_ways = 0_u64;
    let mut referenced_node_ids = Vec::new();
    let mut ways_needing_coords = 0_u64;
    let mut index_all_nodes = false;
    let referenced_node_sparse_limit =
        (counts.nodes / REFERENCED_NODE_SPARSE_LIMIT_DIVISOR).max(1) as usize;
    reader.for_each(|element| {
        if let Element::Way(way) = element {
            pass2_ways += 1;
            if pass2_ways % 250_000 == 0 {
                progress.tick(
                    "pass2_collect_referenced_nodes",
                    format!(
                        "ways_seen={} ways_needing_coords={} referenced_node_ids={} index_all_nodes={}",
                        pass2_ways,
                        ways_needing_coords,
                        referenced_node_ids.len(),
                        index_all_nodes
                    ),
                );
            }
            if index_all_nodes {
                return;
            }
            let tags = way
                .tags()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect::<Vec<_>>();
            let tag_match = mapping.way_match(
                tags.iter()
                    .map(|(key, value)| (key.as_str(), value.as_str())),
                writer.tag_values,
            );
            let relation_members = relation_way_members
                .get(&way.id())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            if !tag_match.has_known && relation_members.is_empty() {
                return;
            }
            ways_needing_coords += 1;
            referenced_node_ids.extend(way.refs());
            if referenced_node_ids.len() > referenced_node_sparse_limit {
                index_all_nodes = true;
                referenced_node_ids.clear();
                referenced_node_ids.shrink_to_fit();
                progress.event(
                    "pass2_collect_referenced_nodes",
                    format!(
                        "fallback_all_nodes referenced_node_ids_exceeded={} node_count={}",
                        referenced_node_sparse_limit, counts.nodes
                    ),
                );
            }
        }
    })?;
    let collect_refs_millis = collect_refs_started.elapsed().as_millis();
    progress.phase_done("pass2_collect_referenced_nodes");

    progress.phase_start("sort_node_index");
    let sort_started = Instant::now();
    if index_all_nodes {
        node_index = Vec::with_capacity(counts.nodes as usize);
    } else {
        referenced_node_ids.sort_unstable();
        referenced_node_ids.dedup();
        node_index = referenced_node_ids
            .into_iter()
            .map(|id| (id, missing_node_coord()))
            .collect();
        stats.nodes_indexed = node_index.len() as u64;
        node_index.sort_unstable_by_key(|(id, _)| *id);
    }
    let sort_node_index_millis = sort_started.elapsed().as_millis();
    progress.phase_done("sort_node_index");

    progress.phase_start("pass3_index_referenced_nodes");
    let index_nodes_started = Instant::now();
    let reader = ElementReader::from_path(input)?;
    let mut pass3_elements = 0_u64;
    let mut indexed_node_coords = 0_u64;
    let mut disk_node_index_builder = if index_all_nodes {
        Some(DiskNodeIndexBuilder::create(temp_node_index_path())?)
    } else {
        None
    };
    let mut node_index_error = None;
    reader.for_each(|element| match element {
        Element::Node(node) => {
            if node_index_error.is_some() {
                return;
            }
            pass3_elements += 1;
            if index_all_nodes {
                if let Some(builder) = disk_node_index_builder.as_mut() {
                    match builder.push(node.id(), NodeCoord::from_degrees(node.lat(), node.lon())) {
                        Ok(()) => {
                            indexed_node_coords += 1;
                        }
                        Err(error) => {
                            node_index_error = Some(error);
                            return;
                        }
                    }
                }
            } else if collect_node_coord(node.id(), node.lat(), node.lon(), &mut node_index) {
                indexed_node_coords += 1;
            }
            if pass3_elements % 1_000_000 == 0 {
                progress.tick(
                    "pass3_index_referenced_nodes",
                    format!(
                        "elements={} referenced_nodes={} nodes_indexed={}",
                        pass3_elements, stats.nodes_indexed, indexed_node_coords
                    ),
                );
            }
        }
        Element::DenseNode(node) => {
            if node_index_error.is_some() {
                return;
            }
            pass3_elements += 1;
            if index_all_nodes {
                if let Some(builder) = disk_node_index_builder.as_mut() {
                    match builder.push(node.id(), NodeCoord::from_degrees(node.lat(), node.lon())) {
                        Ok(()) => {
                            indexed_node_coords += 1;
                        }
                        Err(error) => {
                            node_index_error = Some(error);
                            return;
                        }
                    }
                }
            } else if collect_node_coord(node.id(), node.lat(), node.lon(), &mut node_index) {
                indexed_node_coords += 1;
            }
            if pass3_elements % 1_000_000 == 0 {
                progress.tick(
                    "pass3_index_referenced_nodes",
                    format!(
                        "elements={} referenced_nodes={} nodes_indexed={}",
                        pass3_elements, stats.nodes_indexed, indexed_node_coords
                    ),
                );
            }
        }
        Element::Way(_) | Element::Relation(_) => {}
    })?;
    if let Some(error) = node_index_error {
        return Err(std::io::Error::other(error).into());
    }
    let mut node_lookup = if index_all_nodes {
        NodeLookupIndex::Disk(
            disk_node_index_builder
                .take()
                .ok_or("missing disk node index builder")?
                .finish()?,
        )
    } else {
        node_index.retain(|(_, coord)| !is_missing_node_coord(*coord));
        NodeLookupIndex::Memory(node_index)
    };
    if index_all_nodes {
        progress.event(
            "pass3_index_referenced_nodes",
            format!(
                "disk_node_index records={} blocks={} block_size={}",
                node_lookup.len(),
                indexed_node_coords.div_ceil(DISK_NODE_INDEX_BLOCK_SIZE),
                DISK_NODE_INDEX_BLOCK_SIZE
            ),
        );
    }
    stats.nodes_indexed = node_lookup.len() as u64;
    let index_nodes_millis = index_nodes_started.elapsed().as_millis();
    progress.phase_done("pass3_index_referenced_nodes");

    progress.phase_start("pass4_filter_ways_tile_candidates");
    let pass4_started = Instant::now();
    let reader = ElementReader::from_path(input)?;
    let mut pass4_ways = 0_u64;
    let mut stage_error = None;
    reader.for_each(|element| {
        if stage_error.is_some() {
            return;
        }
        if let Element::Way(way) = element {
            pass4_ways += 1;
            if pass4_ways % 250_000 == 0 {
                progress.tick(
                    "pass4_filter_ways_tile_candidates",
                    format!(
                        "ways_seen={} ways_needing_handling={} ways_with_renderable_tags={} way_tile_candidates={} way_tile_intersections={} relation_way_geometries={}",
                        pass4_ways,
                        stats.ways_needing_handling,
                        stats.ways_with_renderable_tags,
                        stats.way_tile_candidates,
                        stats.way_tile_intersections,
                        relation_way_geometries.len()
                    ),
                );
            }
            let tags = way
                .tags()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect::<Vec<_>>();
            let tag_match = mapping.way_match(
                tags.iter()
                    .map(|(key, value)| (key.as_str(), value.as_str())),
                writer.tag_values,
            );
            let relation_members = relation_way_members
                .get(&way.id())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            if !tag_match.has_known && relation_members.is_empty() {
                return;
            }
            stats.ways_needing_handling += 1;
            let refs_iter = way.refs();
            let refs_capacity = refs_iter.size_hint().0;
            let mut refs_seen = 0_u64;
            let mut way_min_lat = f64::INFINITY;
            let mut way_min_lon = f64::INFINITY;
            let mut way_max_lat = f64::NEG_INFINITY;
            let mut way_max_lon = f64::NEG_INFINITY;
            let mut way_coords = Vec::with_capacity(refs_capacity);
            for node_id in refs_iter {
                refs_seen += 1;
                match node_lookup.lookup(node_id) {
                    Ok(Some(coord)) => {
                        way_coords.push(coord);
                        let lat = coord.lat();
                        let lon = coord.lon();
                        way_min_lat = way_min_lat.min(lat);
                        way_min_lon = way_min_lon.min(lon);
                        way_max_lat = way_max_lat.max(lat);
                        way_max_lon = way_max_lon.max(lon);
                    }
                    Ok(None) => {
                        stats.missing_way_nodes += 1;
                    }
                    Err(error) => {
                        stage_error = Some(error);
                        return;
                    }
                }
            }
            if refs_seen == 0 || !way_min_lat.is_finite() {
                return;
            }
            if !bbox.overlaps(way_min_lat, way_min_lon, way_max_lat, way_max_lon) {
                return;
            }
            stats.ways_overlapping_bbox += 1;
            let way_is_closed = is_closed_way(&way_coords);
            let suppress_standalone_inner_way =
                should_suppress_standalone_inner_way(&tag_match, relation_members, way_is_closed);
            let effective_tag_match =
                merge_relation_member_tags(tag_match, relation_members, way_is_closed);
            if effective_tag_match.min_renderable_zoom.is_some() && !suppress_standalone_inner_way
            {
                stats.ways_with_renderable_tags += 1;
            }
            if !relation_members.is_empty() {
                relation_way_geometries.insert(
                    way.id(),
                    RelationWayGeometry {
                        coords: way_coords.clone(),
                    },
                );
            }
            if suppress_standalone_inner_way {
                stats.inner_ways_without_additional_tags += 1;
                return;
            }
            tag_frequencies.record_way(&effective_tag_match.tag_ids);
            if let Some(min_zoom) = effective_tag_match.min_renderable_zoom {
                let area =
                    way_is_closed && is_area_tags(&tags) && !effective_tag_match.force_polygon_line;
                if way_coords.len() >= 2 {
                    for (interval_index, interval) in writer.zoom_intervals.iter().enumerate() {
                        if min_zoom <= interval.max {
                            let range = way_tile_range(
                                way_min_lat,
                                way_min_lon,
                                way_max_lat,
                                way_max_lon,
                                interval.base,
                                writer.bbox_enlargement_meters,
                            );
                            if let Some(clipped_range) = tile_ranges[interval_index].clipped(range)
                            {
                                stats.way_tile_candidates += clipped_range.count();
                                for (tile_x, tile_y) in clipped_range.iter() {
                                    stats.way_tile_intersection_tests += 1;
                                    let tile_bounds = tile_bounds(
                                        tile_x,
                                        tile_y,
                                        interval.base,
                                        writer.bbox_enlargement_meters,
                                    );
                                    if way_intersects_rect(&way_coords, tile_bounds, area) {
                                        stats.way_tile_intersections += 1;
                                    }
                                }
                            }
                        }
                    }
                    way_coords.shrink_to_fit();
                    let own_special = extract_special_tags(
                        tags.iter()
                            .map(|(key, value)| (key.as_str(), value.as_str())),
                        &writer.preferred_languages,
                    );
                    let staged_way = WriterWay {
                        id: way.id(),
                        min_zoom,
                        area,
                        coords: way_coords,
                        inner_coords: Vec::new(),
                        min_lat: way_min_lat,
                        min_lon: way_min_lon,
                        max_lat: way_max_lat,
                        max_lon: way_max_lon,
                        tag_ids: effective_tag_match.tag_ids.clone(),
                        tag_values: effective_tag_match.tag_values.clone(),
                        special: merge_relation_member_special(
                            own_special,
                            relation_members,
                            way_is_closed,
                        ),
                    };
                    match staged_way_writer.push(&staged_way) {
                        Ok(_) => {
                            staged_way_ids.insert(staged_way.id);
                        }
                        Err(error) => {
                            stage_error = Some(error.to_string());
                            return;
                        }
                    }
                }
            }
        }
    })?;
    if let Some(error) = stage_error {
        return Err(std::io::Error::other(error).into());
    }
    let pass4_millis = pass4_started.elapsed().as_millis();
    progress.phase_done("pass4_filter_ways_tile_candidates");
    let staged_way_store = staged_way_writer.finish()?;
    if let Some(detail) = node_lookup.progress_detail() {
        progress.event("pass4_filter_ways_tile_candidates", detail);
    }
    node_lookup.cleanup()?;
    drop(relation_way_members);

    progress.phase_start("multipolygon_assembly");
    let multipolygon_started = Instant::now();
    attach_supported_multipolygon_relations_staged(
        &staged_way_ids,
        &mut generated_ways,
        &mut inner_attachments,
        &multipolygon_relations,
        &relation_way_geometries,
        &mut tag_frequencies,
        &mut stats,
        Some(&mut progress),
    );
    let multipolygon_millis = multipolygon_started.elapsed().as_millis();
    progress.phase_done("multipolygon_assembly");
    if let Some(message) = unsupported_multipolygon_summary(&stats) {
        progress.event("multipolygon_assembly", message);
    }
    generated_ways.shrink_to_fit();
    drop(multipolygon_relations);
    drop(relation_way_geometries);
    drop(staged_way_ids);

    progress.phase_start("optimize_tags");
    let optimize_started = Instant::now();
    let optimized_poi_tags = mapping.optimized_poi_tags(&tag_frequencies.poi);
    let optimized_way_tags = mapping.optimized_way_tags(&tag_frequencies.way);
    let optimized_poi_id_map = mapping.optimized_poi_id_map(&tag_frequencies.poi);
    let optimized_way_id_map = mapping.optimized_way_id_map(&tag_frequencies.way);
    let optimize_millis = optimize_started.elapsed().as_millis();
    progress.phase_done("optimize_tags");
    drop(tag_frequencies);

    let mut write_map_millis = 0_u128;
    if let Some(output) = &writer.output {
        progress.phase_start("write_map_file");
        let write_started = Instant::now();
        write_map_file(
            output,
            bbox,
            writer,
            &pois,
            &staged_way_store,
            &generated_ways,
            &inner_attachments,
            &optimized_poi_id_map,
            &optimized_way_id_map,
            &optimized_poi_tags,
            &optimized_way_tags,
            Some(&mut progress),
        )?;
        write_map_millis = write_started.elapsed().as_millis();
        progress.phase_done("write_map_file");
    }
    fs::remove_file(&staged_way_store.path)?;
    fs::remove_file(&staged_way_store.index_path)?;

    println!("mode=tile-index");
    println!("input={}", input.display());
    println!("tag_conf_file={}", tag_conf_file.display());
    if let Some(output) = &writer.output {
        println!("output={}", output.display());
    }
    println!(
        "bbox={},{},{},{}",
        bbox.min_lat, bbox.min_lon, bbox.max_lat, bbox.max_lon
    );
    println!(
        "zoom_intervals={}",
        format_zoom_intervals(&writer.zoom_intervals)
    );
    println!("bbox_enlargement_meters={}", writer.bbox_enlargement_meters);
    println!("encoding={}", format_encoding(writer.encoding));
    println!("tag_values={}", writer.tag_values);
    println!("debug_file={}", writer.debug_file);
    println!(
        "preferred_languages={}",
        writer.preferred_languages.join(",")
    );
    if let Some(position) = writer.map_start_position {
        println!("map_start_position={},{}", position.lat, position.lon);
    }
    if let Some(zoom) = writer.map_start_zoom {
        println!("map_start_zoom={zoom}");
    }
    if let Some(comment) = &writer.comment {
        println!("comment={comment}");
    }
    println!("type={}", format_writer_type(writer.writer_type));
    println!("progress_logs={}", writer.progress_logs);
    println!("elapsed_millis={}", total_started.elapsed().as_millis());
    println!("count_millis={count_millis}");
    println!("pass1_index_nodes_relations_millis={pass1_millis}");
    println!("pass1_collect_pois_relations_millis={pass1_millis}");
    println!("pass2_collect_referenced_nodes_millis={collect_refs_millis}");
    println!("sort_node_index_millis={sort_node_index_millis}");
    println!("pass3_index_referenced_nodes_millis={index_nodes_millis}");
    println!("pass2_filter_ways_tile_candidates_millis={pass4_millis}");
    println!("pass4_filter_ways_tile_candidates_millis={pass4_millis}");
    println!("multipolygon_assembly_millis={multipolygon_millis}");
    println!("optimize_tags_millis={optimize_millis}");
    println!("write_map_file_millis={write_map_millis}");
    println!(
        "peak_rss_bytes={}",
        peak_rss_bytes()
            .map(|bytes| bytes.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    );
    print_counts(counts);
    println!("nodes_indexed={}", stats.nodes_indexed);
    println!("poi_nodes={}", stats.poi_nodes);
    println!("poi_tile_assignments={}", stats.poi_tile_assignments);
    println!("optimized_poi_tags={}", optimized_poi_tags.len());
    println!("optimized_way_tags={}", optimized_way_tags.len());
    println!("multipolygon_relations={}", stats.multipolygon_relations);
    println!(
        "render_relevant_multipolygon_relations={}",
        stats.render_relevant_multipolygon_relations
    );
    println!("relation_way_members={}", stats.relation_way_members);
    println!(
        "multipolygon_member_refs={}",
        stats.multipolygon_member_refs
    );
    println!(
        "simple_multipolygon_relations_with_inner_rings={}",
        stats.simple_multipolygon_relations_with_inner_rings
    );
    println!(
        "multipolygon_inner_rings_attached={}",
        stats.multipolygon_inner_rings_attached
    );
    println!(
        "inner_ways_without_additional_tags={}",
        stats.inner_ways_without_additional_tags
    );
    println!(
        "partial_multipolygon_relations={}",
        stats.partial_multipolygon_relations
    );
    println!(
        "unsupported_multipolygon_relations={}",
        stats.unsupported_multipolygon_relations
    );
    println!(
        "unsupported_multipolygon_no_valid_rings={}",
        stats.unsupported_multipolygon_no_valid_rings
    );
    println!(
        "unsupported_multipolygon_relation_failures={}",
        stats.unsupported_multipolygon_relation_failures
    );
    println!(
        "unsupported_multipolygon_empty_relations={}",
        stats.unsupported_multipolygon_empty_relations
    );
    println!(
        "unsupported_multipolygon_missing_min_zoom={}",
        stats.unsupported_multipolygon_missing_min_zoom
    );
    println!(
        "unsupported_multipolygon_empty_bounds={}",
        stats.unsupported_multipolygon_empty_bounds
    );
    println!("ways_needing_handling={}", stats.ways_needing_handling);
    println!(
        "ways_with_renderable_tags={}",
        stats.ways_with_renderable_tags
    );
    println!("ways_overlapping_bbox={}", stats.ways_overlapping_bbox);
    println!("way_tile_candidates={}", stats.way_tile_candidates);
    println!(
        "way_tile_intersection_tests={}",
        stats.way_tile_intersection_tests
    );
    println!("way_tile_intersections={}", stats.way_tile_intersections);
    println!("missing_way_nodes={}", stats.missing_way_nodes);
    println!("tile_assignment_semantics=java_closed_way_area_semantics_with_open_polyline_repeated_point_and_self_intersection_noding_simple_polygon_simple_inner_ring_and_endpoint_stitched_multipolygon_tile_clipping_and_default_simplification");

    Ok(())
}

fn handle_node<'a, I>(
    id: i64,
    lat: f64,
    lon: f64,
    tags: I,
    mapping: &TagMapping,
    bbox: BBox,
    tile_ranges: &[TileRange],
    zoom_intervals: &[ZoomInterval],
    tag_values: bool,
    preferred_languages: &[String],
    stats: &mut TileIndexStats,
    tag_frequencies: &mut TagFrequencies,
    pois: &mut Vec<WriterPoi>,
) where
    I: Iterator<Item = (&'a str, &'a str)>,
{
    let tags = tags
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect::<Vec<_>>();
    let tag_match = mapping.poi_match(
        tags.iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
        tag_values,
    );
    let coord = NodeCoord::from_degrees(lat, lon);
    if let Some(min_zoom) = tag_match.min_renderable_zoom {
        if bbox.contains(lat, lon) {
            stats.poi_nodes += 1;
            tag_frequencies.record_poi(&tag_match.tag_ids);
            let special = extract_special_tags(
                tags.iter()
                    .map(|(key, value)| (key.as_str(), value.as_str())),
                preferred_languages,
            );
            pois.push(WriterPoi {
                id,
                coord,
                min_zoom,
                tag_ids: tag_match.tag_ids.clone(),
                tag_values: tag_match.tag_values.clone(),
                special,
            });
            for (interval_index, interval) in zoom_intervals.iter().enumerate() {
                if min_zoom <= interval.max {
                    let x = longitude_to_tile_x(lon, interval.base);
                    let y = latitude_to_tile_y(lat, interval.base);
                    if tile_ranges[interval_index].contains(x, y) {
                        stats.poi_tile_assignments += 1;
                    }
                }
            }
        }
    }
}

fn missing_node_coord() -> NodeCoord {
    NodeCoord {
        lat_micro: i32::MIN,
        lon_micro: i32::MIN,
    }
}

fn is_missing_node_coord(coord: NodeCoord) -> bool {
    coord == missing_node_coord()
}

fn collect_node_coord(id: i64, lat: f64, lon: f64, node_index: &mut [(i64, NodeCoord)]) -> bool {
    if let Ok(index) = node_index.binary_search_by_key(&id, |(node_id, _)| *node_id) {
        node_index[index].1 = NodeCoord::from_degrees(lat, lon);
        return true;
    }
    false
}

fn merge_relation_member_tags(
    mut way_match: TagMatch,
    relation_members: &[RelationMemberInfo],
    way_is_closed: bool,
) -> TagMatch {
    if !way_is_closed {
        return way_match;
    }

    for relation in relation_members
        .iter()
        .filter(|relation| !relation.is_inner)
    {
        for (tag_id, tag_value) in relation.tag_ids.iter().zip(relation.tag_values.iter()) {
            if let Some(existing) = way_match
                .tag_ids
                .iter()
                .position(|existing_id| existing_id == tag_id)
            {
                way_match.tag_values[existing] = tag_value.clone();
            } else {
                way_match.tag_ids.push(*tag_id);
                way_match.tag_values.push(tag_value.clone());
            }
        }
        if !relation.tag_ids.is_empty() {
            way_match.has_known = true;
        }
        way_match.force_polygon_line |= relation.force_polygon_line;
        if let Some(zoom) = relation.min_renderable_zoom {
            way_match.min_renderable_zoom = Some(
                way_match
                    .min_renderable_zoom
                    .map_or(zoom, |current| current.min(zoom)),
            );
        }
    }

    way_match
}

fn should_suppress_standalone_inner_way(
    way_match: &TagMatch,
    relation_members: &[RelationMemberInfo],
    way_is_closed: bool,
) -> bool {
    way_is_closed
        && !way_match.tag_ids.is_empty()
        && relation_members
            .iter()
            .filter(|relation| relation.is_inner)
            .any(|relation| {
                tag_pairs_cover(
                    &relation.tag_ids,
                    &relation.tag_values,
                    &way_match.tag_ids,
                    &way_match.tag_values,
                )
            })
}

fn tag_pairs_cover(
    covering_ids: &[u16],
    covering_values: &[Option<TagValue>],
    covered_ids: &[u16],
    covered_values: &[Option<TagValue>],
) -> bool {
    covered_ids
        .iter()
        .zip(covered_values.iter())
        .all(|(covered_id, covered_value)| {
            covering_ids
                .iter()
                .zip(covering_values.iter())
                .any(|(covering_id, covering_value)| {
                    covering_id == covered_id && covering_value == covered_value
                })
        })
}

fn merge_relation_member_special(
    mut special: SpecialTags,
    relation_members: &[RelationMemberInfo],
    way_is_closed: bool,
) -> SpecialTags {
    if !way_is_closed {
        return special;
    }

    for relation in relation_members
        .iter()
        .filter(|relation| !relation.is_inner)
    {
        if special.name.is_none() {
            special.name = relation.special.name.clone();
        }
        if special.ref_value.is_none() {
            special.ref_value = relation.special.ref_value.clone();
        }
    }

    special
}

#[cfg(test)]
fn attach_supported_multipolygon_relations(
    ways: &mut Vec<WriterWay>,
    relations: &[MultipolygonRelationInfo],
    relation_way_geometries: &HashMap<i64, RelationWayGeometry>,
    tag_frequencies: &mut TagFrequencies,
    stats: &mut TileIndexStats,
    mut progress: Option<&mut ProgressLog>,
) {
    let mut way_indices = HashMap::new();
    for (index, way) in ways.iter().enumerate() {
        way_indices.insert(way.id, index);
    }

    for (relation_index, relation) in relations.iter().enumerate() {
        if relation_index % 500 == 0 {
            if let Some(progress) = progress.as_deref_mut() {
                progress.tick(
                    "multipolygon_assembly",
                    format!(
                        "relations_seen={} total_relations={} ways={} simple_relations={} inner_rings={} partial={} unsupported={}",
                        relation_index,
                        relations.len(),
                        ways.len(),
                        stats.simple_multipolygon_relations_with_inner_rings,
                        stats.multipolygon_inner_rings_attached,
                        stats.partial_multipolygon_relations,
                        stats.unsupported_multipolygon_relations
                    ),
                );
            }
        }
        let relation_started = Instant::now();
        if relation.members.len() >= 500 {
            if let Some(progress) = progress.as_deref_mut() {
                progress.event(
                    "multipolygon_assembly",
                    format!(
                        "complex_relation_start relation_id={} index={} members={}",
                        relation.id,
                        relation_index,
                        relation.members.len()
                    ),
                );
            }
        }
        if relation.force_polygon_line {
            continue;
        }
        let member_refs = relation.members.iter().collect::<Vec<_>>();
        let Some(polygonized) = polygonize_member_rings(&member_refs, relation_way_geometries)
        else {
            stats.unsupported_multipolygon_relations += 1;
            stats.unsupported_multipolygon_no_valid_rings += 1;
            continue;
        };
        if polygonized.partial {
            stats.partial_multipolygon_relations += 1;
        }
        let rings = polygonized.rings;
        let ring_coord_count = rings.iter().map(|ring| ring.coords.len()).sum::<usize>();
        if rings.len() >= 500 || ring_coord_count >= 50_000 {
            if let Some(progress) = progress.as_deref_mut() {
                progress.event(
                    "multipolygon_assembly",
                    format!(
                        "complex_relation_polygonized relation_id={} index={} members={} rings={} coords={} partial={}",
                        relation.id,
                        relation_index,
                        relation.members.len(),
                        rings.len(),
                        ring_coord_count,
                        polygonized.partial
                    ),
                );
            }
        }
        let Some(outer_to_inner) = relate_polygon_rings(&rings) else {
            stats.unsupported_multipolygon_relations += 1;
            stats.unsupported_multipolygon_relation_failures += 1;
            continue;
        };
        if outer_to_inner.is_empty() {
            stats.unsupported_multipolygon_relations += 1;
            stats.unsupported_multipolygon_empty_relations += 1;
            continue;
        }

        for (outer_index, inner_indices) in outer_to_inner {
            let outer_ring = &rings[outer_index];
            let inner_coords = inner_indices
                .iter()
                .map(|index| rings[*index].coords.clone())
                .collect::<Vec<_>>();

            if inner_coords.is_empty()
                && outer_ring.member_ids.len() == 1
                && way_indices.contains_key(&outer_ring.member_ids[0])
            {
                continue;
            }

            if outer_ring.member_ids.len() == 1 {
                let outer_id = outer_ring.member_ids[0];
                if let Some(&way_index) = way_indices.get(&outer_id) {
                    let attached_count = inner_coords.len() as u64;
                    let outer = &mut ways[way_index];
                    outer.area = true;
                    outer.inner_coords.extend(inner_coords);
                    stats.simple_multipolygon_relations_with_inner_rings += 1;
                    stats.multipolygon_inner_rings_attached += attached_count;
                    continue;
                }
            }

            let Some(min_zoom) = relation.min_renderable_zoom else {
                stats.unsupported_multipolygon_relations += 1;
                stats.unsupported_multipolygon_missing_min_zoom += 1;
                continue;
            };
            let Some((min_lat, min_lon, max_lat, max_lon)) = bounds_for_coords(&outer_ring.coords)
            else {
                stats.unsupported_multipolygon_relations += 1;
                stats.unsupported_multipolygon_empty_bounds += 1;
                continue;
            };
            let attached_count = inner_coords.len() as u64;
            ways.push(WriterWay {
                id: -((ways.len() as i64) + 1),
                min_zoom,
                area: true,
                coords: outer_ring.coords.clone(),
                inner_coords,
                min_lat,
                min_lon,
                max_lat,
                max_lon,
                tag_ids: relation.tag_ids.clone(),
                tag_values: relation.tag_values.clone(),
                special: relation.special.clone(),
            });
            tag_frequencies.record_way(&relation.tag_ids);
            way_indices.insert(
                ways.last().expect("virtual way should exist").id,
                ways.len() - 1,
            );
            stats.simple_multipolygon_relations_with_inner_rings += 1;
            stats.multipolygon_inner_rings_attached += attached_count;
        }
        if relation_started.elapsed() >= PROGRESS_INTERVAL {
            if let Some(progress) = progress.as_deref_mut() {
                progress.event(
                    "multipolygon_assembly",
                    format!(
                        "slow_relation_done relation_id={} index={} millis={} members={} ways={} simple_relations={} inner_rings={} partial={} unsupported={}",
                        relation.id,
                        relation_index,
                        relation_started.elapsed().as_millis(),
                        relation.members.len(),
                        ways.len(),
                        stats.simple_multipolygon_relations_with_inner_rings,
                        stats.multipolygon_inner_rings_attached,
                        stats.partial_multipolygon_relations,
                        stats.unsupported_multipolygon_relations
                    ),
                );
            }
        }
    }
}

fn attach_supported_multipolygon_relations_staged(
    staged_way_ids: &HashSet<i64>,
    generated_ways: &mut Vec<WriterWay>,
    inner_attachments: &mut HashMap<i64, Vec<Vec<NodeCoord>>>,
    relations: &[MultipolygonRelationInfo],
    relation_way_geometries: &HashMap<i64, RelationWayGeometry>,
    tag_frequencies: &mut TagFrequencies,
    stats: &mut TileIndexStats,
    mut progress: Option<&mut ProgressLog>,
) {
    for (relation_index, relation) in relations.iter().enumerate() {
        if relation_index % 500 == 0 {
            if let Some(progress) = progress.as_deref_mut() {
                progress.tick(
                    "multipolygon_assembly",
                    format!(
                        "relations_seen={} total_relations={} generated_ways={} simple_relations={} inner_rings={} partial={} unsupported={}",
                        relation_index,
                        relations.len(),
                        generated_ways.len(),
                        stats.simple_multipolygon_relations_with_inner_rings,
                        stats.multipolygon_inner_rings_attached,
                        stats.partial_multipolygon_relations,
                        stats.unsupported_multipolygon_relations
                    ),
                );
            }
        }
        let relation_started = Instant::now();
        if relation.members.len() >= 500 {
            if let Some(progress) = progress.as_deref_mut() {
                progress.event(
                    "multipolygon_assembly",
                    format!(
                        "complex_relation_start relation_id={} index={} members={}",
                        relation.id,
                        relation_index,
                        relation.members.len()
                    ),
                );
            }
        }
        if relation.force_polygon_line {
            continue;
        }
        let member_refs = relation.members.iter().collect::<Vec<_>>();
        let Some(polygonized) = polygonize_member_rings(&member_refs, relation_way_geometries)
        else {
            stats.unsupported_multipolygon_relations += 1;
            stats.unsupported_multipolygon_no_valid_rings += 1;
            continue;
        };
        if polygonized.partial {
            stats.partial_multipolygon_relations += 1;
        }
        let rings = polygonized.rings;
        let ring_coord_count = rings.iter().map(|ring| ring.coords.len()).sum::<usize>();
        if rings.len() >= 500 || ring_coord_count >= 50_000 {
            if let Some(progress) = progress.as_deref_mut() {
                progress.event(
                    "multipolygon_assembly",
                    format!(
                        "complex_relation_polygonized relation_id={} index={} members={} rings={} coords={} partial={}",
                        relation.id,
                        relation_index,
                        relation.members.len(),
                        rings.len(),
                        ring_coord_count,
                        polygonized.partial
                    ),
                );
            }
        }
        let Some(outer_to_inner) = relate_polygon_rings(&rings) else {
            stats.unsupported_multipolygon_relations += 1;
            stats.unsupported_multipolygon_relation_failures += 1;
            continue;
        };
        if outer_to_inner.is_empty() {
            stats.unsupported_multipolygon_relations += 1;
            stats.unsupported_multipolygon_empty_relations += 1;
            continue;
        }

        for (outer_index, inner_indices) in outer_to_inner {
            let outer_ring = &rings[outer_index];
            let inner_coords = inner_indices
                .iter()
                .map(|index| rings[*index].coords.clone())
                .collect::<Vec<_>>();

            if inner_coords.is_empty()
                && outer_ring.member_ids.len() == 1
                && staged_way_ids.contains(&outer_ring.member_ids[0])
            {
                continue;
            }

            if outer_ring.member_ids.len() == 1 {
                let outer_id = outer_ring.member_ids[0];
                if staged_way_ids.contains(&outer_id) {
                    let attached_count = inner_coords.len() as u64;
                    inner_attachments
                        .entry(outer_id)
                        .or_default()
                        .extend(inner_coords);
                    stats.simple_multipolygon_relations_with_inner_rings += 1;
                    stats.multipolygon_inner_rings_attached += attached_count;
                    continue;
                }
            }

            let Some(min_zoom) = relation.min_renderable_zoom else {
                stats.unsupported_multipolygon_relations += 1;
                stats.unsupported_multipolygon_missing_min_zoom += 1;
                continue;
            };
            let Some((min_lat, min_lon, max_lat, max_lon)) = bounds_for_coords(&outer_ring.coords)
            else {
                stats.unsupported_multipolygon_relations += 1;
                stats.unsupported_multipolygon_empty_bounds += 1;
                continue;
            };
            let attached_count = inner_coords.len() as u64;
            generated_ways.push(WriterWay {
                id: -((generated_ways.len() as i64) + 1),
                min_zoom,
                area: true,
                coords: outer_ring.coords.clone(),
                inner_coords,
                min_lat,
                min_lon,
                max_lat,
                max_lon,
                tag_ids: relation.tag_ids.clone(),
                tag_values: relation.tag_values.clone(),
                special: relation.special.clone(),
            });
            tag_frequencies.record_way(&relation.tag_ids);
            stats.simple_multipolygon_relations_with_inner_rings += 1;
            stats.multipolygon_inner_rings_attached += attached_count;
        }
        if relation_started.elapsed() >= PROGRESS_INTERVAL {
            if let Some(progress) = progress.as_deref_mut() {
                progress.event(
                    "multipolygon_assembly",
                    format!(
                        "slow_relation_done relation_id={} index={} millis={} members={} generated_ways={} simple_relations={} inner_rings={} partial={} unsupported={}",
                        relation.id,
                        relation_index,
                        relation_started.elapsed().as_millis(),
                        relation.members.len(),
                        generated_ways.len(),
                        stats.simple_multipolygon_relations_with_inner_rings,
                        stats.multipolygon_inner_rings_attached,
                        stats.partial_multipolygon_relations,
                        stats.unsupported_multipolygon_relations
                    ),
                );
            }
        }
    }
}

fn unsupported_multipolygon_summary(stats: &TileIndexStats) -> Option<String> {
    (stats.unsupported_multipolygon_relations > 0).then(|| {
        format!(
            "unsupported_multipolygon_relations={} no_valid_rings={} relation_failures={} empty_relations={} missing_min_zoom={} empty_bounds={}",
            stats.unsupported_multipolygon_relations,
            stats.unsupported_multipolygon_no_valid_rings,
            stats.unsupported_multipolygon_relation_failures,
            stats.unsupported_multipolygon_empty_relations,
            stats.unsupported_multipolygon_missing_min_zoom,
            stats.unsupported_multipolygon_empty_bounds
        )
    })
}

struct PolygonizedRelationRings {
    rings: Vec<RelationPolygonRing>,
    partial: bool,
}

fn polygonize_member_rings(
    members: &[&RelationMemberRef],
    relation_way_geometries: &HashMap<i64, RelationWayGeometry>,
) -> Option<PolygonizedRelationRings> {
    if members.is_empty() {
        return None;
    }

    let mut rings = Vec::new();
    let mut partial = false;
    let mut unused = members.to_vec();
    while !unused.is_empty() {
        let first = unused.remove(0);
        let Some(first_geometry) = relation_way_geometries.get(&first.way_id) else {
            partial = true;
            continue;
        };
        let mut ring = first_geometry.coords.clone();
        let mut member_ids = vec![first.way_id];

        if !is_closed_way(&ring) {
            while ring.first() != ring.last() {
                let Some(start) = ring.first().copied() else {
                    partial = true;
                    break;
                };
                let Some(end) = ring.last().copied() else {
                    partial = true;
                    break;
                };
                let Some((index, placement)) =
                    unused.iter().enumerate().find_map(|(index, member)| {
                        let coords = &relation_way_geometries.get(&member.way_id)?.coords;
                        match (coords.first().copied(), coords.last().copied()) {
                            (Some(first), Some(last)) if last == start => {
                                Some((index, RingSegmentPlacement::PrependForward))
                            }
                            (Some(first), Some(_last)) if first == start => {
                                Some((index, RingSegmentPlacement::PrependReversed))
                            }
                            (Some(first), Some(_last)) if first == end => {
                                Some((index, RingSegmentPlacement::AppendForward))
                            }
                            (Some(_first), Some(last)) if last == end => {
                                Some((index, RingSegmentPlacement::AppendReversed))
                            }
                            _ => None,
                        }
                    })
                else {
                    partial = true;
                    break;
                };
                let member = unused.remove(index);
                let Some(coords) = relation_way_geometries
                    .get(&member.way_id)
                    .map(|geometry| geometry.coords.as_slice())
                else {
                    partial = true;
                    continue;
                };
                if stitch_ring_segment(&mut ring, coords, placement).is_none() {
                    partial = true;
                    continue;
                }
                member_ids.push(member.way_id);
            }
        }

        if !is_valid_ring_block(&ring) {
            partial = true;
            continue;
        }
        let Some((min_lat, min_lon, max_lat, max_lon)) = bounds_for_coords(&ring) else {
            partial = true;
            continue;
        };
        rings.push(RelationPolygonRing {
            coords: ring,
            member_ids,
            min_lat,
            min_lon,
            max_lat,
            max_lon,
        });
    }

    (!rings.is_empty()).then_some(PolygonizedRelationRings { rings, partial })
}

#[derive(Clone, Copy, Debug)]
enum RingSegmentPlacement {
    PrependForward,
    PrependReversed,
    AppendForward,
    AppendReversed,
}

fn stitch_ring_segment(
    ring: &mut Vec<NodeCoord>,
    coords: &[NodeCoord],
    placement: RingSegmentPlacement,
) -> Option<()> {
    if coords.len() < 2 {
        return None;
    }

    match placement {
        RingSegmentPlacement::PrependForward => {
            let mut prefix = coords.to_vec();
            prefix.pop();
            prefix.append(ring);
            *ring = prefix;
        }
        RingSegmentPlacement::PrependReversed => {
            let mut prefix = coords.iter().rev().copied().collect::<Vec<_>>();
            prefix.pop();
            prefix.append(ring);
            *ring = prefix;
        }
        RingSegmentPlacement::AppendForward => {
            ring.extend(coords.iter().skip(1).copied());
        }
        RingSegmentPlacement::AppendReversed => {
            ring.extend(coords.iter().rev().skip(1).copied());
        }
    }

    Some(())
}

fn relate_polygon_rings(rings: &[RelationPolygonRing]) -> Option<Vec<(usize, Vec<usize>)>> {
    let ring_index = RTree::bulk_load(
        rings
            .iter()
            .enumerate()
            .map(|(index, ring)| RingEnvelope {
                index,
                envelope: ring_envelope(ring),
            })
            .collect(),
    );
    let mut parent = vec![None; rings.len()];

    for inner_index in 0..rings.len() {
        let inner_envelope = ring_envelope(&rings[inner_index]);
        let mut best_parent = None;
        let mut best_parent_area = f64::INFINITY;
        for candidate in ring_index.locate_in_envelope_intersecting(&inner_envelope) {
            let outer_index = candidate.index;
            if outer_index == inner_index
                || !ring_bounds_cover(&rings[outer_index], &rings[inner_index])
            {
                continue;
            }
            let candidate_area = ring_bounds_area(&rings[outer_index]);
            if candidate_area >= best_parent_area {
                continue;
            }
            if ring_covers_coord(&rings[outer_index].coords, rings[inner_index].coords[0]) {
                best_parent = Some(outer_index);
                best_parent_area = candidate_area;
            }
        }
        parent[inner_index] = best_parent;
    }

    let mut outer_to_inner: HashMap<usize, Vec<usize>> = HashMap::new();
    for (index, parent) in parent.into_iter().enumerate() {
        if let Some(parent) = parent {
            outer_to_inner.entry(parent).or_default().push(index);
        } else {
            outer_to_inner.entry(index).or_default();
        }
    }

    let mut related = outer_to_inner.into_iter().collect::<Vec<_>>();
    related.sort_by_key(|(outer, _)| *outer);
    Some(related)
}

fn ring_envelope(ring: &RelationPolygonRing) -> AABB<[f64; 2]> {
    AABB::from_corners([ring.min_lon, ring.min_lat], [ring.max_lon, ring.max_lat])
}

fn ring_bounds_cover(outer: &RelationPolygonRing, inner: &RelationPolygonRing) -> bool {
    outer.min_lat <= inner.min_lat
        && outer.min_lon <= inner.min_lon
        && outer.max_lat >= inner.max_lat
        && outer.max_lon >= inner.max_lon
}

fn ring_bounds_area(ring: &RelationPolygonRing) -> f64 {
    (ring.max_lat - ring.min_lat) * (ring.max_lon - ring.min_lon)
}

fn ring_covers_coord(ring: &[NodeCoord], point: NodeCoord) -> bool {
    if ring.len() < 4 {
        return false;
    }

    let point_x = point.lon_micro as i64;
    let point_y = point.lat_micro as i64;
    let mut inside = false;
    for segment in ring.windows(2) {
        let start_x = segment[0].lon_micro as i64;
        let start_y = segment[0].lat_micro as i64;
        let end_x = segment[1].lon_micro as i64;
        let end_y = segment[1].lat_micro as i64;

        if point_on_microdegree_segment(point_x, point_y, start_x, start_y, end_x, end_y) {
            return true;
        }

        let crosses_ray = (start_y > point_y) != (end_y > point_y);
        if crosses_ray {
            let intersection_x = (end_x - start_x) as f64 * (point_y - start_y) as f64
                / (end_y - start_y) as f64
                + start_x as f64;
            if point_x as f64 <= intersection_x {
                inside = !inside;
            }
        }
    }

    inside
}

fn point_on_microdegree_segment(
    point_x: i64,
    point_y: i64,
    start_x: i64,
    start_y: i64,
    end_x: i64,
    end_y: i64,
) -> bool {
    let cross = (point_y - start_y) * (end_x - start_x) - (point_x - start_x) * (end_y - start_y);
    if cross != 0 {
        return false;
    }

    point_x >= start_x.min(end_x)
        && point_x <= start_x.max(end_x)
        && point_y >= start_y.min(end_y)
        && point_y <= start_y.max(end_y)
}

fn bounds_for_coords(coords: &[NodeCoord]) -> Option<(f64, f64, f64, f64)> {
    if coords.is_empty() {
        return None;
    }
    let mut min_lat = f64::INFINITY;
    let mut min_lon = f64::INFINITY;
    let mut max_lat = f64::NEG_INFINITY;
    let mut max_lon = f64::NEG_INFINITY;
    for coord in coords {
        let lat = coord.lat();
        let lon = coord.lon();
        min_lat = min_lat.min(lat);
        min_lon = min_lon.min(lon);
        max_lat = max_lat.max(lat);
        max_lon = max_lon.max(lon);
    }
    Some((min_lat, min_lon, max_lat, max_lon))
}

fn is_closed_way(coords: &[NodeCoord]) -> bool {
    coords.len() >= 4 && coords.first() == coords.last()
}

fn is_area_tags(tags: &[(String, String)]) -> bool {
    let mut result = true;
    for (key, value) in tags {
        let key = key.to_ascii_lowercase();
        let value = value.to_ascii_lowercase();
        if key == "area" {
            if value == "yes" || value == "y" || value == "true" {
                return true;
            }
            if value == "no" || value == "n" || value == "false" {
                return false;
            }
        }
        if matches!(
            key.as_str(),
            "aeroway" | "building" | "landuse" | "leisure" | "natural" | "amenity"
        ) {
            return true;
        }
        if key == "highway" || key == "barrier" {
            result = false;
        }
        if key == "railway"
            && matches!(
                value.as_str(),
                "rail"
                    | "tram"
                    | "subway"
                    | "monorail"
                    | "narrow_gauge"
                    | "preserved"
                    | "light_rail"
                    | "construction"
            )
        {
            result = false;
        }
    }
    result
}

fn write_map_file(
    output: &PathBuf,
    bbox: BBox,
    writer: &WriterArgs,
    pois: &[WriterPoi],
    staged_ways: &StagedWayStore,
    generated_ways: &[WriterWay],
    inner_attachments: &HashMap<i64, Vec<Vec<NodeCoord>>>,
    optimized_poi_ids: &HashMap<u16, u16>,
    optimized_way_ids: &HashMap<u16, u16>,
    poi_tags: &[String],
    way_tags: &[String],
    mut progress: Option<&mut ProgressLog>,
) -> Result<(), Box<dyn std::error::Error>> {
    let tile_ranges = bbox_tile_ranges(bbox, &writer.zoom_intervals);
    let mut subfiles = Vec::with_capacity(writer.zoom_intervals.len());
    for ((interval_index, interval), tile_range) in writer
        .zoom_intervals
        .iter()
        .enumerate()
        .zip(tile_ranges.iter())
    {
        if let Some(progress) = progress.as_deref_mut() {
            progress.event(
                "write_map_file",
                format!(
                    "build_poi_buckets_start interval_index={} base_zoom={} pois={}",
                    interval_index,
                    interval.base,
                    pois.len()
                ),
            );
        }
        let poi_buckets =
            build_poi_buckets_for_interval(pois, interval_index, *interval, *tile_range);
        if let Some(progress) = progress.as_deref_mut() {
            progress.event(
                "write_map_file",
                format!(
                    "build_poi_buckets_done interval_index={} buckets={}",
                    interval_index,
                    poi_buckets.len()
                ),
            );
        }
        if let Some(progress) = progress.as_deref_mut() {
            progress.event(
                "write_map_file",
                format!(
                    "write_subfile_start interval_index={} base_zoom={} tiles={} staged_ways={} generated_ways={} chunk_tile_limit={}",
                    interval_index,
                    interval.base,
                    tile_range.count(),
                    staged_ways.count,
                    generated_ways.len(),
                    WAY_BUCKET_CHUNK_TILE_LIMIT
                ),
            );
        }
        let subfile = write_subfile_to_temp_file(
            output,
            interval_index,
            *interval,
            *tile_range,
            &poi_buckets,
            pois,
            optimized_poi_ids,
            staged_ways,
            generated_ways,
            inner_attachments,
            optimized_way_ids,
            writer.encoding,
            writer.bbox_enlargement_meters,
            writer.debug_file,
            progress.as_deref_mut(),
        )?;
        if let Some(progress) = progress.as_deref_mut() {
            progress.event(
                "write_map_file",
                format!(
                    "write_subfile_done interval_index={} bytes={}",
                    interval_index, subfile.size
                ),
            );
        }
        subfiles.push(subfile);
    }
    let zero_metadata = vec![binary::SubfileMetadata::default(); writer.zoom_intervals.len()];
    let creation_date_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock is before Unix epoch: {error}"))?
        .as_millis() as u64;
    let header_without_offsets = binary::write_header(binary::HeaderOptions {
        bbox,
        file_size: 0,
        creation_date_millis,
        zoom_intervals: &writer.zoom_intervals,
        subfiles: &zero_metadata,
        poi_tags,
        way_tags,
        map_start_position: writer.map_start_position,
        map_start_zoom: writer.map_start_zoom,
        preferred_languages: &writer.preferred_languages,
        comment: writer.comment.as_deref(),
        debug_file: writer.debug_file,
        created_with: "mapsforge-writer-rust-spike",
    })?;

    let mut next_start = header_without_offsets.len() as u64;
    let mut metadata = Vec::with_capacity(subfiles.len());
    for subfile in &subfiles {
        metadata.push(binary::SubfileMetadata {
            start_address: next_start,
            size: subfile.size,
        });
        next_start += subfile.size;
    }
    let file_size = next_start;
    if let Some(progress) = progress.as_deref_mut() {
        progress.event(
            "write_map_file",
            format!("write_header_start file_size={}", file_size),
        );
    }
    let header = binary::write_header(binary::HeaderOptions {
        bbox,
        file_size,
        creation_date_millis,
        zoom_intervals: &writer.zoom_intervals,
        subfiles: &metadata,
        poi_tags,
        way_tags,
        map_start_position: writer.map_start_position,
        map_start_zoom: writer.map_start_zoom,
        preferred_languages: &writer.preferred_languages,
        comment: writer.comment.as_deref(),
        debug_file: writer.debug_file,
        created_with: "mapsforge-writer-rust-spike",
    })?;

    if let Some(progress) = progress.as_deref_mut() {
        progress.event(
            "write_map_file",
            format!("stream_file_start bytes={}", file_size),
        );
    }
    let mut file = File::create(output)?;
    file.write_all(&header)?;
    for subfile in &subfiles {
        file.write_all(&subfile.index)?;
        let mut tile_data = File::open(&subfile.tile_data_path)?;
        io::copy(&mut tile_data, &mut file)?;
    }
    file.flush()?;
    for subfile in &subfiles {
        fs::remove_file(&subfile.tile_data_path)?;
    }
    if let Some(progress) = progress.as_deref_mut() {
        progress.event(
            "write_map_file",
            format!("stream_file_done bytes={}", file_size),
        );
    }
    Ok(())
}

fn build_poi_buckets_for_interval(
    pois: &[WriterPoi],
    interval_index: usize,
    interval: ZoomInterval,
    tile_range: TileRange,
) -> HashMap<TileKey, Vec<usize>> {
    let mut buckets: HashMap<TileKey, Vec<usize>> = HashMap::new();
    for (poi_index, poi) in pois.iter().enumerate() {
        if poi.min_zoom > interval.max {
            continue;
        }
        let x = longitude_to_tile_x(poi.coord.lon(), interval.base);
        let y = latitude_to_tile_y(poi.coord.lat(), interval.base);
        if tile_range.contains(x, y) {
            buckets
                .entry(TileKey {
                    interval_index,
                    x,
                    y,
                })
                .or_default()
                .push(poi_index);
        }
    }
    for bucket in buckets.values_mut() {
        bucket.sort_by_key(|index| pois[*index].id);
    }
    buckets
}

fn build_way_buckets_for_interval(
    staged_ways: &StagedWayStore,
    generated_ways: &[WriterWay],
    interval_index: usize,
    interval: ZoomInterval,
    tile_range: TileRange,
    bbox_enlargement_meters: f64,
    mut progress: Option<&mut ProgressLog>,
) -> Result<(HashMap<TileKey, Vec<WayBucketEntry>>, WayBucketEntryStats), String> {
    let mut buckets: HashMap<TileKey, Vec<WayBucketEntry>> = HashMap::new();
    let mut bucket_stats = WayBucketEntryStats::default();
    let mut way_index = 0_usize;
    let mut staged_way_reader = StagedWayReader::open(staged_ways)?;
    iter_staged_way_summaries(staged_ways, |summary| {
        if way_index % 250_000 == 0 {
            if let Some(progress) = progress.as_deref_mut() {
                progress.tick(
                    "write_map_file",
                    format!(
                        "build_way_buckets interval_index={} tile_top={} tile_bottom={} ways_seen={} buckets={}",
                        interval_index,
                        tile_range.top,
                        tile_range.bottom,
                        way_index,
                        buckets.len()
                    ),
                );
            }
        }
        if summary.min_zoom > interval.max {
            way_index += 1;
            return Ok(());
        }
        let range = way_tile_range(
            summary.min_lat,
            summary.min_lon,
            summary.max_lat,
            summary.max_lon,
            interval.base,
            bbox_enlargement_meters,
        );
        if tile_range.clipped(range).is_none() {
            way_index += 1;
            return Ok(());
        }
        let way = staged_way_reader.read_at(summary.offset)?;
        let added_entries = add_way_to_interval_buckets(
            &mut buckets,
            interval_index,
            interval,
            tile_range,
            bbox_enlargement_meters,
            &way,
            WaySource::Staged {
                offset: summary.offset,
            },
        );
        if added_entries > 0 {
            bucket_stats.bucket_entries += added_entries;
            bucket_stats.unique_staged_ways += 1;
        }
        way_index += 1;
        Ok(())
    })?;
    for (way_index, way) in generated_ways.iter().enumerate() {
        let added_entries = add_way_to_interval_buckets(
            &mut buckets,
            interval_index,
            interval,
            tile_range,
            bbox_enlargement_meters,
            way,
            WaySource::Generated { index: way_index },
        );
        bucket_stats.bucket_entries += added_entries;
        bucket_stats.generated_entries += added_entries;
    }
    for bucket in buckets.values_mut() {
        bucket.sort_by_key(|entry| entry.id);
    }
    Ok((buckets, bucket_stats))
}

fn add_way_to_interval_buckets(
    buckets: &mut HashMap<TileKey, Vec<WayBucketEntry>>,
    interval_index: usize,
    interval: ZoomInterval,
    tile_range: TileRange,
    bbox_enlargement_meters: f64,
    way: &WriterWay,
    source: WaySource,
) -> usize {
    if way.min_zoom > interval.max {
        return 0;
    }
    let range = way_tile_range(
        way.min_lat,
        way.min_lon,
        way.max_lat,
        way.max_lon,
        interval.base,
        bbox_enlargement_meters,
    );
    let mut added_entries = 0_usize;
    if let Some(clipped_range) = tile_range.clipped(range) {
        for (tile_x, tile_y) in clipped_range.iter() {
            let tile_bounds = tile_bounds(tile_x, tile_y, interval.base, bbox_enlargement_meters);
            if way_intersects_rect(&way.coords, tile_bounds, way.area) {
                buckets
                    .entry(TileKey {
                        interval_index,
                        x: tile_x,
                        y: tile_y,
                    })
                    .or_default()
                    .push(WayBucketEntry {
                        id: way.id,
                        min_zoom: way.min_zoom,
                        source,
                    });
                added_entries += 1;
            }
        }
    }
    added_entries
}

fn write_subfile_to_temp_file(
    output: &PathBuf,
    interval_index: usize,
    interval: ZoomInterval,
    tile_range: TileRange,
    poi_buckets: &HashMap<TileKey, Vec<usize>>,
    pois: &[WriterPoi],
    optimized_poi_ids: &HashMap<u16, u16>,
    staged_ways: &StagedWayStore,
    generated_ways: &[WriterWay],
    inner_attachments: &HashMap<i64, Vec<Vec<NodeCoord>>>,
    optimized_way_ids: &HashMap<u16, u16>,
    encoding: EncodingChoice,
    bbox_enlargement_meters: f64,
    debug_file: bool,
    mut progress: Option<&mut ProgressLog>,
) -> Result<TempSubfile, String> {
    let tile_count = tile_range.count();
    let index_signature_size = if debug_file {
        DEBUG_INDEX_SIGNATURE.len() as u64
    } else {
        0
    };
    let index_size = tile_count
        .checked_mul(5)
        .and_then(|size| size.checked_add(index_signature_size))
        .ok_or_else(|| "subfile tile index is too large".to_string())?;
    if index_size > ((1_u64 << 39) - 1) {
        return Err("subfile tile index exceeds mapsforge 39-bit offset range".to_string());
    }

    let mut index = binary::BinaryEncoder::new();
    if debug_file {
        index.write_bytes(DEBUG_INDEX_SIGNATURE.as_bytes());
    }
    let tile_data_path = temp_subfile_tile_data_path(output, interval_index);
    let mut tiles = File::create(&tile_data_path).map_err(|error| {
        format!(
            "cannot create temporary subfile tile data {}: {error}",
            tile_data_path.display()
        )
    })?;
    let mut staged_way_reader = StagedWayReader::open(staged_ways)?;
    let mut current_offset = index_size;
    let mut tile_index = 0_u64;
    let mut chunk_top = tile_range.top;
    while chunk_top <= tile_range.bottom {
        let chunk_range = tile_bucket_chunk_range(tile_range, chunk_top);
        if let Some(progress) = progress.as_deref_mut() {
            progress.event(
                "write_map_file",
                format!(
                    "build_way_buckets_start interval_index={} base_zoom={} tile_top={} tile_bottom={} ways={}",
                    interval_index,
                    interval.base,
                    chunk_range.top,
                    chunk_range.bottom,
                    staged_ways.count + generated_ways.len() as u64
                ),
            );
        }
        let (way_buckets, bucket_stats) = build_way_buckets_for_interval(
            staged_ways,
            generated_ways,
            interval_index,
            interval,
            chunk_range,
            bbox_enlargement_meters,
            progress.as_deref_mut(),
        )?;
        if let Some(progress) = progress.as_deref_mut() {
            progress.event(
                "write_map_file",
                format!(
                    "build_way_buckets_done interval_index={} tile_top={} tile_bottom={} buckets={} bucket_entries={} unique_staged_ways={} generated_entries={}",
                    interval_index,
                    chunk_range.top,
                    chunk_range.bottom,
                    way_buckets.len(),
                    bucket_stats.bucket_entries,
                    bucket_stats.unique_staged_ways,
                    bucket_stats.generated_entries
                ),
            );
        }
        // P0: Collect all unique staged-way offsets referenced by this chunk's
        // buckets, then bulk-read them in a single sequential pass over the
        // staged-way data file. This replaces ~N random seeks with one sorted
        // sequential scan per chunk.
        let mut chunk_staged_offsets: HashSet<u64> = HashSet::new();
        for entries in way_buckets.values() {
            for entry in entries {
                if let WaySource::Staged { offset } = entry.source {
                    chunk_staged_offsets.insert(offset);
                }
            }
        }
        let bulk_ways = if !chunk_staged_offsets.is_empty() {
            if let Some(progress) = progress.as_deref_mut() {
                progress.event(
                    "write_map_file",
                    format!(
                        "bulk_read_start interval_index={} tile_top={} tile_bottom={} unique_offsets={}",
                        interval_index,
                        chunk_range.top,
                        chunk_range.bottom,
                        chunk_staged_offsets.len()
                    ),
                );
            }
            let bulk = staged_way_reader.bulk_read_offsets(&chunk_staged_offsets)?;
            if let Some(progress) = progress.as_deref_mut() {
                progress.event(
                    "write_map_file",
                    format!(
                        "bulk_read_done interval_index={} tile_top={} tile_bottom={} bulk_ways={}",
                        interval_index,
                        chunk_range.top,
                        chunk_range.bottom,
                        bulk.len()
                    ),
                );
            }
            Some(bulk)
        } else {
            None
        };
        let bulk_ways_ref = bulk_ways.as_ref();

        let mut chunk_tiles = 0_u64;
        let mut chunk_nonempty_tiles = 0_u64;
        let mut chunk_payload_bytes = 0_u64;
        let mut chunk_poi_entries = 0_u64;
        let mut chunk_way_entries = 0_u64;
        for (tile_x, tile_y) in chunk_range.iter() {
            if tile_index > 0 && tile_index % 1_000 == 0 {
                if let Some(progress) = progress.as_deref_mut() {
                    progress.tick(
                        "write_map_file",
                        format!(
                            "write_subfile interval_index={} tile_top={} tile_bottom={} tiles_written={} total_tiles={} bulk_ways={}",
                            interval_index,
                            chunk_range.top,
                            chunk_range.bottom,
                            tile_index,
                            tile_count,
                            bulk_ways_ref.map_or(0, |m| m.len())
                        ),
                    );
                }
            }
            let tile_pois = poi_buckets
                .get(&TileKey {
                    interval_index,
                    x: tile_x,
                    y: tile_y,
                })
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let tile_ways = way_buckets
                .get(&TileKey {
                    interval_index,
                    x: tile_x,
                    y: tile_y,
                })
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            chunk_tiles += 1;
            chunk_poi_entries += tile_pois.len() as u64;
            chunk_way_entries += tile_ways.len() as u64;
            if !tile_pois.is_empty() || !tile_ways.is_empty() {
                chunk_nonempty_tiles += 1;
            }
            let tile_payload = write_tile_payload(
                interval,
                tile_x,
                tile_y,
                pois,
                tile_pois,
                optimized_poi_ids,
                &mut staged_way_reader,
                bulk_ways_ref,
                generated_ways,
                inner_attachments,
                tile_ways,
                optimized_way_ids,
                encoding,
                bbox_enlargement_meters,
                debug_file,
            )?;
            chunk_payload_bytes += tile_payload.len() as u64;
            index.write_five_byte_offset(current_offset)?;
            tiles
                .write_all(&tile_payload)
                .map_err(|error| format!("cannot write temporary subfile tile data: {error}"))?;
            current_offset = current_offset
                .checked_add(tile_payload.len() as u64)
                .ok_or_else(|| "subfile size overflow".to_string())?;
            if current_offset > ((1_u64 << 39) - 1) {
                return Err("subfile exceeds mapsforge 39-bit offset range".to_string());
            }
            tile_index += 1;
        }
        if let Some(progress) = progress.as_deref_mut() {
            progress.event(
                "write_map_file",
                format!(
                    "write_subfile_chunk_done interval_index={} tile_top={} tile_bottom={} tiles={} nonempty_tiles={} payload_bytes={} poi_entries={} way_entries={} cache_hits={} cache_misses={} cache_entries={}",
                    interval_index,
                    chunk_range.top,
                    chunk_range.bottom,
                    chunk_tiles,
                    chunk_nonempty_tiles,
                    chunk_payload_bytes,
                    chunk_poi_entries,
                    chunk_way_entries,
                    staged_way_reader.cache_hits,
                    staged_way_reader.cache_misses,
                    staged_way_reader.cache.len()
                ),
            );
        }
        if chunk_range.bottom == i32::MAX {
            break;
        }
        chunk_top = chunk_range.bottom + 1;
    }
    if let Some(progress) = progress.as_deref_mut() {
        progress.event(
            "write_map_file",
            format!(
                "write_subfile_cache interval_index={} cache_hits={} cache_misses={} cache_entries={}",
                interval_index,
                staged_way_reader.cache_hits,
                staged_way_reader.cache_misses,
                staged_way_reader.cache.len()
            ),
        );
    }
    tiles
        .flush()
        .map_err(|error| format!("cannot flush temporary subfile tile data: {error}"))?;

    Ok(TempSubfile {
        index: index.into_bytes(),
        tile_data_path,
        size: current_offset,
    })
}

fn temp_subfile_tile_data_path(output: &PathBuf, interval_index: usize) -> PathBuf {
    let mut path = output.clone();
    let file_name = output
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("mapsforge-output.map");
    path.set_file_name(format!(
        "{file_name}.{}.interval-{interval_index}.tiles.tmp",
        process::id()
    ));
    path
}

fn tile_bucket_chunk_range(tile_range: TileRange, top: i32) -> TileRange {
    let width = (tile_range.right - tile_range.left + 1) as u64;
    let rows = (WAY_BUCKET_CHUNK_TILE_LIMIT / width).max(1);
    let bottom = (top as i64 + rows as i64 - 1)
        .min(tile_range.bottom as i64)
        .try_into()
        .expect("chunk bottom should fit in i32");
    TileRange {
        left: tile_range.left,
        right: tile_range.right,
        top,
        bottom,
    }
}

fn temp_staged_way_path() -> PathBuf {
    env::temp_dir().join(format!("mapsforge-staged-ways-{}.tmp", process::id()))
}

fn temp_node_index_path() -> PathBuf {
    env::temp_dir().join(format!("mapsforge-node-index-{}.tmp", process::id()))
}

fn temp_staged_way_index_path(path: &Path) -> PathBuf {
    let mut index_path = path.to_path_buf();
    index_path.set_extension("idx");
    index_path
}

fn write_staged_way_summary(output: &mut File, offset: u64, way: &WriterWay) -> Result<(), String> {
    output
        .write_all(&offset.to_be_bytes())
        .map_err(|error| format!("cannot write staged way index offset: {error}"))?;
    output
        .write_all(&way.min_zoom.to_be_bytes())
        .map_err(|error| format!("cannot write staged way index min zoom: {error}"))?;
    write_f64_file(output, way.min_lat, "min lat")?;
    write_f64_file(output, way.min_lon, "min lon")?;
    write_f64_file(output, way.max_lat, "max lat")?;
    write_f64_file(output, way.max_lon, "max lon")?;
    Ok(())
}

fn encode_staged_way(way: &WriterWay) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    write_i64_bytes(&mut bytes, way.id);
    bytes.push(way.min_zoom);
    bytes.push(u8::from(way.area));
    write_coords_bytes(&mut bytes, &way.coords)?;
    write_u32_len(
        &mut bytes,
        way.inner_coords.len(),
        "inner coordinate block count",
    )?;
    for inner in &way.inner_coords {
        write_coords_bytes(&mut bytes, inner)?;
    }
    write_f64_bytes(&mut bytes, way.min_lat);
    write_f64_bytes(&mut bytes, way.min_lon);
    write_f64_bytes(&mut bytes, way.max_lat);
    write_f64_bytes(&mut bytes, way.max_lon);
    write_u32_len(&mut bytes, way.tag_ids.len(), "tag id count")?;
    for tag_id in &way.tag_ids {
        bytes.extend_from_slice(&tag_id.to_be_bytes());
    }
    write_u32_len(&mut bytes, way.tag_values.len(), "tag value count")?;
    for value in &way.tag_values {
        encode_staged_tag_value(&mut bytes, value)?;
    }
    encode_staged_special_tags(&mut bytes, &way.special)?;
    Ok(bytes)
}

fn iter_staged_way_summaries(
    store: &StagedWayStore,
    mut on_way: impl FnMut(StagedWaySummary) -> Result<(), String>,
) -> Result<(), String> {
    let mut file = File::open(&store.index_path).map_err(|error| {
        format!(
            "cannot open staged way index file {}: {error}",
            store.index_path.display()
        )
    })?;
    for _ in 0..store.count {
        on_way(read_staged_way_summary(&mut file)?)?;
    }
    Ok(())
}

fn read_staged_way_summary(file: &mut File) -> Result<StagedWaySummary, String> {
    let mut offset_bytes = [0_u8; 8];
    file.read_exact(&mut offset_bytes)
        .map_err(|error| format!("cannot read staged way index offset: {error}"))?;
    let mut min_zoom = [0_u8; 1];
    file.read_exact(&mut min_zoom)
        .map_err(|error| format!("cannot read staged way index min zoom: {error}"))?;
    Ok(StagedWaySummary {
        offset: u64::from_be_bytes(offset_bytes),
        min_zoom: min_zoom[0],
        min_lat: read_f64_file(file, "min lat")?,
        min_lon: read_f64_file(file, "min lon")?,
        max_lat: read_f64_file(file, "max lat")?,
        max_lon: read_f64_file(file, "max lon")?,
    })
}

fn read_staged_way(file: &mut File) -> Result<WriterWay, String> {
    let mut len_bytes = [0_u8; 4];
    file.read_exact(&mut len_bytes)
        .map_err(|error| format!("cannot read staged way length: {error}"))?;
    let record_len = u32::from_be_bytes(len_bytes) as usize;
    let mut record = vec![0_u8; record_len];
    file.read_exact(&mut record)
        .map_err(|error| format!("cannot read staged way record: {error}"))?;
    let mut cursor = StagedWayCursor::new(&record);
    let id = cursor.read_i64()?;
    let min_zoom = cursor.read_u8()?;
    let area = cursor.read_u8()? != 0;
    let coords = cursor.read_coords()?;
    let inner_count = cursor.read_u32()? as usize;
    let mut inner_coords = Vec::with_capacity(inner_count);
    for _ in 0..inner_count {
        inner_coords.push(cursor.read_coords()?);
    }
    let min_lat = cursor.read_f64()?;
    let min_lon = cursor.read_f64()?;
    let max_lat = cursor.read_f64()?;
    let max_lon = cursor.read_f64()?;
    let tag_id_count = cursor.read_u32()? as usize;
    let mut tag_ids = Vec::with_capacity(tag_id_count);
    for _ in 0..tag_id_count {
        tag_ids.push(cursor.read_u16()?);
    }
    let tag_value_count = cursor.read_u32()? as usize;
    let mut tag_values = Vec::with_capacity(tag_value_count);
    for _ in 0..tag_value_count {
        tag_values.push(cursor.read_tag_value()?);
    }
    let special = cursor.read_special_tags()?;
    if !cursor.is_empty() {
        return Err("staged way record has trailing bytes".to_string());
    }
    Ok(WriterWay {
        id,
        min_zoom,
        area,
        coords,
        inner_coords,
        min_lat,
        min_lon,
        max_lat,
        max_lon,
        tag_ids,
        tag_values,
        special,
    })
}

fn write_coords_bytes(output: &mut Vec<u8>, coords: &[NodeCoord]) -> Result<(), String> {
    write_u32_len(output, coords.len(), "coordinate count")?;
    for coord in coords {
        output.extend_from_slice(&coord.lat_micro.to_be_bytes());
        output.extend_from_slice(&coord.lon_micro.to_be_bytes());
    }
    Ok(())
}

fn encode_staged_tag_value(output: &mut Vec<u8>, value: &Option<TagValue>) -> Result<(), String> {
    match value {
        None => output.push(0),
        Some(TagValue::Byte(value)) => {
            output.push(1);
            output.push(*value as u8);
        }
        Some(TagValue::Short(value)) => {
            output.push(2);
            output.extend_from_slice(&value.to_be_bytes());
        }
        Some(TagValue::Int(value)) => {
            output.push(3);
            output.extend_from_slice(&value.to_be_bytes());
        }
        Some(TagValue::Float(value)) => {
            output.push(4);
            output.extend_from_slice(&value.to_bits().to_be_bytes());
        }
        Some(TagValue::String(value)) => {
            output.push(5);
            write_string_bytes(output, value)?;
        }
    }
    Ok(())
}

fn encode_staged_special_tags(output: &mut Vec<u8>, special: &SpecialTags) -> Result<(), String> {
    write_optional_string_bytes(output, special.name.as_deref())?;
    write_optional_string_bytes(output, special.housenumber.as_deref())?;
    write_optional_string_bytes(output, special.ref_value.as_deref())?;
    write_optional_string_bytes(output, special.relation_type.as_deref())?;
    output.extend_from_slice(&special.layer.to_be_bytes());
    output.extend_from_slice(&special.elevation.to_be_bytes());
    Ok(())
}

fn write_i64_bytes(output: &mut Vec<u8>, value: i64) {
    output.extend_from_slice(&value.to_be_bytes());
}

fn write_f64_bytes(output: &mut Vec<u8>, value: f64) {
    output.extend_from_slice(&value.to_bits().to_be_bytes());
}

fn write_f64_file(output: &mut File, value: f64, name: &str) -> Result<(), String> {
    output
        .write_all(&value.to_bits().to_be_bytes())
        .map_err(|error| format!("cannot write staged way index {name}: {error}"))
}

fn read_f64_file(input: &mut File, name: &str) -> Result<f64, String> {
    let mut bytes = [0_u8; 8];
    input
        .read_exact(&mut bytes)
        .map_err(|error| format!("cannot read staged way index {name}: {error}"))?;
    Ok(f64::from_bits(u64::from_be_bytes(bytes)))
}

fn write_u32_len(output: &mut Vec<u8>, value: usize, name: &str) -> Result<(), String> {
    let value = u32::try_from(value).map_err(|_| format!("{name} is too large"))?;
    output.extend_from_slice(&value.to_be_bytes());
    Ok(())
}

fn write_optional_string_bytes(output: &mut Vec<u8>, value: Option<&str>) -> Result<(), String> {
    if let Some(value) = value {
        output.push(1);
        write_string_bytes(output, value)?;
    } else {
        output.push(0);
    }
    Ok(())
}

fn write_string_bytes(output: &mut Vec<u8>, value: &str) -> Result<(), String> {
    write_u32_len(output, value.len(), "string length")?;
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

struct StagedWayCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> StagedWayCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn is_empty(&self) -> bool {
        self.offset == self.bytes.len()
    }

    fn read_exact<const N: usize>(&mut self) -> Result<[u8; N], String> {
        let end = self
            .offset
            .checked_add(N)
            .ok_or_else(|| "staged way record offset overflow".to_string())?;
        let slice = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| "truncated staged way record".to_string())?;
        self.offset = end;
        Ok(slice.try_into().expect("slice length should match"))
    }

    fn read_u8(&mut self) -> Result<u8, String> {
        Ok(self.read_exact::<1>()?[0])
    }

    fn read_u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_be_bytes(self.read_exact()?))
    }

    fn read_u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_be_bytes(self.read_exact()?))
    }

    fn read_i64(&mut self) -> Result<i64, String> {
        Ok(i64::from_be_bytes(self.read_exact()?))
    }

    fn read_f64(&mut self) -> Result<f64, String> {
        Ok(f64::from_bits(u64::from_be_bytes(self.read_exact()?)))
    }

    fn read_i32(&mut self) -> Result<i32, String> {
        Ok(i32::from_be_bytes(self.read_exact()?))
    }

    fn read_coords(&mut self) -> Result<Vec<NodeCoord>, String> {
        let count = self.read_u32()? as usize;
        let mut coords = Vec::with_capacity(count);
        for _ in 0..count {
            coords.push(NodeCoord {
                lat_micro: self.read_i32()?,
                lon_micro: self.read_i32()?,
            });
        }
        Ok(coords)
    }

    fn read_tag_value(&mut self) -> Result<Option<TagValue>, String> {
        match self.read_u8()? {
            0 => Ok(None),
            1 => Ok(Some(TagValue::Byte(self.read_u8()? as i8))),
            2 => Ok(Some(TagValue::Short(i16::from_be_bytes(
                self.read_exact()?,
            )))),
            3 => Ok(Some(TagValue::Int(i32::from_be_bytes(self.read_exact()?)))),
            4 => Ok(Some(TagValue::Float(f32::from_bits(u32::from_be_bytes(
                self.read_exact()?,
            ))))),
            5 => Ok(Some(TagValue::String(self.read_string()?))),
            tag => Err(format!("unsupported staged tag value marker: {tag}")),
        }
    }

    fn read_special_tags(&mut self) -> Result<SpecialTags, String> {
        Ok(SpecialTags {
            name: self.read_optional_string()?,
            housenumber: self.read_optional_string()?,
            ref_value: self.read_optional_string()?,
            relation_type: self.read_optional_string()?,
            layer: i8::from_be_bytes(self.read_exact()?),
            elevation: i16::from_be_bytes(self.read_exact()?),
        })
    }

    fn read_optional_string(&mut self) -> Result<Option<String>, String> {
        match self.read_u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.read_string()?)),
            marker => Err(format!("unsupported optional string marker: {marker}")),
        }
    }

    fn read_string(&mut self) -> Result<String, String> {
        let length = self.read_u32()? as usize;
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| "staged string offset overflow".to_string())?;
        let slice = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| "truncated staged string".to_string())?;
        self.offset = end;
        String::from_utf8(slice.to_vec()).map_err(|error| format!("invalid staged UTF-8: {error}"))
    }
}

fn write_tile_payload(
    interval: ZoomInterval,
    tile_x: i32,
    tile_y: i32,
    pois: &[WriterPoi],
    poi_indices: &[usize],
    optimized_poi_ids: &HashMap<u16, u16>,
    staged_way_reader: &mut StagedWayReader,
    bulk_ways: Option<&HashMap<u64, WriterWay>>,
    generated_ways: &[WriterWay],
    inner_attachments: &HashMap<i64, Vec<Vec<NodeCoord>>>,
    way_entries: &[WayBucketEntry],
    optimized_way_ids: &HashMap<u16, u16>,
    encoding: EncodingChoice,
    bbox_enlargement_meters: f64,
    debug_file: bool,
) -> Result<Vec<u8>, String> {
    let rows = (interval.max - interval.min + 1) as usize;
    let mut poi_by_row = vec![Vec::new(); rows];
    for poi_index in poi_indices {
        let poi = &pois[*poi_index];
        let zoom = poi.min_zoom.max(interval.min);
        poi_by_row[(zoom - interval.min) as usize].push(*poi_index);
    }
    let mut way_by_row = vec![Vec::new(); rows];
    for entry in way_entries {
        let zoom = entry.min_zoom.max(interval.min);
        way_by_row[(zoom - interval.min) as usize].push(*entry);
    }

    let tile_lat =
        NodeCoord::from_degrees(tile_y_to_latitude(tile_y, interval.base), 0.0).lat_micro;
    let tile_lon =
        NodeCoord::from_degrees(0.0, tile_x_to_longitude(tile_x, interval.base)).lon_micro;
    let mut poi_data = Vec::new();
    let mut way_data = Vec::new();
    let mut zoom_counts = Vec::with_capacity(rows);
    for row_index in 0..rows {
        for poi_index in &poi_by_row[row_index] {
            let poi = &pois[*poi_index];
            if debug_file {
                poi_data.extend_from_slice(&debug_signature(
                    DEBUG_POI_HEAD,
                    poi.id,
                    DEBUG_POI_TAIL,
                ));
            }
            poi_data.extend_from_slice(&write_poi(poi, tile_lat, tile_lon, optimized_poi_ids)?);
        }
        let mut way_count = 0_u32;
        for entry in &way_by_row[row_index] {
            let way =
                resolve_way_entry(*entry, staged_way_reader, bulk_ways, generated_ways, inner_attachments)?;
            let subtile_mask =
                compute_subtile_mask(&way, tile_x, tile_y, interval.base, bbox_enlargement_meters);
            let way_data_blocks = way_coordinate_blocks_for_tile(
                &way,
                tile_bounds(tile_x, tile_y, interval.base, bbox_enlargement_meters),
                interval,
            );
            if way_data_blocks.is_empty() {
                continue;
            }
            let way_bytes = write_way(
                &way,
                tile_lat,
                tile_lon,
                optimized_way_ids,
                encoding,
                subtile_mask,
                &way_data_blocks,
            )?;
            if debug_file {
                way_data.extend_from_slice(&debug_signature(
                    DEBUG_WAY_HEAD,
                    way.id,
                    DEBUG_WAY_TAIL,
                ));
            }
            encoder_write_len_prefixed(&mut way_data, &way_bytes)?;
            way_count += 1;
        }
        zoom_counts.push((poi_by_row[row_index].len() as u32, way_count));
    }

    let mut encoder = binary::BinaryEncoder::new();
    if debug_file {
        encoder.write_bytes(&debug_tile_signature(tile_x, tile_y));
    }
    for (poi_count, way_count) in zoom_counts {
        encoder.write_var_uint(poi_count);
        encoder.write_var_uint(way_count);
    }
    encoder.write_var_uint(
        u32::try_from(poi_data.len()).map_err(|_| "POI data is too large".to_string())?,
    );
    let mut tile = encoder.into_bytes();
    tile.extend_from_slice(&poi_data);
    tile.extend_from_slice(&way_data);
    Ok(tile)
}

fn resolve_way_entry(
    entry: WayBucketEntry,
    staged_way_reader: &mut StagedWayReader,
    bulk_ways: Option<&HashMap<u64, WriterWay>>,
    generated_ways: &[WriterWay],
    inner_attachments: &HashMap<i64, Vec<Vec<NodeCoord>>>,
) -> Result<WriterWay, String> {
    let mut way = match entry.source {
        WaySource::Staged { offset } => {
            // Try the pre-loaded bulk map first (P0 optimization path)
            if let Some(bulk) = bulk_ways {
                if let Some(w) = bulk.get(&offset) {
                    w.clone()
                } else {
                    // Fallback: shouldn't happen if bulk was built correctly
                    staged_way_reader.read_at(offset)?
                }
            } else {
                staged_way_reader.read_at(offset)?
            }
        }
        WaySource::Generated { index } => generated_ways
            .get(index)
            .cloned()
            .ok_or_else(|| format!("missing generated way at index {index}"))?,
    };
    if let Some(inners) = inner_attachments.get(&way.id) {
        way.area = true;
        way.inner_coords.extend(inners.iter().cloned());
    }
    Ok(way)
}

fn encoder_write_len_prefixed(output: &mut Vec<u8>, bytes: &[u8]) -> Result<(), String> {
    let mut encoder = binary::BinaryEncoder::new();
    encoder.write_var_uint(
        u32::try_from(bytes.len()).map_err(|_| "way payload is too large".to_string())?,
    );
    output.extend_from_slice(&encoder.into_bytes());
    output.extend_from_slice(bytes);
    Ok(())
}

fn debug_tile_signature(tile_x: i32, tile_y: i32) -> [u8; DEBUG_BLOCK_SIZE] {
    debug_padded_string(&format!(
        "{DEBUG_TILE_HEAD}{tile_x},{tile_y}{DEBUG_TILE_TAIL}"
    ))
}

fn debug_signature(head: &str, id: i64, tail: &str) -> [u8; DEBUG_BLOCK_SIZE] {
    debug_padded_string(&format!("{head}{id}{tail}"))
}

fn debug_padded_string(value: &str) -> [u8; DEBUG_BLOCK_SIZE] {
    let mut result = [b' '; DEBUG_BLOCK_SIZE];
    let bytes = value.as_bytes();
    let length = bytes.len().min(DEBUG_BLOCK_SIZE);
    result[..length].copy_from_slice(&bytes[..length]);
    result
}

fn write_poi(
    poi: &WriterPoi,
    tile_lat: i32,
    tile_lon: i32,
    optimized_poi_ids: &HashMap<u16, u16>,
) -> Result<Vec<u8>, String> {
    let mut encoder = binary::BinaryEncoder::new();
    encoder.write_var_int(poi.coord.lat_micro - tile_lat);
    encoder.write_var_int(poi.coord.lon_micro - tile_lon);
    encoder.write_u8(layer_and_tag_count_byte(
        poi.special.layer,
        poi.tag_ids.len(),
    )?);
    for tag_id in &poi.tag_ids {
        let optimized_id = optimized_poi_ids
            .get(tag_id)
            .ok_or_else(|| format!("missing optimized POI tag id for original id {tag_id}"))?;
        encoder.write_var_uint(*optimized_id as u32);
    }
    write_tag_values(&mut encoder, &poi.tag_values, true)?;
    encoder.write_u8(poi_feature_byte(
        poi.special.name.as_deref(),
        poi.special.elevation,
        poi.special.housenumber.as_deref(),
    ));
    if let Some(name) = poi
        .special
        .name
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        encoder.write_utf8(name)?;
    }
    if let Some(housenumber) = poi
        .special
        .housenumber
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        encoder.write_utf8(housenumber)?;
    }
    if poi.special.elevation != 0 {
        encoder.write_var_int(poi.special.elevation as i32);
    }
    Ok(encoder.into_bytes())
}

fn write_tag_values(
    encoder: &mut binary::BinaryEncoder,
    values: &[Option<TagValue>],
    poi: bool,
) -> Result<(), String> {
    for value in values.iter().flatten() {
        match value {
            TagValue::Byte(value) if poi => encoder.write_i16(*value as i16),
            TagValue::Byte(value) => encoder.write_u8(*value as u8),
            TagValue::Short(value) => encoder.write_i16(*value),
            TagValue::Int(value) => encoder.write_i32(*value),
            TagValue::Float(value) => encoder.write_u32(value.to_bits()),
            TagValue::String(value) => encoder.write_utf8(value)?,
        }
    }
    Ok(())
}

fn write_way(
    way: &WriterWay,
    tile_lat: i32,
    tile_lon: i32,
    optimized_way_ids: &HashMap<u16, u16>,
    requested_encoding: EncodingChoice,
    subtile_mask: u16,
    way_data_blocks: &[WayDataBlock],
) -> Result<Vec<u8>, String> {
    let (encoding, encoded_blocks) = encode_way_data_blocks(way_data_blocks, requested_encoding)?;

    let mut encoder = binary::BinaryEncoder::new();
    encoder.write_u16(subtile_mask);
    encoder.write_u8(layer_and_tag_count_byte(
        way.special.layer,
        way.tag_ids.len(),
    )?);
    for tag_id in &way.tag_ids {
        let optimized_id = optimized_way_ids
            .get(tag_id)
            .ok_or_else(|| format!("missing optimized way tag id for original id {tag_id}"))?;
        encoder.write_var_uint(*optimized_id as u32);
    }
    write_tag_values(&mut encoder, &way.tag_values, false)?;
    encoder.write_u8(way_feature_byte(
        way.special.name.as_deref(),
        way.special.housenumber.as_deref(),
        way.special.ref_value.as_deref(),
        false,
        encoded_blocks.len(),
        encoding,
    ));
    if let Some(name) = way
        .special
        .name
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        encoder.write_utf8(name)?;
    }
    if let Some(housenumber) = way
        .special
        .housenumber
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        encoder.write_utf8(housenumber)?;
    }
    if let Some(ref_value) = way
        .special
        .ref_value
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        encoder.write_utf8(ref_value)?;
    }
    if encoded_blocks.len() > 1 {
        encoder.write_var_uint(
            u32::try_from(encoded_blocks.len())
                .map_err(|_| "too many way coordinate blocks".to_string())?,
        );
    }
    for block in &encoded_blocks {
        encoder.write_var_uint(
            u32::try_from(1 + block.inners.len())
                .map_err(|_| "too many coordinate blocks in way data block".to_string())?,
        );
        write_way_coordinate_block(&mut encoder, &block.outer, tile_lat, tile_lon)?;
        for inner in &block.inners {
            write_way_coordinate_block(&mut encoder, inner, tile_lat, tile_lon)?;
        }
    }
    Ok(encoder.into_bytes())
}

fn encode_way_data_blocks(
    way_data_blocks: &[WayDataBlock],
    requested_encoding: EncodingChoice,
) -> Result<(CoordinateEncoding, Vec<EncodedWayDataBlock>), String> {
    if way_data_blocks.is_empty() {
        return Err("way must have at least one coordinate block".to_string());
    }

    let raw_blocks = way_data_blocks
        .iter()
        .map(|block| RawWayDataBlock {
            outer: raw_coordinate_block(&block.outer),
            inners: block
                .inners
                .iter()
                .map(|inner| raw_coordinate_block(inner))
                .collect(),
        })
        .collect::<Vec<_>>();

    let encoding = match requested_encoding {
        EncodingChoice::Single => CoordinateEncoding::SingleDelta,
        EncodingChoice::Double => CoordinateEncoding::DoubleDelta,
        EncodingChoice::Auto => choose_way_data_block_encoding(&raw_blocks)?,
    };
    let encoded_blocks = raw_blocks
        .iter()
        .map(|block| {
            Ok::<EncodedWayDataBlock, String>(EncodedWayDataBlock {
                outer: encode_raw_coordinate_block(&block.outer, encoding)?,
                inners: block
                    .inners
                    .iter()
                    .map(|inner| encode_raw_coordinate_block(inner, encoding))
                    .collect::<Result<Vec<_>, _>>()?,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok((encoding, encoded_blocks))
}

#[derive(Clone, Debug)]
struct RawWayDataBlock {
    outer: Vec<i32>,
    inners: Vec<Vec<i32>>,
}

fn raw_coordinate_block(block: &[NodeCoord]) -> Vec<i32> {
    block
        .iter()
        .flat_map(|coord| [coord.lat_micro, coord.lon_micro])
        .collect()
}

fn encode_raw_coordinate_block(
    coordinates: &[i32],
    encoding: CoordinateEncoding,
) -> Result<Vec<i32>, String> {
    match encoding {
        CoordinateEncoding::SingleDelta => delta_encode_coordinates(coordinates),
        CoordinateEncoding::DoubleDelta => double_delta_encode_coordinates(coordinates),
    }
}

fn choose_way_data_block_encoding(
    blocks: &[RawWayDataBlock],
) -> Result<CoordinateEncoding, String> {
    let mut single_size = 0;
    let mut double_size = 0;
    for block in blocks {
        single_size += serialized_signed_varint_size(&delta_encode_coordinates(&block.outer)?);
        double_size +=
            serialized_signed_varint_size(&double_delta_encode_coordinates(&block.outer)?);
        for inner in &block.inners {
            single_size += serialized_signed_varint_size(&delta_encode_coordinates(inner)?);
            double_size += serialized_signed_varint_size(&double_delta_encode_coordinates(inner)?);
        }
    }
    if single_size <= double_size {
        Ok(CoordinateEncoding::SingleDelta)
    } else {
        Ok(CoordinateEncoding::DoubleDelta)
    }
}

fn write_way_coordinate_block(
    encoder: &mut binary::BinaryEncoder,
    encoded_coordinates: &[i32],
    tile_lat: i32,
    tile_lon: i32,
) -> Result<(), String> {
    if encoded_coordinates.len() < 4 || encoded_coordinates.len() % 2 != 0 {
        return Err("way coordinate block must contain at least two lat/lon pairs".to_string());
    }
    encoder.write_var_uint((encoded_coordinates.len() / 2) as u32);
    encoder.write_var_int(encoded_coordinates[0] - tile_lat);
    encoder.write_var_int(encoded_coordinates[1] - tile_lon);
    for coordinate in &encoded_coordinates[2..] {
        encoder.write_var_int(*coordinate);
    }
    Ok(())
}

fn lookup_node(node_index: &[(i64, NodeCoord)], id: i64) -> Option<NodeCoord> {
    node_index
        .binary_search_by_key(&id, |(node_id, _)| *node_id)
        .ok()
        .map(|index| node_index[index].1)
}

fn load_tag_mapping(path: &PathBuf) -> Result<TagMapping, Box<dyn std::error::Error>> {
    let xml = fs::read_to_string(path)?;
    let document = Document::parse(&xml)?;
    let default_zoom_appear = document
        .root_element()
        .attribute("default-zoom-appear")
        .unwrap_or("16")
        .parse::<u8>()?;
    let mut mapping = TagMapping::default();

    for section in document
        .descendants()
        .filter(|node| node.has_tag_name("pois") || node.has_tag_name("ways"))
    {
        let is_poi = section.has_tag_name("pois");
        for osm_tag in section
            .children()
            .filter(|node| node.has_tag_name("osm-tag"))
        {
            let Some(key) = osm_tag.attribute("key") else {
                continue;
            };
            let Some(value) = osm_tag.attribute("value") else {
                continue;
            };
            let zoom_appear = osm_tag
                .attribute("zoom-appear")
                .unwrap_or("")
                .parse::<u8>()
                .unwrap_or(default_zoom_appear);
            let renderable = osm_tag
                .attribute("renderable")
                .map(|value| value == "true")
                .unwrap_or(true);
            let force_polygon_line = osm_tag
                .attribute("force-polygon-line")
                .map(|value| value == "true")
                .unwrap_or(false);
            let tag_info = add_tag_mapping(
                &mut mapping,
                is_poi,
                key,
                value,
                zoom_appear,
                renderable,
                force_polygon_line,
            )?;
            if let Some(equivalent_values) = osm_tag.attribute("equivalent-values") {
                for equivalent_value in equivalent_values.split(',') {
                    let equivalent_value = equivalent_value.trim();
                    if !equivalent_value.is_empty() {
                        add_equivalent_tag_mapping(
                            &mut mapping,
                            is_poi,
                            key,
                            equivalent_value,
                            tag_info,
                        );
                    }
                }
            }
        }
    }

    add_wildcard_alternatives(&mut mapping, true)?;
    add_wildcard_alternatives(&mut mapping, false)?;

    Ok(mapping)
}

fn add_wildcard_alternatives(
    mapping: &mut TagMapping,
    is_poi: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let numeric_wildcards = if is_poi {
        mapping
            .poi_defs
            .iter()
            .enumerate()
            .filter(|(_, tag)| tag.value == "%f")
            .map(|(id, tag)| {
                let info = mapping.poi_wildcards.iter().find_map(|(_, _, info)| {
                    if info.id == id as u16 {
                        Some(*info)
                    } else {
                        None
                    }
                });
                (tag.key.clone(), info)
            })
            .collect::<Vec<_>>()
    } else {
        mapping
            .way_defs
            .iter()
            .enumerate()
            .filter(|(_, tag)| tag.value == "%f")
            .map(|(id, tag)| {
                let info = mapping.way_wildcards.iter().find_map(|(_, _, info)| {
                    if info.id == id as u16 {
                        Some(*info)
                    } else {
                        None
                    }
                });
                (tag.key.clone(), info)
            })
            .collect::<Vec<_>>()
    };

    for (key, info) in numeric_wildcards {
        let info = info.ok_or_else(|| format!("missing wildcard tag info for {key}=%f"))?;
        for value in ["%b", "%h", "%i"] {
            if !mapping_has_tag_def(mapping, is_poi, &key, value) {
                add_tag_mapping(
                    mapping,
                    is_poi,
                    &key,
                    value,
                    info.zoom_appear,
                    info.renderable,
                    info.force_polygon_line,
                )?;
            }
        }
    }
    Ok(())
}

fn mapping_has_tag_def(mapping: &TagMapping, is_poi: bool, key: &str, value: &str) -> bool {
    let defs = if is_poi {
        &mapping.poi_defs
    } else {
        &mapping.way_defs
    };
    defs.iter().any(|tag| tag.key == key && tag.value == value)
}

fn add_tag_mapping(
    mapping: &mut TagMapping,
    is_poi: bool,
    key: &str,
    value: &str,
    zoom_appear: u8,
    renderable: bool,
    force_polygon_line: bool,
) -> Result<TagInfo, Box<dyn std::error::Error>> {
    let defs = if is_poi {
        &mut mapping.poi_defs
    } else {
        &mut mapping.way_defs
    };
    let id = u16::try_from(defs.len()).map_err(|_| {
        format!(
            "too many {} tags in tag mapping",
            if is_poi { "POI" } else { "way" }
        )
    })?;
    let info = TagInfo {
        id,
        zoom_appear,
        renderable,
        force_polygon_line,
    };
    defs.push(TagDef {
        key: key.to_string(),
        value: value.to_string(),
    });

    if value.starts_with('%') {
        if is_poi {
            mapping
                .poi_wildcards
                .push((key.to_string(), value.to_string(), info));
        } else {
            mapping
                .way_wildcards
                .push((key.to_string(), value.to_string(), info));
        }
        return Ok(info);
    }

    if is_poi {
        mapping.poi_tags.insert(tag_key(key, value), info);
    } else {
        mapping.way_tags.insert(tag_key(key, value), info);
    }
    Ok(info)
}

fn add_equivalent_tag_mapping(
    mapping: &mut TagMapping,
    is_poi: bool,
    key: &str,
    value: &str,
    info: TagInfo,
) {
    if is_poi {
        mapping.poi_tags.insert(tag_key(key, value), info);
    } else {
        mapping.way_tags.insert(tag_key(key, value), info);
    }
}

fn record_tag_ids(frequencies: &mut HashMap<u16, u64>, tag_ids: &[u16]) {
    for tag_id in tag_ids {
        *frequencies.entry(*tag_id).or_insert(0) += 1;
    }
}

fn optimized_tag_keys(defs: &[TagDef], frequencies: &HashMap<u16, u64>) -> Vec<String> {
    optimized_tag_order(defs, frequencies)
        .into_iter()
        .map(|(_, _, tag_key)| tag_key)
        .collect()
}

fn optimized_tag_id_map(defs: &[TagDef], frequencies: &HashMap<u16, u64>) -> HashMap<u16, u16> {
    optimized_tag_order(defs, frequencies)
        .into_iter()
        .enumerate()
        .map(|(optimized_id, (original_id, _, _))| (original_id, optimized_id as u16))
        .collect()
}

fn optimized_tag_order(
    defs: &[TagDef],
    frequencies: &HashMap<u16, u64>,
) -> Vec<(u16, u64, String)> {
    let mut entries = frequencies
        .iter()
        .filter_map(|(id, frequency)| {
            defs.get(*id as usize)
                .map(|tag| (*id, *frequency, tag.tag_key()))
        })
        .collect::<Vec<_>>();

    entries.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    entries
}

fn is_hex_color(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 7
        && bytes[0] == b'#'
        && bytes[1..].iter().all(|byte| {
            byte.is_ascii_digit() || (b'a'..=b'f').contains(byte) || (b'A'..=b'F').contains(byte)
        })
}

fn css_named_color(value: &str) -> Option<i32> {
    let color: u32 = match value {
        "aliceblue" => 0xfff0f8ff,
        "antiquewhite" => 0xfffaebd7,
        "aqua" => 0xff00ffff,
        "aquamarine" => 0xff7fffd4,
        "azure" => 0xfff0ffff,
        "beige" => 0xfff5f5dc,
        "bisque" => 0xffffe4c4,
        "black" => 0xff000000,
        "blanchedalmond" => 0xffffebcd,
        "blue" => 0xff0000ff,
        "blueviolet" => 0xff8a2be2,
        "brown" => 0xffa52a2a,
        "burlywood" => 0xffdeb887,
        "cadetblue" => 0xff5f9ea0,
        "chartreuse" => 0xff7fff00,
        "chocolate" => 0xffd2691e,
        "coral" => 0xffff7f50,
        "cornflowerblue" => 0xff6495ed,
        "cornsilk" => 0xfffff8dc,
        "crimson" => 0xffdc143c,
        "cyan" => 0xff00ffff,
        "darkblue" => 0xff00008b,
        "darkcyan" => 0xff008b8b,
        "darkgoldenrod" => 0xffb8860b,
        "darkgray" => 0xffa9a9a9,
        "darkgreen" => 0xff006400,
        "darkgrey" => 0xffa9a9a9,
        "darkkhaki" => 0xffbdb76b,
        "darkmagenta" => 0xff8b008b,
        "darkolivegreen" => 0xff556b2f,
        "darkorange" => 0xffff8c00,
        "darkorchid" => 0xff9932cc,
        "darkred" => 0xff8b0000,
        "darksalmon" => 0xffe9967a,
        "darkseagreen" => 0xff8fbc8f,
        "darkslateblue" => 0xff483d8b,
        "darkslategray" => 0xff2f4f4f,
        "darkslategrey" => 0xff2f4f4f,
        "darkturquoise" => 0xff00ced1,
        "darkviolet" => 0xff9400d3,
        "deeppink" => 0xffff1493,
        "deepskyblue" => 0xff00bfff,
        "dimgray" => 0xff696969,
        "dimgrey" => 0xff696969,
        "dodgerblue" => 0xff1e90ff,
        "firebrick" => 0xffb22222,
        "floralwhite" => 0xfffffaf0,
        "forestgreen" => 0xff228b22,
        "fuchsia" => 0xffff00ff,
        "gainsboro" => 0xffdcdcdc,
        "ghostwhite" => 0xfff8f8ff,
        "gold" => 0xffffd700,
        "goldenrod" => 0xffdaa520,
        "gray" => 0xff808080,
        "green" => 0xff008000,
        "greenyellow" => 0xffadff2f,
        "grey" => 0xff808080,
        "honeydew" => 0xfff0fff0,
        "hotpink" => 0xffff69b4,
        "indianred" => 0xffcd5c5c,
        "indigo" => 0xff4b0082,
        "ivory" => 0xfffffff0,
        "khaki" => 0xfff0e68c,
        "lavender" => 0xffe6e6fa,
        "lavenderblush" => 0xfffff0f5,
        "lawngreen" => 0xff7cfc00,
        "lemonchiffon" => 0xfffffacd,
        "lightblue" => 0xffadd8e6,
        "lightcoral" => 0xfff08080,
        "lightcyan" => 0xffe0ffff,
        "lightgoldenrodyellow" => 0xfffafad2,
        "lightgray" => 0xffd3d3d3,
        "lightgreen" => 0xff90ee90,
        "lightgrey" => 0xffd3d3d3,
        "lightpink" => 0xffffb6c1,
        "lightsalmon" => 0xffffa07a,
        "lightseagreen" => 0xff20b2aa,
        "lightskyblue" => 0xff87cefa,
        "lightslategray" => 0xff778899,
        "lightslategrey" => 0xff778899,
        "lightsteelblue" => 0xffb0c4de,
        "lightyellow" => 0xffffffe0,
        "lime" => 0xff00ff00,
        "limegreen" => 0xff32cd32,
        "linen" => 0xfffaf0e6,
        "magenta" => 0xffff00ff,
        "maroon" => 0xff800000,
        "mediumaquamarine" => 0xff66cdaa,
        "mediumblue" => 0xff0000cd,
        "mediumorchid" => 0xffba55d3,
        "mediumpurple" => 0xff9370db,
        "mediumseagreen" => 0xff3cb371,
        "mediumslateblue" => 0xff7b68ee,
        "mediumspringgreen" => 0xff00fa9a,
        "mediumturquoise" => 0xff48d1cc,
        "mediumvioletred" => 0xffc71585,
        "midnightblue" => 0xff191970,
        "mintcream" => 0xfff5fffa,
        "mistyrose" => 0xffffe4e1,
        "moccasin" => 0xffffe4b5,
        "navajowhite" => 0xffffdead,
        "navy" => 0xff000080,
        "oldlace" => 0xfffdf5e6,
        "olive" => 0xff808000,
        "olivedrab" => 0xff6b8e23,
        "orange" => 0xffffa500,
        "orangered" => 0xffff4500,
        "orchid" => 0xffda70d6,
        "palegoldenrod" => 0xffeee8aa,
        "palegreen" => 0xff98fb98,
        "paleturquoise" => 0xffafeeee,
        "palevioletred" => 0xffdb7093,
        "papayawhip" => 0xffffefd5,
        "peachpuff" => 0xffffdab9,
        "peru" => 0xffcd853f,
        "pink" => 0xffffc0cb,
        "plum" => 0xffdda0dd,
        "powderblue" => 0xffb0e0e6,
        "purple" => 0xff800080,
        "red" => 0xffff0000,
        "rosybrown" => 0xffbc8f8f,
        "royalblue" => 0xff4169e1,
        "saddlebrown" => 0xff8b4513,
        "salmon" => 0xfffa8072,
        "sandybrown" => 0xfff4a460,
        "seagreen" => 0xff2e8b57,
        "seashell" => 0xfffff5ee,
        "sienna" => 0xffa0522d,
        "silver" => 0xffc0c0c0,
        "skyblue" => 0xff87ceeb,
        "slateblue" => 0xff6a5acd,
        "slategray" => 0xff708090,
        "slategrey" => 0xff708090,
        "snow" => 0xfffffafa,
        "springgreen" => 0xff00ff7f,
        "steelblue" => 0xff4682b4,
        "tan" => 0xffd2b48c,
        "teal" => 0xff008080,
        "thistle" => 0xffd8bfd8,
        "tomato" => 0xffff6347,
        "turquoise" => 0xff40e0d0,
        "violet" => 0xffee82ee,
        "wheat" => 0xfff5deb3,
        "white" => 0xffffffff,
        "whitesmoke" => 0xfff5f5f5,
        "yellow" => 0xffffff00,
        "yellowgreen" => 0xff9acd32,
        _ => return None,
    };
    Some(color as i32)
}

fn tag_key(key: &str, value: &str) -> String {
    let mut result = String::with_capacity(key.len() + value.len() + 1);
    result.push_str(key);
    result.push('=');
    result.push_str(value);
    result
}

fn bbox_tile_ranges(bbox: BBox, zoom_intervals: &[ZoomInterval]) -> Vec<TileRange> {
    zoom_intervals
        .iter()
        .map(|interval| TileRange {
            left: longitude_to_tile_x(bbox.min_lon, interval.base),
            top: latitude_to_tile_y(bbox.max_lat, interval.base),
            right: longitude_to_tile_x(bbox.max_lon, interval.base),
            bottom: latitude_to_tile_y(bbox.min_lat, interval.base),
        })
        .collect()
}

fn way_tile_range(
    min_lat: f64,
    min_lon: f64,
    max_lat: f64,
    max_lon: f64,
    zoom: u8,
    enlargement_meters: f64,
) -> TileRange {
    let top_left = tile_enlargement(max_lat, enlargement_meters);
    let bottom_right = tile_enlargement(min_lat, enlargement_meters);
    TileRange {
        left: longitude_to_tile_x(min_lon - top_left.1, zoom),
        top: latitude_to_tile_y(max_lat + top_left.0, zoom),
        right: longitude_to_tile_x(max_lon + bottom_right.1, zoom),
        bottom: latitude_to_tile_y(min_lat - bottom_right.0, zoom),
    }
}

fn compute_subtile_mask(
    way: &WriterWay,
    tile_x: i32,
    tile_y: i32,
    zoom: u8,
    enlargement_meters: f64,
) -> u16 {
    let subtile_zoom = zoom + 2;
    let base_x = tile_x * 4;
    let base_y = tile_y * 4;
    let mut mask = 0_u16;
    let mut bit_index = 0;
    for row in 0..4 {
        for column in 0..4 {
            let subtile_bounds = tile_bounds(
                base_x + column,
                base_y + row,
                subtile_zoom,
                enlargement_meters,
            );
            if way_intersects_rect(&way.coords, subtile_bounds, way.area) {
                mask |= 0x8000_u16 >> bit_index;
            }
            bit_index += 1;
        }
    }
    mask
}

fn way_coordinate_blocks_for_tile(
    way: &WriterWay,
    tile_bounds: RectBounds,
    interval: ZoomInterval,
) -> Vec<WayDataBlock> {
    let blocks = if way.area {
        clip_polygon_to_rect(&way.coords, &way.inner_coords, tile_bounds)
    } else {
        clip_polyline_to_rect(&way.coords, tile_bounds)
            .into_iter()
            .map(|outer| WayDataBlock {
                outer,
                inners: Vec::new(),
            })
            .collect()
    };
    simplify_coordinate_blocks(blocks, way.area, interval)
}

fn simplify_coordinate_blocks(
    blocks: Vec<WayDataBlock>,
    closed: bool,
    interval: ZoomInterval,
) -> Vec<WayDataBlock> {
    if DEFAULT_SIMPLIFICATION_FACTOR <= 0.0 || interval.base > DEFAULT_SIMPLIFICATION_MAX_ZOOM {
        return blocks;
    }

    blocks
        .into_iter()
        .map(|block| {
            let block_closed = closed && is_valid_ring_block(&block.outer);
            simplify_coordinate_block(block, block_closed, interval.max)
        })
        .filter(|block| {
            if closed && !block.inners.is_empty() {
                is_valid_ring_block(&block.outer)
            } else if closed && is_closed_way(&block.outer) {
                is_valid_ring_block(&block.outer)
            } else {
                block.outer.len() >= 2
            }
        })
        .collect()
}

fn simplify_coordinate_block(block: WayDataBlock, closed: bool, max_zoom: u8) -> WayDataBlock {
    let outer = simplify_node_coordinate_block(block.outer, closed, max_zoom);
    let inners = if closed {
        block
            .inners
            .into_iter()
            .map(|inner| simplify_node_coordinate_block(inner, true, max_zoom))
            .filter(|inner| is_valid_ring_block(inner))
            .collect()
    } else {
        Vec::new()
    };
    WayDataBlock { outer, inners }
}

fn simplify_node_coordinate_block(
    block: Vec<NodeCoord>,
    closed: bool,
    max_zoom: u8,
) -> Vec<NodeCoord> {
    let lat_max = block
        .iter()
        .map(|coord| coord.lat().abs())
        .fold(0.0_f64, f64::max);
    let epsilon = simplification_delta_lat(DEFAULT_SIMPLIFICATION_FACTOR, lat_max, max_zoom, 256);
    if epsilon <= 0.0 {
        return block;
    }

    if closed {
        let polygon = Polygon::new(line_string_from_node_coords(&block), Vec::new());
        return node_ring_from_geo_exterior(polygon.simplify(epsilon).exterior()).unwrap_or(block);
    }

    let simplified = line_string_from_node_coords(&block).simplify(epsilon);
    let simplified = simplified
        .0
        .iter()
        .map(|coord| NodeCoord::from_degrees(coord.y, coord.x))
        .collect::<Vec<_>>();
    if simplified.len() >= 2 {
        simplified
    } else {
        block
    }
}

fn line_string_from_node_coords(coords: &[NodeCoord]) -> LineString<f64> {
    LineString::from(
        coords
            .iter()
            .map(|coord| Coord {
                x: coord.lon(),
                y: coord.lat(),
            })
            .collect::<Vec<_>>(),
    )
}

#[allow(dead_code)]
fn clip_closed_ring_to_rect(coords: &[NodeCoord], rect: RectBounds) -> Vec<Vec<NodeCoord>> {
    clip_polygon_to_rect(coords, &[], rect)
        .into_iter()
        .map(|block| block.outer)
        .collect()
}

fn clip_polygon_to_rect(
    outer: &[NodeCoord],
    inners: &[Vec<NodeCoord>],
    rect: RectBounds,
) -> Vec<WayDataBlock> {
    if !is_closed_way(outer) {
        return Vec::new();
    }

    let polygon = Polygon::new(
        line_string_from_node_coords(outer),
        inners
            .iter()
            .filter(|inner| is_closed_way(inner))
            .map(|inner| line_string_from_node_coords(inner))
            .collect(),
    );
    let clip_polygon = Rect::new(
        coord! {
            x: rect.min_x,
            y: rect.min_y,
        },
        coord! {
            x: rect.max_x,
            y: rect.max_y,
        },
    )
    .to_polygon();

    let polygon_blocks = polygon
        .intersection(&clip_polygon)
        .into_iter()
        .filter_map(|polygon| {
            let outer = node_ring_from_geo_exterior(polygon.exterior())?;
            let inners = polygon
                .interiors()
                .iter()
                .filter_map(node_ring_from_geo_exterior)
                .collect::<Vec<_>>();
            Some(WayDataBlock { outer, inners })
        })
        .collect::<Vec<_>>();

    if !polygon_blocks.is_empty() {
        return polygon_blocks;
    }

    // JTS returns LineString/MultiLineString for zero-area polygon/tile intersections
    // along shared boundaries, and mapsforge serializes those as way data blocks.
    clip_polyline_to_rect(outer, rect)
        .into_iter()
        .map(|outer| WayDataBlock {
            outer,
            inners: Vec::new(),
        })
        .collect()
}

fn is_valid_ring_block(block: &[NodeCoord]) -> bool {
    block.len() >= 4 && block.first() == block.last() && polygon_area_node_coords(block).abs() != 0
}

fn node_ring_from_geo_exterior(exterior: &LineString<f64>) -> Option<Vec<NodeCoord>> {
    let mut ring = Vec::with_capacity(exterior.0.len());
    for coord in &exterior.0 {
        let node_coord = NodeCoord::from_degrees(coord.y, coord.x);
        if ring.last().copied() != Some(node_coord) {
            ring.push(node_coord);
        }
    }
    if ring.len() < 3 {
        return None;
    }
    if ring.first() != ring.last() {
        ring.push(ring[0]);
    }
    if ring.len() < 4 || polygon_area_node_coords(&ring).abs() == 0_i64 {
        return None;
    }
    Some(ring)
}

fn polygon_area_node_coords(coords: &[NodeCoord]) -> i64 {
    if coords.len() < 4 {
        return 0;
    }
    let mut area = 0_i64;
    for segment in coords.windows(2) {
        area += segment[0].lon_micro as i64 * segment[1].lat_micro as i64
            - segment[1].lon_micro as i64 * segment[0].lat_micro as i64;
    }
    area / 2
}

fn clip_polyline_to_rect(coords: &[NodeCoord], rect: RectBounds) -> Vec<Vec<NodeCoord>> {
    let mut blocks = Vec::new();
    let mut current = Vec::new();
    let mut collapsed_sliver = None;
    let noding_points = repeated_node_coords(coords);

    for segment in coords.windows(2) {
        let Some((start, end)) =
            clip_segment_to_rect(Point::from(segment[0]), Point::from(segment[1]), rect)
        else {
            push_polyline_block(&mut blocks, std::mem::take(&mut current), &noding_points);
            continue;
        };
        let start = NodeCoord::from_degrees(start.y, start.x);
        let end = NodeCoord::from_degrees(end.y, end.x);
        if start == end {
            push_polyline_block(&mut blocks, std::mem::take(&mut current), &noding_points);
            if blocks.is_empty() && collapsed_sliver.is_none() {
                collapsed_sliver = Some(start);
            }
            continue;
        }

        if current.last().copied() == Some(start) {
            current.push(end);
        } else {
            push_polyline_block(&mut blocks, std::mem::take(&mut current), &noding_points);
            current.push(start);
            current.push(end);
        }
    }

    push_polyline_block(&mut blocks, current, &noding_points);
    if blocks.is_empty() {
        if let Some(coord) = collapsed_sliver {
            blocks.push(vec![coord, coord]);
        }
    }
    blocks
}

fn repeated_node_coords(coords: &[NodeCoord]) -> HashSet<NodeCoord> {
    let mut coord_counts: HashMap<NodeCoord, usize> = HashMap::new();
    for coord in coords {
        *coord_counts.entry(*coord).or_default() += 1;
    }
    coord_counts
        .into_iter()
        .filter_map(|(coord, count)| (count > 1).then_some(coord))
        .collect()
}

fn push_polyline_block(
    blocks: &mut Vec<Vec<NodeCoord>>,
    block: Vec<NodeCoord>,
    noding_points: &HashSet<NodeCoord>,
) {
    if block.len() < 2 {
        return;
    }
    blocks.extend(split_polyline_block_at_repeated_points(
        block,
        noding_points,
    ));
}

fn split_polyline_block_at_repeated_points(
    block: Vec<NodeCoord>,
    noding_points: &HashSet<NodeCoord>,
) -> Vec<Vec<NodeCoord>> {
    let mut coord_counts: HashMap<NodeCoord, usize> = HashMap::new();
    for coord in &block {
        *coord_counts.entry(*coord).or_default() += 1;
    }
    let mut result = Vec::new();
    let mut current = Vec::new();
    let block_len = block.len();
    for (index, coord) in block.into_iter().enumerate() {
        let is_repeated_node =
            noding_points.contains(&coord) || coord_counts.get(&coord).copied().unwrap_or(0) > 1;
        if let Some(intersection) = prior_segment_intersection(&current, coord) {
            current.push(intersection);
            result.push(std::mem::take(&mut current));
            current.push(intersection);
            if current.last().copied() != Some(coord) {
                current.push(coord);
            }
        } else if current.last().copied() != Some(coord) {
            current.push(coord);
        }
        if current.len() >= 2 && is_repeated_node && index + 1 < block_len {
            result.push(std::mem::take(&mut current));
            current.push(coord);
        }
    }
    if current.len() >= 2 {
        result.push(current);
    }
    result
}

fn prior_segment_intersection(current: &[NodeCoord], next: NodeCoord) -> Option<NodeCoord> {
    let start = *current.last()?;
    if current.len() < 3 || start == next {
        return None;
    }

    let start_point = Point::from(start);
    let end_point = Point::from(next);
    current
        .windows(2)
        .take(current.len().saturating_sub(2))
        .filter_map(|segment| {
            let intersection = segment_intersection_point(
                Point::from(segment[0]),
                Point::from(segment[1]),
                start_point,
                end_point,
            )?;
            let coord = NodeCoord::from_degrees(intersection.y, intersection.x);
            (coord != start && coord != next)
                .then_some((coord, distance_squared(start_point, intersection)))
        })
        .min_by(|left, right| {
            left.1
                .partial_cmp(&right.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(coord, _)| coord)
}

fn segment_intersection_point(a: Point, b: Point, c: Point, d: Point) -> Option<Point> {
    let r = Point {
        x: b.x - a.x,
        y: b.y - a.y,
    };
    let s = Point {
        x: d.x - c.x,
        y: d.y - c.y,
    };
    let denominator = cross(r, s);
    if almost_zero(denominator) {
        return None;
    }

    let c_minus_a = Point {
        x: c.x - a.x,
        y: c.y - a.y,
    };
    let t = cross(c_minus_a, s) / denominator;
    let u = cross(c_minus_a, r) / denominator;
    if !(-1.0e-12..=1.0 + 1.0e-12).contains(&t) || !(-1.0e-12..=1.0 + 1.0e-12).contains(&u) {
        return None;
    }

    Some(Point {
        x: a.x + t * r.x,
        y: a.y + t * r.y,
    })
}

fn cross(a: Point, b: Point) -> f64 {
    a.x * b.y - a.y * b.x
}

fn distance_squared(a: Point, b: Point) -> f64 {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    dx * dx + dy * dy
}

fn clip_segment_to_rect(a: Point, b: Point, rect: RectBounds) -> Option<(Point, Point)> {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let mut t_min = 0.0;
    let mut t_max = 1.0;

    for (p, q) in [
        (-dx, a.x - rect.min_x),
        (dx, rect.max_x - a.x),
        (-dy, a.y - rect.min_y),
        (dy, rect.max_y - a.y),
    ] {
        if almost_zero(p) {
            if q < 0.0 {
                return None;
            }
            continue;
        }
        let r = q / p;
        if p < 0.0 {
            if r > t_max {
                return None;
            }
            if r > t_min {
                t_min = r;
            }
        } else {
            if r < t_min {
                return None;
            }
            if r < t_max {
                t_max = r;
            }
        }
    }

    Some((
        Point {
            x: a.x + t_min * dx,
            y: a.y + t_min * dy,
        },
        Point {
            x: a.x + t_max * dx,
            y: a.y + t_max * dy,
        },
    ))
}

fn tile_bounds(tile_x: i32, tile_y: i32, zoom: u8, enlargement_meters: f64) -> RectBounds {
    let mut min_y = tile_y_to_latitude(tile_y + 1, zoom);
    let mut max_y = tile_y_to_latitude(tile_y, zoom);
    let mut min_x = tile_x_to_longitude(tile_x, zoom);
    let mut max_x = tile_x_to_longitude(tile_x + 1, zoom);

    if enlargement_meters != 0.0 {
        let (lat_delta, lon_delta) = tile_enlargement(max_y, enlargement_meters);
        min_y -= lat_delta;
        max_y += lat_delta;
        min_x -= lon_delta;
        max_x += lon_delta;
    }

    RectBounds {
        min_x,
        min_y,
        max_x,
        max_y,
    }
}

fn tile_enlargement(latitude: f64, meters: f64) -> (f64, f64) {
    if meters == 0.0 {
        return (0.0, 0.0);
    }
    let lat_degrees =
        (meters * 360.0) / (2.0 * std::f64::consts::PI * EARTH_EQUATORIAL_RADIUS_METERS);
    let lon_degrees = (meters * 360.0)
        / (2.0
            * std::f64::consts::PI
            * EARTH_EQUATORIAL_RADIUS_METERS
            * latitude.to_radians().cos());
    (lat_degrees, lon_degrees)
}

fn simplification_delta_lat(delta_pixel: f64, latitude: f64, zoom: u8, tile_size: u32) -> f64 {
    let map_size = tile_size as f64 * 2_f64.powi(zoom as i32);
    let pixel_y = latitude_to_pixel_y(latitude, map_size);
    let lat2 = pixel_y_to_latitude(pixel_y + delta_pixel, map_size);
    (lat2 - latitude).abs()
}

fn latitude_to_pixel_y(latitude: f64, map_size: f64) -> f64 {
    let sin_latitude = latitude
        .clamp(-MAX_MERCATOR_LATITUDE, MAX_MERCATOR_LATITUDE)
        .to_radians()
        .sin();
    let pixel_y = (0.5
        - ((1.0 + sin_latitude) / (1.0 - sin_latitude)).ln() / (4.0 * std::f64::consts::PI))
        * map_size;
    pixel_y.clamp(0.0, map_size)
}

fn pixel_y_to_latitude(pixel_y: f64, map_size: f64) -> f64 {
    let y = 0.5 - (pixel_y.clamp(0.0, map_size) / map_size);
    90.0 - 360.0 * (y * 2.0 * std::f64::consts::PI).exp().atan() / std::f64::consts::PI
}

fn way_intersects_rect(coords: &[NodeCoord], rect: RectBounds, area: bool) -> bool {
    if coords.is_empty() {
        return false;
    }
    if coords
        .iter()
        .any(|coord| rect.contains(Point::from(*coord)))
    {
        return true;
    }

    for segment in coords.windows(2) {
        if segment_intersects_rect(Point::from(segment[0]), Point::from(segment[1]), rect) {
            return true;
        }
    }

    if area && is_closed_ring(coords) {
        for corner in rect.corners() {
            if point_in_polygon(corner, coords) {
                return true;
            }
        }
    }

    false
}

fn is_closed_ring(coords: &[NodeCoord]) -> bool {
    coords.len() >= 4 && coords.first() == coords.last()
}

fn segment_intersects_rect(a: Point, b: Point, rect: RectBounds) -> bool {
    if rect.contains(a) || rect.contains(b) {
        return true;
    }
    let corners = rect.corners();
    segments_intersect(a, b, corners[0], corners[1])
        || segments_intersect(a, b, corners[1], corners[2])
        || segments_intersect(a, b, corners[2], corners[3])
        || segments_intersect(a, b, corners[3], corners[0])
}

fn segments_intersect(a: Point, b: Point, c: Point, d: Point) -> bool {
    let o1 = orientation(a, b, c);
    let o2 = orientation(a, b, d);
    let o3 = orientation(c, d, a);
    let o4 = orientation(c, d, b);

    if almost_zero(o1) && point_on_segment(c, a, b) {
        return true;
    }
    if almost_zero(o2) && point_on_segment(d, a, b) {
        return true;
    }
    if almost_zero(o3) && point_on_segment(a, c, d) {
        return true;
    }
    if almost_zero(o4) && point_on_segment(b, c, d) {
        return true;
    }

    (o1 > 0.0) != (o2 > 0.0) && (o3 > 0.0) != (o4 > 0.0)
}

fn orientation(a: Point, b: Point, c: Point) -> f64 {
    (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
}

fn point_on_segment(point: Point, a: Point, b: Point) -> bool {
    point.x >= a.x.min(b.x)
        && point.x <= a.x.max(b.x)
        && point.y >= a.y.min(b.y)
        && point.y <= a.y.max(b.y)
}

fn point_in_polygon(point: Point, ring: &[NodeCoord]) -> bool {
    let mut inside = false;
    for segment in ring.windows(2) {
        let a = Point::from(segment[0]);
        let b = Point::from(segment[1]);
        if point_on_segment(point, a, b) && almost_zero(orientation(a, b, point)) {
            return true;
        }
        let crosses_y = (a.y > point.y) != (b.y > point.y);
        if crosses_y {
            let intersect_x = (b.x - a.x) * (point.y - a.y) / (b.y - a.y) + a.x;
            if point.x < intersect_x {
                inside = !inside;
            }
        }
    }
    inside
}

fn almost_zero(value: f64) -> bool {
    value.abs() < 1e-12
}

fn longitude_to_tile_x(longitude: f64, zoom: u8) -> i32 {
    let map_size = 256.0 * 2_f64.powi(zoom as i32);
    let pixel_x = (longitude + 180.0) / 360.0 * map_size;
    pixel_to_tile(pixel_x, zoom)
}

fn tile_x_to_longitude(tile_x: i32, zoom: u8) -> f64 {
    tile_x as f64 / 2_f64.powi(zoom as i32) * 360.0 - 180.0
}

fn tile_y_to_latitude(tile_y: i32, zoom: u8) -> f64 {
    let n = std::f64::consts::PI
        - (2.0 * std::f64::consts::PI * tile_y as f64) / 2_f64.powi(zoom as i32);
    n.sinh().atan().to_degrees()
}

fn latitude_to_tile_y(latitude: f64, zoom: u8) -> i32 {
    let latitude = latitude.clamp(-MAX_MERCATOR_LATITUDE, MAX_MERCATOR_LATITUDE);
    let sin_latitude = latitude.to_radians().sin();
    let map_size = 256.0 * 2_f64.powi(zoom as i32);
    let pixel_y = (0.5
        - ((1.0 + sin_latitude) / (1.0 - sin_latitude)).ln() / (4.0 * std::f64::consts::PI))
        * map_size;
    pixel_to_tile(pixel_y.clamp(0.0, map_size), zoom)
}

fn pixel_to_tile(pixel: f64, zoom: u8) -> i32 {
    let max_tile = (1_i32 << zoom) - 1;
    ((pixel / 256.0).floor() as i32).clamp(0, max_tile)
}

fn print_counts(counts: Counts) {
    println!("nodes={}", counts.nodes);
    println!("ways={}", counts.ways);
    println!("relations={}", counts.relations);
    println!("way_refs={}", counts.way_refs);
    println!("relation_members={}", counts.relation_members);
    println!("tagged_elements={}", counts.tagged_elements);
}

fn format_zoom_intervals(zoom_intervals: &[ZoomInterval]) -> String {
    zoom_intervals
        .iter()
        .flat_map(|interval| [interval.base, interval.min, interval.max])
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn format_encoding(encoding: EncodingChoice) -> &'static str {
    match encoding {
        EncodingChoice::Auto => "auto",
        EncodingChoice::Single => "single",
        EncodingChoice::Double => "double",
    }
}

fn format_writer_type(writer_type: WriterType) -> &'static str {
    match writer_type {
        WriterType::Hd => "hd",
        WriterType::Ram => "ram",
    }
}

fn count_element(element: Element) -> Counts {
    let mut counts = Counts::default();
    match element {
        Element::Node(node) => {
            counts.nodes = 1;
            if node.tags().next().is_some() {
                counts.tagged_elements = 1;
            }
        }
        Element::DenseNode(node) => {
            counts.nodes = 1;
            if node.tags().next().is_some() {
                counts.tagged_elements = 1;
            }
        }
        Element::Way(way) => {
            counts.ways = 1;
            counts.way_refs = way.refs().count() as u64;
            if way.tags().next().is_some() {
                counts.tagged_elements = 1;
            }
        }
        Element::Relation(relation) => {
            counts.relations = 1;
            counts.relation_members = relation.members().count() as u64;
            if relation.tags().next().is_some() {
                counts.tagged_elements = 1;
            }
        }
    }
    counts
}

fn parse_args<I>(args: I) -> Result<Args, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut input = None;
    let mut bbox = None;
    let mut tag_conf_file = None;
    let mut mode = Mode::TileIndex;
    let mut writer = WriterArgs::default();
    let mut positional_input = None;
    let mut iter = args.into_iter();

    while let Some(arg) = iter.next() {
        if arg == "--input" {
            input = Some(PathBuf::from(next_value(&mut iter, "--input")?));
        } else if let Some(value) = arg.strip_prefix("--input=") {
            input = Some(PathBuf::from(value));
        } else if arg == "--bbox" {
            bbox = Some(parse_bbox(&next_value(&mut iter, "--bbox")?)?);
        } else if let Some(value) = arg.strip_prefix("--bbox=") {
            bbox = Some(parse_bbox(value)?);
        } else if arg == "--tag-conf-file" {
            tag_conf_file = Some(PathBuf::from(next_value(&mut iter, "--tag-conf-file")?));
        } else if let Some(value) = arg.strip_prefix("--tag-conf-file=") {
            tag_conf_file = Some(PathBuf::from(value));
        } else if arg == "--mode" {
            mode = parse_mode(&next_value(&mut iter, "--mode")?)?;
        } else if let Some(value) = arg.strip_prefix("--mode=") {
            mode = parse_mode(value)?;
        } else if arg == "--output" {
            writer.output = Some(PathBuf::from(next_value(&mut iter, "--output")?));
        } else if let Some(value) = arg.strip_prefix("--output=") {
            writer.output = Some(PathBuf::from(value));
        } else if arg == "--zoom-interval-conf" {
            writer.zoom_intervals =
                parse_zoom_intervals(&next_value(&mut iter, "--zoom-interval-conf")?)?;
        } else if let Some(value) = arg.strip_prefix("--zoom-interval-conf=") {
            writer.zoom_intervals = parse_zoom_intervals(value)?;
        } else if arg == "--bbox-enlargement" {
            writer.bbox_enlargement_meters =
                parse_non_negative_f64(&next_value(&mut iter, "--bbox-enlargement")?)?;
        } else if let Some(value) = arg.strip_prefix("--bbox-enlargement=") {
            writer.bbox_enlargement_meters = parse_non_negative_f64(value)?;
        } else if arg == "--encoding" {
            writer.encoding = parse_encoding(&next_value(&mut iter, "--encoding")?)?;
        } else if let Some(value) = arg.strip_prefix("--encoding=") {
            writer.encoding = parse_encoding(value)?;
        } else if arg == "--tag-values" {
            writer.tag_values = parse_bool(&next_value(&mut iter, "--tag-values")?, "tag-values")?;
        } else if let Some(value) = arg.strip_prefix("--tag-values=") {
            writer.tag_values = parse_bool(value, "tag-values")?;
        } else if arg == "--preferred-languages" {
            writer.preferred_languages =
                parse_preferred_languages(&next_value(&mut iter, "--preferred-languages")?);
        } else if let Some(value) = arg.strip_prefix("--preferred-languages=") {
            writer.preferred_languages = parse_preferred_languages(value);
        } else if arg == "--map-start-position" {
            writer.map_start_position = Some(parse_map_start_position(&next_value(
                &mut iter,
                "--map-start-position",
            )?)?);
        } else if let Some(value) = arg.strip_prefix("--map-start-position=") {
            writer.map_start_position = Some(parse_map_start_position(value)?);
        } else if arg == "--map-start-zoom" {
            writer.map_start_zoom = Some(parse_zoom(&next_value(&mut iter, "--map-start-zoom")?)?);
        } else if let Some(value) = arg.strip_prefix("--map-start-zoom=") {
            writer.map_start_zoom = Some(parse_zoom(value)?);
        } else if arg == "--comment" {
            writer.comment = Some(next_value(&mut iter, "--comment")?);
        } else if let Some(value) = arg.strip_prefix("--comment=") {
            writer.comment = Some(value.to_string());
        } else if arg == "--type" {
            writer.writer_type = parse_writer_type(&next_value(&mut iter, "--type")?)?;
        } else if let Some(value) = arg.strip_prefix("--type=") {
            writer.writer_type = parse_writer_type(value)?;
        } else if arg == "--progress-logs" {
            writer.progress_logs =
                parse_bool(&next_value(&mut iter, "--progress-logs")?, "progress-logs")?;
        } else if let Some(value) = arg.strip_prefix("--progress-logs=") {
            writer.progress_logs = parse_bool(value, "progress-logs")?;
        } else if arg == "--debug-file" {
            writer.debug_file = parse_bool(&next_value(&mut iter, "--debug-file")?, "debug-file")?;
        } else if let Some(value) = arg.strip_prefix("--debug-file=") {
            writer.debug_file = parse_bool(value, "debug-file")?;
        } else if arg == "--label-position" {
            reject_unsupported_true(
                "label-position",
                &next_value(&mut iter, "--label-position")?,
            )?;
        } else if let Some(value) = arg.strip_prefix("--label-position=") {
            reject_unsupported_true("label-position", value)?;
        } else if arg == "--polylabel" {
            reject_unsupported_true("polylabel", &next_value(&mut iter, "--polylabel")?)?;
        } else if let Some(value) = arg.strip_prefix("--polylabel=") {
            reject_unsupported_true("polylabel", value)?;
        } else if arg == "-h" || arg == "--help" {
            return Err(usage().into());
        } else if arg.starts_with('-') {
            return Err(format!("unknown argument: {arg}\n{}", usage()).into());
        } else if positional_input.is_none() {
            positional_input = Some(PathBuf::from(arg));
        } else {
            return Err(format!("unexpected positional argument: {arg}\n{}", usage()).into());
        }
    }

    let input = input
        .or(positional_input)
        .ok_or_else(|| format!("missing --input <osm.pbf>\n{}", usage()))?;
    let tag_conf_file = tag_conf_file.unwrap_or_else(default_tag_conf_file);

    Ok(Args {
        input,
        bbox,
        tag_conf_file,
        mode,
        writer,
    })
}

fn next_value<I>(iter: &mut I, flag: &str) -> Result<String, Box<dyn std::error::Error>>
where
    I: Iterator<Item = String>,
{
    iter.next()
        .ok_or_else(|| format!("missing value for {flag}").into())
}

fn parse_bbox(value: &str) -> Result<BBox, Box<dyn std::error::Error>> {
    let parts: Vec<f64> = value
        .split(',')
        .map(str::trim)
        .map(str::parse::<f64>)
        .collect::<Result<Vec<_>, _>>()?;
    if parts.len() != 4 {
        return Err("bbox must be minLat,minLon,maxLat,maxLon".into());
    }
    if parts[0] > parts[2] || parts[1] > parts[3] {
        return Err("bbox min values must be less than or equal to max values".into());
    }
    Ok(BBox {
        min_lat: parts[0],
        min_lon: parts[1],
        max_lat: parts[2],
        max_lon: parts[3],
    })
}

fn parse_mode(value: &str) -> Result<Mode, Box<dyn std::error::Error>> {
    match value {
        "count" => Ok(Mode::Count),
        "tile-index" => Ok(Mode::TileIndex),
        _ => Err(format!("unsupported --mode {value}; expected count or tile-index").into()),
    }
}

fn parse_zoom_intervals(value: &str) -> Result<Vec<ZoomInterval>, Box<dyn std::error::Error>> {
    let values = value
        .split(',')
        .map(str::trim)
        .map(str::parse::<u8>)
        .collect::<Result<Vec<_>, _>>()?;
    if values.is_empty() || values.len() % 3 != 0 {
        return Err("--zoom-interval-conf must contain base,min,max triples".into());
    }

    let mut intervals = Vec::with_capacity(values.len() / 3);
    for triple in values.chunks_exact(3) {
        let interval = ZoomInterval {
            base: triple[0],
            min: triple[1],
            max: triple[2],
        };
        if interval.min > interval.base || interval.base > interval.max {
            return Err("--zoom-interval-conf triples must satisfy min <= base <= max".into());
        }
        intervals.push(interval);
    }
    Ok(intervals)
}

fn parse_non_negative_f64(value: &str) -> Result<f64, Box<dyn std::error::Error>> {
    let parsed = value.parse::<f64>()?;
    if !parsed.is_finite() || parsed < 0.0 {
        return Err("--bbox-enlargement must be a non-negative finite number".into());
    }
    Ok(parsed)
}

fn parse_encoding(value: &str) -> Result<EncodingChoice, Box<dyn std::error::Error>> {
    match value {
        "auto" => Ok(EncodingChoice::Auto),
        "single" => Ok(EncodingChoice::Single),
        "double" => Ok(EncodingChoice::Double),
        _ => {
            Err(format!("unsupported --encoding {value}; expected auto, single, or double").into())
        }
    }
}

fn parse_bool(value: &str, name: &str) -> Result<bool, Box<dyn std::error::Error>> {
    match value {
        "true" | "yes" | "1" => Ok(true),
        "false" | "no" | "0" => Ok(false),
        _ => Err(format!("argument --{name} must be true or false").into()),
    }
}

fn parse_preferred_languages(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|language| !language.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_map_start_position(value: &str) -> Result<MapStartPosition, Box<dyn std::error::Error>> {
    let parts = value
        .split(',')
        .map(str::trim)
        .map(str::parse::<f64>)
        .collect::<Result<Vec<_>, _>>()?;
    if parts.len() != 2 {
        return Err("--map-start-position must be lat,lon".into());
    }
    Ok(MapStartPosition {
        lat: parts[0],
        lon: parts[1],
    })
}

fn parse_zoom(value: &str) -> Result<u8, Box<dyn std::error::Error>> {
    let zoom = value.parse::<u8>()?;
    if zoom > 22 {
        return Err("zoom must be between 0 and 22".into());
    }
    Ok(zoom)
}

fn parse_writer_type(value: &str) -> Result<WriterType, Box<dyn std::error::Error>> {
    match value {
        "hd" => Ok(WriterType::Hd),
        "ram" => Ok(WriterType::Ram),
        _ => Err(format!("unsupported --type {value}; expected hd or ram").into()),
    }
}

fn reject_unsupported_true(name: &str, value: &str) -> Result<(), Box<dyn std::error::Error>> {
    if parse_bool(value, name)? {
        return Err(format!("--{name} true is not supported by the Rust writer v1").into());
    }
    Ok(())
}

fn default_tag_conf_file() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../src/main/config/tag-mapping.xml")
}

fn usage() -> &'static str {
    "usage: mapsforge-writer-rust-spike --input <input.osm.pbf> [--output <output.map>] [--bbox minLat,minLon,maxLat,maxLon] [--tag-conf-file <tag-mapping.xml>] [--mode count|tile-index] [writer options]"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binary::{
        var_int, var_uint, write_header, BinaryEncoder, HeaderOptions, SubfileMetadata,
    };

    fn parse(values: &[&str]) -> Result<Args, Box<dyn std::error::Error>> {
        parse_args(values.iter().map(|value| value.to_string()))
    }

    #[test]
    fn disk_node_index_reads_blocks_and_cleans_up() {
        let path = env::temp_dir().join(format!("mapsforge-node-index-test-{}.tmp", process::id()));
        let mut builder =
            DiskNodeIndexBuilder::create(path.clone()).expect("disk node index should be created");
        builder
            .push(
                10,
                NodeCoord {
                    lat_micro: 100,
                    lon_micro: 200,
                },
            )
            .expect("first node should be written");
        builder
            .push(
                20,
                NodeCoord {
                    lat_micro: 300,
                    lon_micro: 400,
                },
            )
            .expect("second node should be written");
        builder
            .push(
                30,
                NodeCoord {
                    lat_micro: 500,
                    lon_micro: 600,
                },
            )
            .expect("third node should be written");

        let mut lookup = NodeLookupIndex::Disk(
            builder
                .finish()
                .expect("disk node index should be readable"),
        );
        assert_eq!(
            fs::metadata(&path)
                .expect("temporary node index should have metadata")
                .len(),
            encode_disk_node_block(&[
                NodeIndexRecord {
                    id_delta: 0,
                    coord: NodeCoord {
                        lat_micro: 100,
                        lon_micro: 200,
                    },
                },
                NodeIndexRecord {
                    id_delta: 10,
                    coord: NodeCoord {
                        lat_micro: 300,
                        lon_micro: 400,
                    },
                },
                NodeIndexRecord {
                    id_delta: 20,
                    coord: NodeCoord {
                        lat_micro: 500,
                        lon_micro: 600,
                    },
                },
            ])
            .len() as u64
        );
        assert_eq!(
            lookup.lookup(20).expect("lookup should not fail"),
            Some(NodeCoord {
                lat_micro: 300,
                lon_micro: 400,
            })
        );
        assert_eq!(lookup.lookup(25).expect("lookup should not fail"), None);
        assert_eq!(
            lookup.lookup(10).expect("lookup should not fail"),
            Some(NodeCoord {
                lat_micro: 100,
                lon_micro: 200,
            })
        );
        assert!(lookup
            .progress_detail()
            .expect("disk lookup should report cache stats")
            .contains("node_index_cache_"));
        lookup.cleanup().expect("temporary node index is removable");
        assert!(!path.exists());
    }

    #[test]
    fn disk_node_index_splits_blocks_when_id_delta_exceeds_u32() {
        let path = env::temp_dir().join(format!(
            "mapsforge-node-index-delta-test-{}.tmp",
            process::id()
        ));
        let mut builder =
            DiskNodeIndexBuilder::create(path.clone()).expect("disk node index should be created");
        builder
            .push(
                1,
                NodeCoord {
                    lat_micro: 100,
                    lon_micro: 200,
                },
            )
            .expect("first node should be written");
        let distant_id = i64::from(u32::MAX) + 2;
        builder
            .push(
                distant_id,
                NodeCoord {
                    lat_micro: 300,
                    lon_micro: 400,
                },
            )
            .expect("distant node should start a new block");

        let mut lookup = NodeLookupIndex::Disk(
            builder
                .finish()
                .expect("disk node index should be readable"),
        );
        assert_eq!(
            lookup.lookup(distant_id).expect("lookup should not fail"),
            Some(NodeCoord {
                lat_micro: 300,
                lon_micro: 400,
            })
        );
        lookup.cleanup().expect("temporary node index is removable");
        assert!(!path.exists());
    }

    #[test]
    fn disk_node_index_rejects_unsorted_ids() {
        let path = env::temp_dir().join(format!(
            "mapsforge-node-index-unsorted-test-{}.tmp",
            process::id()
        ));
        let mut builder =
            DiskNodeIndexBuilder::create(path.clone()).expect("disk node index should be created");
        builder
            .push(
                20,
                NodeCoord {
                    lat_micro: 300,
                    lon_micro: 400,
                },
            )
            .expect("first node should be written");
        assert!(builder
            .push(
                10,
                NodeCoord {
                    lat_micro: 100,
                    lon_micro: 200,
                },
            )
            .is_err());
        drop(builder);
        fs::remove_file(path).expect("temporary unsorted index should be removable");
    }

    #[test]
    fn parses_writer_cli_options() {
        let args = parse(&[
            "--input",
            "in.osm.pbf",
            "--output",
            "out.map",
            "--bbox",
            "61.0,26.0,62.0,28.0",
            "--zoom-interval-conf",
            "5,0,7,10,8,11",
            "--bbox-enlargement",
            "40",
            "--encoding",
            "double",
            "--tag-values",
            "true",
            "--preferred-languages",
            "fi,en",
            "--map-start-position",
            "61.5,27.5",
            "--map-start-zoom",
            "12",
            "--comment",
            "test map",
            "--debug-file",
            "true",
            "--type",
            "ram",
            "--progress-logs",
            "false",
        ])
        .expect("writer args should parse");

        assert_eq!(args.input, PathBuf::from("in.osm.pbf"));
        assert_eq!(args.writer.output, Some(PathBuf::from("out.map")));
        assert_eq!(
            args.writer.zoom_intervals,
            vec![
                ZoomInterval {
                    base: 5,
                    min: 0,
                    max: 7,
                },
                ZoomInterval {
                    base: 10,
                    min: 8,
                    max: 11,
                },
            ]
        );
        assert_eq!(args.writer.bbox_enlargement_meters, 40.0);
        assert_eq!(args.writer.encoding, EncodingChoice::Double);
        assert!(args.writer.tag_values);
        assert_eq!(args.writer.preferred_languages, vec!["fi", "en"]);
        assert_eq!(
            args.writer.map_start_position,
            Some(MapStartPosition {
                lat: 61.5,
                lon: 27.5,
            })
        );
        assert_eq!(args.writer.map_start_zoom, Some(12));
        assert_eq!(args.writer.comment.as_deref(), Some("test map"));
        assert!(args.writer.debug_file);
        assert_eq!(args.writer.writer_type, WriterType::Ram);
        assert!(!args.writer.progress_logs);
    }

    #[test]
    fn accepts_hd_and_ram_as_compatibility_aliases() {
        let hd = parse(&["--input", "in.osm.pbf", "--type", "hd"]).expect("hd should parse");
        let ram = parse(&["--input", "in.osm.pbf", "--type", "ram"]).expect("ram should parse");

        assert_eq!(hd.writer.writer_type, WriterType::Hd);
        assert_eq!(ram.writer.writer_type, WriterType::Ram);
    }

    #[test]
    fn rejects_unsupported_true_options() {
        for option in ["--label-position", "--polylabel"] {
            let error = parse(&["--input", "in.osm.pbf", option, "true"])
                .expect_err("true value should be rejected")
                .to_string();
            assert!(
                error.contains("not supported"),
                "{option} produced unexpected error: {error}"
            );
        }
    }

    #[test]
    fn accepts_debug_file_and_unsupported_options_when_explicitly_false() {
        let args = parse(&[
            "--input",
            "in.osm.pbf",
            "--debug-file",
            "false",
            "--label-position=false",
            "--polylabel",
            "0",
        ])
        .expect("false values should preserve Java-compatible invocation shape");

        assert_eq!(args.input, PathBuf::from("in.osm.pbf"));
        assert!(!args.writer.debug_file);
    }

    #[test]
    fn validates_zoom_interval_triples() {
        let error = parse(&["--input", "in.osm.pbf", "--zoom-interval-conf", "5,0,7,10"])
            .expect_err("incomplete triples should fail")
            .to_string();

        assert!(error.contains("base,min,max triples"));
    }

    #[test]
    fn writes_fixed_width_big_endian_numbers() {
        let mut encoder = BinaryEncoder::new();
        encoder.write_u16(0x1234);
        encoder.write_i16(-2);
        encoder.write_u32(0x1234_5678);
        encoder.write_i32(-2);
        encoder.write_u64(0x0102_0304_0506_0708);
        encoder.write_i64(-2);

        assert_eq!(
            encoder.into_bytes(),
            vec![
                0x12, 0x34, 0xff, 0xfe, 0x12, 0x34, 0x56, 0x78, 0xff, 0xff, 0xff, 0xfe, 0x01, 0x02,
                0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe,
            ]
        );
    }

    #[test]
    fn writes_mapsforge_five_byte_offsets() {
        let mut encoder = BinaryEncoder::new();
        encoder.write_five_byte_offset(0).expect("zero fits");
        encoder.write_five_byte_offset(5).expect("small value fits");
        encoder
            .write_five_byte_offset((1_u64 << 40) - 1)
            .expect("maximum five-byte value fits");

        assert_eq!(
            encoder.into_bytes(),
            vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 5, 0xff, 0xff, 0xff, 0xff, 0xff,]
        );
    }

    #[test]
    fn rejects_five_byte_offsets_outside_mapsforge_range() {
        let mut encoder = BinaryEncoder::new();
        let error = encoder
            .write_five_byte_offset(1_u64 << 40)
            .expect_err("value no longer fits into five bytes");

        assert!(error.contains("out of range"));
    }

    #[test]
    fn encodes_unsigned_variable_bytes_like_java_serializer() {
        assert_eq!(var_uint(0), vec![0x00]);
        assert_eq!(var_uint(127), vec![0x7f]);
        assert_eq!(var_uint(128), vec![0x80, 0x01]);
        assert_eq!(var_uint(16_383), vec![0xff, 0x7f]);
        assert_eq!(var_uint(16_384), vec![0x80, 0x80, 0x01]);
        assert_eq!(var_uint(u32::MAX), vec![0xff, 0xff, 0xff, 0xff, 0x0f]);
    }

    #[test]
    fn encodes_signed_variable_bytes_like_java_serializer() {
        assert_eq!(var_int(0), vec![0x00]);
        assert_eq!(var_int(63), vec![0x3f]);
        assert_eq!(var_int(-63), vec![0x7f]);
        assert_eq!(var_int(64), vec![0xc0, 0x00]);
        assert_eq!(var_int(-64), vec![0xc0, 0x40]);
        assert_eq!(var_int(8_191), vec![0xff, 0x3f]);
        assert_eq!(var_int(-8_191), vec![0xff, 0x7f]);
        assert_eq!(var_int(8_192), vec![0x80, 0xc0, 0x00]);
        assert_eq!(var_int(i32::MAX), vec![0xff, 0xff, 0xff, 0xff, 0x07]);
        assert_eq!(var_int(i32::MIN), vec![0x80, 0x80, 0x80, 0x80, 0x48]);
    }

    #[test]
    fn writes_utf8_strings_with_variable_byte_length_prefix() {
        let mut encoder = BinaryEncoder::new();
        encoder.write_utf8("cafe").expect("ASCII string fits");
        encoder.write_utf8("järvi").expect("UTF-8 string fits");

        assert_eq!(
            encoder.into_bytes(),
            vec![0x04, b'c', b'a', b'f', b'e', 0x06, b'j', 0xc3, 0xa4, b'r', b'v', b'i',]
        );
    }

    #[test]
    fn writes_mapsforge_header_with_patched_header_size_and_placeholders() {
        let poi_tags = vec!["amenity=cafe".to_string()];
        let way_tags = vec!["highway=primary".to_string()];
        let preferred_languages = vec!["fi".to_string(), "en".to_string()];
        let zoom_intervals = vec![
            ZoomInterval {
                base: 5,
                min: 0,
                max: 7,
            },
            ZoomInterval {
                base: 10,
                min: 8,
                max: 11,
            },
        ];
        let subfiles = vec![
            SubfileMetadata {
                start_address: 200,
                size: 30,
            },
            SubfileMetadata {
                start_address: 230,
                size: 40,
            },
        ];

        let header = write_header(HeaderOptions {
            bbox: BBox {
                min_lat: 61.0,
                min_lon: 26.0,
                max_lat: 62.0,
                max_lon: 28.0,
            },
            file_size: 12_345,
            creation_date_millis: 1_700_000_000_000,
            zoom_intervals: &zoom_intervals,
            subfiles: &subfiles,
            poi_tags: &poi_tags,
            way_tags: &way_tags,
            map_start_position: Some(MapStartPosition {
                lat: 61.5,
                lon: 27.5,
            }),
            map_start_zoom: Some(12),
            preferred_languages: &preferred_languages,
            comment: Some("test"),
            debug_file: false,
            created_with: "mapsforge-writer-rust-spike",
        })
        .expect("header should encode");

        assert_eq!(&header[0..20], b"mapsforge binary OSM");
        assert_eq!(read_i32(&header, 20), (header.len() - 24) as i32);
        assert_eq!(read_u32(&header, 24), 5);
        assert_eq!(read_u64(&header, 28), 12_345);
        assert_eq!(read_u64(&header, 36), 1_700_000_000_000);
        assert_eq!(read_i32(&header, 44), 61_000_000);
        assert_eq!(read_i32(&header, 48), 26_000_000);
        assert_eq!(read_i32(&header, 52), 62_000_000);
        assert_eq!(read_i32(&header, 56), 28_000_000);
        assert_eq!(read_u16(&header, 60), 256);
        assert_eq!(header[62], 8);
        assert_eq!(&header[63..71], b"Mercator");
        assert_eq!(header[71], 0x04 | 0x08 | 0x10 | 0x20 | 0x40);
        assert_eq!(read_i32(&header, 72), 61_500_000);
        assert_eq!(read_i32(&header, 76), 27_500_000);
        assert_eq!(header[80], 12);

        let tail = &header[81..];
        assert!(tail.windows("fi,en".len()).any(|window| window == b"fi,en"));
        assert!(tail
            .windows("mapsforge-writer-rust-spike".len())
            .any(|window| window == b"mapsforge-writer-rust-spike"));
        assert_eq!(&header[header.len() - 38..header.len() - 35], &[5, 0, 7]);
        assert_eq!(read_u64(&header, header.len() - 35), 200);
        assert_eq!(read_u64(&header, header.len() - 27), 30);
        assert_eq!(&header[header.len() - 19..header.len() - 16], &[10, 8, 11]);
        assert_eq!(read_u64(&header, header.len() - 16), 230);
        assert_eq!(read_u64(&header, header.len() - 8), 40);
    }

    #[test]
    fn writes_empty_subfile_with_index_entries_and_empty_tile_payloads() {
        let interval = ZoomInterval {
            base: 5,
            min: 0,
            max: 1,
        };
        let output = env::temp_dir().join(format!(
            "mapsforge-empty-subfile-test-{}.map",
            process::id()
        ));
        let staged_path = env::temp_dir().join(format!(
            "mapsforge-empty-subfile-staged-test-{}.tmp",
            process::id()
        ));
        let staged_index_path = temp_staged_way_index_path(&staged_path);
        File::create(&staged_path).expect("empty staged way file should be creatable");
        File::create(&staged_index_path).expect("empty staged way index file should be creatable");
        let staged_ways = StagedWayStore {
            path: staged_path,
            index_path: staged_index_path,
            count: 0,
        };
        let subfile = write_subfile_to_temp_file(
            &output,
            0,
            interval,
            TileRange {
                left: 0,
                top: 0,
                right: 1,
                bottom: 0,
            },
            &HashMap::new(),
            &[],
            &HashMap::new(),
            &staged_ways,
            &[],
            &HashMap::new(),
            &HashMap::new(),
            EncodingChoice::Single,
            0.0,
            false,
            None,
        )
        .expect("empty subfile should encode");
        let mut subfile_bytes = subfile.index.clone();
        subfile_bytes.extend_from_slice(
            &fs::read(&subfile.tile_data_path).expect("temporary tile data should be readable"),
        );
        fs::remove_file(&subfile.tile_data_path).expect("temporary tile data should be removable");
        fs::remove_file(&staged_ways.path).expect("empty staged way file should be removable");
        fs::remove_file(&staged_ways.index_path)
            .expect("empty staged way index file should be removable");

        assert_eq!(subfile.size, 20);
        assert_eq!(subfile_bytes.len(), 20);
        assert_eq!(&subfile_bytes[0..5], &[0, 0, 0, 0, 10]);
        assert_eq!(&subfile_bytes[5..10], &[0, 0, 0, 0, 15]);
        assert_eq!(&subfile_bytes[10..15], &[0, 0, 0, 0, 0]);
        assert_eq!(&subfile_bytes[15..20], &[0, 0, 0, 0, 0]);
    }

    #[test]
    fn writes_poi_payload_with_offsets_tags_and_special_fields() {
        let poi = WriterPoi {
            id: 1,
            coord: NodeCoord {
                lat_micro: 0,
                lon_micro: 0,
            },
            min_zoom: 0,
            tag_ids: vec![3],
            tag_values: Vec::new(),
            special: SpecialTags {
                name: Some("Cafe".to_string()),
                housenumber: Some("1".to_string()),
                elevation: 10,
                layer: 5,
                ..SpecialTags::default()
            },
        };
        let optimized_ids = HashMap::from([(3, 7)]);

        assert_eq!(
            write_poi(&poi, 0, 0, &optimized_ids).expect("POI should serialize"),
            vec![0, 0, 0x51, 7, 0xe0, 4, b'C', b'a', b'f', b'e', 1, b'1', 10]
        );
    }

    #[test]
    fn writes_simple_way_payload_with_single_delta_coordinates() {
        let way = WriterWay {
            id: 1,
            min_zoom: 0,
            area: false,
            coords: vec![
                NodeCoord {
                    lat_micro: 0,
                    lon_micro: 0,
                },
                NodeCoord {
                    lat_micro: 100,
                    lon_micro: 100,
                },
            ],
            inner_coords: Vec::new(),
            min_lat: 0.0,
            min_lon: 0.0,
            max_lat: 0.0001,
            max_lon: 0.0001,
            tag_ids: vec![2],
            tag_values: Vec::new(),
            special: SpecialTags {
                name: Some("Road".to_string()),
                ref_value: Some("A1".to_string()),
                layer: 5,
                ..SpecialTags::default()
            },
        };
        let optimized_ids = HashMap::from([(2, 4)]);

        assert_eq!(
            write_way(
                &way,
                0,
                0,
                &optimized_ids,
                EncodingChoice::Single,
                0xf000,
                &[WayDataBlock {
                    outer: way.coords.clone(),
                    inners: Vec::new()
                }]
            )
            .expect("way should serialize"),
            vec![
                0xf0, 0x00, 0x51, 4, 0xa0, 4, b'R', b'o', b'a', b'd', 2, b'A', b'1', 1, 2, 0, 0,
                0xe4, 0, 0xe4, 0,
            ]
        );
    }

    #[test]
    fn computes_subtile_mask_row_wise_like_java_writer() {
        let way = WriterWay {
            id: 1,
            min_zoom: 0,
            area: false,
            coords: vec![
                NodeCoord::from_degrees(80.0, -170.0),
                NodeCoord::from_degrees(80.0, 170.0),
            ],
            inner_coords: Vec::new(),
            min_lat: 80.0,
            min_lon: -170.0,
            max_lat: 80.0,
            max_lon: 170.0,
            tag_ids: Vec::new(),
            tag_values: Vec::new(),
            special: SpecialTags::default(),
        };

        assert_eq!(compute_subtile_mask(&way, 0, 0, 0, 0.0), 0xf000);
    }

    #[test]
    fn computes_subtile_mask_for_single_subtile_way() {
        let way = WriterWay {
            id: 1,
            min_zoom: 0,
            area: false,
            coords: vec![
                NodeCoord::from_degrees(80.0, -170.0),
                NodeCoord::from_degrees(70.0, -160.0),
            ],
            inner_coords: Vec::new(),
            min_lat: 70.0,
            min_lon: -170.0,
            max_lat: 80.0,
            max_lon: -160.0,
            tag_ids: Vec::new(),
            tag_values: Vec::new(),
            special: SpecialTags::default(),
        };

        assert_eq!(compute_subtile_mask(&way, 0, 0, 0, 0.0), 0x8000);
    }

    #[test]
    fn clips_open_polyline_to_multiple_tile_coordinate_blocks() {
        let coords = vec![
            NodeCoord::from_degrees(5.0, -5.0),
            NodeCoord::from_degrees(5.0, 5.0),
            NodeCoord::from_degrees(5.0, 15.0),
            NodeCoord::from_degrees(6.0, 15.0),
            NodeCoord::from_degrees(6.0, 5.0),
            NodeCoord::from_degrees(6.0, -5.0),
        ];

        let blocks = clip_polyline_to_rect(
            &coords,
            RectBounds {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 10.0,
                max_y: 10.0,
            },
        );

        assert_eq!(
            blocks,
            vec![
                vec![
                    NodeCoord::from_degrees(5.0, 0.0),
                    NodeCoord::from_degrees(5.0, 5.0),
                    NodeCoord::from_degrees(5.0, 10.0),
                ],
                vec![
                    NodeCoord::from_degrees(6.0, 10.0),
                    NodeCoord::from_degrees(6.0, 5.0),
                    NodeCoord::from_degrees(6.0, 0.0),
                ],
            ]
        );
    }

    #[test]
    fn preserves_microdegree_collapsed_clipped_line_sliver() {
        let coords = vec![
            NodeCoord::from_degrees(0.0, 1.000001),
            NodeCoord::from_degrees(0.0, 1.000000),
        ];

        let blocks = clip_polyline_to_rect(
            &coords,
            RectBounds {
                min_x: 0.0,
                min_y: -1.0,
                max_x: 1.0000004,
                max_y: 1.0,
            },
        );

        assert_eq!(
            blocks,
            vec![vec![
                NodeCoord::from_degrees(0.0, 1.000000),
                NodeCoord::from_degrees(0.0, 1.000000),
            ]]
        );
    }

    #[test]
    fn splits_open_polyline_blocks_at_repeated_points_like_jts_noding() {
        let coords = vec![
            NodeCoord::from_degrees(5.0, 1.0),
            NodeCoord::from_degrees(5.0, 5.0),
            NodeCoord::from_degrees(6.0, 5.0),
            NodeCoord::from_degrees(5.0, 5.0),
            NodeCoord::from_degrees(5.0, 9.0),
        ];

        let blocks = clip_polyline_to_rect(
            &coords,
            RectBounds {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 10.0,
                max_y: 10.0,
            },
        );

        assert_eq!(
            blocks,
            vec![
                vec![
                    NodeCoord::from_degrees(5.0, 1.0),
                    NodeCoord::from_degrees(5.0, 5.0),
                ],
                vec![
                    NodeCoord::from_degrees(5.0, 5.0),
                    NodeCoord::from_degrees(6.0, 5.0),
                    NodeCoord::from_degrees(5.0, 5.0),
                ],
                vec![
                    NodeCoord::from_degrees(5.0, 5.0),
                    NodeCoord::from_degrees(5.0, 9.0),
                ],
            ]
        );
    }

    #[test]
    fn splits_open_polyline_blocks_at_self_intersections_like_jts_noding() {
        let coords = vec![
            NodeCoord::from_degrees(1.0, 1.0),
            NodeCoord::from_degrees(9.0, 9.0),
            NodeCoord::from_degrees(9.0, 1.0),
            NodeCoord::from_degrees(1.0, 9.0),
        ];

        let blocks = clip_polyline_to_rect(
            &coords,
            RectBounds {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 10.0,
                max_y: 10.0,
            },
        );

        assert_eq!(
            blocks,
            vec![
                vec![
                    NodeCoord::from_degrees(1.0, 1.0),
                    NodeCoord::from_degrees(9.0, 9.0),
                    NodeCoord::from_degrees(9.0, 1.0),
                    NodeCoord::from_degrees(5.0, 5.0),
                ],
                vec![
                    NodeCoord::from_degrees(5.0, 5.0),
                    NodeCoord::from_degrees(1.0, 9.0),
                ],
            ]
        );
    }

    #[test]
    fn simplification_uses_java_default_zoom_gate() {
        let blocks = vec![WayDataBlock {
            outer: vec![
                NodeCoord::from_degrees(0.0, 0.0),
                NodeCoord::from_degrees(0.000001, 0.000001),
                NodeCoord::from_degrees(0.000002, 0.000002),
                NodeCoord::from_degrees(0.01, 0.01),
            ],
            inners: Vec::new(),
        }];

        let simplified = simplify_coordinate_blocks(
            blocks.clone(),
            false,
            ZoomInterval {
                base: 10,
                min: 8,
                max: 11,
            },
        );
        let unchanged = simplify_coordinate_blocks(
            blocks.clone(),
            false,
            ZoomInterval {
                base: 14,
                min: 12,
                max: 21,
            },
        );

        assert!(simplified[0].outer.len() < blocks[0].outer.len());
        assert_eq!(unchanged, blocks);
    }

    #[test]
    fn simplification_preserves_valid_closed_ring() {
        let ring = vec![
            NodeCoord::from_degrees(0.0, 0.0),
            NodeCoord::from_degrees(0.0, 0.000001),
            NodeCoord::from_degrees(0.0, 0.01),
            NodeCoord::from_degrees(0.01, 0.01),
            NodeCoord::from_degrees(0.01, 0.0),
            NodeCoord::from_degrees(0.0, 0.0),
        ];

        let simplified = simplify_coordinate_block(
            WayDataBlock {
                outer: ring,
                inners: Vec::new(),
            },
            true,
            11,
        );

        assert!(simplified.outer.len() >= 4);
        assert_eq!(simplified.outer.first(), simplified.outer.last());
        assert_ne!(polygon_area_node_coords(&simplified.outer), 0);
    }

    #[test]
    fn clips_closed_ring_to_tile_rectangle() {
        let coords = vec![
            NodeCoord::from_degrees(5.0, -5.0),
            NodeCoord::from_degrees(5.0, 5.0),
            NodeCoord::from_degrees(15.0, 5.0),
            NodeCoord::from_degrees(15.0, -5.0),
            NodeCoord::from_degrees(5.0, -5.0),
        ];

        let clipped_blocks = clip_closed_ring_to_rect(
            &coords,
            RectBounds {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 10.0,
                max_y: 10.0,
            },
        );

        assert_eq!(clipped_blocks.len(), 1);
        let clipped = &clipped_blocks[0];
        assert_eq!(clipped.first(), clipped.last());
        assert!(clipped.contains(&NodeCoord::from_degrees(5.0, 0.0)));
        assert!(clipped.contains(&NodeCoord::from_degrees(5.0, 5.0)));
        assert!(clipped.contains(&NodeCoord::from_degrees(10.0, 5.0)));
        assert!(clipped.contains(&NodeCoord::from_degrees(10.0, 0.0)));
    }

    #[test]
    fn clips_polygon_that_contains_entire_tile() {
        let coords = vec![
            NodeCoord::from_degrees(-5.0, -5.0),
            NodeCoord::from_degrees(-5.0, 15.0),
            NodeCoord::from_degrees(15.0, 15.0),
            NodeCoord::from_degrees(15.0, -5.0),
            NodeCoord::from_degrees(-5.0, -5.0),
        ];

        let clipped_blocks = clip_closed_ring_to_rect(
            &coords,
            RectBounds {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 10.0,
                max_y: 10.0,
            },
        );

        assert_eq!(clipped_blocks.len(), 1);
        let clipped = &clipped_blocks[0];
        assert_eq!(clipped.first(), clipped.last());
        assert_eq!(clipped.len(), 5);
        assert!(clipped.contains(&NodeCoord::from_degrees(0.0, 0.0)));
        assert!(clipped.contains(&NodeCoord::from_degrees(0.0, 10.0)));
        assert!(clipped.contains(&NodeCoord::from_degrees(10.0, 10.0)));
        assert!(clipped.contains(&NodeCoord::from_degrees(10.0, 0.0)));
    }

    #[test]
    fn clips_closed_ring_to_disconnected_tile_blocks() {
        let coords = vec![
            NodeCoord::from_degrees(-5.0, -5.0),
            NodeCoord::from_degrees(4.0, -5.0),
            NodeCoord::from_degrees(4.0, 4.0),
            NodeCoord::from_degrees(2.0, 4.0),
            NodeCoord::from_degrees(2.0, 2.0),
            NodeCoord::from_degrees(-5.0, 2.0),
            NodeCoord::from_degrees(-5.0, 8.0),
            NodeCoord::from_degrees(2.0, 8.0),
            NodeCoord::from_degrees(2.0, 6.0),
            NodeCoord::from_degrees(4.0, 6.0),
            NodeCoord::from_degrees(15.0, -5.0),
            NodeCoord::from_degrees(15.0, 15.0),
            NodeCoord::from_degrees(4.0, 15.0),
            NodeCoord::from_degrees(-5.0, -5.0),
        ];

        let clipped_blocks = clip_closed_ring_to_rect(
            &coords,
            RectBounds {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 10.0,
                max_y: 10.0,
            },
        );

        assert!(clipped_blocks.len() > 1);
        assert!(clipped_blocks
            .iter()
            .all(|block| block.len() >= 4 && block.first() == block.last()));
    }

    #[test]
    fn clips_polygon_with_inner_ring_to_way_data_block() {
        let outer = vec![
            NodeCoord::from_degrees(-1.0, -1.0),
            NodeCoord::from_degrees(-1.0, 11.0),
            NodeCoord::from_degrees(11.0, 11.0),
            NodeCoord::from_degrees(11.0, -1.0),
            NodeCoord::from_degrees(-1.0, -1.0),
        ];
        let inner = vec![
            NodeCoord::from_degrees(4.0, 4.0),
            NodeCoord::from_degrees(4.0, 6.0),
            NodeCoord::from_degrees(6.0, 6.0),
            NodeCoord::from_degrees(6.0, 4.0),
            NodeCoord::from_degrees(4.0, 4.0),
        ];

        let blocks = clip_polygon_to_rect(
            &outer,
            &[inner],
            RectBounds {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 10.0,
                max_y: 10.0,
            },
        );

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].inners.len(), 1);
        assert!(is_valid_ring_block(&blocks[0].outer));
        assert!(is_valid_ring_block(&blocks[0].inners[0]));
    }

    #[test]
    fn clips_partial_inner_ring_into_outer_boundary() {
        let outer = vec![
            NodeCoord::from_degrees(-1.0, -1.0),
            NodeCoord::from_degrees(-1.0, 11.0),
            NodeCoord::from_degrees(11.0, 11.0),
            NodeCoord::from_degrees(11.0, -1.0),
            NodeCoord::from_degrees(-1.0, -1.0),
        ];
        let inner = vec![
            NodeCoord::from_degrees(4.0, 8.0),
            NodeCoord::from_degrees(4.0, 12.0),
            NodeCoord::from_degrees(6.0, 12.0),
            NodeCoord::from_degrees(6.0, 8.0),
            NodeCoord::from_degrees(4.0, 8.0),
        ];

        let blocks = clip_polygon_to_rect(
            &outer,
            &[inner],
            RectBounds {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 10.0,
                max_y: 10.0,
            },
        );

        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].inners.is_empty());
        assert!(is_valid_ring_block(&blocks[0].outer));
        let clipped_area = polygon_area_node_coords(&blocks[0].outer).abs();
        let full_tile_area = polygon_area_node_coords(&[
            NodeCoord::from_degrees(0.0, 0.0),
            NodeCoord::from_degrees(0.0, 10.0),
            NodeCoord::from_degrees(10.0, 10.0),
            NodeCoord::from_degrees(10.0, 0.0),
            NodeCoord::from_degrees(0.0, 0.0),
        ])
        .abs();
        assert!(clipped_area < full_tile_area);
    }

    #[test]
    fn drops_closed_ring_when_clip_degenerates() {
        let coords = vec![
            NodeCoord::from_degrees(20.0, 20.0),
            NodeCoord::from_degrees(20.0, 21.0),
            NodeCoord::from_degrees(21.0, 21.0),
            NodeCoord::from_degrees(21.0, 20.0),
            NodeCoord::from_degrees(20.0, 20.0),
        ];

        assert_eq!(
            clip_closed_ring_to_rect(
                &coords,
                RectBounds {
                    min_x: 0.0,
                    min_y: 0.0,
                    max_x: 10.0,
                    max_y: 10.0,
                },
            ),
            Vec::<Vec<NodeCoord>>::new()
        );
    }

    #[test]
    fn preserves_polygon_boundary_line_clip_like_java_jts() {
        let coords = vec![
            NodeCoord::from_degrees(0.0, -1.0),
            NodeCoord::from_degrees(0.0, 11.0),
            NodeCoord::from_degrees(-1.0, 11.0),
            NodeCoord::from_degrees(-1.0, -1.0),
            NodeCoord::from_degrees(0.0, -1.0),
        ];

        let blocks = clip_polygon_to_rect(
            &coords,
            &[],
            RectBounds {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 10.0,
                max_y: 10.0,
            },
        );

        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].inners.is_empty());
        assert_eq!(
            blocks[0].outer,
            vec![
                NodeCoord::from_degrees(0.0, 0.0),
                NodeCoord::from_degrees(0.0, 10.0),
            ]
        );
    }

    #[test]
    fn simplification_keeps_area_boundary_line_blocks() {
        let blocks = vec![WayDataBlock {
            outer: vec![
                NodeCoord::from_degrees(0.0, 0.0),
                NodeCoord::from_degrees(0.0, 10.0),
            ],
            inners: Vec::new(),
        }];

        let simplified = simplify_coordinate_blocks(
            blocks.clone(),
            true,
            ZoomInterval {
                base: 10,
                min: 8,
                max: 11,
            },
        );

        assert_eq!(simplified, blocks);
    }

    #[test]
    fn attaches_closed_inner_members_to_single_outer_multipolygon() {
        let outer_coords = vec![
            NodeCoord::from_degrees(0.0, 0.0),
            NodeCoord::from_degrees(0.0, 10.0),
            NodeCoord::from_degrees(10.0, 10.0),
            NodeCoord::from_degrees(10.0, 0.0),
            NodeCoord::from_degrees(0.0, 0.0),
        ];
        let inner_coords = vec![
            NodeCoord::from_degrees(4.0, 4.0),
            NodeCoord::from_degrees(4.0, 6.0),
            NodeCoord::from_degrees(6.0, 6.0),
            NodeCoord::from_degrees(6.0, 4.0),
            NodeCoord::from_degrees(4.0, 4.0),
        ];
        let mut ways = vec![WriterWay {
            id: 10,
            min_zoom: 0,
            area: true,
            coords: outer_coords.clone(),
            inner_coords: Vec::new(),
            min_lat: 0.0,
            min_lon: 0.0,
            max_lat: 10.0,
            max_lon: 10.0,
            tag_ids: Vec::new(),
            tag_values: Vec::new(),
            special: SpecialTags::default(),
        }];
        let relations = vec![MultipolygonRelationInfo {
            id: 1,
            members: vec![
                RelationMemberRef { way_id: 10 },
                RelationMemberRef { way_id: 20 },
            ],
            tag_ids: Vec::new(),
            tag_values: Vec::new(),
            min_renderable_zoom: None,
            force_polygon_line: false,
            special: SpecialTags::default(),
        }];
        let geometries = HashMap::from([
            (
                10,
                RelationWayGeometry {
                    coords: outer_coords,
                },
            ),
            (
                20,
                RelationWayGeometry {
                    coords: inner_coords.clone(),
                },
            ),
        ]);
        let mut stats = TileIndexStats::default();
        let mut frequencies = TagFrequencies::default();

        attach_supported_multipolygon_relations(
            &mut ways,
            &relations,
            &geometries,
            &mut frequencies,
            &mut stats,
            None,
        );

        assert_eq!(ways[0].inner_coords, vec![inner_coords]);
        assert_eq!(stats.simple_multipolygon_relations_with_inner_rings, 1);
        assert_eq!(stats.multipolygon_inner_rings_attached, 1);
        assert_eq!(stats.unsupported_multipolygon_relations, 0);
    }

    #[test]
    fn stitches_open_inner_members_into_single_hole() {
        let outer_coords = vec![
            NodeCoord::from_degrees(0.0, 0.0),
            NodeCoord::from_degrees(0.0, 10.0),
            NodeCoord::from_degrees(10.0, 10.0),
            NodeCoord::from_degrees(10.0, 0.0),
            NodeCoord::from_degrees(0.0, 0.0),
        ];
        let inner_a = vec![
            NodeCoord::from_degrees(4.0, 4.0),
            NodeCoord::from_degrees(4.0, 6.0),
        ];
        let inner_b = vec![
            NodeCoord::from_degrees(6.0, 6.0),
            NodeCoord::from_degrees(4.0, 6.0),
        ];
        let inner_c = vec![
            NodeCoord::from_degrees(6.0, 6.0),
            NodeCoord::from_degrees(6.0, 4.0),
        ];
        let inner_d = vec![
            NodeCoord::from_degrees(4.0, 4.0),
            NodeCoord::from_degrees(6.0, 4.0),
        ];
        let mut ways = vec![WriterWay {
            id: 10,
            min_zoom: 0,
            area: true,
            coords: outer_coords.clone(),
            inner_coords: Vec::new(),
            min_lat: 0.0,
            min_lon: 0.0,
            max_lat: 10.0,
            max_lon: 10.0,
            tag_ids: Vec::new(),
            tag_values: Vec::new(),
            special: SpecialTags::default(),
        }];
        let relations = vec![MultipolygonRelationInfo {
            id: 1,
            members: vec![
                RelationMemberRef { way_id: 10 },
                RelationMemberRef { way_id: 20 },
                RelationMemberRef { way_id: 30 },
                RelationMemberRef { way_id: 40 },
                RelationMemberRef { way_id: 50 },
            ],
            tag_ids: Vec::new(),
            tag_values: Vec::new(),
            min_renderable_zoom: None,
            force_polygon_line: false,
            special: SpecialTags::default(),
        }];
        let geometries = HashMap::from([
            (
                10,
                RelationWayGeometry {
                    coords: outer_coords,
                },
            ),
            (20, RelationWayGeometry { coords: inner_a }),
            (30, RelationWayGeometry { coords: inner_b }),
            (40, RelationWayGeometry { coords: inner_c }),
            (50, RelationWayGeometry { coords: inner_d }),
        ]);
        let mut stats = TileIndexStats::default();
        let mut frequencies = TagFrequencies::default();

        attach_supported_multipolygon_relations(
            &mut ways,
            &relations,
            &geometries,
            &mut frequencies,
            &mut stats,
            None,
        );

        assert_eq!(ways.len(), 1);
        assert_eq!(ways[0].inner_coords.len(), 1);
        assert!(is_valid_ring_block(&ways[0].inner_coords[0]));
        assert_eq!(stats.simple_multipolygon_relations_with_inner_rings, 1);
        assert_eq!(stats.multipolygon_inner_rings_attached, 1);
        assert_eq!(stats.unsupported_multipolygon_relations, 0);
    }

    #[test]
    fn stitches_open_outer_members_into_virtual_multipolygon_way() {
        let segment_a = vec![
            NodeCoord::from_degrees(0.0, 0.0),
            NodeCoord::from_degrees(0.0, 10.0),
        ];
        let segment_b = vec![
            NodeCoord::from_degrees(10.0, 10.0),
            NodeCoord::from_degrees(0.0, 10.0),
        ];
        let segment_c = vec![
            NodeCoord::from_degrees(10.0, 10.0),
            NodeCoord::from_degrees(10.0, 0.0),
            NodeCoord::from_degrees(0.0, 0.0),
        ];
        let relations = vec![MultipolygonRelationInfo {
            id: 1,
            members: vec![
                RelationMemberRef { way_id: 10 },
                RelationMemberRef { way_id: 20 },
                RelationMemberRef { way_id: 30 },
            ],
            tag_ids: vec![7],
            tag_values: vec![None],
            min_renderable_zoom: Some(12),
            force_polygon_line: false,
            special: SpecialTags {
                name: Some("Forest".to_string()),
                layer: 5,
                ..SpecialTags::default()
            },
        }];
        let geometries = HashMap::from([
            (10, RelationWayGeometry { coords: segment_a }),
            (20, RelationWayGeometry { coords: segment_b }),
            (30, RelationWayGeometry { coords: segment_c }),
        ]);
        let mut ways = Vec::new();
        let mut frequencies = TagFrequencies::default();
        let mut stats = TileIndexStats::default();

        attach_supported_multipolygon_relations(
            &mut ways,
            &relations,
            &geometries,
            &mut frequencies,
            &mut stats,
            None,
        );

        assert_eq!(ways.len(), 1);
        assert!(ways[0].area);
        assert!(is_valid_ring_block(&ways[0].coords));
        assert_eq!(ways[0].tag_ids, vec![7]);
        assert_eq!(ways[0].special.name.as_deref(), Some("Forest"));
        assert_eq!(frequencies.way.get(&7), Some(&1));
        assert_eq!(stats.unsupported_multipolygon_relations, 0);
    }

    #[test]
    fn polygonizes_multiple_outer_rings_from_one_relation() {
        let island_a = vec![
            NodeCoord::from_degrees(0.0, 0.0),
            NodeCoord::from_degrees(0.0, 1.0),
            NodeCoord::from_degrees(1.0, 1.0),
            NodeCoord::from_degrees(1.0, 0.0),
            NodeCoord::from_degrees(0.0, 0.0),
        ];
        let island_b = vec![
            NodeCoord::from_degrees(2.0, 2.0),
            NodeCoord::from_degrees(2.0, 3.0),
            NodeCoord::from_degrees(3.0, 3.0),
            NodeCoord::from_degrees(3.0, 2.0),
            NodeCoord::from_degrees(2.0, 2.0),
        ];
        let relations = vec![MultipolygonRelationInfo {
            id: 1,
            members: vec![
                RelationMemberRef { way_id: 10 },
                RelationMemberRef { way_id: 20 },
            ],
            tag_ids: vec![7],
            tag_values: vec![None],
            min_renderable_zoom: Some(12),
            force_polygon_line: false,
            special: SpecialTags::default(),
        }];
        let geometries = HashMap::from([
            (
                10,
                RelationWayGeometry {
                    coords: island_a.clone(),
                },
            ),
            (
                20,
                RelationWayGeometry {
                    coords: island_b.clone(),
                },
            ),
        ]);
        let mut ways = Vec::new();
        let mut frequencies = TagFrequencies::default();
        let mut stats = TileIndexStats::default();

        attach_supported_multipolygon_relations(
            &mut ways,
            &relations,
            &geometries,
            &mut frequencies,
            &mut stats,
            None,
        );

        assert_eq!(ways.len(), 2);
        assert!(ways.iter().all(|way| way.area));
        assert!(ways.iter().all(|way| way.inner_coords.is_empty()));
        assert!(ways.iter().any(|way| way.coords == island_a));
        assert!(ways.iter().any(|way| way.coords == island_b));
        assert_eq!(frequencies.way.get(&7), Some(&2));
        assert_eq!(stats.simple_multipolygon_relations_with_inner_rings, 2);
        assert_eq!(stats.unsupported_multipolygon_relations, 0);
    }

    #[test]
    fn relates_member_rings_by_containment_even_when_roles_are_wrong() {
        let outer = vec![
            NodeCoord::from_degrees(0.0, 0.0),
            NodeCoord::from_degrees(0.0, 10.0),
            NodeCoord::from_degrees(10.0, 10.0),
            NodeCoord::from_degrees(10.0, 0.0),
            NodeCoord::from_degrees(0.0, 0.0),
        ];
        let inner = vec![
            NodeCoord::from_degrees(4.0, 4.0),
            NodeCoord::from_degrees(4.0, 6.0),
            NodeCoord::from_degrees(6.0, 6.0),
            NodeCoord::from_degrees(6.0, 4.0),
            NodeCoord::from_degrees(4.0, 4.0),
        ];
        let relations = vec![MultipolygonRelationInfo {
            id: 1,
            members: vec![
                RelationMemberRef { way_id: 10 },
                RelationMemberRef { way_id: 20 },
            ],
            tag_ids: vec![7],
            tag_values: vec![None],
            min_renderable_zoom: Some(12),
            force_polygon_line: false,
            special: SpecialTags::default(),
        }];
        let geometries = HashMap::from([
            (
                10,
                RelationWayGeometry {
                    coords: outer.clone(),
                },
            ),
            (
                20,
                RelationWayGeometry {
                    coords: inner.clone(),
                },
            ),
        ]);
        let mut ways = Vec::new();
        let mut frequencies = TagFrequencies::default();
        let mut stats = TileIndexStats::default();

        attach_supported_multipolygon_relations(
            &mut ways,
            &relations,
            &geometries,
            &mut frequencies,
            &mut stats,
            None,
        );

        assert_eq!(ways.len(), 1);
        assert_eq!(ways[0].coords, outer);
        assert_eq!(ways[0].inner_coords, vec![inner]);
        assert_eq!(stats.unsupported_multipolygon_relations, 0);
    }

    #[test]
    fn ring_covers_coord_handles_inside_outside_and_boundary_points() {
        let ring = vec![
            NodeCoord::from_degrees(0.0, 0.0),
            NodeCoord::from_degrees(0.0, 10.0),
            NodeCoord::from_degrees(10.0, 10.0),
            NodeCoord::from_degrees(10.0, 0.0),
            NodeCoord::from_degrees(0.0, 0.0),
        ];

        assert!(ring_covers_coord(&ring, NodeCoord::from_degrees(5.0, 5.0)));
        assert!(ring_covers_coord(&ring, NodeCoord::from_degrees(0.0, 5.0)));
        assert!(!ring_covers_coord(
            &ring,
            NodeCoord::from_degrees(11.0, 5.0)
        ));
    }

    #[test]
    fn unsupported_multipolygon_geometry_is_reported_without_aborting() {
        let relations = vec![MultipolygonRelationInfo {
            id: 1,
            members: vec![
                RelationMemberRef { way_id: 10 },
                RelationMemberRef { way_id: 20 },
            ],
            tag_ids: vec![7],
            tag_values: vec![None],
            min_renderable_zoom: Some(12),
            force_polygon_line: false,
            special: SpecialTags::default(),
        }];
        let geometries = HashMap::from([
            (
                10,
                RelationWayGeometry {
                    coords: vec![
                        NodeCoord::from_degrees(0.0, 0.0),
                        NodeCoord::from_degrees(0.0, 1.0),
                    ],
                },
            ),
            (
                20,
                RelationWayGeometry {
                    coords: vec![
                        NodeCoord::from_degrees(2.0, 2.0),
                        NodeCoord::from_degrees(2.0, 3.0),
                    ],
                },
            ),
        ]);
        let mut ways = Vec::new();
        let mut frequencies = TagFrequencies::default();
        let mut stats = TileIndexStats::default();

        attach_supported_multipolygon_relations(
            &mut ways,
            &relations,
            &geometries,
            &mut frequencies,
            &mut stats,
            None,
        );

        let summary =
            unsupported_multipolygon_summary(&stats).expect("unsupported geometry is reported");

        assert!(ways.is_empty());
        assert_eq!(stats.unsupported_multipolygon_relations, 1);
        assert_eq!(stats.unsupported_multipolygon_no_valid_rings, 1);
        assert!(summary.contains("no_valid_rings=1"));
    }

    #[test]
    fn partial_multipolygon_geometry_keeps_valid_rings() {
        let valid_ring = vec![
            NodeCoord::from_degrees(0.0, 0.0),
            NodeCoord::from_degrees(0.0, 1.0),
            NodeCoord::from_degrees(1.0, 1.0),
            NodeCoord::from_degrees(1.0, 0.0),
            NodeCoord::from_degrees(0.0, 0.0),
        ];
        let relations = vec![MultipolygonRelationInfo {
            id: 1,
            members: vec![
                RelationMemberRef { way_id: 10 },
                RelationMemberRef { way_id: 20 },
            ],
            tag_ids: vec![7],
            tag_values: vec![None],
            min_renderable_zoom: Some(12),
            force_polygon_line: false,
            special: SpecialTags::default(),
        }];
        let geometries = HashMap::from([
            (
                10,
                RelationWayGeometry {
                    coords: vec![
                        NodeCoord::from_degrees(2.0, 2.0),
                        NodeCoord::from_degrees(2.0, 3.0),
                    ],
                },
            ),
            (
                20,
                RelationWayGeometry {
                    coords: valid_ring.clone(),
                },
            ),
        ]);
        let mut ways = Vec::new();
        let mut frequencies = TagFrequencies::default();
        let mut stats = TileIndexStats::default();

        attach_supported_multipolygon_relations(
            &mut ways,
            &relations,
            &geometries,
            &mut frequencies,
            &mut stats,
            None,
        );

        assert_eq!(ways.len(), 1);
        assert_eq!(ways[0].coords, valid_ring);
        assert_eq!(frequencies.way.get(&7), Some(&1));
        assert_eq!(stats.partial_multipolygon_relations, 1);
        assert_eq!(stats.unsupported_multipolygon_relations, 0);
        assert!(unsupported_multipolygon_summary(&stats).is_none());
    }

    #[test]
    fn closed_non_area_way_is_clipped_as_polyline() {
        let coords = vec![
            NodeCoord::from_degrees(-5.0, -5.0),
            NodeCoord::from_degrees(-5.0, 15.0),
            NodeCoord::from_degrees(15.0, 15.0),
            NodeCoord::from_degrees(15.0, -5.0),
            NodeCoord::from_degrees(-5.0, -5.0),
        ];
        let mut way = WriterWay {
            id: 1,
            min_zoom: 0,
            area: false,
            coords,
            inner_coords: Vec::new(),
            min_lat: -5.0,
            min_lon: -5.0,
            max_lat: 15.0,
            max_lon: 15.0,
            tag_ids: Vec::new(),
            tag_values: Vec::new(),
            special: SpecialTags::default(),
        };
        let tile_bounds = RectBounds {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 10.0,
            max_y: 10.0,
        };
        let interval = ZoomInterval {
            base: 14,
            min: 12,
            max: 21,
        };

        assert!(way_coordinate_blocks_for_tile(&way, tile_bounds, interval).is_empty());

        way.area = true;
        assert!(!way_coordinate_blocks_for_tile(&way, tile_bounds, interval).is_empty());
    }

    #[test]
    fn detects_area_tags_like_java_writer() {
        assert!(is_area_tags(&[("building".to_string(), "yes".to_string())]));
        assert!(!is_area_tags(&[(
            "highway".to_string(),
            "residential".to_string()
        )]));
        assert!(!is_area_tags(&[("area".to_string(), "no".to_string())]));
        assert!(!is_area_tags(&[(
            "railway".to_string(),
            "rail".to_string()
        )]));
        assert!(is_area_tags(&[(
            "boundary".to_string(),
            "administrative".to_string()
        )]));
    }

    #[test]
    fn writes_multiple_way_coordinate_blocks() {
        let way = WriterWay {
            id: 1,
            min_zoom: 0,
            area: false,
            coords: Vec::new(),
            inner_coords: Vec::new(),
            min_lat: 0.0,
            min_lon: 0.0,
            max_lat: 0.0,
            max_lon: 0.0,
            tag_ids: Vec::new(),
            tag_values: Vec::new(),
            special: SpecialTags {
                layer: 5,
                ..SpecialTags::default()
            },
        };
        let blocks = vec![
            WayDataBlock {
                outer: vec![
                    NodeCoord::from_degrees(0.0, 0.0),
                    NodeCoord::from_degrees(0.0, 1.0),
                ],
                inners: Vec::new(),
            },
            WayDataBlock {
                outer: vec![
                    NodeCoord::from_degrees(1.0, 0.0),
                    NodeCoord::from_degrees(1.0, 1.0),
                ],
                inners: Vec::new(),
            },
        ];

        let bytes = write_way(
            &way,
            0,
            0,
            &HashMap::new(),
            EncodingChoice::Single,
            0xffff,
            &blocks,
        )
        .expect("multi-block way should serialize");

        assert_eq!(
            &bytes[0..4],
            &[0xff, 0xff, 0x50, FEATURE_MULTIPLE_WAY_BLOCKS]
        );
        assert_eq!(bytes[4], 2);
        assert_eq!(bytes[5], 1);
    }

    #[test]
    fn writes_inner_way_coordinate_blocks_for_multipolygon() {
        let way = WriterWay {
            id: 1,
            min_zoom: 0,
            area: true,
            coords: Vec::new(),
            inner_coords: Vec::new(),
            min_lat: 0.0,
            min_lon: 0.0,
            max_lat: 0.0,
            max_lon: 0.0,
            tag_ids: Vec::new(),
            tag_values: Vec::new(),
            special: SpecialTags {
                layer: 5,
                ..SpecialTags::default()
            },
        };
        let blocks = vec![WayDataBlock {
            outer: vec![
                NodeCoord::from_degrees(0.0, 0.0),
                NodeCoord::from_degrees(0.0, 0.0001),
            ],
            inners: vec![vec![
                NodeCoord::from_degrees(0.00001, 0.00001),
                NodeCoord::from_degrees(0.00002, 0.00002),
            ]],
        }];

        let bytes = write_way(
            &way,
            0,
            0,
            &HashMap::new(),
            EncodingChoice::Single,
            0xffff,
            &blocks,
        )
        .expect("multipolygon way should serialize");

        assert_eq!(&bytes[0..4], &[0xff, 0xff, 0x50, 0x00]);
        assert_eq!(bytes[4], 2);
        assert_eq!(bytes[5], 2);
        assert_eq!(bytes[11], 2);
    }

    #[test]
    fn maps_equivalent_values_to_original_configured_tag() {
        let mut mapping = TagMapping::default();
        let info = add_tag_mapping(
            &mut mapping,
            true,
            "amenity",
            "charging_station",
            10,
            true,
            false,
        )
        .expect("canonical tag should be added");
        add_equivalent_tag_mapping(&mut mapping, true, "amenity", "ev_charging", info);

        let canonical = mapping.poi_match([("amenity", "charging_station")].into_iter(), false);
        let equivalent = mapping.poi_match([("amenity", "ev_charging")].into_iter(), false);

        assert_eq!(canonical.tag_ids, equivalent.tag_ids);
        assert_eq!(canonical.tag_ids, vec![0]);
        assert_eq!(mapping.poi_defs[0].tag_key(), "amenity=charging_station");
    }

    #[test]
    fn wildcard_tags_require_tag_values_enabled() {
        let mut mapping = TagMapping::default();
        add_tag_mapping(&mut mapping, false, "height", "%f", 14, true, false)
            .expect("wildcard tag should be added");

        let disabled = mapping.way_match([("height", "12.5")].into_iter(), false);
        let enabled = mapping.way_match([("height", "12.5")].into_iter(), true);

        assert!(!disabled.has_known);
        assert!(enabled.has_known);
        assert_eq!(enabled.tag_ids, vec![0]);
        assert_eq!(enabled.tag_values, vec![Some(TagValue::Float(12.5))]);
    }

    #[test]
    fn wildcard_numeric_values_use_narrowest_tag_value_type() {
        let mut mapping = TagMapping::default();
        add_tag_mapping(&mut mapping, false, "height", "%f", 14, true, false)
            .expect("wildcard tag should be added");
        add_wildcard_alternatives(&mut mapping, false).expect("alternatives should be added");

        let matched = mapping.way_match([("height", "12")].into_iter(), true);

        assert_eq!(matched.tag_ids, vec![1]);
        assert_eq!(matched.tag_values, vec![Some(TagValue::Byte(12))]);
        assert_eq!(mapping.way_defs[1].tag_key(), "height=%b");
    }

    #[test]
    fn matched_tags_preserve_force_polygon_line_metadata() {
        let mut mapping = TagMapping::default();
        add_tag_mapping(
            &mut mapping,
            false,
            "boundary",
            "administrative",
            0,
            true,
            true,
        )
        .expect("mapping should be added");

        let matched = mapping.way_match([("boundary", "administrative")].into_iter(), false);

        assert!(matched.has_known);
        assert!(matched.force_polygon_line);
    }

    #[test]
    fn classifies_tag_value_types_like_java_writer() {
        assert_eq!(tag_value_type("height", "12"), "%b");
        assert_eq!(tag_value_type("height", "200"), "%h");
        assert_eq!(tag_value_type("height", "40000"), "%i");
        assert_eq!(tag_value_type("height", "12.5"), "%f");
        assert_eq!(tag_value_type("building:colour", "#ff0000"), "%i");
        assert_eq!(tag_value_type("building:colour", "orange"), "%i");
        assert_eq!(tag_value_type("material", "wood"), "%s");
    }

    #[test]
    fn parses_css_named_colors_like_java_writer() {
        assert_eq!(parse_tag_int("red"), Some(0xffff0000_u32 as i32));
        assert_eq!(parse_tag_int("lightgrey"), Some(0xffd3d3d3_u32 as i32));
        assert_eq!(parse_tag_int("not-a-color"), None);
    }

    #[test]
    fn writes_optional_tag_values_like_java_writer() {
        let mut way_encoder = BinaryEncoder::new();
        write_tag_values(
            &mut way_encoder,
            &[
                Some(TagValue::Byte(127)),
                Some(TagValue::Short(258)),
                Some(TagValue::Int(0x0102_0304)),
                Some(TagValue::Float(1.5)),
                Some(TagValue::String("x".to_string())),
            ],
            false,
        )
        .expect("way tag values should serialize");
        assert_eq!(
            way_encoder.into_bytes(),
            vec![0x7f, 0x01, 0x02, 0x01, 0x02, 0x03, 0x04, 0x3f, 0xc0, 0x00, 0x00, 0x01, b'x',]
        );

        let mut poi_encoder = BinaryEncoder::new();
        write_tag_values(&mut poi_encoder, &[Some(TagValue::Byte(127))], true)
            .expect("POI tag values should serialize");
        assert_eq!(poi_encoder.into_bytes(), vec![0x00, 0x7f]);
    }

    #[test]
    fn optimizes_tag_table_by_frequency_then_original_id() {
        let mut mapping = TagMapping::default();
        add_tag_mapping(&mut mapping, false, "highway", "primary", 8, true, false)
            .expect("first tag should be added");
        add_tag_mapping(&mut mapping, false, "building", "yes", 15, true, false)
            .expect("second tag should be added");
        add_tag_mapping(&mut mapping, false, "amenity", "parking", 12, true, false)
            .expect("third tag should be added");

        let mut frequencies = HashMap::new();
        frequencies.insert(2, 4);
        frequencies.insert(0, 4);
        frequencies.insert(1, 9);

        assert_eq!(
            mapping.optimized_way_tags(&frequencies),
            vec!["building=yes", "highway=primary", "amenity=parking"]
        );
    }

    #[test]
    fn extracts_basic_special_tags() {
        let special = extract_special_tags(
            [
                ("name", "Main Street"),
                ("ref", "A1"),
                ("addr:housenumber", "42"),
                ("layer", "-1"),
                ("ele", "123,5m"),
                ("type", "multipolygon"),
            ]
            .into_iter(),
            &[],
        );

        assert_eq!(
            special,
            SpecialTags {
                name: Some("Main Street".to_string()),
                ref_value: Some("A1".to_string()),
                housenumber: Some("42".to_string()),
                layer: 4,
                elevation: 123,
                relation_type: Some("multipolygon".to_string()),
            }
        );
    }

    #[test]
    fn extracts_preferred_single_language_name() {
        let special = extract_special_tags(
            [
                ("name", "Helsinki"),
                ("name:sv", "Helsingfors"),
                ("name:fi", "Helsinki"),
            ]
            .into_iter(),
            &["sv".to_string()],
        );

        assert_eq!(special.name.as_deref(), Some("Helsingfors"));
    }

    #[test]
    fn extracts_multilingual_names_with_mapsforge_delimiters() {
        let special = extract_special_tags(
            [
                ("name", "Default"),
                ("name:fi", "Suomi"),
                ("name:sv", "Svenska"),
            ]
            .into_iter(),
            &["fi".to_string(), "sv".to_string()],
        );

        assert_eq!(
            special.name.as_deref(),
            Some("Default\rfi\u{0008}Suomi\rsv\u{0008}Svenska")
        );
    }

    #[test]
    fn falls_back_to_base_language_for_multilingual_names() {
        let special = extract_special_tags(
            [("name", "Default"), ("name:en", "English")].into_iter(),
            &["en-US".to_string(), "fi".to_string()],
        );

        assert_eq!(special.name.as_deref(), Some("Default\ren\u{0008}English"));
    }

    #[test]
    fn delta_encodes_coordinates_like_java_delta_encoder() {
        let coordinates = mock_coordinates();

        assert_eq!(
            delta_encode_coordinates(&coordinates).expect("coordinates are valid"),
            vec![52_000_000, 13_000_000, 100, 100, 400, 400, -100, -100, 400, 400, 200, 200,]
        );
    }

    #[test]
    fn double_delta_encodes_coordinates_like_java_delta_encoder() {
        let coordinates = mock_coordinates();

        assert_eq!(
            double_delta_encode_coordinates(&coordinates).expect("coordinates are valid"),
            vec![52_000_000, 13_000_000, 100, 100, 300, 300, -500, -500, 500, 500, -200, -200,]
        );
    }

    #[test]
    fn auto_coordinate_encoding_chooses_smaller_serialized_form() {
        let coordinates = [
            1_000_000, 1_000_000, 1_000_100, 1_000_100, 1_000_200, 1_000_200, 1_000_300, 1_000_300,
        ];

        let (encoding, encoded) =
            encode_coordinates(&coordinates, EncodingChoice::Auto).expect("coordinates are valid");

        assert_eq!(encoding, CoordinateEncoding::DoubleDelta);
        assert_eq!(encoded, vec![1_000_000, 1_000_000, 100, 100, 0, 0, 0, 0]);
    }

    #[test]
    fn rejects_odd_coordinate_lists() {
        let error =
            delta_encode_coordinates(&[1, 2, 3]).expect_err("odd coordinate lists are invalid");

        assert!(error.contains("lat/lon pairs"));
    }

    #[test]
    fn encodes_layer_and_tag_count_byte_like_java_writer() {
        assert_eq!(layer_and_tag_count_byte(5, 3).expect("valid"), 0x53);
        assert_eq!(layer_and_tag_count_byte(-3, 0).expect("clamped low"), 0x00);
        assert_eq!(
            layer_and_tag_count_byte(15, 15).expect("clamped high"),
            0xaf
        );
    }

    #[test]
    fn rejects_too_many_tags_for_half_byte_tag_count() {
        let error = layer_and_tag_count_byte(5, 16).expect_err("tag count no longer fits");

        assert!(error.contains("15 tags"));
    }

    #[test]
    fn encodes_poi_feature_byte_like_java_writer() {
        assert_eq!(poi_feature_byte(Some("Cafe"), 42, Some("7")), 0xe0);
        assert_eq!(poi_feature_byte(Some(""), 0, None), 0x00);
    }

    #[test]
    fn encodes_way_feature_byte_like_java_writer() {
        assert_eq!(
            way_feature_byte(
                Some("Road"),
                Some("9"),
                Some("A1"),
                true,
                2,
                CoordinateEncoding::DoubleDelta,
            ),
            0xfc
        );
        assert_eq!(
            way_feature_byte(None, None, None, false, 1, CoordinateEncoding::SingleDelta),
            0x00
        );
    }

    #[test]
    fn merges_relation_tags_into_closed_outer_way_like_java_writer() {
        let way_match = TagMatch {
            has_known: true,
            min_renderable_zoom: Some(14),
            force_polygon_line: false,
            tag_ids: vec![1],
            tag_values: vec![None],
        };
        let relation = RelationMemberInfo {
            is_inner: false,
            tag_ids: vec![2, 1],
            tag_values: vec![Some(TagValue::String("forest".to_string())), None],
            min_renderable_zoom: Some(10),
            force_polygon_line: false,
            special: SpecialTags::default(),
        };

        let merged = merge_relation_member_tags(way_match, &[relation], true);

        assert!(merged.has_known);
        assert_eq!(merged.min_renderable_zoom, Some(10));
        assert_eq!(merged.tag_ids, vec![1, 2]);
        assert_eq!(
            merged.tag_values,
            vec![None, Some(TagValue::String("forest".to_string()))]
        );
    }

    #[test]
    fn does_not_merge_inner_or_open_relation_members() {
        let relation = RelationMemberInfo {
            is_inner: true,
            tag_ids: vec![2],
            tag_values: vec![None],
            min_renderable_zoom: Some(10),
            force_polygon_line: false,
            special: SpecialTags {
                name: Some("Relation".to_string()),
                ref_value: Some("R1".to_string()),
                ..SpecialTags::default()
            },
        };
        let way_match = TagMatch {
            has_known: false,
            min_renderable_zoom: None,
            force_polygon_line: false,
            tag_ids: Vec::new(),
            tag_values: Vec::new(),
        };
        let special = SpecialTags {
            name: Some("Way".to_string()),
            ..SpecialTags::default()
        };

        let merged_inner = merge_relation_member_tags(way_match.clone(), &[relation.clone()], true);
        let merged_open = merge_relation_member_tags(way_match, &[relation.clone()], false);
        let merged_special = merge_relation_member_special(special.clone(), &[relation], true);

        assert!(!merged_inner.has_known);
        assert!(!merged_open.has_known);
        assert_eq!(merged_special.name.as_deref(), Some("Way"));
        assert_eq!(merged_special.ref_value, None);
    }

    #[test]
    fn suppresses_inner_way_when_relation_covers_all_tags() {
        let relation = RelationMemberInfo {
            is_inner: true,
            tag_ids: vec![1, 2],
            tag_values: vec![None, Some(TagValue::String("forest".to_string()))],
            min_renderable_zoom: Some(10),
            force_polygon_line: false,
            special: SpecialTags::default(),
        };
        let covered_way = TagMatch {
            has_known: true,
            min_renderable_zoom: Some(12),
            force_polygon_line: false,
            tag_ids: vec![2],
            tag_values: vec![Some(TagValue::String("forest".to_string()))],
        };
        let extra_tag_way = TagMatch {
            tag_ids: vec![2, 3],
            tag_values: vec![Some(TagValue::String("forest".to_string())), None],
            ..covered_way.clone()
        };
        let outer_member = RelationMemberInfo {
            is_inner: false,
            ..relation.clone()
        };

        assert!(should_suppress_standalone_inner_way(
            &covered_way,
            &[relation.clone()],
            true
        ));
        assert!(!should_suppress_standalone_inner_way(
            &extra_tag_way,
            &[relation.clone()],
            true
        ));
        assert!(!should_suppress_standalone_inner_way(
            &covered_way,
            &[outer_member],
            true
        ));
        assert!(!should_suppress_standalone_inner_way(
            &covered_way,
            &[relation],
            false
        ));
    }

    #[test]
    fn relation_name_and_ref_fill_closed_outer_way_special_fields() {
        let relation = RelationMemberInfo {
            is_inner: false,
            tag_ids: Vec::new(),
            tag_values: Vec::new(),
            min_renderable_zoom: None,
            force_polygon_line: false,
            special: SpecialTags {
                name: Some("Lake".to_string()),
                ref_value: Some("L1".to_string()),
                housenumber: Some("ignored".to_string()),
                layer: 2,
                ..SpecialTags::default()
            },
        };

        let merged = merge_relation_member_special(SpecialTags::default(), &[relation], true);

        assert_eq!(merged.name.as_deref(), Some("Lake"));
        assert_eq!(merged.ref_value.as_deref(), Some("L1"));
        assert_eq!(merged.housenumber, None);
        assert_eq!(merged.layer, 0);
    }

    #[test]
    fn detects_closed_ways_from_microdegree_coordinates() {
        let coords = vec![
            NodeCoord::from_degrees(0.0, 0.0),
            NodeCoord::from_degrees(0.0, 1.0),
            NodeCoord::from_degrees(1.0, 1.0),
            NodeCoord::from_degrees(0.0, 0.0),
        ];

        assert!(is_closed_way(&coords));
        assert!(!is_closed_way(&coords[..3]));
    }

    #[test]
    fn converts_degrees_to_microdegrees_like_java_truncation() {
        assert_eq!(
            NodeCoord::from_degrees(0.0000019, -0.0000019),
            NodeCoord {
                lat_micro: 1,
                lon_micro: -1,
            }
        );
    }

    fn mock_coordinates() -> Vec<i32> {
        vec![
            52_000_000, 13_000_000, 52_000_100, 13_000_100, 52_000_500, 13_000_500, 52_000_400,
            13_000_400, 52_000_800, 13_000_800, 52_001_000, 13_001_000,
        ]
    }

    fn read_u16(bytes: &[u8], offset: usize) -> u16 {
        u16::from_be_bytes([bytes[offset], bytes[offset + 1]])
    }

    fn read_i32(bytes: &[u8], offset: usize) -> i32 {
        i32::from_be_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ])
    }

    fn read_u32(bytes: &[u8], offset: usize) -> u32 {
        u32::from_be_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ])
    }

    fn read_u64(bytes: &[u8], offset: usize) -> u64 {
        u64::from_be_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
            bytes[offset + 4],
            bytes[offset + 5],
            bytes[offset + 6],
            bytes[offset + 7],
        ])
    }
}
