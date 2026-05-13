use egui::epaint::{RectShape, StrokeKind};
use egui::*;

pub fn plot_barchart(
    ui: &mut egui::Ui,
    size: Vec2,
    values: &[f32],
    top_value: f32,
    _value_unit: &'static str,
    _value_decimals: usize,
) -> egui::Response {
    let (rect, response) = ui.allocate_at_least(size, Sense::hover());
    let style = ui.style().noninteractive();

    let mut shapes = vec![Shape::Rect(RectShape::new(
        rect,
        style.corner_radius,
        ui.visuals().extreme_bg_color,
        style.bg_stroke,
        StrokeKind::Outside,
    ))];

    let rect = rect.shrink(4.0);
    let half_bar_width = rect.width() / values.len() as f32 * 0.5;

    for (i, &value) in values.iter().rev().enumerate() {
        let x = remap(i as f32, values.len() as f32..=0.0, rect.x_range());
        let x_min = ui.painter().round_to_pixel(x - half_bar_width);
        let x_max = ui.painter().round_to_pixel(x + half_bar_width);
        let y = remap_clamp(value, 0.0..=top_value, rect.bottom_up_range());
        let bar = Rect {
            min: pos2(x_min, y),
            max: pos2(x_max, rect.bottom()),
        };

        let fill_color = if ui.rect_contains_pointer(bar) {
            ui.visuals().text_color()
        } else {
            ui.visuals().weak_text_color()
        };

        shapes.push(Shape::Rect(RectShape::filled(bar, CornerRadius::ZERO, fill_color)));
    }

    ui.painter().extend(shapes);

    response
}
