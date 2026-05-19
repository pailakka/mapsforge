#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
spike_dir="$(cd "$script_dir/.." && pwd)"
repo_dir="$(cd "$spike_dir/../.." && pwd)"

sample_pbf="${SAMPLE_PBF:-$repo_dir/testdata/63240150.osm.pbf}"
theme_spec="${THEME_SPEC:-${THEME_FILE:-}}"
if [[ -z "$theme_spec" ]]; then
  if [[ "$(basename "$sample_pbf")" == "finland-260517.osm.pbf" ]]; then
    theme_spec="internal:DEFAULT"
  else
    theme_spec="$repo_dir/testdata/peruskartta/Peruskartta.xml"
  fi
fi
output_map="${OUTPUT_MAP:-/tmp/mapsforge-rust-sample.map}"
bbox="${BBOX:-61.2929618,26.767643,62.5010047,28.6301968}"
zoom_interval_conf="${ZOOM_INTERVAL_CONF:-5,0,7,10,8,11,14,12,21}"
tag_values="${TAG_VALUES:-false}"
scan_zoom="${SCAN_ZOOM:-14}"
scan_zooms="${SCAN_ZOOMS:-$scan_zoom}"
render_zoom="${RENDER_ZOOM:-$scan_zoom}"
generate_map="${GENERATE_MAP:-true}"
render="${RENDER:-false}"
render_png="${RENDER_PNG:-/tmp/mapsforge-rust-render-smoke.png}"
classes_dir="${CLASSES_DIR:-}"
if [[ -z "$classes_dir" ]]; then
  classes_dir="$(mktemp -d /tmp/mapsforge-rust-validation-classes.XXXXXX)"
fi
helper_src="${classes_dir}/RustMapReaderSmoke.java"
render_helper_src="${classes_dir}/RustMapRenderSmoke.java"

if [[ "$generate_map" == "true" && ! -f "$sample_pbf" ]]; then
  echo "missing sample PBF: $sample_pbf" >&2
  exit 1
fi

if [[ "$generate_map" != "true" && ! -f "$output_map" ]]; then
  echo "missing map file: $output_map" >&2
  exit 1
fi

theme_spec_upper="${theme_spec^^}"
if [[ "$theme_spec_upper" != INTERNAL:* && "$theme_spec_upper" != "DEFAULT" && "$theme_spec_upper" != "OSMARENDER" && ! -f "$theme_spec" ]]; then
  echo "missing theme file: $theme_spec" >&2
  exit 1
fi

if [[ "$generate_map" == "true" ]]; then
  cd "$spike_dir"
  cargo run --release -- \
    --input "$sample_pbf" \
    --bbox "$bbox" \
    --mode tile-index \
    --output "$output_map" \
    --zoom-interval-conf "$zoom_interval_conf" \
    --bbox-enlargement 20 \
    --encoding auto \
    --tag-values "$tag_values" \
    --type ram \
    --debug-file false \
    --label-position false \
    --polylabel false
fi

cd "$repo_dir"
./gradlew :mapsforge-core:jar :mapsforge-map:jar --no-daemon

mkdir -p "$classes_dir"

cat > "$helper_src" <<'JAVA'
import org.mapsforge.core.model.BoundingBox;
import org.mapsforge.core.model.Tile;
import org.mapsforge.core.util.MercatorProjection;
import org.mapsforge.map.datastore.MapReadResult;
import org.mapsforge.map.reader.MapFile;
import org.mapsforge.map.reader.header.MapFileInfo;

import java.io.File;

public final class RustMapReaderSmoke {
    public static void main(String[] args) {
        if (args.length != 3) {
            throw new IllegalArgumentException("usage: RustMapReaderSmoke <map-file> <theme-spec> <scan-zoom>");
        }

        File mapPath = new File(args[0]);
        String themeSpec = args[1];
        byte zoom = Byte.parseByte(args[2]);

        MapFile mapFile = new MapFile(mapPath);
        try {
            MapFileInfo info = mapFile.getMapFileInfo();
            BoundingBox bbox = info.boundingBox;
            int minTileX = MercatorProjection.longitudeToTileX(bbox.minLongitude, zoom);
            int maxTileX = MercatorProjection.longitudeToTileX(bbox.maxLongitude, zoom);
            int minTileY = MercatorProjection.latitudeToTileY(bbox.maxLatitude, zoom);
            int maxTileY = MercatorProjection.latitudeToTileY(bbox.minLatitude, zoom);

            long poiCount = 0;
            long wayCount = 0;
            long tilesWithData = 0;
            for (int tileX = minTileX; tileX <= maxTileX; tileX++) {
                for (int tileY = minTileY; tileY <= maxTileY; tileY++) {
                    MapReadResult result = mapFile.readMapData(new Tile(tileX, tileY, zoom, info.tilePixelSize));
                    poiCount += result.pois.size();
                    wayCount += result.ways.size();
                    if (!result.pois.isEmpty() || !result.ways.isEmpty()) {
                        tilesWithData++;
                    }
                }
            }

            System.out.println("map=" + mapPath.getAbsolutePath());
            System.out.println("theme_spec=" + themeSpec);
            System.out.println("bbox=" + bbox);
            System.out.println("file_version=" + info.fileVersion);
            System.out.println("tile_size=" + info.tilePixelSize);
            System.out.println("zoom_range=" + info.zoomLevelMin + ".." + info.zoomLevelMax);
            System.out.println("poi_tag_count=" + info.poiTags.length);
            System.out.println("way_tag_count=" + info.wayTags.length);
            System.out.println("scan_zoom=" + zoom);
            System.out.println("scan_tiles=" + ((long) (maxTileX - minTileX + 1) * (long) (maxTileY - minTileY + 1)));
            System.out.println("tiles_with_data=" + tilesWithData);
            System.out.println("pois=" + poiCount);
            System.out.println("ways=" + wayCount);
        } finally {
            mapFile.close();
        }
    }
}
JAVA

javac -d "$classes_dir" \
  -cp "mapsforge-core/build/libs/mapsforge-core-master-SNAPSHOT.jar:mapsforge-map/build/libs/mapsforge-map-master-SNAPSHOT.jar" \
  $(find mapsforge-map-reader/src/main/java -name '*.java' | sort) \
  "$helper_src"

IFS=',' read -r -a scan_zoom_list <<< "$scan_zooms"
for zoom in "${scan_zoom_list[@]}"; do
  java -cp "$classes_dir:mapsforge-core/build/libs/mapsforge-core-master-SNAPSHOT.jar:mapsforge-map/build/libs/mapsforge-map-master-SNAPSHOT.jar" \
    RustMapReaderSmoke "$output_map" "$theme_spec" "$zoom"
done

if [[ "$render" == "true" ]]; then
  kxml_jar="${KXML2_JAR:-}"
  if [[ -z "$kxml_jar" ]]; then
    kxml_jar="$(find "$HOME/.gradle/caches/modules-2/files-2.1/net.sf.kxml/kxml2" -name 'kxml2-2.3.0.jar' -print -quit 2>/dev/null || true)"
  fi
  if [[ -z "$kxml_jar" || ! -f "$kxml_jar" ]]; then
    echo "missing kxml2 jar; set KXML2_JAR" >&2
    exit 1
  fi

  svg_jar="${SVG_SALAMANDER_JAR:-}"
  if [[ -z "$svg_jar" ]]; then
    svg_jar="$(find "$HOME/.gradle/caches/modules-2/files-2.1" -name 'svgSalamander-1.1.3.jar' -print -quit 2>/dev/null || true)"
  fi
  if [[ -z "$svg_jar" && -f /tmp/svgSalamander-1.1.3.jar ]]; then
    svg_jar="/tmp/svgSalamander-1.1.3.jar"
  fi
  if [[ -z "$svg_jar" && "${DOWNLOAD_RENDER_DEPS:-false}" == "true" ]]; then
    svg_jar="/tmp/svgSalamander-1.1.3.jar"
    curl -L -o "$svg_jar" https://repo1.maven.org/maven2/guru/nidi/com/kitfox/svgSalamander/1.1.3/svgSalamander-1.1.3.jar
  fi
  if [[ -z "$svg_jar" || ! -f "$svg_jar" ]]; then
    echo "missing svgSalamander jar; set SVG_SALAMANDER_JAR or DOWNLOAD_RENDER_DEPS=true" >&2
    exit 1
  fi

  cat > "$render_helper_src" <<'JAVA'
import org.mapsforge.core.graphics.TileBitmap;
import org.mapsforge.core.model.BoundingBox;
import org.mapsforge.core.model.Tile;
import org.mapsforge.core.util.MercatorProjection;
import org.mapsforge.map.awt.graphics.AwtGraphicFactory;
import org.mapsforge.map.datastore.MapReadResult;
import org.mapsforge.map.layer.renderer.DirectRenderer;
import org.mapsforge.map.layer.renderer.RendererJob;
import org.mapsforge.map.model.DisplayModel;
import org.mapsforge.map.reader.MapFile;
import org.mapsforge.map.reader.header.MapFileInfo;
import org.mapsforge.map.rendertheme.ExternalRenderTheme;
import org.mapsforge.map.rendertheme.XmlRenderTheme;
import org.mapsforge.map.rendertheme.internal.MapsforgeThemes;
import org.mapsforge.map.rendertheme.rule.RenderThemeFuture;

import javax.imageio.ImageIO;
import java.awt.image.BufferedImage;
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.FileOutputStream;
import java.util.HashSet;
import java.util.Locale;
import java.util.Set;

public final class RustMapRenderSmoke {
    public static void main(String[] args) throws Exception {
        if (args.length != 4) {
    throw new IllegalArgumentException("usage: RustMapRenderSmoke <map-file> <theme-spec> <render-zoom> <png-output>");
        }

        File mapPath = new File(args[0]);
        String themeSpec = args[1];
        byte zoom = Byte.parseByte(args[2]);
        File pngOutput = new File(args[3]);

        MapFile mapFile = new MapFile(mapPath);
        RenderThemeFuture themeFuture = null;
        try {
            MapFileInfo info = mapFile.getMapFileInfo();
            DisplayModel displayModel = new DisplayModel();
            BoundingBox bbox = info.boundingBox;
            int minTileX = MercatorProjection.longitudeToTileX(bbox.minLongitude, zoom);
            int maxTileX = MercatorProjection.longitudeToTileX(bbox.maxLongitude, zoom);
            int minTileY = MercatorProjection.latitudeToTileY(bbox.maxLatitude, zoom);
            int maxTileY = MercatorProjection.latitudeToTileY(bbox.minLatitude, zoom);

            Tile selected = null;
            MapReadResult selectedData = null;
            for (int tileX = minTileX; tileX <= maxTileX && selected == null; tileX++) {
                for (int tileY = minTileY; tileY <= maxTileY; tileY++) {
                    Tile tile = new Tile(tileX, tileY, zoom, info.tilePixelSize);
                    MapReadResult result = mapFile.readMapData(tile);
                    if (!result.ways.isEmpty() || !result.pois.isEmpty()) {
                        selected = tile;
                        selectedData = result;
                        break;
                    }
                }
            }
            if (selected == null) {
                throw new IllegalStateException("no tile with map data found in bbox at zoom " + zoom);
            }

            XmlRenderTheme theme = resolveTheme(themeSpec);
            themeFuture = new RenderThemeFuture(AwtGraphicFactory.INSTANCE, theme, displayModel);
            themeFuture.run();

            DirectRenderer renderer = new DirectRenderer(mapFile, AwtGraphicFactory.INSTANCE, null, false, false, null);
            RendererJob job = new RendererJob(selected, mapFile, themeFuture, displayModel, 1f, false, false);
            TileBitmap bitmap = renderer.executeJob(job);
            if (bitmap == null) {
                throw new IllegalStateException("renderer returned null bitmap");
            }

            ByteArrayOutputStream buffer = new ByteArrayOutputStream();
            bitmap.compress(buffer);
            byte[] png = buffer.toByteArray();
            try (FileOutputStream output = new FileOutputStream(pngOutput)) {
                output.write(png);
            }

            BufferedImage image = ImageIO.read(new ByteArrayInputStream(png));
            if (image == null) {
                throw new IllegalStateException("rendered PNG could not be decoded");
            }
            Set<Integer> sampledColors = new HashSet<>();
            for (int y = 0; y < image.getHeight(); y += 8) {
                for (int x = 0; x < image.getWidth(); x += 8) {
                    sampledColors.add(image.getRGB(x, y));
                }
            }
            if (sampledColors.size() < 2) {
                throw new IllegalStateException("rendered tile appears blank; sampled color count=" + sampledColors.size());
            }

            System.out.println("render_tile=" + selected.tileX + "," + selected.tileY + "," + selected.zoomLevel);
            System.out.println("render_tile_pois=" + selectedData.pois.size());
            System.out.println("render_tile_ways=" + selectedData.ways.size());
            System.out.println("render_png=" + pngOutput.getAbsolutePath());
            System.out.println("render_png_bytes=" + png.length);
            System.out.println("render_sampled_colors=" + sampledColors.size());
        } finally {
            if (themeFuture != null) {
                themeFuture.decrementRefCount();
            }
            mapFile.close();
        }
    }

    private static XmlRenderTheme resolveTheme(String themeSpec) throws Exception {
        String normalized = themeSpec.toUpperCase(Locale.ROOT);
        if ("DEFAULT".equals(normalized)) {
            return MapsforgeThemes.DEFAULT;
        }
        if ("OSMARENDER".equals(normalized)) {
            return MapsforgeThemes.OSMARENDER;
        }
        if (normalized.startsWith("INTERNAL:")) {
            return MapsforgeThemes.valueOf(normalized.substring("INTERNAL:".length()));
        }
        return new ExternalRenderTheme(new File(themeSpec));
    }
}
JAVA

  javac -d "$classes_dir" \
    -cp "$classes_dir:mapsforge-core/build/libs/mapsforge-core-master-SNAPSHOT.jar:mapsforge-map/build/libs/mapsforge-map-master-SNAPSHOT.jar:$kxml_jar:$svg_jar" \
    $(find mapsforge-map-awt/src/main/java -name '*.java' | sort) \
    $(find mapsforge-themes/src/main/java -name '*.java' | sort) \
    "$render_helper_src"

  java -Djava.awt.headless=true \
    -cp "$classes_dir:mapsforge-themes/src/main/resources:mapsforge-core/build/libs/mapsforge-core-master-SNAPSHOT.jar:mapsforge-map/build/libs/mapsforge-map-master-SNAPSHOT.jar:$kxml_jar:$svg_jar" \
    RustMapRenderSmoke "$output_map" "$theme_spec" "$render_zoom" "$render_png"
fi
