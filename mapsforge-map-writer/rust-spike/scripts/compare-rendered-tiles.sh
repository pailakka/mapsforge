#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -ne 3 ]]; then
  echo "usage: $0 <left.map> <right.map> <theme.xml|internal:DEFAULT>" >&2
  exit 2
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
spike_dir="$(cd "$script_dir/.." && pwd)"
repo_dir="$(cd "$spike_dir/../.." && pwd)"

left_map="$1"
right_map="$2"
theme_spec="$3"
left_label="${LEFT_LABEL:-left}"
right_label="${RIGHT_LABEL:-right}"
scan_zoom="${SCAN_ZOOM:-14}"
sample_tiles="${SAMPLE_TILES:-8}"
render_tiles="${RENDER_TILES:-}"
max_differing_pixels="${MAX_DIFFERING_PIXELS:-0}"
classes_dir="${CLASSES_DIR:-$(mktemp -d /tmp/mapsforge-render-compare-classes.XXXXXX)}"
helper_src="${classes_dir}/CompareRenderedTiles.java"

for file in "$left_map" "$right_map"; do
  if [[ ! -f "$file" ]]; then
    echo "missing file: $file" >&2
    exit 1
  fi
done

theme_spec_upper="${theme_spec^^}"
if [[ "$theme_spec_upper" != INTERNAL:* && "$theme_spec_upper" != "DEFAULT" && "$theme_spec_upper" != "OSMARENDER" && ! -f "$theme_spec" ]]; then
  echo "missing theme file: $theme_spec" >&2
  exit 1
fi

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

cd "$repo_dir"
./gradlew :mapsforge-core:jar :mapsforge-map:jar --no-daemon

mkdir -p "$classes_dir"
cat > "$helper_src" <<'JAVA'
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
import java.util.ArrayList;
import java.util.List;
import java.util.Locale;

public final class CompareRenderedTiles {
    private static final class RenderedTile {
        final BufferedImage image;
        final int pngBytes;

        RenderedTile(BufferedImage image, int pngBytes) {
            this.image = image;
            this.pngBytes = pngBytes;
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 9) {
            throw new IllegalArgumentException("usage: CompareRenderedTiles <left-map> <right-map> <theme-spec> <scan-zoom> <sample-tiles> <tile-list> <left-label> <right-label> <max-differing-pixels>");
        }

        File leftPath = new File(args[0]);
        File rightPath = new File(args[1]);
        String themeSpec = args[2];
        byte scanZoom = Byte.parseByte(args[3]);
        int sampleTiles = Integer.parseInt(args[4]);
        String tileList = args[5];
        String leftLabel = args[6];
        String rightLabel = args[7];
        int maxDifferingPixels = Integer.parseInt(args[8]);

        MapFile left = new MapFile(leftPath);
        MapFile right = new MapFile(rightPath);
        RenderThemeFuture themeFuture = null;
        try {
            DisplayModel displayModel = new DisplayModel();
            XmlRenderTheme theme = resolveTheme(themeSpec);
            themeFuture = new RenderThemeFuture(AwtGraphicFactory.INSTANCE, theme, displayModel);
            themeFuture.run();

            List<Tile> tiles = tileList.isEmpty()
                    ? sampleTiles(left, right, scanZoom, sampleTiles)
                    : parseTiles(tileList, left.getMapFileInfo().tilePixelSize);
            System.out.println("render_compare_tiles=" + tiles.size());

            boolean failed = false;
            for (Tile tile : tiles) {
                RenderedTile leftRendered = render(left, tile, themeFuture, displayModel);
                RenderedTile rightRendered = render(right, tile, themeFuture, displayModel);
                int differingPixels = differingPixels(leftRendered.image, rightRendered.image);
                System.out.println("render_tile=" + tile.tileX + "," + tile.tileY + "," + tile.zoomLevel
                        + " " + leftLabel + "_png_bytes=" + leftRendered.pngBytes
                        + " " + rightLabel + "_png_bytes=" + rightRendered.pngBytes
                        + " differing_pixels=" + differingPixels);
                if (differingPixels > maxDifferingPixels) {
                    failed = true;
                }
            }

            if (failed) {
                throw new IllegalStateException("rendered tile parity exceeded differing pixel threshold");
            }
        } finally {
            if (themeFuture != null) {
                themeFuture.decrementRefCount();
            }
            left.close();
            right.close();
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

    private static List<Tile> sampleTiles(MapFile left, MapFile right, byte zoom, int sampleTiles) {
        MapFileInfo leftInfo = left.getMapFileInfo();
        MapFileInfo rightInfo = right.getMapFileInfo();
        BoundingBox bbox = overlappingBbox(leftInfo.boundingBox, rightInfo.boundingBox);
        int minTileX = MercatorProjection.longitudeToTileX(bbox.minLongitude, zoom);
        int maxTileX = MercatorProjection.longitudeToTileX(bbox.maxLongitude, zoom);
        int minTileY = MercatorProjection.latitudeToTileY(bbox.maxLatitude, zoom);
        int maxTileY = MercatorProjection.latitudeToTileY(bbox.minLatitude, zoom);

        List<Tile> result = new ArrayList<>();
        for (int tileX = minTileX; tileX <= maxTileX && result.size() < sampleTiles; tileX++) {
            for (int tileY = minTileY; tileY <= maxTileY && result.size() < sampleTiles; tileY++) {
                Tile tile = new Tile(tileX, tileY, zoom, leftInfo.tilePixelSize);
                MapReadResult leftData = left.readMapData(tile);
                MapReadResult rightData = right.readMapData(tile);
                if (!leftData.pois.isEmpty() || !leftData.ways.isEmpty()
                        || !rightData.pois.isEmpty() || !rightData.ways.isEmpty()) {
                    result.add(tile);
                }
            }
        }
        return result;
    }

    private static List<Tile> parseTiles(String tileList, int tileSize) {
        List<Tile> result = new ArrayList<>();
        for (String item : tileList.split(",")) {
            String[] parts = item.split(":");
            if (parts.length != 3) {
                throw new IllegalArgumentException("tile must be x:y:z: " + item);
            }
            result.add(new Tile(Integer.parseInt(parts[0]), Integer.parseInt(parts[1]), Byte.parseByte(parts[2]), tileSize));
        }
        return result;
    }

    private static RenderedTile render(MapFile mapFile, Tile tile, RenderThemeFuture themeFuture, DisplayModel displayModel) throws Exception {
        DirectRenderer renderer = new DirectRenderer(mapFile, AwtGraphicFactory.INSTANCE, null, false, false, null);
        RendererJob job = new RendererJob(tile, mapFile, themeFuture, displayModel, 1f, false, false);
        TileBitmap bitmap = renderer.executeJob(job);
        if (bitmap == null) {
            throw new IllegalStateException("renderer returned null bitmap for " + tile);
        }
        ByteArrayOutputStream buffer = new ByteArrayOutputStream();
        bitmap.compress(buffer);
        byte[] png = buffer.toByteArray();
        BufferedImage image = ImageIO.read(new ByteArrayInputStream(png));
        if (image == null) {
            throw new IllegalStateException("rendered PNG could not be decoded for " + tile);
        }
        return new RenderedTile(image, png.length);
    }

    private static int differingPixels(BufferedImage left, BufferedImage right) {
        if (left.getWidth() != right.getWidth() || left.getHeight() != right.getHeight()) {
            return Integer.MAX_VALUE;
        }
        int count = 0;
        for (int y = 0; y < left.getHeight(); y++) {
            for (int x = 0; x < left.getWidth(); x++) {
                if (left.getRGB(x, y) != right.getRGB(x, y)) {
                    count++;
                }
            }
        }
        return count;
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
}
JAVA

javac -d "$classes_dir" \
  -cp "mapsforge-core/build/libs/mapsforge-core-master-SNAPSHOT.jar:mapsforge-map/build/libs/mapsforge-map-master-SNAPSHOT.jar:$kxml_jar:$svg_jar" \
  $(find mapsforge-map-reader/src/main/java -name '*.java' | sort) \
  $(find mapsforge-map-awt/src/main/java -name '*.java' | sort) \
  $(find mapsforge-themes/src/main/java -name '*.java' | sort) \
  "$helper_src"

java -Djava.awt.headless=true \
  -cp "$classes_dir:mapsforge-themes/src/main/resources:mapsforge-core/build/libs/mapsforge-core-master-SNAPSHOT.jar:mapsforge-map/build/libs/mapsforge-map-master-SNAPSHOT.jar:$kxml_jar:$svg_jar" \
  CompareRenderedTiles "$left_map" "$right_map" "$theme_spec" "$scan_zoom" "$sample_tiles" "$render_tiles" "$left_label" "$right_label" "$max_differing_pixels"
