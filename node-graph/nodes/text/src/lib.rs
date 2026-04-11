mod font_cache;
mod path_builder;
mod text_context;
mod to_path;

use dyn_any::DynAny;
pub use font_cache::*;
pub use text_context::TextContext;
pub use to_path::*;

// Re-export for convenience
pub use core_types as gcore;
pub use vector_types;

/// Horizontal alignment of the main body of a text block.
#[repr(C)]
#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, Hash, DynAny, node_macro::ChoiceType)]
#[widget(Radio)]
pub enum TextAlign {
	#[default]
	Left,
	Center,
	Right,
	#[label("Justify")]
	JustifyLeft,
}

impl From<TextAlign> for parley::Alignment {
	fn from(val: TextAlign) -> Self {
		match val {
			TextAlign::Left => parley::Alignment::Left,
			TextAlign::Center => parley::Alignment::Center,
			TextAlign::Right => parley::Alignment::Right,
			TextAlign::JustifyLeft => parley::Alignment::Justify,
		}
	}
}

impl TextAlign {
	/// Returns `true` if this is the justify variant.
	pub fn is_justify(self) -> bool {
		self == Self::JustifyLeft
	}
}

/// Alignment of the last line of a justified paragraph.
/// Only applies when the main alignment is `TextAlign::JustifyLeft`.
#[repr(C)]
#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, Hash, DynAny, node_macro::ChoiceType)]
#[widget(Radio)]
pub enum LastLineAlign {
	#[default]
	Left,
	Center,
	Right,
}

impl LastLineAlign {
	/// Returns the `parley::Alignment` correction needed for the last line, or `None` when no
	/// correction is needed (left-align is already parley's default justify behaviour).
	pub fn last_line_correction(self) -> Option<parley::Alignment> {
		match self {
			Self::Left => None,
			Self::Center => Some(parley::Alignment::Center),
			Self::Right => Some(parley::Alignment::Right),
		}
	}
}

#[derive(PartialEq, Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct TypesettingConfig {
	pub font_size: f64,
	pub line_height_ratio: f64,
	pub character_spacing: f64,
	pub max_width: Option<f64>,
	pub max_height: Option<f64>,
	pub tilt: f64,
	pub align: TextAlign,
	pub last_line_align: LastLineAlign,
}

impl Default for TypesettingConfig {
	fn default() -> Self {
		Self {
			font_size: 24.,
			line_height_ratio: 1.2,
			character_spacing: 0.,
			max_width: None,
			max_height: None,
			tilt: 0.,
			align: TextAlign::default(),
			last_line_align: LastLineAlign::default(),
		}
	}
}
