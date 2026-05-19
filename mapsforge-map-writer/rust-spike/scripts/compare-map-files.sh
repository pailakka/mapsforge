#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -lt 2 || "$#" -gt 3 ]]; then
  echo "usage: $0 <left.map> <right.map> [scan-zoom]" >&2
  exit 2
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
spike_dir="$(cd "$script_dir/.." && pwd)"
repo_dir="$(cd "$spike_dir/../.." && pwd)"

left_map="$1"
right_map="$2"
scan_zoom="${3:-${SCAN_ZOOM:-14}}"
left_label="${LEFT_LABEL:-left}"
right_label="${RIGHT_LABEL:-right}"
max_tile_deltas="${MAX_TILE_DELTAS:-20}"
classes_dir="${CLASSES_DIR:-}"
if [[ -z "$classes_dir" ]]; then
  classes_dir="$(mktemp -d /tmp/mapsforge-map-compare-classes.XXXXXX)"
fi
helper_src="${classes_dir}/CompareMapFiles.java"

if [[ ! -f "$left_map" ]]; then
  echo "missing left map file: $left_map" >&2
  exit 1
fi

if [[ ! -f "$right_map" ]]; then
  echo "missing right map file: $right_map" >&2
  exit 1
fi

cd "$repo_dir"
./gradlew :mapsforge-core:jar :mapsforge-map:jar --no-daemon

mkdir -p "$classes_dir"

cat > "$helper_src" <<'JAVA'
import org.mapsforge.core.model.BoundingBox;
import org.mapsforge.core.model.Tag;
import org.mapsforge.core.model.Tile;
import org.mapsforge.core.util.MercatorProjection;
import org.mapsforge.map.datastore.MapReadResult;
import org.mapsforge.map.reader.MapFile;
import org.mapsforge.map.reader.header.MapFileInfo;
import org.mapsforge.map.reader.header.SubFileParameter;

import java.io.File;
import java.util.LinkedHashSet;
import java.util.Set;

public final class CompareMapFiles {
    private static final class ScanTotals {
        long pois;
        long ways;
        long tilesWithData;
    }

    public static void main(String[] args) {
        if (args.length != 6) {
            throw new IllegalArgumentException("usage: CompareMapFiles <left-map> <right-map> <scan-zoom> <left-label> <right-label> <max-tile-deltas>");
        }

        File leftPath = new File(args[0]);
        File rightPath = new File(args[1]);
        byte zoom = Byte.parseByte(args[2]);
        String leftLabel = args[3];
        String rightLabel = args[4];
        int maxTileDeltas = Integer.parseInt(args[5]);

        MapFile left = new MapFile(leftPath);
        MapFile right = new MapFile(rightPath);
        try {
            MapFileInfo leftInfo = left.getMapFileInfo();
            MapFileInfo rightInfo = right.getMapFileInfo();
            String leftZoomIntervals = zoomIntervals(left);
            String rightZoomIntervals = zoomIntervals(right);

            printInfo(leftLabel, leftPath, leftInfo, leftZoomIntervals);
            printInfo(rightLabel, rightPath, rightInfo, rightZoomIntervals);
            printMetadataComparison(leftInfo, rightInfo, leftZoomIntervals, rightZoomIntervals);
            printTagComparison("poi", leftInfo.poiTags, rightInfo.poiTags);
            printTagComparison("way", leftInfo.wayTags, rightInfo.wayTags);

            BoundingBox bbox = overlappingBbox(leftInfo.boundingBox, rightInfo.boundingBox);
            int minTileX = MercatorProjection.longitudeToTileX(bbox.minLongitude, zoom);
            int maxTileX = MercatorProjection.longitudeToTileX(bbox.maxLongitude, zoom);
            int minTileY = MercatorProjection.latitudeToTileY(bbox.maxLatitude, zoom);
            int maxTileY = MercatorProjection.latitudeToTileY(bbox.minLatitude, zoom);

            ScanTotals leftTotals = new ScanTotals();
            ScanTotals rightTotals = new ScanTotals();
            long differingPoiTiles = 0;
            long differingWayTiles = 0;
            long maxPoiDelta = 0;
            long maxWayDelta = 0;
            int printedDeltas = 0;

            for (int tileX = minTileX; tileX <= maxTileX; tileX++) {
                for (int tileY = minTileY; tileY <= maxTileY; tileY++) {
                    Tile leftTile = new Tile(tileX, tileY, zoom, leftInfo.tilePixelSize);
                    Tile rightTile = new Tile(tileX, tileY, zoom, rightInfo.tilePixelSize);
                    MapReadResult leftResult = left.readMapData(leftTile);
                    MapReadResult rightResult = right.readMapData(rightTile);

                    addTotals(leftTotals, leftResult);
                    addTotals(rightTotals, rightResult);

                    long poiDelta = Math.abs((long) leftResult.pois.size() - (long) rightResult.pois.size());
                    long wayDelta = Math.abs((long) leftResult.ways.size() - (long) rightResult.ways.size());
                    if (poiDelta != 0) {
                        differingPoiTiles++;
                        maxPoiDelta = Math.max(maxPoiDelta, poiDelta);
                    }
                    if (wayDelta != 0) {
                        differingWayTiles++;
                        maxWayDelta = Math.max(maxWayDelta, wayDelta);
                    }
                    if ((poiDelta != 0 || wayDelta != 0) && printedDeltas < maxTileDeltas) {
                        System.out.println("tile_delta=" + tileX + "," + tileY + "," + zoom
                                + " " + leftLabel + "_pois=" + leftResult.pois.size()
                                + " " + rightLabel + "_pois=" + rightResult.pois.size()
                                + " " + leftLabel + "_ways=" + leftResult.ways.size()
                                + " " + rightLabel + "_ways=" + rightResult.ways.size());
                        printedDeltas++;
                    }
                }
            }

            long scanTiles = (long) (maxTileX - minTileX + 1) * (long) (maxTileY - minTileY + 1);
            System.out.println("scan_zoom=" + zoom);
            System.out.println("scan_bbox=" + bbox);
            System.out.println("scan_tiles=" + scanTiles);
            printScanTotals(leftLabel, leftTotals);
            printScanTotals(rightLabel, rightTotals);
            System.out.println("delta_pois=" + (leftTotals.pois - rightTotals.pois));
            System.out.println("delta_ways=" + (leftTotals.ways - rightTotals.ways));
            System.out.println("differing_poi_tiles=" + differingPoiTiles);
            System.out.println("differing_way_tiles=" + differingWayTiles);
            System.out.println("max_poi_delta_per_tile=" + maxPoiDelta);
            System.out.println("max_way_delta_per_tile=" + maxWayDelta);
        } finally {
            left.close();
            right.close();
        }
    }

    private static void addTotals(ScanTotals totals, MapReadResult result) {
        totals.pois += result.pois.size();
        totals.ways += result.ways.size();
        if (!result.pois.isEmpty() || !result.ways.isEmpty()) {
            totals.tilesWithData++;
        }
    }

    private static BoundingBox overlappingBbox(BoundingBox left, BoundingBox right) {
        double minLatitude = Math.max(left.minLatitude, right.minLatitude);
        double minLongitude = Math.max(left.minLongitude, right.minLongitude);
        double maxLatitude = Math.min(left.maxLatitude, right.maxLatitude);
        double maxLongitude = Math.min(left.maxLongitude, right.maxLongitude);
        if (minLatitude > maxLatitude || minLongitude > maxLongitude) {
            throw new IllegalArgumentException("map bounding boxes do not overlap: left=" + left + " right=" + right);
        }
        return new BoundingBox(minLatitude, minLongitude, maxLatitude, maxLongitude);
    }

    private static void printInfo(String label, File path, MapFileInfo info, String zoomIntervals) {
        System.out.println(label + "_map=" + path.getAbsolutePath());
        System.out.println(label + "_file_size=" + info.fileSize);
        System.out.println(label + "_file_version=" + info.fileVersion);
        System.out.println(label + "_bbox=" + info.boundingBox);
        System.out.println(label + "_tile_size=" + info.tilePixelSize);
        System.out.println(label + "_projection=" + info.projectionName);
        System.out.println(label + "_zoom_range=" + info.zoomLevelMin + ".." + info.zoomLevelMax);
        System.out.println(label + "_zoom_intervals=" + zoomIntervals);
        System.out.println(label + "_start_position=" + info.startPosition);
        System.out.println(label + "_start_zoom=" + info.startZoomLevel);
        System.out.println(label + "_languages=" + info.languagesPreference);
        System.out.println(label + "_comment=" + info.comment);
        System.out.println(label + "_created_by=" + info.createdBy);
        System.out.println(label + "_poi_tag_count=" + info.poiTags.length);
        System.out.println(label + "_way_tag_count=" + info.wayTags.length);
    }

    private static void printMetadataComparison(MapFileInfo left, MapFileInfo right, String leftZoomIntervals, String rightZoomIntervals) {
        System.out.println("same_bbox=" + left.boundingBox.equals(right.boundingBox));
        System.out.println("same_tile_size=" + (left.tilePixelSize == right.tilePixelSize));
        System.out.println("same_projection=" + stringEquals(left.projectionName, right.projectionName));
        System.out.println("same_zoom_range=" + (left.zoomLevelMin == right.zoomLevelMin && left.zoomLevelMax == right.zoomLevelMax));
        System.out.println("same_zoom_intervals=" + stringEquals(leftZoomIntervals, rightZoomIntervals));
        System.out.println("same_start_position=" + objectEquals(left.startPosition, right.startPosition));
        System.out.println("same_start_zoom=" + objectEquals(left.startZoomLevel, right.startZoomLevel));
        System.out.println("same_languages=" + stringEquals(left.languagesPreference, right.languagesPreference));
        System.out.println("same_comment=" + stringEquals(left.comment, right.comment));
    }

    private static String zoomIntervals(MapFile mapFile) {
        MapFileInfo info = mapFile.getMapFileInfo();
        StringBuilder result = new StringBuilder();
        SubFileParameter previous = null;
        for (byte zoom = info.zoomLevelMin; zoom <= info.zoomLevelMax; zoom++) {
            SubFileParameter current = mapFile.getMapFileHeader().getSubFileParameter(zoom);
            if (current == null || current == previous) {
                continue;
            }
            if (result.length() > 0) {
                result.append(',');
            }
            result.append(current.baseZoomLevel)
                    .append(':')
                    .append(current.zoomLevelMin)
                    .append('-')
                    .append(current.zoomLevelMax);
            previous = current;
        }
        return result.toString();
    }

    private static void printTagComparison(String prefix, Tag[] left, Tag[] right) {
        Set<String> leftSet = tagSet(left);
        Set<String> rightSet = tagSet(right);
        Set<String> leftOnly = new LinkedHashSet<>(leftSet);
        leftOnly.removeAll(rightSet);
        Set<String> rightOnly = new LinkedHashSet<>(rightSet);
        rightOnly.removeAll(leftSet);
        System.out.println(prefix + "_tag_order_equal=" + tagOrderEqual(left, right));
        System.out.println(prefix + "_tag_set_equal=" + leftSet.equals(rightSet));
        System.out.println(prefix + "_left_only_count=" + leftOnly.size());
        System.out.println(prefix + "_right_only_count=" + rightOnly.size());
        printSample(prefix + "_left_only", leftOnly);
        printSample(prefix + "_right_only", rightOnly);
    }

    private static Set<String> tagSet(Tag[] tags) {
        Set<String> result = new LinkedHashSet<>();
        for (Tag tag : tags) {
            result.add(tag.key + "=" + tag.value);
        }
        return result;
    }

    private static boolean tagOrderEqual(Tag[] left, Tag[] right) {
        if (left.length != right.length) {
            return false;
        }
        for (int i = 0; i < left.length; i++) {
            if (!left[i].equals(right[i])) {
                return false;
            }
        }
        return true;
    }

    private static void printSample(String key, Set<String> values) {
        int count = 0;
        StringBuilder sample = new StringBuilder();
        for (String value : values) {
            if (count > 0) {
                sample.append(',');
            }
            sample.append(value);
            count++;
            if (count == 10) {
                break;
            }
        }
        System.out.println(key + "_sample=" + sample);
    }

    private static void printScanTotals(String label, ScanTotals totals) {
        System.out.println(label + "_tiles_with_data=" + totals.tilesWithData);
        System.out.println(label + "_pois=" + totals.pois);
        System.out.println(label + "_ways=" + totals.ways);
    }

    private static boolean stringEquals(String left, String right) {
        return left == null ? right == null : left.equals(right);
    }

    private static boolean objectEquals(Object left, Object right) {
        return left == null ? right == null : left.equals(right);
    }
}
JAVA

javac -d "$classes_dir" \
  -cp "mapsforge-core/build/libs/mapsforge-core-master-SNAPSHOT.jar:mapsforge-map/build/libs/mapsforge-map-master-SNAPSHOT.jar" \
  $(find mapsforge-map-reader/src/main/java -name '*.java' | sort) \
  "$helper_src"

java -cp "$classes_dir:mapsforge-core/build/libs/mapsforge-core-master-SNAPSHOT.jar:mapsforge-map/build/libs/mapsforge-map-master-SNAPSHOT.jar" \
  CompareMapFiles "$left_map" "$right_map" "$scan_zoom" "$left_label" "$right_label" "$max_tile_deltas"
