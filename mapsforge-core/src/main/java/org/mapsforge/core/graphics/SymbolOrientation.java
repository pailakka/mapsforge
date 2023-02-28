/*
 * Copyright mapsforge.org
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
package org.mapsforge.core.graphics;

public enum SymbolOrientation {

    /**
     * Automatic rotation of symbols along the line with requirement to be always readable.
     */
    AUTO,

    /**
     * Same as "AUTO" just everything is upside-down.
     */
    AUTO_DOWN,

    /**
     * Rotate the symbols up when the path segment (where the text is to be placed) is facing east/right.
     */
    RIGHT,

    /**
     * Rotate symbol up when path segment the text goes to the west/left.
     */
    LEFT,

    /**
     * All symbols are rotated always "up".
     */
    UP,

    /**
     * All symbols are rotated always "down".
     */
    DOWN;

    public static SymbolOrientation fromString(String value) {
        switch (value) {
            case "auto_down":
                return AUTO_DOWN;
            case "right":
                return RIGHT;
            case "left":
                return LEFT;
            case "up":
                return UP;
            case "down":
                return DOWN;
            default: // "auto"
                return AUTO;
        }
    }
}
