use super::path_builder::PathBuilder;
use super::{LineJustification, TypesettingConfig};
use core::cell::RefCell;
use core::ops::Deref;
use core_types::list::List;
use glam::DVec2;
use graphene_resource::{Resource, ResourceHash};
use parley::fontique::{Blob, FamilyId, FontInfo};
use parley::{AlignmentOptions, Cluster, FontContext, GlyphRun, Layout, LayoutContext, LineHeight, PositionedLayoutItem, StyleProperty};
use skrifa::MetadataProvider;
use skrifa::instance::{LocationRef, Size};
use skrifa::raw::FontRef;
use std::collections::HashMap;
use vector_types::Vector;

thread_local! {
	static THREAD_TEXT: RefCell<TextContext> = RefCell::new(TextContext::default());
}

/// A laid-out text block, together with the two measurements its lines are fitted against: the width of the font's space
/// glyph that the [`Justification`](super::Justification) percentages are relative to, and the horizontal scale the desired glyph scaling draws
/// the block at. The layout itself is measured pre-divided by that scale, so every horizontal quantity read off it must be
/// scaled back up before it describes drawn geometry.
pub struct TextLayout {
	layout: Layout<()>,
	space_advance: f64,
	glyph_scale: f64,
}

impl Deref for TextLayout {
	type Target = Layout<()>;

	fn deref(&self) -> &Self::Target {
		&self.layout
	}
}

impl TextLayout {
	/// The block's drawn width, excluding the trailing whitespace hanging past each line's end.
	pub fn drawn_width(&self) -> f64 {
		self.layout.width() as f64 * self.glyph_scale
	}

	/// The block's drawn width, including that trailing whitespace.
	pub fn drawn_full_width(&self) -> f64 {
		self.layout.full_width() as f64 * self.glyph_scale
	}

	/// Number of composed lines in this block.
	pub fn line_count(&self) -> usize {
		self.layout.len()
	}
}

/// Counts the space glyphs and glyphs of `clusters` that a fitted line shares its extra spacing over, skipping the
/// trailing whitespace that hangs past the margin.
fn count_fitted<'a>(clusters: impl Iterator<Item = Cluster<'a, ()>>, visible_end: usize) -> (usize, usize) {
	clusters
		.filter(|cluster| cluster.text_range().start < visible_end)
		.fold((0, 0), |(spaces, glyphs), cluster| (spaces + cluster.is_space_or_nbsp() as usize, glyphs + 1))
}

/// Iterates the glyph runs of a laid-out text in reading order, fitting each line to the column width and skipping lines
/// clipped by `max_height`. Shared by the vector shaper and the SVG/Vello text renderers so the justification logic lives
/// in one place.
///
/// Justified alignments are fitted here rather than by parley, whose own justification only ever stretches spaces and
/// cannot be bounded; see [`Justification::fit_line`](super::Justification::fit_line) for the proportional fit applied to each line.
pub fn for_each_styled_glyph_run(text_layout: &TextLayout, text: &str, typesetting: TypesettingConfig, mut visit: impl FnMut(&GlyphRun<'_, ()>, LineJustification)) {
	let (layout, glyph_scale) = (&text_layout.layout, text_layout.glyph_scale);
	// The percentages measure against the space as drawn, so the font's own advance carries the desired glyph scaling
	let space_advance = text_layout.space_advance * glyph_scale;
	let alignment_width = typesetting.max_width.unwrap_or_else(|| text_layout.drawn_full_width());
	let justified = typesetting.align.is_justified();

	for line in layout.lines() {
		let metrics = line.metrics();

		// Lines pushed past the block's maximum height are not drawn at all
		if typesetting.max_height.is_some_and(|max_height| metrics.baseline > max_height as f32) {
			continue;
		}

		let range = line.text_range();
		// Parley always includes a hard-break `\n` as the last byte of the preceding line's range, so the line is at the end of
		// a paragraph if it's the very last line of the buffer or its text ends with `\n`.
		let is_last_para_line = range.end == text.len() || text.get(range.clone()).is_some_and(|s| s.ends_with('\n'));

		// Only justified alignments fill the column; the last line of each of their paragraphs instead takes the correction its mode asks for.
		let correction = match (justified, is_last_para_line) {
			(false, _) => None,
			(true, false) => Some(parley::Alignment::Justify),
			(true, true) => typesetting.align.last_line_correction(),
		};

		// Trailing whitespace hangs past the margin, so it counts towards neither the fitted width nor the items sharing the
		// extra spacing. Parley's `trailing_whitespace` is an advance rather than a byte count, so re-derive the boundary here.
		let line_text = text.get(range.clone()).unwrap_or("");
		let visible_end = range.end - (line_text.len() - line_text.trim_end().len());

		let mut justification = LineJustification::unfitted(glyph_scale as f32);
		if let Some(correction) = correction {
			let advance = (metrics.advance - metrics.trailing_whitespace) as f64 * glyph_scale;
			let free = alignment_width - advance;

			match correction {
				parley::Alignment::Center => justification.x_offset = (free * 0.5) as f32,
				parley::Alignment::Right => justification.x_offset = free as f32,
				_ => {
					let (spaces, glyphs) = line
						.runs()
						.map(|run| count_fitted(run.clusters(), visible_end))
						.fold((0, 0), |total, run| (total.0 + run.0, total.1 + run.1));

					// Extra letter spacing lands in the gaps between glyphs, one fewer than the glyphs themselves
					justification = typesetting.justification.fit_line(free, advance, space_advance, spaces, glyphs.saturating_sub(1));
				}
			}
		}

		// Parley's run offsets describe the unfitted line, so each run starts where the runs before it on this line left off
		// once their own extra spacing is spent.
		let mut spent = justification.x_offset;
		let spends_extra = justification.space_extra != 0. || justification.letter_extra != 0.;
		for item in line.items() {
			if let PositionedLayoutItem::GlyphRun(glyph_run) = item {
				visit(&glyph_run, LineJustification { x_offset: spent, ..justification });

				if spends_extra {
					let (spaces, glyphs) = count_fitted(glyph_run.run().clusters(), visible_end);
					spent += spaces as f32 * justification.space_extra + glyphs as f32 * justification.letter_extra;
				}
			}
		}
	}
}

/// Unified thread-local text processing context that combines font and layout management
/// for efficient text rendering operations.
#[derive(Default)]
pub struct TextContext {
	font_context: FontContext,
	layout_context: LayoutContext<()>,
	font_info_cache: HashMap<ResourceHash, CachedFont>,
}

/// What one registered font resource is remembered by: its family and face, plus the advance of its space glyph in em
/// units, which the justification percentages measure against.
#[derive(Clone)]
struct CachedFont {
	family_id: FamilyId,
	font_info: FontInfo,
	space_advance_em: f64,
}

/// The advance of a font's space glyph, in em units. Falls back to a quarter em for a font that has no space glyph.
fn space_advance_em(data: &[u8], index: u32) -> f64 {
	const FALLBACK: f64 = 0.25;

	let Ok(font_ref) = FontRef::from_index(data, index) else { return FALLBACK };

	// `Size::new(1.)` scales font units by `1 / units_per_em`, so the advance comes back already expressed in em units
	font_ref
		.charmap()
		.map(' ')
		.and_then(|glyph| font_ref.glyph_metrics(Size::new(1.), LocationRef::default()).advance_width(glyph))
		.map_or(FALLBACK, |advance| advance as f64)
}

impl TextContext {
	/// Access the thread-local TextContext instance for text processing operations
	pub fn with_thread_local<F, R>(f: F) -> R
	where
		F: FnOnce(&mut TextContext) -> R,
	{
		THREAD_TEXT.with_borrow_mut(f)
	}

	/// Get or cache font information for the given font resource.
	fn get_font_info(&mut self, font: &Resource) -> Option<(String, FontInfo, f64)> {
		let hash = font.hash();
		if let Some(cached) = self.font_info_cache.get(&hash)
			&& let Some(family_name) = self.font_context.collection.family_name(cached.family_id)
		{
			return Some((family_name.to_string(), cached.font_info.clone(), cached.space_advance_em));
		}

		let families = self.font_context.collection.register_fonts(Blob::new(font.into()), None);

		families.first().and_then(|(family_id, fonts_info)| {
			fonts_info.first().and_then(|font_info| {
				self.font_context.collection.family_name(*family_id).map(|family_name| {
					let cached = CachedFont {
						family_id: *family_id,
						font_info: font_info.clone(),
						space_advance_em: space_advance_em(font.as_ref(), font_info.index()),
					};
					self.font_info_cache.insert(hash, cached.clone());

					(family_name.to_string(), cached.font_info, cached.space_advance_em)
				})
			})
		})
	}

	/// Create a text layout from the given font resource and typesetting configuration.
	pub fn layout_text(&mut self, text: &str, font: &Resource, typesetting: TypesettingConfig) -> Option<TextLayout> {
		let (font_family, font_info, space_advance_em) = self.get_font_info(font)?;
		let space_advance = space_advance_em * typesetting.font_size;
		let justification = typesetting.justification.validated();
		let glyph_scale = justification.desired_glyph_scale();

		const DISPLAY_SCALE: f32 = 1.;
		let mut build_layout = |word_offset: f64, letter_offset: f64| {
			let mut builder = self.layout_context.ranged_builder(&mut self.font_context, text, DISPLAY_SCALE, false);
			builder.push_default(StyleProperty::FontSize(typesetting.font_size as f32));
			builder.push_default(StyleProperty::LetterSpacing(letter_offset as f32));
			builder.push_default(StyleProperty::WordSpacing(word_offset as f32));
			builder.push_default(StyleProperty::FontFamily(parley::FontFamily::Single(parley::FontFamilyName::Named(std::borrow::Cow::Owned(
				font_family.clone(),
			)))));
			builder.push_default(StyleProperty::FontWeight(font_info.weight()));
			builder.push_default(StyleProperty::FontStyle(font_info.style()));
			builder.push_default(StyleProperty::FontWidth(font_info.width()));
			builder.push_default(LineHeight::FontSizeRelative(typesetting.line_height_ratio as f32));
			builder.build(text)
		};

		// Desired values shape every line. For justified, width-constrained text, a second minimum-spacing composition
		// chooses the legal break opportunities: words remain on a line whenever all three ranges can compress them to fit.
		// Those same cluster counts are then applied to the desired layout, which the renderer fits within the exact ranges.
		let mut layout: Layout<()> = build_layout(justification.desired_word_offset(space_advance), justification.desired_letter_offset(space_advance));
		if let Some(max_width) = typesetting.max_width
			&& typesetting.align.is_justified()
		{
			let minimum_scale = justification.minimum_glyph_scale();
			let mut minimum_layout = build_layout(justification.minimum_word_offset(space_advance), justification.minimum_letter_offset(space_advance));
			minimum_layout.break_all_lines(Some((max_width / minimum_scale) as f32));

			let clusters_per_line = minimum_layout.lines().map(|line| line.runs().map(|run| run.clusters().count()).sum::<usize>()).collect::<Vec<_>>();
			let mut breaker = layout.break_lines();
			for cluster_count in clusters_per_line {
				if breaker.break_next_with_length(cluster_count as u32).is_none() {
					break;
				}
			}
			breaker.finish();
		} else {
			// Horizontal glyph scaling is equivalent to breaking against the wider unscaled column and drawing the result scaled.
			layout.break_all_lines(typesetting.max_width.map(|max_width| (max_width / glyph_scale) as f32));
		}
		layout.align(typesetting.align.into(), AlignmentOptions::default());

		Some(TextLayout { layout, space_advance, glyph_scale })
	}

	/// Convert text to vector paths using the specified font and typesetting configuration
	pub fn to_path(&mut self, text: &str, font: &Resource, typesetting: TypesettingConfig, per_glyph_items: bool) -> List<Vector> {
		let Some(layout) = self.layout_text(text, font, typesetting) else {
			return List::new_from_element(Vector::default());
		};

		let text_frame_size = DVec2::new(
			typesetting.max_width.unwrap_or_else(|| layout.drawn_full_width()),
			typesetting.max_height.unwrap_or_else(|| layout.height() as f64),
		);

		// First glyph offset (pre-height-filter) so the empty placeholder item in `per_glyph_items`
		// mode keeps the same item 0's transform, preventing `local_transforms` from jumping mid-drag
		let first_glyph_offset = layout
			.lines()
			.flat_map(|line| line.items())
			.find_map(|item| match item {
				PositionedLayoutItem::GlyphRun(run) => run
					.glyphs()
					.next()
					.map(|glyph| DVec2::new((run.offset() + glyph.x) as f64 * layout.glyph_scale, (run.baseline() - glyph.y) as f64)),
				_ => None,
			})
			.unwrap_or_default();

		let mut path_builder = PathBuilder::new(per_glyph_items, layout.scale() as f64, text_frame_size, first_glyph_offset);

		for_each_styled_glyph_run(&layout, text, typesetting, |glyph_run, justification| {
			path_builder.render_glyph_run(glyph_run, typesetting.letter_tilt, per_glyph_items, justification);
		});

		path_builder.finalize()
	}

	/// Calculate the bounding box of text using the specified font and typesetting configuration
	pub fn bounding_box(&mut self, text: &str, font: &Resource, typesetting: TypesettingConfig, for_clipping_test: bool) -> DVec2 {
		let Some(layout) = self.layout_text(text, font, typesetting) else {
			return DVec2::ZERO;
		};

		let layout_width = layout.drawn_full_width();
		let layout_height = layout.height() as f64;

		if for_clipping_test {
			return DVec2::new(layout_width, layout_height);
		}

		let width = typesetting.max_width.unwrap_or(layout_width);
		let height = typesetting.max_height.unwrap_or(layout_height);

		DVec2::new(width, height)
	}

	/// Check if text lines are being clipped due to height constraints
	pub fn lines_clipping(&mut self, text: &str, font: &Resource, typesetting: TypesettingConfig) -> bool {
		let Some(max_height) = typesetting.max_height else { return false };
		let bounds = self.bounding_box(text, font, typesetting, true);
		max_height < bounds.y
	}
}
