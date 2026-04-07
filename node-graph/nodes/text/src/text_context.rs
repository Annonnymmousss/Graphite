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

/// TODO (Phase 2): swap to per-language selection once multi-language support lands.
// SAFETY: `Standard` contains only `Arc`-wrapped data, making it `Send + Sync`.
static EN_US_DICT: OnceLock<Standard> = OnceLock::new();

fn get_en_us_dict() -> Option<&'static Standard> {
	EN_US_DICT.get_or_init(|| Standard::from_embedded(Language::EnglishUS).expect("embed_all feature must be enabled")).into()
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
		// If hyphenation is enabled, also inject soft hyphens at syllable boundaries.
		let injected: String;
		let layout_text = if typesetting.max_width.is_some() {
			let semantic = inject_semantic_breaks(text);
			injected = if typesetting.hyphenate {
				apply_hyphenation(&semantic)
			} else {
				semantic
			};
			&injected
		} else {
			text
		};

		const DISPLAY_SCALE: f32 = 1.;

		// TODO: This entire two-pass approach (measure hyphen advance, inject '-', re-layout) exists because
		// Parley does not yet implement `StyleProperty::Hyphens(Hyphens::Auto)`. Once Parley adds native
		// hyphenation support, replace the `apply_hyphenation` + `resolve_hyphen_breaks` pipeline with a
		// single style push: `builder.push_default(StyleProperty::Hyphens(Hyphens::Auto))`.
		let mut layout: Layout<()> = if typesetting.hyphenate && typesetting.max_width.is_some() {
			// Measure the hyphen glyph advance so we can reserve space for it in Pass 1.
			// Without this, Pass 1 breaks where `word-prefix` fits, but `word-prefix-` (with
			// the real hyphen added in Pass 2) overflows, causing '-' to land at line start.
			let hyphen_advance = {
				let mut h = build_parley_layout(&mut self.layout_context, &mut self.font_context, "-", typesetting, &font_family, &font_info, DISPLAY_SCALE);
				h.break_all_lines(None);
				h.lines().next().map(|l| l.metrics().advance).unwrap_or(0.)
			};

			// Pass 1: break at (max_width - hyphen_advance) to leave room for the hyphen.
			let max_w_pass1 = typesetting.max_width.map(|mw| (mw as f32 - hyphen_advance).max(0.));
			let mut first = build_parley_layout(&mut self.layout_context, &mut self.font_context, layout_text, typesetting, &font_family, &font_info, DISPLAY_SCALE);
			first.break_all_lines(max_w_pass1);

			let resolved = resolve_hyphen_breaks(layout_text, &first);
			if resolved != layout_text {
				// Pass 2: re-layout with real '-' substituted at break positions.
				build_parley_layout(&mut self.layout_context, &mut self.font_context, &resolved, typesetting, &font_family, &font_info, DISPLAY_SCALE)
			} else {
				first // No soft-hyphen breaks taken — reuse the first layout.
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
/// Configure and build a parley layout for `text` with the given typesetting parameters.
///
/// Extracted as a free function so it can be called twice during the two-pass hyphenation
/// flow without running into the borrow-checker constraint that prevents mutably borrowing
/// two fields of the same struct inside a closure.
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
	b.push_default(StyleProperty::FontStack(parley::FontStack::Single(parley::FontFamily::Named(std::borrow::Cow::Owned(font_family.to_owned())))));
	b.push_default(StyleProperty::FontWeight(font_info.weight()));
	b.push_default(StyleProperty::FontStyle(font_info.style()));
	b.push_default(StyleProperty::FontWidth(font_info.width()));
	b.push_default(LineHeight::FontSizeRelative(typesetting.line_height_ratio as f32));
	// Safety-net: break at character boundaries when nothing else allows a break.
	b.push_default(StyleProperty::OverflowWrap(OverflowWrap::BreakWord));
	b.build(text)
}

/// Examine a first-pass parley layout (after `break_all_lines`) and resolve soft hyphens:
/// - U+00AD at a line-end position → replaced with a real `'-'` (visible break)
/// - U+00AD anywhere else → removed (invisible, no break taken)
///
/// Returns the modified string, or a clone of `text` if no soft hyphens were present.
fn resolve_hyphen_breaks(text: &str, layout: &Layout<()>) -> String {
	const SOFT_HYPHEN: char = '\u{00AD}';
	if !text.contains(SOFT_HYPHEN) {
		return text.to_string();
	}

	// Collect byte offsets of U+00AD that sit at a line end.
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

	// Rebuild: soft hyphens at break positions → '-' + ZWSP (ZWSP anchors the break
	// opportunity after the hyphen so it always stays at the end of the line, never the start
	// of the next), all others → dropped.
	let mut out = String::with_capacity(text.len());
	for (i, c) in text.char_indices() {
		if c == SOFT_HYPHEN {
			if break_positions.contains(&i) {
				out.push('-');
				out.push('\u{200B}'); // ZWSP: break always happens AFTER the hyphen
			}
			// Non-broken soft hyphens are silently dropped.
		} else {
			out.push(c);
		}
	}
	out
}

/// Insert U+00AD (soft hyphen) at Knuth-Liang syllable boundaries for each alphabetic
/// word run in `text`. Parley will render a visible `-` only where a line break is taken.
///
/// - Uses the lazily-loaded English US dictionary from [`get_en_us_dict`].
/// - If the dictionary is unavailable, returns the input unchanged (silent no-op).
/// - Non-alphabetic characters (spaces, punctuation, ZWSP) are copied unchanged.
fn apply_hyphenation(text: &str) -> String {
	let Some(dict) = get_en_us_dict() else {
		return text.to_string();
	};
	const SOFT_HYPHEN: char = '\u{00AD}';
	let mut out = String::with_capacity(text.len() + text.len() / 8);
	let mut word_start: Option<usize> = None;

	/// Push syllable segments of `word` into `out`, separated by soft hyphens.
	/// Avoids the `Vec` + `join` allocations — writes directly into `out`.
	fn push_hyphenated(out: &mut String, dict: &hyphenation::Standard, word: &str) {
		let mut segs = dict.hyphenate(word).into_iter().segments().peekable();
		while let Some(seg) = segs.next() {
			out.push_str(seg);
			if segs.peek().is_some() {
				out.push('\u{00AD}');
			}
		}
	}

	// Walk character-by-character, collecting alphabetic runs then hyphenating each.
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
	// Flush trailing alphabetic word.
	if let Some(start) = word_start {
		push_hyphenated(&mut out, dict, &text[start..]);
	}
	out
}

// TODO: `inject_semantic_breaks` manually injects U+200B (ZWSP) at semantic boundaries
// (URL slashes, email @/dots, compound-word hyphens, etc.) because Parley's UAX #14
// line-breaking does not treat these special-character tokens as break opportunities.
// Remove this function once Parley gains native handling for such cases.
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

		prev = Some(c);
	}

	out
}

#[cfg(test)]
mod tests {
	use super::inject_semantic_breaks;
	use super::apply_hyphenation;

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

	#[test]
	fn underscore_between_letters_gets_zwsp_after() {
		let out = inject_semantic_breaks("foo_bar");
		assert!(has_zwsp_after(&out, '_'), "expected ZWSP after _ in identifier: {out:?}");
	}

	#[test]
	fn underscore_between_digits_no_zwsp() {
		let out = inject_semantic_breaks("100_000");
		assert!(!has_zwsp_after(&out, '_'), "no ZWSP after _ between digits: {out:?}");
	}

	#[test]
	fn question_mark_url_context_zwsp_after() {
		// '?' followed by non-whitespace → URL query start → ZWSP after
		let out = inject_semantic_breaks("example.com?q=hello");
		assert!(has_zwsp_after(&out, '?'), "expected ZWSP after ? in URL: {out:?}");
	}

	#[test]
	fn question_mark_sentence_end_zwsp_before() {
		// '?' followed by whitespace → sentence end → ZWSP before
		let out = inject_semantic_breaks("Really? Yes");
		assert!(has_zwsp_before(&out, '?'), "expected ZWSP before ? at sentence end: {out:?}");
	}

	#[test]
	fn dot_sentence_end_zwsp_before_not_after() {
		// 'word.' with letter before and whitespace/end after → ZWSP before, not after
		let out = inject_semantic_breaks("Hello. World");
		assert!(has_zwsp_before(&out, '.'), "expected ZWSP before sentence dot: {out:?}");
		assert!(!has_zwsp_after(&out, '.'), "no ZWSP after sentence dot: {out:?}");
	}

	#[test]
	fn hyphenation_inserts_soft_hyphens_into_word() {
		// "hyphenation" → "hy\u{00AD}phen\u{00AD}a\u{00AD}tion" per the Knuth-Liang EN-US pattern.
		let out = apply_hyphenation("hyphenation");
		assert!(out.contains('\u{00AD}'), "soft hyphen must be present: got {out:?}");
		// The visible characters (minus soft hyphens) must reconstruct the input.
		let visible: String = out.chars().filter(|&c| c != '\u{00AD}').collect();
		assert_eq!(visible, "hyphenation");
	}

	#[test]
	fn hyphenation_plain_text_unchanged_without_soft_hyphens() {
		// Short common words that the dictionary does not break should pass through intact.
		let out = apply_hyphenation("cat");
		assert!(!out.contains('\u{00AD}'), "short word must not be broken: got {out:?}");
		assert_eq!(out, "cat");
	}
}
