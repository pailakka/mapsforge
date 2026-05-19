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
package org.mapsforge.map.writer.util;

import java.util.Locale;
import java.util.logging.Logger;

/**
 * Centralizes performance log formatting so benchmark scripts can parse stable phase names.
 */
public final class WriterPerformance {
    private static final double NANOS_PER_MILLI = 1000000d;

    public static long now() {
        return System.nanoTime();
    }

    public static void logCount(Logger logger, String name, long value) {
        logger.info("writer-count name=" + name + " value=" + value);
    }

    public static void logPhase(Logger logger, String phase, long startedNanos) {
        logPhaseNanos(logger, phase, System.nanoTime() - startedNanos);
    }

    public static void logPhaseNanos(Logger logger, String phase, long nanos) {
        logger.info(String.format(Locale.ROOT, "writer-phase name=%s millis=%.3f", phase, nanos / NANOS_PER_MILLI));
    }

    private WriterPerformance() {
        throw new IllegalStateException();
    }
}
