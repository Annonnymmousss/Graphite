pub mod fallback;
mod font;
pub mod json;
mod path_builder;
pub mod regex;
mod text_context;
mod to_path;

use convert_case::{Boundary, Converter, pattern};
use core_types::consts::{DEFAULT_FONT_SIZE, DEFAULT_LINE_HEIGHT};
use core_types::graphene_hash::CacheHash;
use core_types::list::{Item, List};
use core_types::math::float_noise::round_away_float_noise;
use core_types::registry::types::{SignedInteger, TextArea};
use core_types::{
	ATTR_FONT, ATTR_FONT_SIZE, ATTR_JUSTIFICATION, ATTR_LETTER_TILT, ATTR_LINE_HEIGHT, ATTR_MAX_HEIGHT, ATTR_MAX_WIDTH, ATTR_TEXT_ALIGN, CloneVarArgs, Context, Ctx, ExtractAll, ExtractVarArgs,
	OwnedContextImpl,
};
use dyn_any::DynAny;
use glam::{DAffine2, DVec2};
use graphene_resource::Resource;
use unicode_segmentation::UnicodeSegmentation;

// Re-export for convenience
pub use core_types as gcore;
pub use fallback::FALLBACK_FONT_RESOURCE;
pub use font::*;
pub use text_context::{TextContext, for_each_styled_glyph_run};
pub use to_path::*;
pub use vector_types;

/// Alignment of lines of type within a text block.
#[repr(C)]
#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, CacheHash, DynAny, node_macro::ChoiceType)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[widget(Radio)]
pub enum TextAlign {
	#[default]
	#[icon("TextAlignLeft")]
	#[cfg_attr(feature = "serde", serde(alias = "Left"))]
	AlignLeft,
	#[icon("TextAlignCenter")]
	#[cfg_attr(feature = "serde", serde(alias = "Center"))]
	AlignCenter,
	#[icon("TextAlignRight")]
	#[cfg_attr(feature = "serde", serde(alias = "Right"))]
	AlignRight,
	#[icon("TextJustifyLeft")]
	JustifyLeft,
	#[icon("TextJustifyCenter")]
	JustifyCenter,
	#[icon("TextJustifyRight")]
	JustifyRight,
	#[icon("TextJustifyAll")]
	JustifyAll,
}

/// The alignment parley lays the block out with. The justified modes lay out left-aligned because Graphite fits their lines
/// itself, in [`for_each_styled_glyph_run`], to honor the [`Justification`] ranges that parley's own `Justify` — which only
/// ever stretches spaces, without limit — cannot express.
impl From<TextAlign> for parley::Alignment {
	fn from(val: TextAlign) -> Self {
		match val {
			TextAlign::AlignCenter => parley::Alignment::Center,
			TextAlign::AlignRight => parley::Alignment::Right,
			_ => parley::Alignment::Left,
		}
	}
}

impl TextAlign {
	/// Whether lines of this alignment are stretched to fill the column width, and so are fitted against the [`Justification`] ranges.
	pub fn is_justified(self) -> bool {
		matches!(self, Self::JustifyLeft | Self::JustifyCenter | Self::JustifyRight | Self::JustifyAll)
	}

	/// How the last line of a paragraph is placed, or `None` if it is simply left where the layout put it.
	///
	/// `JustifyLeft` returns `None` because its last line stays flush left. The other justify modes need it shifted
	/// (`Center`/`Right`) or fitted to the full column width like any other line (`Justify`, for `JustifyAll`).
	pub fn last_line_correction(self) -> Option<parley::Alignment> {
		match self {
			Self::JustifyCenter => Some(parley::Alignment::Center),
			Self::JustifyRight => Some(parley::Alignment::Right),
			Self::JustifyAll => Some(parley::Alignment::Justify),
			_ => None,
		}
	}

	/// CSS `(text-align, text-align-last)` values approximating this alignment for the `contenteditable` text overlay.
	pub fn css(self) -> (&'static str, &'static str) {
		match self {
			Self::AlignLeft => ("left", "auto"),
			Self::AlignCenter => ("center", "auto"),
			Self::AlignRight => ("right", "auto"),
			Self::JustifyLeft => ("justify", "auto"),
			Self::JustifyCenter => ("justify", "center"),
			Self::JustifyRight => ("justify", "right"),
			Self::JustifyAll => ("justify", "justify"),
		}
	}
}

/// The range one spacing quantity may take while a justified line is fitted to its column width.
///
/// *Desired* is what every line is laid out with, justified or not; *minimum* and *maximum* only bound the compression
/// and stretching a justified line adds on top of it.
#[derive(PartialEq, Clone, Copy, Debug, CacheHash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SpacingRange {
	pub minimum: f64,
	pub desired: f64,
	pub maximum: f64,
}

impl SpacingRange {
	pub const fn new(minimum: f64, desired: f64, maximum: f64) -> Self {
		Self { minimum, desired, maximum }
	}

	/// Clamps all three values to the supported domain and makes the minimum and maximum enclose the desired value.
	/// This is a final line of defence for documents or graph inputs that bypass the Properties panel validation.
	fn validated(self, domain_minimum: f64, domain_maximum: f64) -> Self {
		let finite_or = |value: f64, fallback: f64| if value.is_finite() { value } else { fallback };
		let desired = finite_or(self.desired, domain_minimum).clamp(domain_minimum, domain_maximum);
		let minimum = finite_or(self.minimum, desired).clamp(domain_minimum, desired);
		let maximum = finite_or(self.maximum, desired).clamp(desired, domain_maximum);
		Self::new(minimum, desired, maximum)
	}

	/// How far the quantity may move below and above *desired*, where `per_unit` is what one unit of the range is worth
	/// in drawn width. Callers validate the range first, so these bounds always enclose zero.
	fn offsets(self, per_unit: f64) -> (f64, f64) {
		((self.minimum - self.desired) * per_unit, (self.maximum - self.desired) * per_unit)
	}

	/// The same bounds as [`Self::offsets`] for a range read as a scale factor: a multiple of *desired* rather than an offset from it.
	fn ratios(self) -> (f64, f64) {
		(self.minimum / self.desired, self.maximum / self.desired)
	}
}

/// How far a justified line may stretch or compress to reach its column width, mirroring Illustrator's Justification
/// controls: word spacing and glyph scaling as percentages of the font's natural widths, and letter spacing as a
/// percentage of the font's natural space width.
///
/// The defaults are Illustrator's own. They permit moderate word-space fitting while leaving letter spacing and glyph
/// scaling at their natural values; a line that needs more adjustment keeps a ragged edge rather than exceeding a bound.
///
/// Reference: <https://helpx.adobe.com/illustrator/desktop/design-with-text/edit-format-text/adjust-word-and-letterspacing-in-justified-text.html>
#[derive(PartialEq, Clone, Copy, Debug, CacheHash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Justification {
	/// Width of each space glyph, as a percentage of the font's own space width.
	pub word_spacing: SpacingRange,
	/// Space inserted between glyphs, as a percentage of the font's own space width.
	pub letter_spacing: SpacingRange,
	/// Horizontal scale of the glyphs themselves, as a percentage of their natural width.
	pub glyph_scaling: SpacingRange,
}

impl Default for Justification {
	fn default() -> Self {
		Self {
			word_spacing: SpacingRange::new(80., 100., 133.),
			letter_spacing: SpacingRange::new(0., 0., 0.),
			glyph_scaling: SpacingRange::new(100., 100., 100.),
		}
	}
}

/// The spacing one line is drawn with once fitted: where its glyphs start, the advance each space and each glyph gains,
/// and the horizontal scale of the glyphs.
#[derive(PartialEq, Clone, Copy, Debug)]
pub struct LineJustification {
	pub x_offset: f32,
	pub space_extra: f32,
	pub letter_extra: f32,
	pub glyph_scale: f32,
}

impl LineJustification {
	/// A line drawn exactly as laid out, carrying only the desired glyph scaling the layout was measured against.
	pub const fn unfitted(glyph_scale: f32) -> Self {
		Self {
			x_offset: 0.,
			space_extra: 0.,
			letter_extra: 0.,
			glyph_scale,
		}
	}
}

impl Justification {
	/// Applies Illustrator's supported value domains and guarantees `minimum <= desired <= maximum` for every range.
	pub fn validated(self) -> Self {
		Self {
			word_spacing: self.word_spacing.validated(0., 1000.),
			letter_spacing: self.letter_spacing.validated(-100., 500.),
			glyph_scaling: self.glyph_scaling.validated(50., 200.),
		}
	}

	/// The horizontal scale every line is drawn at before fitting, as a ratio.
	pub fn desired_glyph_scale(self) -> f64 {
		self.validated().glyph_scaling.desired / 100.
	}

	/// The narrowest horizontal glyph scale a justified line may use, as a ratio.
	pub(crate) fn minimum_glyph_scale(self) -> f64 {
		self.validated().glyph_scaling.minimum / 100.
	}

	/// The advance the desired word spacing adds to each space glyph, given the font's natural `space_advance`.
	pub fn desired_word_offset(self, space_advance: f64) -> f64 {
		space_advance * (self.validated().word_spacing.desired - 100.) / 100.
	}

	/// The advance the desired letter spacing adds between glyphs, given the font's natural `space_advance`.
	pub fn desired_letter_offset(self, space_advance: f64) -> f64 {
		space_advance * self.validated().letter_spacing.desired / 100.
	}

	/// The advance the minimum word spacing adds to each space glyph.
	pub(crate) fn minimum_word_offset(self, space_advance: f64) -> f64 {
		space_advance * (self.validated().word_spacing.minimum - 100.) / 100.
	}

	/// The advance the minimum letter spacing adds between glyphs.
	pub(crate) fn minimum_letter_offset(self, space_advance: f64) -> f64 {
		space_advance * self.validated().letter_spacing.minimum / 100.
	}

	/// Fits one line across the `free` width left over in its column. Word spacing, letter spacing, and glyph scaling all
	/// move through their desired-to-minimum or desired-to-maximum ranges by the same proportion. This is the requested
	/// inter-word versus inter-character ratio: widening one range gives that mechanism proportionally more influence.
	/// If all available capacity is exhausted, the line remains short or long instead of violating a bound.
	///
	/// `advance` is the line's drawn glyph advance and `space_advance` the drawn width of the font's space glyph, both
	/// excluding the trailing whitespace that hangs past the margin. The extras are shared over `spaces` space glyphs
	/// and `gaps` inter-glyph gaps.
	pub fn fit_line(self, free: f64, advance: f64, space_advance: f64, spaces: usize, gaps: usize) -> LineJustification {
		let justification = self.validated();
		let word_offsets = justification.word_spacing.offsets(space_advance / 100.);
		let letter_offsets = justification.letter_spacing.offsets(space_advance / 100.);
		let glyph_ratios = justification.glyph_scaling.ratios();

		let choose = |bounds: (f64, f64)| if free < 0. { bounds.0 } else { bounds.1 };
		let space_limit = choose(word_offsets);
		let letter_limit = choose(letter_offsets);
		let scale_limit = choose(glyph_ratios) - 1.;
		let capacity = space_limit * spaces as f64 + letter_limit * gaps as f64 + scale_limit * advance;
		let progress = if capacity.abs() > f64::EPSILON { (free / capacity).clamp(0., 1.) } else { 0. };

		LineJustification {
			x_offset: 0.,
			space_extra: (space_limit * progress) as f32,
			letter_extra: (letter_limit * progress) as f32,
			glyph_scale: (justification.desired_glyph_scale() * (1. + scale_limit * progress)) as f32,
		}
	}
}

#[derive(PartialEq, Clone, Copy, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TypesettingConfig {
	pub font_size: f64,
	pub line_height_ratio: f64,
	pub letter_tilt: f64,
	pub max_width: Option<f64>,
	pub max_height: Option<f64>,
	pub align: TextAlign,
	pub justification: Justification,
}

impl Default for TypesettingConfig {
	fn default() -> Self {
		Self {
			font_size: DEFAULT_FONT_SIZE,
			line_height_ratio: DEFAULT_LINE_HEIGHT,
			letter_tilt: 0.,
			max_width: None,
			max_height: None,
			align: TextAlign::default(),
			justification: Justification::default(),
		}
	}
}

/// The typography attributes a styled string carries, read the same way by everything that draws one: the vector shaper,
/// the SVG and Vello renderers, and the click-target pass.
pub trait TextItemAttributes {
	/// The attribute stored under `key`, or `default` if the item leaves it unset or stores another type there.
	fn typography<T: Clone + 'static>(&self, key: &str, default: T) -> T;
}

impl TextItemAttributes for Item<String> {
	fn typography<T: Clone + 'static>(&self, key: &str, default: T) -> T {
		self.attribute_cloned_or(key, default)
	}
}

impl TypesettingConfig {
	/// Reads a text item's typography, falling back to each attribute's implicit default where the item leaves it unset.
	pub fn from_text_item(item: &impl TextItemAttributes) -> Self {
		let defaults = Self::default();

		Self {
			font_size: item.typography(ATTR_FONT_SIZE, defaults.font_size),
			line_height_ratio: item.typography(ATTR_LINE_HEIGHT, defaults.line_height_ratio),
			letter_tilt: item.typography(ATTR_LETTER_TILT, defaults.letter_tilt),
			max_width: item.typography(ATTR_MAX_WIDTH, defaults.max_width),
			max_height: item.typography(ATTR_MAX_HEIGHT, defaults.max_height),
			align: item.typography(ATTR_TEXT_ALIGN, defaults.align),
			justification: item.typography(ATTR_JUSTIFICATION, defaults.justification),
		}
	}
}

/// The font a text item is drawn with, substituting the built-in fallback when it carries none.
pub fn text_item_font(item: &impl TextItemAttributes) -> Resource {
	let font: Resource = item.typography(ATTR_FONT, Resource::default());

	if font.is_empty() { FALLBACK_FONT_RESOURCE.clone() } else { font }
}

/// Converts escape sequence representations (`\n`, `\r`, `\t`, `\0`, `\\`) into their corresponding control characters.
/// Unrecognized escape sequences (e.g. `\x`) are preserved as-is.
fn unescape_string(input: String) -> String {
	let mut result = String::with_capacity(input.len());
	let mut chars = input.chars();

	while let Some(c) = chars.next() {
		if c == '\\' {
			match chars.next() {
				Some('n') => result.push('\n'),
				Some('r') => result.push('\r'),
				Some('t') => result.push('\t'),
				Some('0') => result.push('\0'),
				Some('\\') => result.push('\\'),
				Some(unrecognized) => result.extend(['\\', unrecognized]),
				None => result.push('\\'),
			}
		} else {
			result.push(c);
		}
	}

	result
}

/// Converts control characters (newline, carriage return, tab, null, backslash) back into their escape sequence representations.
fn escape_string(input: String) -> String {
	let mut result = String::with_capacity(input.len());

	for c in input.chars() {
		match c {
			'\n' => result.push_str("\\n"),
			'\r' => result.push_str("\\r"),
			'\t' => result.push_str("\\t"),
			'\0' => result.push_str("\\0"),
			'\\' => result.push_str("\\\\"),
			other => result.push(other),
		}
	}

	result
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, CacheHash, dyn_any::DynAny, node_macro::ChoiceType, serde::Serialize, serde::Deserialize)]
#[widget(Dropdown)]
pub enum StringCapitalization {
	/// "on the origin of species" — Converts all letters to lower case.
	#[default]
	#[label("lower case")]
	LowerCase,
	/// "ON THE ORIGIN OF SPECIES" — Converts all letters to upper case.
	#[label("UPPER CASE")]
	UpperCase,
	/// "On The Origin Of Species" — Converts the first letter of every word to upper case.
	#[label("Capital Case")]
	CapitalCase,
	/// "On the Origin of Species" — Converts the first letter of significant words to upper case.
	#[label("Headline Case")]
	HeadlineCase,
	/// "On the origin of species" — Converts the first letter of every word to lower case, except the initial word which is made upper case.
	#[label("Sentence case")]
	SentenceCase,
	/// "on The Origin Of Species" — Converts the first letter of every word to upper case, except the initial word which is made lower case.
	#[label("camel Case")]
	CamelCase,
}

/// Constructs a string value which may be set to any plain text.
#[node_macro::node(category("Value"))]
fn string_value(_: impl Ctx, _primary: (), string: Item<TextArea>) -> Item<String> {
	string
}

/// Type-asserts a value to be a string.
#[node_macro::node(category("Type Assertion"))]
fn as_string(_: impl Ctx, value: Item<String>) -> Item<String> {
	value
}

/// Joins two strings together.
#[node_macro::node(category("Text"))]
fn string_concatenate(_: impl Ctx, #[implementations(String)] first: Item<String>, second: Item<TextArea>) -> Item<String> {
	let mut first = first;
	first.element_mut().push_str(second.element());
	first
}

/// Replaces all occurrences of "From" with "To" in the input string.
#[node_macro::node(category("Text"))]
fn string_replace(_: impl Ctx, string: Item<String>, from: Item<TextArea>, to: Item<TextArea>) -> Item<String> {
	let mut string = string;
	let result = string.element().replace(from.element().as_str(), to.element());

	*string.element_mut() = result;
	string
}

/// Extracts a substring from the input string, starting at "Start" and ending before "End".
///
/// Negative indices count from the end of the string. If the index of "Start" equals or exceeds "End", the result is an empty string.
#[node_macro::node(category("Text"))]
fn string_slice(_: impl Ctx, string: Item<String>, start: Item<SignedInteger>, end: Item<SignedInteger>) -> Item<String> {
	let mut string = string;
	let (start, end) = (*start.element(), *end.element());

	let total_graphemes = string.element().graphemes(true).count();

	let start = if start < 0. {
		total_graphemes.saturating_sub(start.abs() as usize)
	} else {
		(start as usize).min(total_graphemes)
	};
	let end = if end <= 0. {
		total_graphemes.saturating_sub(end.abs() as usize)
	} else {
		(end as usize).min(total_graphemes)
	};

	let result = if start >= end {
		String::new()
	} else {
		string.element().graphemes(true).skip(start).take(end - start).collect()
	};

	*string.element_mut() = result;
	string
}

/// Clips the string to a maximum character length, optionally appending a suffix (like "…") when truncation occurs. Strings already within the limit are not modified.
#[node_macro::node(category("Text"))]
fn string_truncate(
	_: impl Ctx,
	/// The string to truncate.
	string: Item<String>,
	/// The maximum number of characters allowed, including the suffix if one is appended.
	#[default(80)]
	length: Item<u32>,
	/// A suffix appended to indicate truncation occurred, unless empty. Its length counts towards the character budget.
	#[default("…")]
	suffix: Item<String>,
) -> Item<String> {
	let mut string = string;
	let max_length = *length.element() as usize;
	let grapheme_count = string.element().graphemes(true).count();

	if grapheme_count <= max_length {
		return string;
	}

	let suffix: String = suffix.element().graphemes(true).take(max_length).collect();
	let keep = max_length - suffix.graphemes(true).count();

	let mut truncated: String = string.element().graphemes(true).take(keep).collect();
	truncated.push_str(&suffix);

	*string.element_mut() = truncated;
	string
}

/// Formats a number as a string with control over decimal places, decimal separator, and thousands grouping.
#[node_macro::node(category("Text"), properties("format_number_properties"))]
fn format_number(
	_: impl Ctx,
	/// The number to format as a string.
	number: Item<f64>,
	/// The amount of digits after the decimal point. The value is rounded to fit. Set to 0 to show only whole numbers.
	#[default(2)]
	decimal_places: Item<u32>,
	/// The character(s) used as the decimal point.
	#[default(".")]
	decimal_separator: Item<String>,
	/// Always show the exact number of decimal places, even if they are trailing zeros.
	#[default(true)]
	fixed_decimals: Item<bool>,
	/// Whether to group digits with a thousands separator.
	use_thousands_separator: Item<bool>,
	/// The character(s) inserted between digit groups.
	#[default(",")]
	thousands_separator: Item<String>,
	/// Don't group 4-digit numbers with a thousands separator (only start grouping at 10,000 and above).
	#[name("Start at 10,000")]
	start_at_10000: Item<bool>,
) -> Item<String> {
	let (number, attributes) = number.into_parts();
	let number = round_away_float_noise(number);
	let (decimal_places, fixed_decimals, use_thousands_separator, start_at_10000) =
		(*decimal_places.element(), *fixed_decimals.element(), *use_thousands_separator.element(), *start_at_10000.element());
	let decimal_separator = decimal_separator.element().clone();
	let thousands_separator = thousands_separator.element().clone();

	// Find the maximum meaningful decimal precision by detecting where float noise begins.
	// This works correctly whether the value originated as f32 or f64, since we find the
	// shortest decimal representation that round-trips back to the same f64 value.
	let requested_places = decimal_places as usize;
	let max_places = {
		let whole_digits = if number == 0. { 1 } else { (number.abs().log10().floor() as usize).saturating_add(1) };
		let upper_bound = 17_usize.saturating_sub(whole_digits);
		let mut meaningful = upper_bound;
		for p in 0..=upper_bound {
			let s = format!("{number:.p$}");
			if s.parse::<f64>() == Ok(number) {
				meaningful = p;
				break;
			}
		}
		meaningful
	};
	let places = requested_places.min(max_places);
	let formatted = format!("{number:.places$}");

	// If the user requested more decimal places than the float can represent, pad with zeros
	let extra_zeros = requested_places.saturating_sub(places);

	// Split into sign, whole, and decimal parts
	let (sign, unsigned) = if let Some(rest) = formatted.strip_prefix('-') { ("-", rest) } else { ("", formatted.as_str()) };

	let (whole_string, decimal_string) = match unsigned.split_once('.') {
		Some((w, d)) => {
			let padded = if extra_zeros > 0 { format!("{d}{:0>width$}", "", width = extra_zeros) } else { d.to_string() };
			(w.to_string(), Some(padded))
		}
		None => (unsigned.to_string(), None),
	};

	// Apply thousands grouping to the whole number part
	let grouped_whole = if use_thousands_separator && !thousands_separator.is_empty() {
		let skip = start_at_10000 && whole_string.len() <= 4;
		if skip {
			whole_string.clone()
		} else {
			let mut result = String::new();
			for (i, ch) in whole_string.chars().rev().enumerate() {
				if i > 0 && i % 3 == 0 {
					result.push_str(&thousands_separator.chars().rev().collect::<String>());
				}
				result.push(ch);
			}
			result.chars().rev().collect()
		}
	} else {
		whole_string
	};

	// Build the final string
	let result = match decimal_string {
		None if fixed_decimals && requested_places > 0 => {
			let zeros = "0".repeat(requested_places);
			format!("{sign}{grouped_whole}{decimal_separator}{zeros}")
		}
		None => format!("{sign}{grouped_whole}"),
		Some(decimal_string) if fixed_decimals => format!("{sign}{grouped_whole}{decimal_separator}{decimal_string}"),
		Some(decimal_string) => {
			let trimmed = decimal_string.trim_end_matches('0');
			if trimmed.is_empty() {
				format!("{sign}{grouped_whole}")
			} else {
				format!("{sign}{grouped_whole}{decimal_separator}{trimmed}")
			}
		}
	};

	Item::from_parts(result, attributes)
}

/// Parses a string into a number. Falls back to the chosen value if the string is not a valid number.
#[node_macro::node(category("Text"), name("String to Number"))]
fn string_to_number(
	_: impl Ctx,
	/// The string containing a number. Surrounding whitespace is ignored, a decimal point (.) may be included, sign prefixes (+/-) are respected, and scientific notation (e.g. "1e-3") is supported.
	string: Item<String>,
	/// The value of the result if the string cannot be parsed as a valid number.
	fallback: Item<f64>,
) -> Item<f64> {
	let (string, attributes) = string.into_parts();

	Item::from_parts(string.trim().parse::<f64>().unwrap_or(*fallback.element()), attributes)
}

/// Removes leading and/or trailing whitespace from a string. Common whitespace characters include spaces, tabs, and newlines.
#[node_macro::node(category("Text"))]
fn string_trim(
	_: impl Ctx,
	/// The string that may contain leading and trailing whitespace that should be removed.
	string: Item<String>,
	/// Whether the start of the string should have its whitespace removed.
	#[default(true)]
	start: Item<bool>,
	/// Whether the end of the string should have its whitespace removed.
	#[default(true)]
	end: Item<bool>,
) -> Item<String> {
	let mut string = string;
	let (start, end) = (*start.element(), *end.element());

	let result = match (start, end) {
		(true, true) => string.element().trim().to_string(),
		(true, false) => string.element().trim_start().to_string(),
		(false, true) => string.element().trim_end().to_string(),
		(false, false) => return string,
	};

	*string.element_mut() = result;
	string
}

/// Converts between literal escape sequences and their corresponding control characters within a string.
///
/// Unescape: `\n` (newline), `\r` (carriage return), `\t` (tab), `\0` (null), and `\\` (backslash) are converted into the actual special characters.
/// Escape: the actual special characters are converted back into their escape sequence representations.
#[node_macro::node(category("Text"))]
fn string_escape(
	_: impl Ctx,
	/// The string that contains either literal escape sequences or control characters to be converted to the opposite representation.
	string: Item<String>,
	/// Convert the control characters back into their escape sequence representations.
	#[default(true)]
	unescape: Item<bool>,
) -> Item<String> {
	let mut string = string;
	let input = std::mem::take(string.element_mut());

	let result = if *unescape.element() { unescape_string(input) } else { escape_string(input) };

	*string.element_mut() = result;
	string
}

/// Reverses the sequence of characters making up the string so it reads back-to-front. ("Backwards text" becomes "txet sdrawkcaB".)
#[node_macro::node(category("Text"))]
fn string_reverse(
	_: impl Ctx,
	/// The string to be reversed.
	string: Item<String>,
) -> Item<String> {
	let mut string = string;
	let result: String = string.element().graphemes(true).rev().collect();

	*string.element_mut() = result;
	string
}

/// Repeats the string a given number of times, optionally with a separator between each repetition.
#[node_macro::node(category("Text"))]
fn string_repeat(
	_: impl Ctx,
	/// The string to be repeated.
	string: Item<String>,
	/// The number of times the string should appear in the output.
	#[default(2)]
	#[hard(1..)]
	count: Item<u32>,
	/// The string placed between each repetition.
	#[default("\\n")]
	separator: Item<String>,
	/// Whether to convert escape sequences found in the separator into their corresponding characters:
	/// "\n" (newline), "\r" (carriage return), "\t" (tab), "\0" (null), and "\\" (backslash).
	#[default(true)]
	separator_escaping: Item<bool>,
) -> Item<String> {
	let mut string = string;
	let separator = separator.element().clone();
	let separator = if *separator_escaping.element() { unescape_string(separator) } else { separator };

	let count = *count.element() as usize;

	let mut result = String::with_capacity((string.element().len() + separator.len()) * count);
	for i in 0..count {
		if i > 0 {
			result.push_str(&separator);
		}
		result.push_str(string.element());
	}

	*string.element_mut() = result;
	string
}

/// Pads the string to a target length by filling with the given repeated substring. If the string already meets or exceeds the target length, it is returned unchanged.
#[node_macro::node(category("Text"))]
fn string_pad(
	_: impl Ctx,
	/// The string to be padded to a target length.
	string: Item<String>,
	/// The target character length after padding. When "Up To" is set, this length concerns only the portion before (or after) that substring.
	#[default(10)]
	length: Item<u32>,
	/// The repeated substring used to fill the remaining space. A multi-charcter substring may end partway through its final repetition.
	#[default("#")]
	padding: Item<String>,
	/// Pad only the length of the string encountered before the start of the first (or after the end of the last) occurrence of this substring, if given and present (otherwise the full string is considered).
	///
	/// For example, this can pad numbers with leading zeros to align them before the decimal point.
	up_to: Item<String>,
	/// Pad at the end of the string instead of the start.
	from_end: Item<bool>,
) -> Item<String> {
	let mut string = string;
	let target_length = *length.element() as usize;
	let padding = padding.element().clone();
	let up_to = up_to.element().clone();
	let from_end = *from_end.element();

	if padding.is_empty() {
		return string;
	}

	// Split the string at the "up to" substring if provided, and only pad that portion
	if !up_to.is_empty()
		&& let Some(position) = if from_end { string.element().rfind(&*up_to) } else { string.element().find(&*up_to) }
	{
		let (before, after) = string.element().split_at(position);

		if from_end {
			// Pad the portion after the substring
			let after_substring = &after[up_to.len()..];
			let current_length = after_substring.graphemes(true).count();
			if current_length >= target_length {
				return string;
			}
			let pad_length = target_length - current_length;
			let padding: String = padding.graphemes(true).cycle().take(pad_length).collect();
			let result = format!("{before}{up_to}{after_substring}{padding}");

			*string.element_mut() = result;
			return string;
		} else {
			// Pad the portion before the substring
			let current_length = before.graphemes(true).count();
			if current_length >= target_length {
				return string;
			}
			let pad_length = target_length - current_length;
			let padding: String = padding.graphemes(true).cycle().take(pad_length).collect();
			let result = format!("{padding}{before}{after}");

			*string.element_mut() = result;
			return string;
		}
	}

	let current_length = string.element().graphemes(true).count();
	if current_length >= target_length {
		return string;
	}

	let pad_length = target_length - current_length;
	let padding: String = padding.graphemes(true).cycle().take(pad_length).collect();

	let result = if from_end { string.element().clone() + &padding } else { padding + string.element() };

	*string.element_mut() = result;
	string
}

/// Checks whether the string contains the given substring. Optionally restricts the match to only the start and/or end of the string.
#[node_macro::node(category("Text"))]
fn string_contains(
	_: impl Ctx,
	/// The string to search within.
	string: Item<String>,
	/// The substring to search for.
	substring: Item<String>,
	/// Only match if the substring appears at the start of the string.
	at_start: Item<bool>,
	/// Only match if the substring appears at the end of the string.
	at_end: Item<bool>,
) -> Item<bool> {
	let (string, attributes) = string.into_parts();
	let substring = substring.element().as_str();
	let (at_start, at_end) = (*at_start.element(), *at_end.element());

	let result = match (at_start, at_end) {
		(true, true) => string.starts_with(substring) && string.ends_with(substring),
		(true, false) => string.starts_with(substring),
		(false, true) => string.ends_with(substring),
		(false, false) => string.contains(substring),
	};

	Item::from_parts(result, attributes)
}

/// Similar to the **String Contains** node, this searches within the input string for the first (or last) occurrence of a substring and returns the index of where that begins, or -1 if not found.
#[node_macro::node(category("Text"))]
fn string_find_index(
	_: impl Ctx,
	/// The string to search within.
	string: Item<String>,
	/// The substring to search for.
	substring: Item<String>,
	/// Find the start index of the last occurrence instead of the first.
	from_end: Item<bool>,
) -> Item<f64> {
	let (string, attributes) = string.into_parts();
	let substring = substring.element().as_str();
	let from_end = *from_end.element();

	if substring.is_empty() {
		let result = if from_end { string.graphemes(true).count() as f64 } else { 0. };
		return Item::from_parts(result, attributes);
	}

	let result = if from_end {
		// Search backwards by finding all byte-level matches and taking the last one
		string
			.rmatch_indices(substring)
			.next()
			.map_or(-1., |(byte_index, _)| string[..byte_index].graphemes(true).count() as f64)
	} else {
		string
			.match_indices(substring)
			.next()
			.map_or(-1., |(byte_index, _)| string[..byte_index].graphemes(true).count() as f64)
	};

	Item::from_parts(result, attributes)
}

/// Counts the number of occurrences of a substring within the string.
#[node_macro::node(category("Text"))]
fn string_occurrences(
	_: impl Ctx,
	/// The string to search within.
	string: Item<String>,
	/// The substring to count occurrences of.
	substring: Item<String>,
	/// Whether to count overlapping occurrences, using the substring as a sliding window.
	///
	/// For example, "aa" occurs twice in "aaaa" without overlapping but three times with overlapping.
	overlapping: Item<bool>,
) -> Item<f64> {
	let (string, attributes) = string.into_parts();
	let substring = substring.element().as_str();

	if substring.is_empty() {
		return Item::from_parts(0., attributes);
	}

	// NON-OVERLAPPING: Simple linear scan.
	// O(n), where n = string length
	if !*overlapping.element() {
		return Item::from_parts(string.matches(substring).count() as f64, attributes);
	}

	// OVERLAPPING: KMP (Knuth-Morris-Pratt) algorithm.
	// O(n + m), where n = string length, m = substring length

	let pattern: Vec<char> = substring.chars().collect();
	let text: Vec<char> = string.chars().collect();

	// Build the KMP failure function:
	// For each position in the pattern, the length of the longest proper prefix that is also a suffix.
	// This lets us skip ahead on mismatches instead of restarting from scratch.
	let mut failure = vec![0_usize; pattern.len()];
	let mut k = 0;
	for i in 1..pattern.len() {
		while k > 0 && pattern[k] != pattern[i] {
			k = failure[k - 1];
		}

		if pattern[k] == pattern[i] {
			k += 1;
		}

		failure[i] = k;
	}

	// Scan the text, advancing the pattern cursor without ever backtracking in the text
	let mut count: usize = 0;
	let mut pattern_cursor = 0;
	for &text_char in &text {
		while pattern_cursor > 0 && pattern[pattern_cursor] != text_char {
			pattern_cursor = failure[pattern_cursor - 1];
		}

		if pattern[pattern_cursor] == text_char {
			pattern_cursor += 1;
		}

		if pattern_cursor == pattern.len() {
			count += 1;

			// Reset using failure function to allow overlapping matches
			pattern_cursor = failure[pattern_cursor - 1];
		}
	}

	Item::from_parts(count as f64, attributes)
}

/// Converts a string's capitalization style to another of the common upper and lower case patterns, optionally joining words with a chosen separator.
#[node_macro::node(category("Text"), properties("string_capitalization_properties"))]
fn string_capitalization(
	_: impl Ctx,
	/// The string to have its letter capitalization converted.
	string: Item<String>,
	/// The capitalization style to apply.
	capitalization: Item<StringCapitalization>,
	/// Whether to split the string into words and reconnect with the chosen joiner. When disabled, the existing word structure separators are preserved.
	use_joiner: Item<bool>,
	/// The string placed between each word.
	joiner: Item<String>,
) -> Item<String> {
	let mut string = string;
	let capitalization = *capitalization.element();
	let use_joiner = *use_joiner.element();
	let joiner = joiner.element().clone();
	let input = std::mem::take(string.element_mut());

	// When the joiner is enabled, apply word-level casing and optionally reconnect words with the selected joiner
	let result = if use_joiner {
		match capitalization {
			// Simple case mappings that preserve the string's existing structure
			StringCapitalization::LowerCase => input.to_lowercase(),
			StringCapitalization::UpperCase => input.to_uppercase(),

			// Word-aware capitalizations that split on word boundaries and rejoin with the joiner
			StringCapitalization::CapitalCase => Converter::new().set_boundaries(&Boundary::defaults()).set_pattern(pattern::capital).set_delim(&joiner).convert(&input),
			StringCapitalization::HeadlineCase => {
				// First split into words with convert_case so word boundaries like "AlphaNumeric" are detected consistently with other modes,
				// then apply the titlecase crate for smart capitalization (lowercasing short words like "of", "the", etc.),
				// then rejoin with the custom joiner without mangling the capitalization
				let spaced = Converter::new().set_boundaries(&Boundary::defaults()).set_pattern(pattern::capital).set_delim(" ").convert(&input);
				let headline = titlecase::titlecase(&spaced);
				Converter::new().set_boundaries(&[Boundary::SPACE]).set_pattern(pattern::noop).set_delim(&joiner).convert(&headline)
			}
			StringCapitalization::SentenceCase => Converter::new().set_boundaries(&Boundary::defaults()).set_pattern(pattern::sentence).set_delim(&joiner).convert(&input),
			StringCapitalization::CamelCase => Converter::new().set_boundaries(&Boundary::defaults()).set_pattern(pattern::camel).set_delim(&joiner).convert(&input),
		}
	}
	// When the joiner is disabled, apply only character-level casing while preserving the string's existing structure
	else {
		match capitalization {
			StringCapitalization::LowerCase => input.to_lowercase(),
			StringCapitalization::UpperCase => input.to_uppercase(),
			StringCapitalization::CapitalCase => {
				let mut capitalize_next = true;
				input.chars().fold(String::with_capacity(input.len()), |mut result, c| {
					if c.is_whitespace() || c == '_' || c == '-' {
						capitalize_next = true;
						result.push(c);
					} else if capitalize_next {
						capitalize_next = false;
						result.extend(c.to_uppercase());
					} else {
						result.push(c);
					}
					result
				})
			}
			StringCapitalization::HeadlineCase => titlecase::titlecase(&input),
			StringCapitalization::SentenceCase => {
				let mut chars = input.chars();
				match chars.next() {
					Some(first) => first.to_uppercase().to_string() + &chars.as_str().to_lowercase(),
					None => String::new(),
				}
			}
			StringCapitalization::CamelCase => {
				let mut capitalize_next = false;
				input.chars().fold(String::with_capacity(input.len()), |mut result, c| {
					if c.is_whitespace() || c == '_' || c == '-' {
						capitalize_next = true;
						result.push(c);
					} else if capitalize_next {
						capitalize_next = false;
						result.extend(c.to_uppercase());
					} else {
						result.extend(c.to_lowercase());
					}
					result
				})
			}
		}
	};

	*string.element_mut() = result;
	string
}

// TODO: Return u32, u64, or usize instead of f64 after #1621 is resolved and has allowed us to implement automatic type conversion in the node graph for nodes with generic type inputs.
// TODO: (Currently automatic type conversion only works for concrete types, via the Graphene preprocessor and not the full Graphene type system.)
/// Counts the number of characters in a string.
#[node_macro::node(category("Text"))]
fn string_length(_: impl Ctx, string: Item<String>) -> Item<f64> {
	let (string, attributes) = string.into_parts();

	Item::from_parts(string.graphemes(true).count() as f64, attributes)
}

/// Splits a string into a list of substrings based on the specified delimiter. This is the inverse of the **String Join** node.
///
/// For example, splitting "a, b, c" with delimiter ", " produces `["a", "b", "c"]`.
#[node_macro::node(category("Text"))]
fn string_split(
	_: impl Ctx,
	/// The string to split into substrings.
	string: Item<String>,
	/// The character(s) that separate the substrings. These are not included in the outputs.
	#[default("\\n")]
	delimiter: Item<String>,
	/// Whether to convert escape sequences found in the delimiter into their corresponding characters:
	/// "\n" (newline), "\r" (carriage return), "\t" (tab), "\0" (null), and "\\" (backslash).
	#[default(true)]
	delimiter_escaping: Item<bool>,
) -> List<String> {
	let delimiter = delimiter.element().clone();
	let delimiter = if *delimiter_escaping.element() { unescape_string(delimiter) } else { delimiter };

	string.element().split(&delimiter).map(str::to_string).map(Item::new_from_element).collect()
}

/// Joins a list of strings together with a separator between each pair. This is the inverse of the **String Split** node.
///
/// For example, joining `["a", "b", "c"]` with separator ", " produces "a, b, c".
#[node_macro::node(category("Text"))]
fn string_join(
	_: impl Ctx,
	/// The list of strings to join together.
	strings: List<String>,
	/// The text placed between each pair of strings.
	#[default(", ")]
	separator: Item<String>,
	/// Whether to convert escape sequences found in the separator into their corresponding characters:
	/// "\n" (newline), "\r" (carriage return), "\t" (tab), "\0" (null), and "\\" (backslash).
	#[default(true)]
	separator_escaping: Item<bool>,
) -> Item<String> {
	let (separator, separator_escaping) = (separator.into_element(), separator_escaping.into_element());
	let separator = if separator_escaping { unescape_string(separator) } else { separator };

	let joined = strings.iter_element_values().map(|s| s.as_str()).collect::<Vec<_>>().join(&separator);

	Item::new_from_element(joined)
}

/// Iterates over a list of strings, evaluating the mapped operation for each one. Use the **Read String** node to access the current string inside the loop.
#[node_macro::node(category("Text"))]
async fn map_string(
	ctx: impl Ctx + CloneVarArgs + ExtractAll,
	strings: List<String>,
	#[expose]
	#[implementations(Context -> Item<String>)]
	mapped: impl Node<Context<'static>, Output = Item<String>>,
) -> List<String> {
	let mut result = List::new();

	for (i, row) in strings.into_iter().enumerate() {
		let owned_ctx = OwnedContextImpl::from(ctx.clone());
		let owned_ctx = owned_ctx.with_vararg(Box::new(row)).with_index(i);
		let mapped_string = mapped.eval(owned_ctx.into_context()).await;

		result.push(mapped_string);
	}

	result
}

/// Reads the current string from within a **Map String** node's loop.
#[node_macro::node(category("Context"))]
fn read_string(ctx: impl Ctx + ExtractVarArgs) -> Item<String> {
	let Ok(var_arg) = ctx.vararg(0) else { return Item::new_from_element(String::new()) };
	let var_arg = var_arg as &dyn std::any::Any;

	var_arg.downcast_ref::<Item<String>>().cloned().unwrap_or_default()
}

/// Converts a value to a JSON string representation.
#[node_macro::node(category("Debug"))]
fn serialize<T: serde::Serialize>(_: impl Ctx, #[implementations(String, bool, f64, u32, u64, DVec2, DAffine2)] value: Item<T>) -> Item<String> {
	let (value, attributes) = value.into_parts();

	let result = serde_json::to_string(&value).unwrap_or_else(|_| "Serialization Error".to_string());

	Item::from_parts(result, attributes)
}

#[cfg(test)]
mod justification_tests {
	use super::*;
	use core_types::ATTR_TRANSFORM;

	/// How far right the ink of a text block reaches once shaped, which is what a justified line pushes to the margin.
	fn drawn_right_edge(text: &str, typesetting: TypesettingConfig) -> f64 {
		let shaped = to_path(text, &FALLBACK_FONT_RESOURCE, typesetting, false);
		shaped.element(0).and_then(|vector| vector.bounding_box()).map_or(0., |bounds| bounds[1].x)
	}

	/// A paragraph long enough to wrap into several lines within `WRAP_WIDTH`, so it has non-final lines to justify.
	const PARAGRAPH: &str = "The quick brown fox jumps over the lazy dog while the sun sets behind the distant hills";
	const WRAP_WIDTH: f64 = 300.;

	fn wrapped(align: TextAlign, justification: Justification) -> TypesettingConfig {
		TypesettingConfig {
			max_width: Some(WRAP_WIDTH),
			align,
			justification,
			..TypesettingConfig::default()
		}
	}

	fn line_count(text: &str, typesetting: TypesettingConfig) -> usize {
		TextContext::with_thread_local(|context| context.layout_text(text, &FALLBACK_FONT_RESOURCE, typesetting).map_or(0, |layout| layout.line_count()))
	}

	#[test]
	fn justified_lines_reach_the_margin_on_word_spacing_alone() {
		let flexible_words = Justification {
			word_spacing: SpacingRange::new(80., 100., 1000.),
			..Justification::default()
		};
		let ragged = drawn_right_edge(PARAGRAPH, wrapped(TextAlign::AlignLeft, flexible_words));
		let justified = drawn_right_edge(PARAGRAPH, wrapped(TextAlign::JustifyLeft, flexible_words));

		// A left-aligned block ends wherever its longest line happens to end, short of the column it wraps within
		assert!(ragged < WRAP_WIDTH - 1., "the ragged block should not fill the column, but reached {ragged}");
		// Letter spacing and glyph scaling have no room, so word spacing alone has to fill the line
		assert!(justified > WRAP_WIDTH - 5., "the justified block should fill the column, but reached {justified}");
		assert!(justified <= WRAP_WIDTH, "the justified block should not overrun the column, but reached {justified}");
	}

	/// The distance between the first two glyphs of the block, which extra letter spacing opens up and extra word spacing does not.
	fn first_glyph_gap(text: &str, typesetting: TypesettingConfig) -> f64 {
		let shaped = to_path(text, &FALLBACK_FONT_RESOURCE, typesetting, true);
		let glyph_x = |index| shaped.attribute_cloned_or_default::<DAffine2>(ATTR_TRANSFORM, index).translation.x;

		glyph_x(1) - glyph_x(0)
	}

	#[test]
	fn letter_spacing_takes_over_where_word_spacing_is_capped() {
		// Word spacing pinned to its desired width has nothing to give, so letter spacing is the available fitting mechanism
		let capped = Justification {
			word_spacing: SpacingRange::new(100., 100., 100.),
			..Justification::default()
		};
		let with_letter_spacing = Justification {
			letter_spacing: SpacingRange::new(0., 0., 100.),
			..capped
		};

		let natural = first_glyph_gap(PARAGRAPH, wrapped(TextAlign::JustifyLeft, capped));
		let loosened = first_glyph_gap(PARAGRAPH, wrapped(TextAlign::JustifyLeft, with_letter_spacing));

		assert!(loosened > natural + 0.5, "letter spacing should have opened the glyphs up: {loosened} vs {natural}");
		assert!(
			loosened <= natural + TypesettingConfig::default().font_size * 0.5 + 1e-3,
			"letter spacing should stay within its maximum: {loosened} vs {natural}"
		);
	}

	/// The drawn width of the block's first glyph, which fitted glyph scaling widens and the spacing ranges do not.
	fn first_glyph_width(text: &str, typesetting: TypesettingConfig) -> f64 {
		let shaped = to_path(text, &FALLBACK_FONT_RESOURCE, typesetting, true);

		shaped.element(0).and_then(|glyph| glyph.bounding_box()).map_or(0., |bounds| bounds[1].x - bounds[0].x)
	}

	#[test]
	fn glyph_scaling_takes_over_once_the_spacing_ranges_are_spent() {
		let pinned = Justification {
			word_spacing: SpacingRange::new(100., 100., 100.),
			..Justification::default()
		};
		let stretchable = Justification {
			glyph_scaling: SpacingRange::new(100., 100., 200.),
			..pinned
		};

		let natural = first_glyph_width(PARAGRAPH, wrapped(TextAlign::JustifyLeft, pinned));
		let stretched = first_glyph_width(PARAGRAPH, wrapped(TextAlign::JustifyLeft, stretchable));

		assert!(stretched > natural * 1.01, "the glyphs themselves should have been widened: {stretched} vs {natural}");
		assert!(stretched <= natural * 2. + 1e-3, "glyph scaling should stay within its maximum: {stretched} vs {natural}");
	}

	#[test]
	fn a_column_that_cannot_be_filled_within_the_ranges_keeps_the_maximums() {
		// Every range pinned to its desired value leaves no fitting room, so a line must remain short instead of
		// silently overrunning the values the user requested.
		let pinned = Justification {
			word_spacing: SpacingRange::new(100., 100., 100.),
			..Justification::default()
		};

		let edge = drawn_right_edge(PARAGRAPH, wrapped(TextAlign::JustifyLeft, pinned));

		assert!(edge < WRAP_WIDTH - 5., "the justified block should retain a ragged edge when every range is pinned, but reached {edge}");
	}

	#[test]
	fn minimum_word_spacing_changes_which_words_fit_on_a_line() {
		let compressible = Justification {
			word_spacing: SpacingRange::new(0., 500., 500.),
			..Justification::default()
		};
		let pinned = Justification {
			word_spacing: SpacingRange::new(500., 500., 500.),
			..Justification::default()
		};

		let compressed_lines = line_count(PARAGRAPH, wrapped(TextAlign::JustifyLeft, compressible));
		let pinned_lines = line_count(PARAGRAPH, wrapped(TextAlign::JustifyLeft, pinned));

		assert!(
			compressed_lines < pinned_lines,
			"a lower minimum should keep more words on each line: {compressed_lines} vs {pinned_lines}"
		);
	}

	#[test]
	fn desired_glyph_scaling_condenses_the_block() {
		let natural = drawn_right_edge("Hamburgefonstiv", TypesettingConfig::default());
		let condensed = drawn_right_edge(
			"Hamburgefonstiv",
			TypesettingConfig {
				justification: Justification {
					glyph_scaling: SpacingRange::new(50., 50., 50.),
					..Justification::default()
				},
				..TypesettingConfig::default()
			},
		);

		assert!((condensed - natural * 0.5).abs() < 0.5, "half-scaled text should be half as wide: {condensed} vs {natural}");
	}

	#[test]
	fn fitting_moves_all_enabled_ranges_by_the_same_proportion() {
		let justification = Justification {
			// One percentage point is worth a tenth of the 10-unit space, so this range gives each space 1 unit to spend
			word_spacing: SpacingRange::new(100., 100., 110.),
			letter_spacing: SpacingRange::new(0., 0., 5.),
			glyph_scaling: SpacingRange::new(100., 100., 200.),
		};

		let fitted = justification.fit_line(100., 200., 10., 2, 3);
		// Full expansion contributes 2 spaces × 1, 3 gaps × 0.5, and 200 units of glyph expansion.
		let progress = 100. / 203.5;

		assert!((fitted.space_extra as f64 - progress).abs() < 1e-6);
		assert!((fitted.letter_extra as f64 - 0.5 * progress).abs() < 1e-6);
		assert!((fitted.glyph_scale as f64 - (1. + progress)).abs() < 1e-6);
	}

	#[test]
	fn a_line_that_cannot_be_filled_stops_at_the_maximums() {
		// Only word spacing has any room, and far less than the line needs
		let justification = Justification {
			word_spacing: SpacingRange::new(100., 100., 110.),
			letter_spacing: SpacingRange::new(0., 0., 0.),
			glyph_scaling: SpacingRange::new(100., 100., 100.),
		};

		let fitted = justification.fit_line(100., 200., 10., 2, 3);

		assert_eq!(fitted.space_extra, 1., "word spacing must stop at its configured maximum");
		assert_eq!(fitted.letter_extra, 0.);
		assert_eq!(fitted.glyph_scale, 1.);
	}

	#[test]
	fn a_range_that_excludes_its_desired_value_is_safely_clamped() {
		// Nonsense ranges (here a maximum under the desired value) must not drag every line off its desired spacing
		let justification = Justification {
			word_spacing: SpacingRange::new(50., 100., 60.),
			letter_spacing: SpacingRange::new(4., 2., 3.),
			glyph_scaling: SpacingRange::new(50., 100., 60.),
		};

		let fitted = justification.fit_line(100., 200., 10., 2, 3);

		assert!(fitted.space_extra >= 0., "word spacing should not be forced below its desired width");
		assert!(fitted.letter_extra >= 0., "letter spacing should not be forced below its desired width");
		assert!(fitted.glyph_scale >= 1., "glyph scaling should not be forced below its desired width");
	}
}
