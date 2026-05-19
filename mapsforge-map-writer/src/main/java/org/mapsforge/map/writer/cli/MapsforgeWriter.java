/*
 * Copyright 2026 mapsforge.org
 *
 * This program is free software: you can redistribute it and/or modify it under the
 * terms of the GNU Lesser General Public License as published by the Free Software
 * Foundation, either version 3 of the License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful, but WITHOUT ANY
 * WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A
 * PARTICULAR PURPOSE. See the GNU Lesser General Public License for more details.
 *
 * You should have received a copy of the GNU Lesser General Public License along with
 * this program. If not, see <http://www.gnu.org/licenses/>.
 */
package org.mapsforge.map.writer.cli;

import crosby.binary.osmosis.OsmosisReader;
import org.mapsforge.map.writer.model.MapWriterConfiguration;
import org.mapsforge.map.writer.osmosis.MapFileWriterTask;
import org.mapsforge.map.writer.util.Constants;
import org.mapsforge.map.writer.util.WriterPerformance;

import java.io.File;
import java.io.FileInputStream;
import java.io.IOException;
import java.util.Arrays;
import java.util.HashMap;
import java.util.HashSet;
import java.util.Map;
import java.util.Set;
import java.util.logging.Logger;

/**
 * Standalone PBF-to-mapsforge writer entrypoint.
 */
public final class MapsforgeWriter {
    private static final Logger LOGGER = Logger.getLogger(MapsforgeWriter.class.getName());

    private static final Set<String> BOOLEAN_OPTIONS = new HashSet<>(Arrays.asList(
            "debug-file",
            "label-position",
            "polygon-clipping",
            "polylabel",
            "progress-logs",
            "skip-invalid-relations",
            "tag-values",
            "way-clipping"
    ));

    private static final Set<String> KNOWN_OPTIONS = new HashSet<>(Arrays.asList(
            "bbox",
            "bbox-enlargement",
            "comment",
            "debug-file",
            "encoding",
            "file",
            "input",
            "label-position",
            "map-start-position",
            "map-start-zoom",
            "output",
            "polygon-clipping",
            "polylabel",
            "preferred-languages",
            "progress-logs",
            "simplification-factor",
            "simplification-max-zoom",
            "skip-invalid-relations",
            "tag-conf-file",
            "tag-values",
            "threads",
            "type",
            "way-clipping",
            "zoom-interval-conf"
    ));

    public static void main(String[] args) {
        int exitCode = new MapsforgeWriter().run(args);
        if (exitCode != 0) {
            System.exit(exitCode);
        }
    }

    int run(String[] args) {
        long started = WriterPerformance.now();
        try {
            Map<String, String> options = parseArguments(args);
            if (options.containsKey("help")) {
                printUsage();
                return 0;
            }

            File inputFile = requiredFile(options, "input");
            String output = firstPresent(options, "output", "file");
            if (output == null) {
                throw new IllegalArgumentException("missing required --output argument");
            }
            options.put("file", output);

            validateKnownOptions(options);

            MapWriterConfiguration configuration = createConfiguration(options);
            try (FileInputStream inputStream = new FileInputStream(inputFile)) {
                OsmosisReader reader = new OsmosisReader(inputStream);
                reader.setSink(new MapFileWriterTask(configuration));
                reader.run();
            }

            WriterPerformance.logPhase(LOGGER, "standalone-total", started);
            return 0;
        } catch (IllegalArgumentException e) {
            System.err.println(e.getMessage());
            printUsage();
            return 2;
        } catch (IOException e) {
            e.printStackTrace(System.err);
            return 1;
        } catch (RuntimeException e) {
            e.printStackTrace(System.err);
            return 1;
        }
    }

    private static MapWriterConfiguration createConfiguration(Map<String, String> options) {
        MapWriterConfiguration configuration = new MapWriterConfiguration();
        configuration.addOutputFile(value(options, "file", Constants.DEFAULT_PARAM_OUTFILE));
        configuration.setTagValues(booleanValue(options, "tag-values", false));
        configuration.loadTagMappingFile(value(options, "tag-conf-file", null));

        configuration.addMapStartPosition(value(options, "map-start-position", null));
        configuration.addMapStartZoom(value(options, "map-start-zoom", null));
        configuration.addBboxConfiguration(value(options, "bbox", null));
        configuration.addZoomIntervalConfiguration(value(options, "zoom-interval-conf", null));

        configuration.setComment(value(options, "comment", null));
        configuration.setDebugStrings(booleanValue(options, "debug-file", false));
        configuration.setPolygonClipping(booleanValue(options, "polygon-clipping", true));
        configuration.setPolylabel(booleanValue(options, "polylabel", false));
        configuration.setProgressLogs(booleanValue(options, "progress-logs", true));
        configuration.setWayClipping(booleanValue(options, "way-clipping", true));
        configuration.setLabelPosition(booleanValue(options, "label-position", false));
        configuration.setSimplification(doubleValue(options, "simplification-factor",
                Constants.DEFAULT_SIMPLIFICATION_FACTOR));
        configuration.setSimplificationMaxZoom((byte) intValue(options, "simplification-max-zoom",
                Constants.DEFAULT_SIMPLIFICATION_MAX_ZOOM));
        configuration.setSkipInvalidRelations(booleanValue(options, "skip-invalid-relations", false));
        configuration.setDataProcessorType(value(options, "type", Constants.DEFAULT_PARAM_TYPE));
        configuration.setBboxEnlargement(intValue(options, "bbox-enlargement",
                Constants.DEFAULT_PARAM_BBOX_ENLARGEMENT));
        configuration.addPreferredLanguages(value(options, "preferred-languages", null));
        configuration.addEncodingChoice(value(options, "encoding", Constants.DEFAULT_PARAM_ENCODING));
        configuration.setThreads(intValue(options, "threads", 1));
        configuration.validate();
        return configuration;
    }

    private static boolean booleanValue(Map<String, String> options, String name, boolean defaultValue) {
        String value = options.get(name);
        if (value == null) {
            return defaultValue;
        }
        if ("true".equalsIgnoreCase(value) || "yes".equalsIgnoreCase(value) || "1".equals(value)) {
            return true;
        }
        if ("false".equalsIgnoreCase(value) || "no".equalsIgnoreCase(value) || "0".equals(value)) {
            return false;
        }
        throw new IllegalArgumentException("argument --" + name + " must be true or false");
    }

    private static double doubleValue(Map<String, String> options, String name, double defaultValue) {
        String value = options.get(name);
        if (value == null) {
            return defaultValue;
        }
        try {
            return Double.parseDouble(value);
        } catch (NumberFormatException e) {
            throw new IllegalArgumentException("argument --" + name + " must be a number", e);
        }
    }

    private static String firstPresent(Map<String, String> options, String first, String second) {
        String value = options.get(first);
        return value != null ? value : options.get(second);
    }

    private static int intValue(Map<String, String> options, String name, int defaultValue) {
        String value = options.get(name);
        if (value == null) {
            return defaultValue;
        }
        try {
            return Integer.parseInt(value);
        } catch (NumberFormatException e) {
            throw new IllegalArgumentException("argument --" + name + " must be an integer", e);
        }
    }

    private static Map<String, String> parseArguments(String[] args) {
        Map<String, String> options = new HashMap<>();
        for (int i = 0; i < args.length; i++) {
            String argument = args[i];
            if ("--help".equals(argument) || "-h".equals(argument)) {
                options.put("help", "true");
                continue;
            }

            String name;
            String value;
            if (argument.startsWith("--")) {
                String raw = argument.substring(2);
                int equals = raw.indexOf('=');
                if (equals >= 0) {
                    name = raw.substring(0, equals);
                    value = raw.substring(equals + 1);
                } else {
                    name = raw;
                    if (BOOLEAN_OPTIONS.contains(name) && (i + 1 == args.length || args[i + 1].startsWith("--"))) {
                        value = "true";
                    } else {
                        if (i + 1 == args.length) {
                            throw new IllegalArgumentException("missing value for --" + name);
                        }
                        value = args[++i];
                    }
                }
            } else {
                int equals = argument.indexOf('=');
                if (equals < 0) {
                    throw new IllegalArgumentException("unexpected argument: " + argument);
                }
                name = argument.substring(0, equals);
                value = argument.substring(equals + 1);
            }

            if (name.isEmpty()) {
                throw new IllegalArgumentException("empty argument name");
            }
            options.put(name, value);
        }
        return options;
    }

    private static void printUsage() {
        System.err.println("Usage: java -jar mapsforge-map-writer-*-jar-with-dependencies.jar "
                + "--input input.osm.pbf --output output.map [writer options]");
        System.err.println("Writer options use the existing mapfile-writer names, for example:");
        System.err.println("  --bbox minLat,minLon,maxLat,maxLon --type ram --threads 1");
        System.err.println("  --tag-conf-file tag-mapping.xml --zoom-interval-conf 5,0,7,10,8,11,14,12,18");
    }

    private static File requiredFile(Map<String, String> options, String name) {
        String value = options.get(name);
        if (value == null || value.trim().isEmpty()) {
            throw new IllegalArgumentException("missing required --" + name + " argument");
        }
        File file = new File(value);
        if (!file.isFile()) {
            throw new IllegalArgumentException("--" + name + " does not point to a readable file: "
                    + file.getAbsolutePath());
        }
        return file;
    }

    private static void validateKnownOptions(Map<String, String> options) {
        for (String name : options.keySet()) {
            if (!KNOWN_OPTIONS.contains(name)) {
                throw new IllegalArgumentException("unknown argument --" + name);
            }
        }
    }

    private static String value(Map<String, String> options, String name, String defaultValue) {
        String value = options.get(name);
        return value != null ? value : defaultValue;
    }

    private MapsforgeWriter() {
    }
}
