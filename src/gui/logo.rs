use egui::{Color32, Painter, Pos2, Rect, Shape, Stroke, pos2, vec2};

const MAROON: Color32 = Color32::from_rgb(114, 44, 63);
const CREAM: Color32 = Color32::from_rgb(255, 245, 230);

pub(super) fn paint(painter: &Painter, rect: Rect) {
    let side = rect.width().min(rect.height());
    let origin = rect.center() - vec2(side, side) * 0.5;
    let scale = side / 32.0;
    let point = |x, y| origin + vec2(x, y) * scale;
    let block = |x, y, w, h| Rect::from_min_size(point(x, y), vec2(w, h) * scale);

    // Keep this geometry aligned with assets/rustide-mark.svg.
    painter.rect_filled(block(1.0, 1.0, 30.0, 30.0), 8.0 * scale, MAROON);
    for (x, y, w, h) in [
        (7.5, 9.5, 2.0, 2.0),
        (7.5, 17.5, 2.0, 2.0),
        (9.0, 8.0, 4.0, 17.0),
    ] {
        painter.rect_filled(block(x, y, w, h), 0.0, CREAM);
    }

    let mut bowl = vec![pos2(11.0, 8.0), pos2(16.8, 8.0)];
    curve(
        &mut bowl,
        [[16.8, 8.0], [22.1, 8.0], [24.0, 10.2], [24.0, 14.2]],
    );
    curve(
        &mut bowl,
        [[24.0, 14.2], [24.0, 18.4], [21.8, 20.1], [16.8, 20.1]],
    );
    bowl.push(pos2(11.0, 20.1));
    polygon(painter, &bowl, origin, scale, CREAM);
    polygon(
        painter,
        &[
            pos2(16.0, 18.5),
            pos2(20.0, 18.5),
            pos2(25.0, 25.0),
            pos2(20.3, 25.0),
        ],
        origin,
        scale,
        CREAM,
    );
    painter.circle_filled(point(23.0, 24.5), 2.5 * scale, CREAM);

    let mut counter = vec![pos2(13.0, 11.7), pos2(16.8, 11.7)];
    curve(
        &mut counter,
        [[16.8, 11.7], [19.3, 11.7], [20.2, 12.5], [20.2, 14.2]],
    );
    curve(
        &mut counter,
        [[20.2, 14.2], [20.2, 16.0], [19.3, 16.6], [16.8, 16.6]],
    );
    counter.push(pos2(13.0, 16.6));
    polygon(painter, &counter, origin, scale, MAROON);
    painter.circle_filled(point(23.0, 24.5), scale, MAROON);
}

fn polygon(painter: &Painter, points: &[Pos2], origin: Pos2, scale: f32, color: Color32) {
    painter.add(Shape::convex_polygon(
        points
            .iter()
            .map(|point| origin + point.to_vec2() * scale)
            .collect(),
        color,
        Stroke::NONE,
    ));
}

fn curve(path: &mut Vec<Pos2>, points: [[f32; 2]; 4]) {
    for step in 1..=16 {
        let t = step as f32 / 16.0;
        let u = 1.0 - t;
        let weights = [u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t];
        let coordinates: [f32; 2] =
            std::array::from_fn(|axis| (0..4).map(|i| weights[i] * points[i][axis]).sum());
        path.push(pos2(coordinates[0], coordinates[1]));
    }
}
