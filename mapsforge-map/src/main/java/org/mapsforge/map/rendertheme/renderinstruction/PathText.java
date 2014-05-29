/*
 * Copyright 2010, 2011, 2012, 2013 mapsforge.org
 * Copyright 2014 Ludwig M Brinckmann
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
package org.mapsforge.map.rendertheme.renderinstruction;

import org.mapsforge.core.graphics.Paint;
import org.mapsforge.core.model.Tile;
import org.mapsforge.map.layer.renderer.PolylineContainer;
import org.mapsforge.map.reader.PointOfInterest;
import org.mapsforge.map.rendertheme.RenderCallback;

/**
 * Represents a text along a polyline on the map.
 */
public class PathText extends RenderInstruction {
	private final float dy;
	private final Paint fill;
	private final float fontSize;
	private final int priority;
	private final Paint stroke;
	private final TextKey textKey;

	PathText(PathTextBuilder pathTextBuilder) {
		super(pathTextBuilder.getCategory());
		this.dy = pathTextBuilder.dy;
		this.fill = pathTextBuilder.fill;
		this.fontSize = pathTextBuilder.fontSize;
		this.priority = pathTextBuilder.priority;
		this.stroke = pathTextBuilder.stroke;
		this.textKey = pathTextBuilder.textKey;
	}

	@Override
	public void destroy() {
		// no-op
	}

	@Override
	public void renderNode(RenderCallback renderCallback, PointOfInterest poi, Tile tile) {
		// do nothing
	}

	@Override
	public void renderWay(RenderCallback renderCallback, PolylineContainer way) {
		String caption = this.textKey.getValue(way.getTags());
		if (caption == null) {
			return;
		}
		renderCallback.renderWayText(way, priority, caption, this.dy, this.fill, this.stroke);
	}

	@Override
	public void scaleStrokeWidth(float scaleFactor) {
		// do nothing
	}

	@Override
	public void scaleTextSize(float scaleFactor) {
		this.fill.setTextSize(this.fontSize * scaleFactor);
		this.stroke.setTextSize(this.fontSize * scaleFactor);
	}
}