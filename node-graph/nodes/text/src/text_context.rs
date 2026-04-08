use super::{Font, FontCache, TypesettingConfig};
use core::cell::RefCell;
use core_types::table::Table;
use glam::DVec2;
use hyphenation::{Hyphenator, Language, Load, Standard};
use parley::fontique::{Blob, FamilyId, FontInfo};
use parley::style::{OverflowWrap, StyleProperty};
use parley::{AlignmentOptions, FontContext, Layout, LayoutContext, LineHeight, PositionedLayoutItem};
use std::collections::HashMap;
use std::sync::OnceLock;
use vector_types::Vector;

use super::path_builder::PathBuilder;

thread_local! {
	static THREAD_TEXT: RefCell<TextContext> = RefCell::new(TextContext::default());
}

static EN_US_DICT: OnceLock<Standard> = OnceLock::new();

/// TODO: swap to per-language selection once multi-language support lands.
fn get_en_us_dict() -> Option<&'static Standard> {
	EN_US_DICT
		.get_or_init(|| Standard::from_embedded(Language::EnglishUS).expect("embed_all feature must be enabled"))
		.into()
}

/// Unified thread-local text processing context that combines font and layout management
/// for efficient text rendering operations.
#[derive(Default)]
pub struct TextContext {
	font_context: FontContext,
	layout_context: LayoutContext<()>,
	/// Cached font metadata for performance optimization
	font_info_cache: HashMap<Font, (FamilyId, FontInfo)>,
}

impl TextContext {
	/// Access the thread-local TextContext instance for text processing operations
	pub fn with_thread_local<F, R>(f: F) -> R
	where
		F: FnOnce(&mut TextContext) -> R,
	{
		THREAD_TEXT.with_borrow_mut(f)
	}

	/// Resolve a font and return its data as a Blob if available
	fn resolve_font_data<'a>(&self, font: &'a Font, font_cache: &'a FontCache) -> Option<(Blob<u8>, &'a Font)> {
		font_cache.get_blob(font)
	}

	/// Get or cache font information for a given font
	fn get_font_info(&mut self, font: &Font, font_data: &Blob<u8>) -> Option<(String, FontInfo)> {
		// Check if we already have the font info cached
		if let Some((family_id, font_info)) = self.font_info_cache.get(font)
			&& let Some(family_name) = self.font_context.collection.family_name(*family_id)
		{
			return Some((family_name.to_string(), font_info.clone()));
		}

		// Register the font and cache the info
		let families = self.font_context.collection.register_fonts(font_data.clone(), None);

		families.first().and_then(|(family_id, fonts_info)| {
			fonts_info.first().and_then(|font_info| {
				self.font_context.collection.family_name(*family_id).map(|family_name| {
					// Cache the font info for future use
					self.font_info_cache.insert(font.clone(), (*family_id, font_info.clone()));
					(family_name.to_string(), font_info.clone())
				})
			})
		})
	}

	/// Create a text layout using the specified font and typesetting configuration
	fn layout_text(&mut self, text: &str, font: &Font, font_cache: &FontCache, typesetting: TypesettingConfig) -> Option<Layout<()>> {
		// Note that the actual_font may not be the desired font if that font is not yet loaded.
		// It is important not to cache the default font under the name of another font.
		let (font_data, actual_font) = self.resolve_font_data(font, font_cache)?;
		let (font_family, font_info) = self.get_font_info(actual_font, &font_data)?;

		// Inject ZWSP so parley can wrap at meaningful boundaries.
		// If hyphenation is enabled, also inject soft hyphens at syllable boundaries because parley doesn't support hyphenation.
		let injected: String;
		let layout_text = if typesetting.max_width.is_some() {
			let semantic = inject_semantic_breaks(text);
			injected = if typesetting.hyphenate { apply_hyphenation(&semantic) } else { semantic };
			&injected
		} else {
			text
		};

		const DISPLAY_SCALE: f32 = 1.;

		// TODO: Replace this two-pass approach with a single style push once Parley adds native hyphenation support.
		let mut layout: Layout<()> = if typesetting.hyphenate && typesetting.max_width.is_some() {
			let hyphen_advance = {
				let mut h = build_parley_layout(&mut self.layout_context, &mut self.font_context, "-", typesetting, &font_family, &font_info, DISPLAY_SCALE);
				h.break_all_lines(None);
				h.lines().next().map(|l| l.metrics().advance).unwrap_or(0.)
			};
			let max_w_pass1 = typesetting.max_width.map(|mw| (mw as f32 - hyphen_advance).max(0.));
			let mut first = build_parley_layout(&mut self.layout_context, &mut self.font_context, layout_text, typesetting, &font_family, &font_info, DISPLAY_SCALE);
			first.break_all_lines(max_w_pass1);
			let resolved = resolve_hyphen_breaks(layout_text, &first);
			if resolved != layout_text {
				build_parley_layout(&mut self.layout_context, &mut self.font_context, &resolved, typesetting, &font_family, &font_info, DISPLAY_SCALE)
			} else {
				first
			}
		} else {
			build_parley_layout(&mut self.layout_context, &mut self.font_context, layout_text, typesetting, &font_family, &font_info, DISPLAY_SCALE)
		};

		layout.break_all_lines(typesetting.max_width.map(|mw| mw as f32));
		layout.align(typesetting.max_width.map(|max_w| max_w as f32), typesetting.align.into(), AlignmentOptions::default());

		Some(layout)
	}

	/// Convert text to vector paths using the specified font and typesetting configuration
	pub fn to_path<Upstream: Default + 'static>(&mut self, text: &str, font: &Font, font_cache: &FontCache, typesetting: TypesettingConfig, per_glyph_instances: bool) -> Table<Vector<Upstream>> {
		let Some(layout) = self.layout_text(text, font, font_cache, typesetting) else {
			return Table::new_from_element(Vector::default());
		};

		let mut path_builder = PathBuilder::new(per_glyph_instances, layout.scale() as f64);

		for line in layout.lines() {
			for item in line.items() {
				if let PositionedLayoutItem::GlyphRun(glyph_run) = item
					&& typesetting.max_height.filter(|&max_height| glyph_run.baseline() > max_height as f32).is_none()
				{
					path_builder.render_glyph_run(&glyph_run, typesetting.tilt, per_glyph_instances);
				}
			}
		}

		path_builder.finalize()
	}

	/// Calculate the bounding box of text using the specified font and typesetting configuration
	pub fn bounding_box(&mut self, text: &str, font: &Font, font_cache: &FontCache, typesetting: TypesettingConfig, for_clipping_test: bool) -> DVec2 {
		let Some(layout) = self.layout_text(text, font, font_cache, typesetting) else {
			return DVec2::ZERO;
		};

		let layout_width = layout.full_width() as f64;
		let layout_height = layout.height() as f64;

		if for_clipping_test {
			return DVec2::new(layout_width, layout_height);
		}

		let width = typesetting.max_width.unwrap_or(layout_width);
		let height = typesetting.max_height.unwrap_or(layout_height);

		DVec2::new(width, height)
	}

	/// Check if text lines are being clipped due to height constraints
	pub fn lines_clipping(&mut self, text: &str, font: &Font, font_cache: &FontCache, typesetting: TypesettingConfig) -> bool {
		let Some(max_height) = typesetting.max_height else { return false };
		let bounds = self.bounding_box(text, font, font_cache, typesetting, true);
		max_height < bounds.y
	}
}

/// Build a parley layout for the given text and typesetting configuration.
fn build_parley_layout(
	layout_ctx: &mut LayoutContext<()>,
	font_ctx: &mut FontContext,
	text: &str,
	typesetting: TypesettingConfig,
	font_family: &str,
	font_info: &FontInfo,
	display_scale: f32,
) -> Layout<()> {
	let mut b = layout_ctx.ranged_builder(font_ctx, text, display_scale, false);
	b.push_default(StyleProperty::FontSize(typesetting.font_size as f32));
	b.push_default(StyleProperty::LetterSpacing(typesetting.character_spacing as f32));
	b.push_default(StyleProperty::FontStack(parley::FontStack::Single(parley::FontFamily::Named(std::borrow::Cow::Owned(
		font_family.to_owned(),
	)))));
	b.push_default(StyleProperty::FontWeight(font_info.weight()));
	b.push_default(StyleProperty::FontStyle(font_info.style()));
	b.push_default(StyleProperty::FontWidth(font_info.width()));
	b.push_default(LineHeight::FontSizeRelative(typesetting.line_height_ratio as f32));
	b.push_default(StyleProperty::OverflowWrap(OverflowWrap::BreakWord));
	b.build(text)
}

/// Resolve hyphen breaks in the given text based on the layout.
/// Note: This function is a temporary solution until Parley adds native hyphenation support.
fn resolve_hyphen_breaks(text: &str, layout: &Layout<()>) -> String {
	const SOFT_HYPHEN: char = '\u{00AD}';
	if !text.contains(SOFT_HYPHEN) {
		return text.to_string();
	}

	let mut break_positions = std::collections::HashSet::<usize>::new();
	for line in layout.lines() {
		let range = line.text_range();
		if range.is_empty() {
			continue;
		}
		let line_text = &text[range.clone()];
		if line_text.ends_with(SOFT_HYPHEN) {
			break_positions.insert(range.end - SOFT_HYPHEN.len_utf8());
		}
	}

	let mut out = String::with_capacity(text.len());
	for (i, c) in text.char_indices() {
		if c == SOFT_HYPHEN {
			if break_positions.contains(&i) {
				out.push('-');
				out.push('\u{200B}');
			}
		} else {
			out.push(c);
		}
	}
	out
}

/// Apply hyphenation to the given text.
fn apply_hyphenation(text: &str) -> String {
	let Some(dict) = get_en_us_dict() else {
		return text.to_string();
	};
	const SOFT_HYPHEN: char = '\u{00AD}';
	let mut out = String::with_capacity(text.len() + text.len() / 8);
	let mut word_start: Option<usize> = None;

	fn push_hyphenated(out: &mut String, dict: &hyphenation::Standard, word: &str) {
		let mut segs = dict.hyphenate(word).into_iter().segments().peekable();
		while let Some(seg) = segs.next() {
			out.push_str(seg);
			if segs.peek().is_some() {
				out.push('\u{00AD}');
			}
		}
	}

	for (i, c) in text.char_indices() {
		if c.is_alphabetic() {
			word_start.get_or_insert(i);
		} else {
			if let Some(start) = word_start.take() {
				push_hyphenated(&mut out, dict, &text[start..i]);
			}
			out.push(c);
		}
	}
	if let Some(start) = word_start {
		push_hyphenated(&mut out, dict, &text[start..]);
	}
	out
}

// TODO: Remove this function once Parley gains native handling for such cases.
fn inject_semantic_breaks(text: &str) -> String {
	const ZWSP: char = '\u{200B}';

	let mut out = String::with_capacity(text.len() + text.len() / 4);
	let mut chars = text.chars().peekable();
	let mut prev: Option<char> = None;

	while let Some(c) = chars.next() {
		let next = chars.peek().copied();

		let pd = prev.is_some_and(|p| p.is_ascii_digit());
		let nd = next.is_some_and(|n| n.is_ascii_digit());
		let need_before = prev.is_some_and(|p| p != ZWSP && !p.is_whitespace());

		match c {
			'\u{00A0}' | '\u{2011}' | '\u{200B}' | '\u{00AD}' => out.push(c),

			'!' | ')' | ']' | '\u{00BB}' => {
				if need_before {
					out.push(ZWSP);
				}
				out.push(c);
			}

			'/' | '&' | '=' | '#' | '~' | '@' => {
				out.push(c);
				out.push(ZWSP);
			}

			'_' => {
				out.push(c);
				if !pd && !nd {
					out.push(ZWSP);
				}
			}

			'-' => {
				out.push(c);
				if prev.is_some_and(|p| p.is_alphabetic()) && next.is_some_and(|n| n.is_alphabetic()) {
					out.push(ZWSP);
				}
			}

			'?' => {
				if next.is_some_and(|n| !n.is_whitespace()) {
					out.push(c);
					out.push(ZWSP);
				} else {
					if need_before {
						out.push(ZWSP);
					}
					out.push(c);
				}
			}

			'.' => {
				if pd && nd {
					out.push(c);
				} else if next.is_none_or(|n| n.is_whitespace()) {
					if need_before {
						out.push(ZWSP);
					}
					out.push(c);
				} else {
					out.push(c);
					out.push(ZWSP);
				}
			}

			',' => {
				if !(pd && nd) && need_before {
					out.push(ZWSP);
				}
				out.push(c);
			}

			_ => out.push(c),
		}

		prev = Some(c);
	}

	out
}
