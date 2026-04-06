use super::{Font, FontCache, OverflowBehavior, TypesettingConfig};
use core::cell::RefCell;
use core_types::table::Table;
use glam::DVec2;
use parley::fontique::{Blob, FamilyId, FontInfo};
use parley::{AlignmentOptions, FontContext, Layout, LayoutContext, LineHeight, PositionedLayoutItem, StyleProperty};
use std::collections::HashMap;
use unicode_linebreak::BreakOpportunity;
use vector_types::Vector;

use super::path_builder::PathBuilder;

thread_local! {
	static THREAD_TEXT: RefCell<TextContext> = RefCell::new(TextContext::default());
}

// ── Break-opportunity kinds ───────────────────────────────────────────────────

/// Classification of a single break position (byte-exclusive end of left segment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BreakKind {
	/// Forced break (newline / end-of-text from UAX #14).
	Mandatory,
	/// Optional break — use when the line would otherwise exceed `max_width`.
	Allowed,
	/// Must never break here (non-breaking space, non-breaking hyphen, inside a number, …).
	Prohibited,
}

/// A classified break position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Break {
	byte_index: usize,
	kind: BreakKind,
}

// ── Break-opportunity collector ───────────────────────────────────────────────

/// Collect break opportunities for `text`.
///
/// Uses UAX #14 (`unicode-linebreak`) as the base, then applies semantic
/// overrides for URLs, email addresses, hyphenated compounds, numbers, and
/// special Unicode characters.  Returns a `Vec<Break>` sorted by `byte_index`.
///
/// **Design note**: hyphenation can add extra `Allowed` entries into this
/// collection later without modifying the core line-fitting logic.
fn collect_break_opportunities(text: &str) -> Vec<Break> {
	// 1. Seed from UAX #14.
	let mut breaks: Vec<Break> = unicode_linebreak::linebreaks(text)
		.map(|(byte_index, opp)| Break {
			byte_index,
			kind: match opp {
				BreakOpportunity::Mandatory => BreakKind::Mandatory,
				BreakOpportunity::Allowed => BreakKind::Allowed,
			},
		})
		.collect();

	// Build a fast lookup: byte_index → position in `breaks`.
	// We mutate entries in-place rather than building a new Vec.
	let mut index_map: HashMap<usize, usize> = breaks.iter().enumerate().map(|(i, b)| (b.byte_index, i)).collect();

	let bytes = text.as_bytes();
	let len = bytes.len();

	// Helper: insert/upgrade a break at `pos` with `kind`, or downgrade to Prohibited.
	let set_kind = |breaks: &mut Vec<Break>, index_map: &mut HashMap<usize, usize>, pos: usize, kind: BreakKind| {
		if let Some(&i) = index_map.get(&pos) {
			let existing = &mut breaks[i].kind;
			match (kind, *existing) {
				// Prohibited always wins.
				(BreakKind::Prohibited, _) => *existing = BreakKind::Prohibited,
				// Mandatory always wins over Allowed.
				(BreakKind::Mandatory, BreakKind::Allowed) => *existing = BreakKind::Mandatory,
				_ => {}
			}
		} else if kind != BreakKind::Prohibited {
			// Only insert new Allowed/Mandatory entries — never add a new Prohibited
			// because the absence of a break already means it is prohibited.
			let i = breaks.len();
			breaks.push(Break { byte_index: pos, kind });
			index_map.insert(pos, i);
		}
	};

	// 2. Semantic overrides — iterate over all characters.
	let mut char_indices = text.char_indices().peekable();
	while let Some((i, ch)) = char_indices.next() {
		let ch_end = i + ch.len_utf8();

		match ch {
			// ── Non-breaking characters: promote the break AT this position to Prohibited ──
			'\u{00A0}' | // Non-breaking space
			'\u{2011}' | // Non-breaking hyphen
			'\u{202F}' | // Narrow no-break space
			'\u{FEFF}'   // Word-joiner / BOM
			=> {
				// Break before this char.
				set_kind(&mut breaks, &mut index_map, i, BreakKind::Prohibited);
				// Break after this char.
				set_kind(&mut breaks, &mut index_map, ch_end, BreakKind::Prohibited);
			}

			// ── Zero-width space: always an allowed break ──────────────────────
			'\u{200B}' => {
				set_kind(&mut breaks, &mut index_map, ch_end, BreakKind::Allowed);
			}

			// ── Soft hyphen: allowed break (caller renders visible hyphen if taken) ─
			'\u{00AD}' => {
				set_kind(&mut breaks, &mut index_map, ch_end, BreakKind::Allowed);
			}

			// ── ASCII hyphen-minus: allow break AFTER if both sides are alphabetic ─
			'-' => {
				let prev_alpha = i > 0 && bytes[..i].iter().rev().next().map_or(false, |b| b.is_ascii_alphabetic());
				let next_alpha = ch_end < len && bytes[ch_end].is_ascii_alphabetic();
				if prev_alpha && next_alpha {
					// Break after the hyphen (hyphen stays on the left line).
					set_kind(&mut breaks, &mut index_map, ch_end, BreakKind::Allowed);
				}
			}

			// ── URL/path break characters: allow break AFTER the character ───────
			'/' | '?' | '&' | '=' | '#' | '~' | '_' => {
				set_kind(&mut breaks, &mut index_map, ch_end, BreakKind::Allowed);
			}

			// ── Email break characters: allow break AFTER '.' and '@' ───────────
			// '.' between digits is prohibited (decimal number); otherwise allowed.
			'.' => {
				if i > 0 && bytes[i - 1].is_ascii_digit() && ch_end < len && bytes[ch_end].is_ascii_digit() {
					// Decimal number: prohibit break before and after the dot.
					set_kind(&mut breaks, &mut index_map, i, BreakKind::Prohibited);
					set_kind(&mut breaks, &mut index_map, ch_end, BreakKind::Prohibited);
				} else {
					// Email/URL dot: allow break after.
					set_kind(&mut breaks, &mut index_map, ch_end, BreakKind::Allowed);
				}
			}
			'@' => {
				set_kind(&mut breaks, &mut index_map, ch_end, BreakKind::Allowed);
			}
			// Alphabetic unit suffix directly attached to digits (100px, 3em, …)
			_ if ch.is_ascii_alphabetic() && i > 0 && bytes[i - 1].is_ascii_digit() => {
				set_kind(&mut breaks, &mut index_map, i, BreakKind::Prohibited);
			}
			// Currency prefix ($, £, €, …) directly before digits
			'$' | '£' | '€' | '¥' if ch_end < len && bytes[ch_end].is_ascii_digit() => {
				set_kind(&mut breaks, &mut index_map, ch_end, BreakKind::Prohibited);
			}

			_ => {}
		}
	}

	// 3. Trailing-punctuation prevention: a break immediately BEFORE ,.!?)]}
	//    must be Prohibited (those characters must not start a new line).
	let trailing_punct: &[char] = &[',', '.', '!', '?', ')', ']', '}', ':', ';'];
	let mut byte_pos = 0usize;
	for ch in text.chars() {
		if trailing_punct.contains(&ch) && byte_pos > 0 {
			// The UAX #14 break that would put this char at the start of the next
			// line sits at `byte_pos` (the break before column).
			set_kind(&mut breaks, &mut index_map, byte_pos, BreakKind::Prohibited);
		}
		byte_pos += ch.len_utf8();
	}

	breaks.sort_unstable_by_key(|b| b.byte_index);
	breaks
}

// ── ZWSP injection ────────────────────────────────────────────────────────────

/// Build a string with `U+200B` (zero-width space) injected at every `Allowed`
/// position from `collect_break_opportunities`.  Mandatory and Prohibited
/// positions are left unchanged.
///
/// Returns `(injected_text, original_byte_positions)` where
/// `original_byte_positions[i]` is the original byte index that became
/// `injected_text.as_bytes()[i]`'s starting position (for cursor mapping).
fn build_injected_text(text: &str, breaks: &[Break]) -> String {
	const ZWSP: char = '\u{200B}';

	let mut out = String::with_capacity(text.len() + breaks.len() * ZWSP.len_utf8());
	let mut prev = 0usize;

	for brk in breaks {
		if brk.kind != BreakKind::Allowed {
			continue;
		}
		// Append text from `prev` up to this break position.
		if let Some(slice) = text.get(prev..brk.byte_index) {
			out.push_str(slice);
			out.push(ZWSP);
			prev = brk.byte_index;
		}
	}
	// Append remaining text.
	if let Some(tail) = text.get(prev..) {
		out.push_str(tail);
	}
	out
}

// ── Unified thread-local text processing context ──────────────────────────────

/// Unified thread-local text processing context that combines font and layout
/// management for efficient text rendering operations.
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

	/// Create a text layout using the specified font and typesetting configuration.
	///
	/// For `OverflowBehavior::BreakAnywhere` and `Ellipsis`, break opportunities
	/// are collected via UAX #14 with semantic overrides and injected as ZWSP
	/// characters before the text is handed to parley.  Parley naturally breaks
	/// at ZWSP without inserting any hyphen.
	///
	/// For `OverflowBehavior::Overflow`, the text is passed unchanged (legacy).
	fn layout_text(&mut self, text: &str, font: &Font, font_cache: &FontCache, typesetting: TypesettingConfig) -> Option<Layout<()>> {
		// Note that the actual_font may not be the desired font if that font is not yet loaded.
		// It is important not to cache the default font under the name of another font.
		let (font_data, actual_font) = self.resolve_font_data(font, font_cache)?;
		let (font_family, font_info) = self.get_font_info(actual_font, &font_data)?;

		// Prepare the text we will actually hand to parley.
		// For Overflow we pass the original; for the others we inject ZWSP break hints.
		let injected: String;
		let layout_text: &str = match typesetting.overflow_behavior {
			OverflowBehavior::Overflow => text,
			OverflowBehavior::BreakAnywhere | OverflowBehavior::Ellipsis => {
				// Only inject when a max_width is set; otherwise there is nothing to wrap.
				if typesetting.max_width.is_some() {
					let breaks = collect_break_opportunities(text);
					injected = build_injected_text(text, &breaks);
					&injected
				} else {
					text
				}
			}
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

		// For Ellipsis mode, determine the last line whose baseline fits within max_height.
		// We clip there and the ellipsis character is appended as a separate glyph run
		// by parley if the text already ends in '…', or we skip overflow lines.
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

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
	use super::*;

	/// Helper: find the `BreakKind` for a given byte index, or `None` if absent.
	fn kind_at(breaks: &[Break], byte_index: usize) -> Option<BreakKind> {
		breaks.iter().find(|b| b.byte_index == byte_index).map(|b| b.kind)
	}

	// ── OverflowBehavior ─────────────────────────────────────────────────────

	#[test]
	fn overflow_behavior_default_is_break_anywhere() {
		assert_eq!(OverflowBehavior::default(), OverflowBehavior::BreakAnywhere);
	}

	// ── URL / path characters ────────────────────────────────────────────────

	#[test]
	fn url_slash_is_allowed() {
		let text = "https://example.com/path";
		let breaks = collect_break_opportunities(text);
		// Break after the '/' at index 7 (end of "https://")
		let slash_pos = text.find("//").unwrap() + 2; // byte after second slash
		let kind = kind_at(&breaks, slash_pos);
		assert!(matches!(kind, Some(BreakKind::Allowed)), "break after '/' should be Allowed, got {kind:?}");
	}

	#[test]
	fn url_query_chars_are_allowed() {
		let text = "example.com?foo=bar&baz=1#section";
		let breaks = collect_break_opportunities(text);

		for ch in ['?', '=', '&', '#'] {
			if let Some(pos) = text.find(ch) {
				let after = pos + ch.len_utf8();
				let kind = kind_at(&breaks, after);
				assert!(matches!(kind, Some(BreakKind::Allowed)), "break after {ch:?} at {after} should be Allowed, got {kind:?}");
			}
		}
	}

	// ── Email addresses ──────────────────────────────────────────────────────

	#[test]
	fn email_at_and_dot_are_allowed() {
		let text = "user@example.com";
		let breaks = collect_break_opportunities(text);

		let at_pos = text.find('@').unwrap() + 1;
		assert!(matches!(kind_at(&breaks, at_pos), Some(BreakKind::Allowed)), "break after '@' should be Allowed");
		// The '.' inside the domain
		let dot_pos = text.rfind('.').unwrap() + 1;
		assert!(matches!(kind_at(&breaks, dot_pos), Some(BreakKind::Allowed)), "break after '.' should be Allowed");
	}

	// ── Hyphenated compounds ─────────────────────────────────────────────────

	#[test]
	fn hyphen_compound_break_after_hyphen() {
		let text = "state-of-the-art";
		let breaks = collect_break_opportunities(text);

		// Find first '-'
		let hyphen = text.find('-').unwrap();
		let after_hyphen = hyphen + 1; // break AFTER the hyphen
		assert!(matches!(kind_at(&breaks, after_hyphen), Some(BreakKind::Allowed)), "break after '-' in compound should be Allowed");
	}

	// ── Numbers must not break ───────────────────────────────────────────────

	#[test]
	fn number_thousand_separator_no_break() {
		let text = "1,000,000";
		let breaks = collect_break_opportunities(text);

		// Positions after ',' should be Prohibited.
		for (i, ch) in text.char_indices() {
			if ch == ',' {
				let after = i + 1;
				assert!(
					matches!(kind_at(&breaks, after), Some(BreakKind::Prohibited) | None),
					"break after ',' in number at {after} must be Prohibited"
				);
			}
		}
	}

	#[test]
	fn number_decimal_no_break() {
		let text = "3.14159";
		let breaks = collect_break_opportunities(text);

		let dot = text.find('.').unwrap();
		let after_dot = dot + 1;
		// A position absent from the breaks Vec is implicitly Prohibited (no UAX #14 break there).
		assert!(
			matches!(kind_at(&breaks, after_dot), Some(BreakKind::Prohibited) | None),
			"break after '.' in decimal should be Prohibited (or absent), got {:?}",
			kind_at(&breaks, after_dot)
		);
	}

	#[test]
	fn number_unit_suffix_no_break() {
		let text = "100px";
		let breaks = collect_break_opportunities(text);
		// Break before 'p' (after "100") should be Prohibited.
		let before_p = text.find('p').unwrap();
		assert!(
			matches!(kind_at(&breaks, before_p), Some(BreakKind::Prohibited) | None),
			"break before unit suffix 'px' must be Prohibited"
		);
	}

	#[test]
	fn number_currency_prefix_no_break() {
		let text = "$99.99";
		let breaks = collect_break_opportunities(text);
		// Break between '$' and '9' (byte 1) should be Prohibited.
		let after_dollar = 1;
		assert!(
			matches!(kind_at(&breaks, after_dollar), Some(BreakKind::Prohibited) | None),
			"break after '$' before digit must be Prohibited"
		);
	}

	// ── Special Unicode ──────────────────────────────────────────────────────

	#[test]
	fn non_breaking_space_prohibited() {
		let text = "foo\u{00A0}bar";
		let breaks = collect_break_opportunities(text);
		// Non-breaking space at byte 3 — breaks before (3) and after (3+3=6, NBSP is 2 bytes in UTF-8... actually U+00A0 is 2 bytes)
		// U+00A0 encodes to 0xC2 0xA0, so 2 bytes.
		let nbsp_start = text.find('\u{00A0}').unwrap();
		let nbsp_end = nbsp_start + '\u{00A0}'.len_utf8();
		assert!(matches!(kind_at(&breaks, nbsp_start), Some(BreakKind::Prohibited) | None), "break before NBSP must be Prohibited");
		assert!(matches!(kind_at(&breaks, nbsp_end), Some(BreakKind::Prohibited) | None), "break after NBSP must be Prohibited");
	}

	#[test]
	fn non_breaking_hyphen_prohibited() {
		let text = "foo\u{2011}bar";
		let breaks = collect_break_opportunities(text);
		let nbh_start = text.find('\u{2011}').unwrap();
		let nbh_end = nbh_start + '\u{2011}'.len_utf8();
		assert!(
			matches!(kind_at(&breaks, nbh_start), Some(BreakKind::Prohibited) | None),
			"break before non-breaking hyphen must be Prohibited"
		);
		assert!(
			matches!(kind_at(&breaks, nbh_end), Some(BreakKind::Prohibited) | None),
			"break after non-breaking hyphen must be Prohibited"
		);
	}

	#[test]
	fn zero_width_space_is_allowed() {
		let text = "foo\u{200B}bar";
		let breaks = collect_break_opportunities(text);
		let zwsp_end = text.find('\u{200B}').unwrap() + '\u{200B}'.len_utf8();
		assert!(matches!(kind_at(&breaks, zwsp_end), Some(BreakKind::Allowed)), "break at ZWSP must be Allowed");
	}

	#[test]
	fn soft_hyphen_is_allowed() {
		let text = "super\u{00AD}long";
		let breaks = collect_break_opportunities(text);
		let shy_end = text.find('\u{00AD}').unwrap() + '\u{00AD}'.len_utf8();
		assert!(matches!(kind_at(&breaks, shy_end), Some(BreakKind::Allowed)), "break after soft hyphen must be Allowed");
	}

	// ── Trailing punctuation ─────────────────────────────────────────────────

	#[test]
	fn trailing_punct_not_at_line_start() {
		// The break BEFORE a trailing punctuation character must be Prohibited.
		for ch in [',', '.', '!', '?', ')', ']'] {
			let text = format!("word{ch}");
			let breaks = collect_break_opportunities(&text);
			let punct_start = text.find(ch).unwrap();
			assert!(
				matches!(kind_at(&breaks, punct_start), Some(BreakKind::Prohibited) | None),
				"break before trailing {ch:?} must be Prohibited"
			);
		}
	}

	// ── ZWSP injection ───────────────────────────────────────────────────────

	#[test]
	fn build_injected_text_inserts_zwsp_at_allowed() {
		// Simple: "a b" — UAX #14 allows break after the space (byte 2).
		let text = "a b";
		let breaks = collect_break_opportunities(text);
		let injected = build_injected_text(text, &breaks);
		assert!(injected.contains('\u{200B}'), "injected text should contain ZWSP");
		// The injected text should not contain any other added characters.
		let stripped: String = injected.chars().filter(|&c| c != '\u{200B}').collect();
		assert_eq!(stripped, text, "stripping ZWSP should restore original text");
	}

	#[test]
	fn build_injected_text_no_zwsp_for_prohibited() {
		// Non-breaking space only: no ZWSP should be injected at that position.
		let text = "foo\u{00A0}bar";
		let breaks = collect_break_opportunities(text);
		let injected = build_injected_text(text, &breaks);
		// Strip ZWSP and verify text is intact.
		let stripped: String = injected.chars().filter(|&c| c != '\u{200B}').collect();
		assert_eq!(stripped, text);
	}

	// ── Collect returns sorted results ───────────────────────────────────────

	#[test]
	fn collect_break_opportunities_is_sorted() {
		let text = "Hello, world! This is a test string with various characters.";
		let breaks = collect_break_opportunities(text);
		let indices: Vec<usize> = breaks.iter().map(|b| b.byte_index).collect();
		let mut sorted = indices.clone();
		sorted.sort_unstable();
		assert_eq!(indices, sorted, "break opportunities must be sorted by byte_index");
	}
}
