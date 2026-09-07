use super::text_context::TextContext;
use super::{TypesettingConfig, text_item_font};
use core_types::blending::BlendMode;
use core_types::list::{Item, List, NodeIdPath};
use core_types::{ATTR_BLEND_MODE, ATTR_EDITOR_LAYER_PATH, ATTR_OPACITY, ATTR_OPACITY_FILL, ATTR_TRANSFORM};
use glam::{DAffine2, DVec2};
use graphene_resource::Resource;
use vector_types::Vector;

pub fn to_path(text: &str, font: &Resource, typesetting: TypesettingConfig, per_glyph_items: bool) -> List<Vector> {
	TextContext::with_thread_local(|ctx| ctx.to_path(text, font, typesetting, per_glyph_items))
}

pub fn bounding_box(text: &str, font: &Resource, typesetting: TypesettingConfig, for_clipping_test: bool) -> DVec2 {
	TextContext::with_thread_local(|ctx| ctx.bounding_box(text, font, typesetting, for_clipping_test))
}

pub fn lines_clipping(text: &str, font: &Resource, typesetting: TypesettingConfig) -> bool {
	TextContext::with_thread_local(|ctx| ctx.lines_clipping(text, font, typesetting))
}

/// Shapes a single styled string item into vector geometry, reading its font and typesetting from the item's
/// attributes (as set by the 'Text' node) and re-applying its transform and blending attributes onto the produced
/// paths. With `separate_glyphs`, each glyph becomes its own item; otherwise a single compound path is produced.
pub fn shape_text_item(item: &Item<String>, separate_glyphs: bool) -> List<Vector> {
	let text = item.element();
	if text.is_empty() {
		return List::new();
	}

	let vectors = to_path(text, &text_item_font(item), TypesettingConfig::from_text_item(item), separate_glyphs);
	let transform = item.attribute_cloned_or_default::<DAffine2>(ATTR_TRANSFORM);
	let layer_path = item.attribute::<NodeIdPath>(ATTR_EDITOR_LAYER_PATH).cloned();
	let blend_mode = item.attribute::<BlendMode>(ATTR_BLEND_MODE).copied();
	let opacity = item.attribute::<f64>(ATTR_OPACITY).copied();
	let opacity_fill = item.attribute::<f64>(ATTR_OPACITY_FILL).copied();

	let mut result = List::new();
	for mut produced in vectors.into_iter() {
		if transform != DAffine2::IDENTITY {
			let local = produced.attribute_cloned_or_default::<DAffine2>(ATTR_TRANSFORM);
			produced.set_attribute(ATTR_TRANSFORM, transform * local);
		}
		if let Some(layer_path) = &layer_path {
			produced.set_attribute(ATTR_EDITOR_LAYER_PATH, layer_path.clone());
		}
		if let Some(blend_mode) = blend_mode {
			produced.set_attribute(ATTR_BLEND_MODE, blend_mode);
		}
		if let Some(opacity) = opacity {
			produced.set_attribute(ATTR_OPACITY, opacity);
		}
		if let Some(opacity_fill) = opacity_fill {
			produced.set_attribute(ATTR_OPACITY_FILL, opacity_fill);
		}
		result.push(produced);
	}

	result
}

/// Shapes each string item of a styled `List<String>` into vector geometry, flattening the per-item results.
pub fn shape_text_list(strings: &List<String>, separate_glyphs: bool) -> List<Vector> {
	let mut result = List::new();

	for index in 0..strings.len() {
		let Some(item) = strings.clone_item(index) else { continue };
		for produced in shape_text_item(&item, separate_glyphs).into_iter() {
			result.push(produced);
		}
	}

	result
}
