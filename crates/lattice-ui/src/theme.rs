/// Color theme for phext-edit.

use gpui::Hsla;

pub fn bg() -> Hsla { gpui::hsla(0.0, 0.0, 0.09, 1.0) }
pub fn panel_bg() -> Hsla { gpui::hsla(0.0, 0.0, 0.07, 1.0) }
pub fn bar_bg() -> Hsla { gpui::hsla(0.0, 0.0, 0.11, 1.0) }
pub fn status_bg() -> Hsla { gpui::hsla(0.0, 0.0, 0.06, 1.0) }
pub fn border() -> Hsla { gpui::hsla(0.0, 0.0, 0.18, 1.0) }
pub fn text() -> Hsla { gpui::hsla(0.0, 0.0, 0.87, 1.0) }
pub fn dim_text() -> Hsla { gpui::hsla(0.0, 0.0, 0.50, 1.0) }
pub fn coord_text() -> Hsla { gpui::hsla(0.15, 0.8, 0.65, 1.0) }
pub fn z_color() -> Hsla { gpui::hsla(0.52, 0.7, 0.60, 1.0) }
pub fn y_color() -> Hsla { gpui::hsla(0.83, 0.6, 0.60, 1.0) }
pub fn x_color() -> Hsla { gpui::hsla(0.35, 0.6, 0.55, 1.0) }

/// Dimension arm color for sentron panel rendering.
#[derive(Debug, Clone, Copy)]
pub enum ArmColor {
    Z,        // Cyan — spatial
    Y,        // Magenta — temporal
    X,        // Green — content
    Neutral,  // Dim gray
}

impl ArmColor {
    pub fn to_hsla(self) -> Hsla {
        match self {
            ArmColor::Z => z_color(),
            ArmColor::Y => y_color(),
            ArmColor::X => x_color(),
            ArmColor::Neutral => dim_text(),
        }
    }
}
