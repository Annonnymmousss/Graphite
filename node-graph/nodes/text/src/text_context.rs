use super::{Font, FontCache, TypesettingConfig};
use core::cell::RefCell;
use core_types::table::Table;
use glam::DVec2;
use parley::fontique::{Blob, FamilyId, FontInfo};
use parley::style::{OverflowWrap, StyleProperty};
use parley::{AlignmentOptions, FontContext, Layout, LayoutContext, LineHeight, PositionedLayoutItem};
use std::collections::HashMap;
use vector_types::Vector;

use super::path_builder::PathBuilder;

thread_local! {
	static THREAD_TEXT: RefCell<TextContext> = RefCell::new(TextContext::default());
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

		// Inject ZWSP so parley can wrap at meaningful boundaries
		let injected: String;
		let layout_text = if typesetting.max_width.is_some() {
			injected = inject_semantic_breaks(text);
			&injected
		} else {
			text
		};

		const DISPLAY_SCALE: f32 = 1.;
		let mut builder = self.layout_context.ranged_builder(&mut self.font_context, layout_text, DISPLAY_SCALE, false);

		builder.push_default(StyleProperty::FontSize(typesetting.font_size as f32));
		builder.push_default(StyleProperty::LetterSpacing(typesetting.character_spacing as f32));
		builder.push_default(StyleProperty::FontStack(parley::FontStack::Single(parley::FontFamily::Named(std::borrow::Cow::Owned(font_family)))));
		builder.push_default(StyleProperty::FontWeight(font_info.weight()));
		builder.push_default(StyleProperty::FontStyle(font_info.style()));
		builder.push_default(StyleProperty::FontWidth(font_info.width()));
		builder.push_default(LineHeight::FontSizeRelative(typesetting.line_height_ratio as f32));
		// Safety-net: if a token has no ZWSP and still overflows, break at any character boundary.
		builder.push_default(StyleProperty::OverflowWrap(OverflowWrap::BreakWord));

		let mut layout: Layout<()> = builder.build(layout_text);

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

/// Inject U+200B (ZWSP) at semantic line-break opportunities so parley can wrap
/// at meaningful boundaries without a user-facing parameter.
///
/// Rules applied:
/// - U+00A0 (non-breaking space) and U+2011 (non-breaking hyphen): never a break point.
/// - U+200B (ZWSP) and U+00AD (soft hyphen): passed through unchanged.
/// - Numbers: `,` and `.` between digits are protected; digit→letter (e.g. `100px`) is protected.
/// - `.` between non-digits: ZWSP after (URL / email domain separator).
/// - `,` not flanked by digits on both sides: ZWSP before (trailing punctuation).
/// - `?` followed by a non-whitespace char: ZWSP after (URL query); otherwise ZWSP before.
/// - `!` `)` `]` `»`: ZWSP before (pure trailing punctuation).
/// - `/` `&` `=` `#` `~`: ZWSP after (URL separators).
/// - `_` between non-digits: ZWSP after (path/identifier separator).
/// - `@`: ZWSP after (email separator).
/// - `-` between two letters: ZWSP after (compound-word hyphen).
fn inject_semantic_breaks(text: &str) -> String {
	const ZWSP: char = '\u{200B}';

	let chars: Vec<char> = text.chars().collect();
	let n = chars.len();
	let mut out = String::with_capacity(text.len() + n / 4);

	for i in 0..n {
		let c = chars[i];
		let prev = (i > 0).then(|| chars[i - 1]);
		let next = (i + 1 < n).then(|| chars[i + 1]);
		
		let pd = prev.is_some_and(|p| p.is_ascii_digit());
		let nd = next.is_some_and(|n| n.is_ascii_digit());
		let need_before = prev.is_some_and(|p| p != ZWSP && !p.is_whitespace());

		match c {
			// Pass through: never a break point or already carry break semantics.
			'\u{00A0}' | '\u{2011}' | '\u{200B}' | '\u{00AD}' => out.push(c),

			// Pure trailing punctuation: ZWSP before.
			'!' | ')' | ']' | '\u{00BB}' => {
				if need_before { out.push(ZWSP); }
				out.push(c);
			}

			// URL / email separators: ZWSP after.
			'/' | '&' | '=' | '#' | '~' | '@' => { out.push(c); out.push(ZWSP); }

			// Underscore: path separator only between non-digits.
			'_' => {
				out.push(c);
				if !pd && !nd { out.push(ZWSP); }
			}

			// Hyphen: ZWSP after only between two letters (compound word).
			'-' => {
				out.push(c);
				if prev.is_some_and(|p| p.is_alphabetic()) && next.is_some_and(|n| n.is_alphabetic()) {
					out.push(ZWSP);
				}
			}

			// Question mark: ZWSP after in URL context, before at sentence end.
			'?' => {
				if next.is_some_and(|n| !n.is_whitespace()) {
					out.push(c); out.push(ZWSP);
				} else {
					if need_before { out.push(ZWSP); }
					out.push(c);
				}
			}

			// Dot: protected inside numbers; ZWSP after for URL/email; ZWSP before at sentence end.
			'.' => {
				if pd && nd { out.push(c); }                               // 3.14 — no break
				else if next.is_none_or(|n| n.is_whitespace()) {          // sentence-ending dot
					if need_before { out.push(ZWSP); }
					out.push(c);
				} else { out.push(c); out.push(ZWSP); }                   // URL/email separator
			}

			// Comma: protected inside numbers; ZWSP before otherwise.
			',' => {
				if !(pd && nd) && need_before { out.push(ZWSP); }
				out.push(c);
			}

			_ => out.push(c),
		}
	}

	out
}

#[cfg(test)]
mod tests {
	use super::inject_semantic_breaks;

	fn zwsp_positions(s: &str) -> Vec<usize> {
		s.char_indices().filter(|(_, c)| *c == '\u{200B}').map(|(i, _)| i).collect()
	}

	fn has_zwsp_after(result: &str, pattern: char) -> bool {
		let mut chars = result.chars().peekable();
		while let Some(c) = chars.next() {
			if c == pattern {
				if chars.peek() == Some(&'\u{200B}') {
					return true;
				}
			}
		}
		false
	}

	fn has_zwsp_before(result: &str, pattern: char) -> bool {
		let chars: Vec<char> = result.chars().collect();
		for i in 1..chars.len() {
			if chars[i] == pattern && chars[i - 1] == '\u{200B}' {
				return true;
			}
		}
		false
	}

	#[test]
	fn url_slash_gets_zwsp_after() {
		let out = inject_semantic_breaks("http://example.com/foo/bar");
		assert!(has_zwsp_after(&out, '/'), "expected ZWSP after /");
	}

	#[test]
	fn url_query_chars_get_zwsp() {
		let out = inject_semantic_breaks("a?b=c&d");
		assert!(has_zwsp_after(&out, '?'));
		assert!(has_zwsp_after(&out, '='));
		assert!(has_zwsp_after(&out, '&'));
	}

	#[test]
	fn email_at_and_dot_get_zwsp() {
		let out = inject_semantic_breaks("user@example.com");
		assert!(has_zwsp_after(&out, '@'));
		// `.com` — dot between letters should get ZWSP after
		assert!(has_zwsp_after(&out, '.'));
	}

	#[test]
	fn hyphen_compound_gets_zwsp_after() {
		let out = inject_semantic_breaks("state-of-the-art");
		assert!(has_zwsp_after(&out, '-'));
	}

	#[test]
	fn number_comma_no_zwsp() {
		let out = inject_semantic_breaks("1,000,000");
		assert!(zwsp_positions(&out).is_empty(), "no ZWSP inside number with commas: {out:?}");
	}

	#[test]
	fn number_decimal_no_zwsp() {
		let out = inject_semantic_breaks("3.14159");
		assert!(zwsp_positions(&out).is_empty(), "no ZWSP inside decimal: {out:?}");
	}

	#[test]
	fn number_currency_prefix_no_zwsp() {
		// $99.99 — the dot is between digits so no ZWSP
		let out = inject_semantic_breaks("$99.99");
		assert!(!has_zwsp_after(&out, '.'), "no ZWSP inside $99.99");
	}

	#[test]
	fn mixed_number_and_text_no_zwsp_in_number() {
		let out = inject_semantic_breaks("Price: $1,000,000.00");
		// The number portion must contain no ZWSP at comma or decimal positions
		let without_prefix = &out["Price: $".len()..];
		assert!(!has_zwsp_after(without_prefix, ','));
		assert!(!has_zwsp_after(without_prefix, '.'));
	}

	#[test]
	fn non_breaking_space_no_zwsp() {
		let input = "hello\u{00A0}world";
		let out = inject_semantic_breaks(input);
		// Non-breaking space must be preserved; no ZWSP anywhere.
		assert!(out.contains('\u{00A0}'));
		assert!(zwsp_positions(&out).is_empty());
	}

	#[test]
	fn non_breaking_hyphen_no_zwsp() {
		let input = "hello\u{2011}world";
		let out = inject_semantic_breaks(input);
		assert!(out.contains('\u{2011}'));
		assert!(zwsp_positions(&out).is_empty());
	}

	#[test]
	fn existing_zwsp_not_doubled() {
		let input = "foo\u{200B}bar";
		let out = inject_semantic_breaks(input);
		// The existing ZWSP is passed through; no second one added.
		let zwsps: Vec<_> = out.chars().filter(|c| *c == '\u{200B}').collect();
		assert_eq!(zwsps.len(), 1);
	}

	#[test]
	fn soft_hyphen_passed_through() {
		let input = "hyph\u{00AD}en";
		let out = inject_semantic_breaks(input);
		assert!(out.contains('\u{00AD}'));
	}

	#[test]
	fn trailing_punct_gets_zwsp_before() {
		for p in [',', '.', '!', '?', ')', ']'] {
			// Use a non-numeric context so number protection doesn't apply.
			let input = format!("hello{p}");
			let out = inject_semantic_breaks(&input);
			if p == ',' || p == '.' {
				// These are trailing-punct only when not between digits.
				// "hello" ends with 'o' (letter), so ZWSP before.
				assert!(has_zwsp_before(&out, p), "expected ZWSP before '{p}' in: {out:?}");
			} else {
				assert!(has_zwsp_before(&out, p), "expected ZWSP before '{p}' in: {out:?}");
			}
		}
	}

	#[test]
	fn no_injection_without_max_width() {
		// When max_width is None, inject_semantic_breaks is never called.
		// We test the function alone: even called, it must not mangle plain text.
		let input = "hello world";
		let out = inject_semantic_breaks(input);
		// Plain text with no special chars must be returned as-is.
		assert_eq!(out, input);
	}

	#[test]
	fn hyphen_between_digits_no_zwsp() {
		let out = inject_semantic_breaks("2024-01-01");
		// Hyphens between digits (date) must not gain ZWSP.
		assert!(!has_zwsp_after(&out, '-'));
	}
}
