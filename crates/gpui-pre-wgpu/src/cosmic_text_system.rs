// Changed in elundqvist/gpui-kit from gpui-pre-wgpu 0.3.3 as published to crates.io: FORK.md at
// the crate's root lists the changes.
use anyhow::{Context as _, Ok, Result};
use collections::HashMap;
use cosmic_text::{
    Attrs, AttrsList, Ellipsize, Family, Font as CosmicTextFont,
    FontFeatures as CosmicFontFeatures, FontSystem, ShapeBuffer, ShapeLine, Stretch, Style, Weight,
};
use gpui::{
    Bounds, DevicePixels, Font, FontFallbacks, FontFeatures, FontId, FontMetrics, FontRun, GlyphId,
    IsZero as _, LineLayout, Pixels, PlatformTextSystem, RenderGlyphParams, SUBPIXEL_VARIANTS_X,
    SUBPIXEL_VARIANTS_Y, ShapedGlyph, ShapedRun, SharedString, Size, TextRenderingMode, point,
    size,
};

use itertools::Itertools;
use parking_lot::RwLock;
use smallvec::SmallVec;
use std::{borrow::Cow, ops::Range, sync::Arc};
use swash::{
    scale::{Render, ScaleContext, Source, StrikeWith},
    zeno::{Format, Vector},
};
use unicode_properties::emoji::{EmojiStatus, UnicodeEmoji as _};
use unicode_segmentation::UnicodeSegmentation;

pub struct CosmicTextSystem(RwLock<CosmicTextSystemState>);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FontKey {
    family: SharedString,
    features: FontFeatures,
    fallbacks: Option<FontFallbacks>,
}

impl FontKey {
    fn new(family: SharedString, features: FontFeatures, fallbacks: Option<FontFallbacks>) -> Self {
        Self {
            family,
            features,
            fallbacks,
        }
    }
}

struct CosmicTextSystemState {
    font_system: FontSystem,
    scratch: ShapeBuffer,
    swash_scale_context: ScaleContext,
    pending_glyph_images: HashMap<RenderGlyphParams, swash::scale::image::Image>,
    /// Contains all already loaded fonts, including all faces. Indexed by `FontId`.
    loaded_fonts: Vec<LoadedFont>,
    /// Caches the `FontId`s associated with a specific family to avoid iterating the font database
    /// for every font face in a family.
    font_ids_by_family_cache: HashMap<FontKey, SmallVec<[FontId; 4]>>,
    system_font_fallback: String,
    /// The colour emoji font an emoji is drawn from when neither the run's font nor its
    /// fallbacks have it, and the properties it is matched with: the first face of
    /// `EMOJI_FONTS` in the database that swash draws in colour. `None` until it is looked up,
    /// and again after fonts are added.
    emoji_font: Option<Option<(FontId, FontMatchProperties)>>,
}

struct LoadedFont {
    font: Arc<CosmicTextFont>,
    features: CosmicFontFeatures,
    /// An emoji font whose glyphs swash draws in colour (`is_emoji_font`): its glyphs are
    /// drawn as colour sprites.
    is_known_emoji_font: bool,
    /// Noto Color Emoji, whose glyph 3 a line leaves out: upstream's guard against a crash
    /// on variation selectors, kept to the font it was written for.
    is_noto_color_emoji: bool,
    /// resolved at load time so `layout_line` shares one chain across faces.
    /// `Arc` keeps clone cheap on the per-run hot path.
    user_fallback_chain: Arc<[(FontId, SharedString)]>,
}

struct FontMatchProperties {
    primary_family_name: SharedString,
    stretch: Stretch,
    style: Style,
    weight: Weight,
    features: CosmicFontFeatures,
    fallback_chain: Arc<[(FontId, SharedString)]>,
}

impl FontMatchProperties {
    fn attributes<'a>(&'a self, font_id: FontId, family_name: &'a str) -> Attrs<'a> {
        Attrs::new()
            .metadata(font_id.0)
            .family(Family::Name(family_name))
            .stretch(self.stretch)
            .style(self.style)
            .weight(self.weight)
            .font_features(self.features.clone())
    }
}

impl CosmicTextSystem {
    pub fn new(system_font_fallback: &str) -> Self {
        let font_system = FontSystem::new();

        Self(RwLock::new(CosmicTextSystemState {
            font_system,
            scratch: ShapeBuffer::default(),
            swash_scale_context: ScaleContext::new(),
            pending_glyph_images: HashMap::default(),
            loaded_fonts: Vec::new(),
            font_ids_by_family_cache: HashMap::default(),
            system_font_fallback: system_font_fallback.to_string(),
            emoji_font: None,
        }))
    }

    pub fn new_without_system_fonts(system_font_fallback: &str) -> Self {
        let font_system = FontSystem::new_with_locale_and_db(
            "en-US".to_string(),
            cosmic_text::fontdb::Database::new(),
        );

        Self(RwLock::new(CosmicTextSystemState {
            font_system,
            scratch: ShapeBuffer::default(),
            swash_scale_context: ScaleContext::new(),
            pending_glyph_images: HashMap::default(),
            loaded_fonts: Vec::new(),
            font_ids_by_family_cache: HashMap::default(),
            system_font_fallback: system_font_fallback.to_string(),
            emoji_font: None,
        }))
    }
}

impl PlatformTextSystem for CosmicTextSystem {
    fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        self.0.write().add_fonts(fonts)
    }

    fn all_font_names(&self) -> Vec<String> {
        let mut result = self
            .0
            .read()
            .font_system
            .db()
            .faces()
            .filter_map(|face| face.families.first().map(|family| family.0.clone()))
            .collect_vec();
        result.sort_unstable();
        result.dedup();
        result
    }

    fn font_id(&self, font: &Font) -> Result<FontId> {
        let mut state = self.0.write();
        let key = FontKey::new(
            font.family.clone(),
            font.features.clone(),
            font.fallbacks.clone(),
        );
        let candidates = if let Some(font_ids) = state.font_ids_by_family_cache.get(&key) {
            font_ids.as_slice()
        } else {
            let font_ids =
                state.load_family(&font.family, &font.features, font.fallbacks.as_ref())?;
            state.font_ids_by_family_cache.insert(key.clone(), font_ids);
            state.font_ids_by_family_cache[&key].as_ref()
        };

        let ix = find_best_match(font, candidates, &state)?;

        Ok(candidates[ix])
    }

    fn prewarm_fonts(&self, font_ids: &[FontId]) {
        self.0.write().prewarm_fonts(font_ids);
    }

    fn font_metrics(&self, font_id: FontId) -> FontMetrics {
        let metrics = self
            .0
            .read()
            .loaded_font(font_id)
            .font
            .as_swash()
            .metrics(&[]);

        FontMetrics {
            units_per_em: metrics.units_per_em as u32,
            ascent: metrics.ascent,
            descent: -metrics.descent,
            line_gap: metrics.leading,
            underline_position: metrics.underline_offset,
            underline_thickness: metrics.stroke_size,
            cap_height: metrics.cap_height,
            x_height: metrics.x_height,
            bounding_box: Bounds {
                origin: point(0.0, 0.0),
                size: size(metrics.max_width, metrics.ascent + metrics.descent),
            },
        }
    }

    fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
        let lock = self.0.read();
        let glyph_metrics = lock.loaded_font(font_id).font.as_swash().glyph_metrics(&[]);
        let glyph_id = glyph_id.0 as u16;
        Ok(Bounds {
            origin: point(0.0, 0.0),
            size: size(
                glyph_metrics.advance_width(glyph_id),
                glyph_metrics.advance_height(glyph_id),
            ),
        })
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        self.0.read().advance(font_id, glyph_id)
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        self.0.read().glyph_for_char(font_id, ch)
    }

    fn glyph_raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        self.0.write().raster_bounds(params)
    }

    fn rasterize_glyph(
        &self,
        params: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        self.0.write().rasterize_glyph(params, raster_bounds)
    }

    fn layout_line(&self, text: &str, font_size: Pixels, runs: &[FontRun]) -> LineLayout {
        self.0.write().layout_line(text, font_size, runs)
    }

    fn recommended_rendering_mode(
        &self,
        _font_id: FontId,
        _font_size: Pixels,
    ) -> TextRenderingMode {
        TextRenderingMode::Subpixel
    }
}

impl CosmicTextSystemState {
    fn loaded_font(&self, font_id: FontId) -> &LoadedFont {
        &self.loaded_fonts[font_id.0]
    }

    fn font_match_properties(&self, font_id: FontId) -> Option<FontMatchProperties> {
        let loaded_font = self.loaded_font(font_id);
        let Some(face) = self.font_system.db().face(loaded_font.font.id()) else {
            log::warn!("font face not found in database for font_id {:?}", font_id);
            return None;
        };
        let Some(first_family) = face.families.first() else {
            log::warn!("font face has no family names for font_id {:?}", font_id);
            return None;
        };

        Some(FontMatchProperties {
            primary_family_name: first_family.0.clone().into(),
            stretch: face.stretch,
            style: face.style,
            weight: face.weight,
            features: loaded_font.features.clone(),
            fallback_chain: Arc::clone(&loaded_font.user_fallback_chain),
        })
    }

    fn prewarm_fonts(&mut self, font_ids: &[FontId]) {
        for &font_id in font_ids {
            let Some(properties) = self.font_match_properties(font_id) else {
                continue;
            };
            let primary_attributes =
                properties.attributes(font_id, &properties.primary_family_name);
            self.font_system.get_font_matches(&primary_attributes);

            for (fallback_id, fallback_name) in &*properties.fallback_chain {
                let fallback_attributes = properties.attributes(*fallback_id, fallback_name);
                self.font_system.get_font_matches(&fallback_attributes);
            }
        }
    }

    #[profiling::function]
    fn add_fonts(&mut self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        let db = self.font_system.db_mut();
        for bytes in fonts {
            db.load_font_source(cosmic_text::fontdb::Source::Binary(Arc::new(bytes)));
        }
        // one of them may be an emoji font, or one before those found so far
        self.emoji_font = None;
        Ok(())
    }

    /// Looks up the font an emoji with no font of the run's to draw it is drawn from, so that
    /// it is the colour emoji font rather than whatever cosmic-text's own fallback list
    /// reaches first, which on Linux is a monochrome symbol font such as Noto Sans Symbols2.
    /// It is found in the database without `load_family`, which would take a face of an
    /// emoji family that swash draws in no colour, one of SVG glyphs, out of the database and
    /// out of cosmic-text's list with it.
    fn look_up_emoji_font(&mut self) {
        if self.emoji_font.is_some() {
            return;
        }
        let db = self.font_system.db();
        let faces: SmallVec<[cosmic_text::fontdb::ID; 2]> = EMOJI_FONTS
            .iter()
            .flat_map(|postscript_name| {
                db.faces()
                    .filter(move |face| face.post_script_name == *postscript_name)
                    .map(|face| face.id)
            })
            .collect();
        let mut found = None;
        for id in faces {
            let Some(font) = self.font_system.get_font(id, cosmic_text::Weight::NORMAL) else {
                continue;
            };
            if !draws_in_colour(font.as_swash()) {
                continue;
            }
            let properties = self
                .font_id_for_cosmic_id(id)
                .inspect_err(|error| log::warn!("emoji font: {error:#}"))
                .ok()
                .and_then(|font_id| Some((font_id, self.font_match_properties(font_id)?)));
            if properties.is_some() {
                found = properties;
                break;
            }
        }
        self.emoji_font = Some(found);
    }

    #[profiling::function]
    fn load_family(
        &mut self,
        name: &str,
        features: &FontFeatures,
        fallbacks: Option<&FontFallbacks>,
    ) -> Result<SmallVec<[FontId; 4]>> {
        // recurse with `fallbacks = None` so a fallback family cannot pull in
        // another chain. missing fallback families are dropped so a typo in
        // settings still lets the primary family load.
        let user_fallback_chain: Arc<[(FontId, SharedString)]> = match fallbacks {
            Some(fallbacks) if !fallbacks.fallback_list().is_empty() => {
                let mut chain: Vec<(FontId, SharedString)> = Vec::new();
                for fallback_name in fallbacks.fallback_list() {
                    let fb_key = FontKey::new(
                        SharedString::from(fallback_name.clone()),
                        features.clone(),
                        None,
                    );
                    let fb_ids = if let Some(cached) = self.font_ids_by_family_cache.get(&fb_key) {
                        cached.clone()
                    } else {
                        let loaded = self.load_family(fallback_name, features, None)?;
                        self.font_ids_by_family_cache
                            .insert(fb_key.clone(), loaded.clone());
                        loaded
                    };
                    let Some(&fb_id) = fb_ids.first() else {
                        continue;
                    };
                    let db_id = self.loaded_fonts[fb_id.0].font.id();
                    if let Some(face) = self.font_system.db().face(db_id)
                        && let Some(family) = face.families.first()
                    {
                        chain.push((fb_id, SharedString::from(family.0.clone())));
                    }
                }
                Arc::from(chain)
            }
            _ => Arc::from(Vec::new()),
        };

        let name = gpui::font_name_with_fallbacks(name, &self.system_font_fallback);

        let families = self
            .font_system
            .db()
            .faces()
            .filter(|face| face.families.iter().any(|family| *name == family.0))
            .map(|face| (face.id, face.post_script_name.clone()))
            .collect::<SmallVec<[_; 4]>>();

        let cosmic_features = cosmic_font_features(features)?;

        let mut loaded_font_ids = SmallVec::new();
        for (font_id, postscript_name) in families {
            let font = self
                .font_system
                .get_font(font_id, cosmic_text::Weight::NORMAL)
                .context("Could not load font")?;

            // HACK: To let the storybook run and render Windows caption icons. We should actually do better font fallback.
            let allowed_bad_font_names = [
                "SegoeFluentIcons", // NOTE: Segoe fluent icons postscript name is inconsistent
                "Segoe Fluent Icons",
            ];

            // an emoji font has no `m` either, and is kept for the emoji it does have: removed,
            // it was gone from the fallbacks cosmic-text tries as well
            if font.as_swash().charmap().map('m') == 0
                && !allowed_bad_font_names.contains(&postscript_name.as_str())
                && !EMOJI_FONTS.contains(&postscript_name.as_str())
                && !has_colour_tables(font.as_swash())
            {
                self.font_system.db_mut().remove_face(font.id());
                continue;
            };

            let font_id = FontId(self.loaded_fonts.len());
            loaded_font_ids.push(font_id);
            self.loaded_fonts.push(LoadedFont {
                is_known_emoji_font: is_emoji_font(&postscript_name, &font),
                is_noto_color_emoji: postscript_name == "NotoColorEmoji",
                font,
                features: cosmic_features.clone(),
                user_fallback_chain: Arc::clone(&user_fallback_chain),
            });
        }

        Ok(loaded_font_ids)
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        let glyph_metrics = self.loaded_font(font_id).font.as_swash().glyph_metrics(&[]);
        Ok(Size {
            width: glyph_metrics.advance_width(glyph_id.0 as u16),
            height: glyph_metrics.advance_height(glyph_id.0 as u16),
        })
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        let glyph_id = self.loaded_font(font_id).font.as_swash().charmap().map(ch);
        if glyph_id == 0 {
            None
        } else {
            Some(GlyphId(glyph_id.into()))
        }
    }

    fn raster_bounds(&mut self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        let image = self.render_glyph_image(params)?;
        let bounds = Bounds {
            origin: point(image.placement.left.into(), (-image.placement.top).into()),
            size: size(image.placement.width.into(), image.placement.height.into()),
        };
        if !bounds.is_zero() {
            self.pending_glyph_images.insert(params.clone(), image);
        }
        Ok(bounds)
    }

    #[profiling::function]
    fn rasterize_glyph(
        &mut self,
        params: &RenderGlyphParams,
        glyph_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        if glyph_bounds.size.width.0 == 0 || glyph_bounds.size.height.0 == 0 {
            anyhow::bail!("glyph bounds are empty");
        }

        let mut image = match self.pending_glyph_images.remove(params) {
            Some(image) => image,
            None => self.render_glyph_image(params)?,
        };
        let bitmap_size = glyph_bounds.size;
        match image.content {
            swash::scale::image::Content::Color | swash::scale::image::Content::SubpixelMask => {
                // Convert from RGBA to BGRA.
                for pixel in image.data.chunks_exact_mut(4) {
                    pixel.swap(0, 2);
                }
                Ok((bitmap_size, image.data))
            }
            swash::scale::image::Content::Mask => {
                // an emoji goes to the colour atlas, of four bytes a pixel, and a glyph of an
                // emoji font with no colour falls through to its outline, a mask of one
                if params.subpixel_rendering || params.is_emoji {
                    // We must always return RGBA data when subpixel rendering is requested.
                    let expanded = image.data.iter().flat_map(|&a| [a, a, a, a]).collect();
                    Ok((bitmap_size, expanded))
                } else {
                    Ok((bitmap_size, image.data))
                }
            }
        }
    }

    fn render_glyph_image(
        &mut self,
        params: &RenderGlyphParams,
    ) -> Result<swash::scale::image::Image> {
        let loaded_font = &self.loaded_fonts[params.font_id.0];
        let font_ref = loaded_font.font.as_swash();
        let pixel_size = f32::from(params.font_size);

        let subpixel_offset = Vector::new(
            params.subpixel_variant.x as f32 / SUBPIXEL_VARIANTS_X as f32 / params.scale_factor,
            params.subpixel_variant.y as f32 / SUBPIXEL_VARIANTS_Y as f32 / params.scale_factor,
        );

        let mut scaler = self
            .swash_scale_context
            .builder(font_ref)
            .size(pixel_size * params.scale_factor)
            .hint(true)
            .build();

        let sources: &[Source] = if params.is_emoji {
            &[
                Source::ColorOutline(0),
                Source::ColorBitmap(StrikeWith::BestFit),
                Source::Outline,
            ]
        } else {
            &[Source::Bitmap(StrikeWith::ExactSize), Source::Outline]
        };

        let mut renderer = Render::new(sources);
        if params.subpixel_rendering {
            // There seems to be a bug in Swash where the B and R values are swapped.
            renderer
                .format(Format::subpixel_bgra())
                .offset(subpixel_offset);
        } else {
            renderer.format(Format::Alpha).offset(subpixel_offset);
        }

        let glyph_id: u16 = params.glyph_id.0.try_into()?;
        renderer
            .render(&mut scaler, glyph_id)
            .with_context(|| format!("unable to render glyph via swash for {params:?}"))
    }

    /// This is used when cosmic_text has chosen a fallback font instead of using the requested
    /// font, typically to handle some unicode characters. When this happens, `loaded_fonts` may not
    /// yet have an entry for this fallback font, and so one is added.
    ///
    /// Note that callers shouldn't use this `FontId` somewhere that will retrieve the corresponding
    /// `LoadedFont.features`, as it will have an arbitrarily chosen or empty value. The only
    /// current use of this field is for the *input* of `layout_line`, and so it's fine to use
    /// `font_id_for_cosmic_id` when computing the *output* of `layout_line`.
    fn font_id_for_cosmic_id(&mut self, id: cosmic_text::fontdb::ID) -> Result<FontId> {
        if let Some(ix) = self
            .loaded_fonts
            .iter()
            .position(|loaded_font| loaded_font.font.id() == id)
        {
            Ok(FontId(ix))
        } else {
            let font = self
                .font_system
                .get_font(id, cosmic_text::Weight::NORMAL)
                .context("failed to get fallback font from cosmic-text font system")?;
            let face = self
                .font_system
                .db()
                .face(id)
                .context("fallback font face not found in cosmic-text database")?;

            let font_id = FontId(self.loaded_fonts.len());
            self.loaded_fonts.push(LoadedFont {
                is_known_emoji_font: is_emoji_font(&face.post_script_name, &font),
                is_noto_color_emoji: face.post_script_name == "NotoColorEmoji",
                font,
                features: CosmicFontFeatures::new(),
                user_fallback_chain: Arc::from(Vec::new()),
            });

            Ok(font_id)
        }
    }

    #[profiling::function]
    fn layout_line(&mut self, text: &str, font_size: Pixels, font_runs: &[FontRun]) -> LineLayout {
        if contains_paragraph_separator(text) {
            self.layout_line_with_separators(text, font_size, font_runs)
        } else {
            self.layout_line_no_separators(text, font_size, font_runs)
        }
    }

    fn layout_line_with_separators(
        &mut self,
        text: &str,
        font_size: Pixels,
        font_runs: &[FontRun],
    ) -> LineLayout {
        let mut layout = LineLayout {
            font_size,
            len: text.len(),
            ..Default::default()
        };
        let mut paragraph_start = 0;

        for (separator_start, separator) in text
            .char_indices()
            .filter(|(_, character)| is_paragraph_separator(*character))
        {
            let separator_end = separator_start + separator.len_utf8();
            self.shape_segment(
                text,
                paragraph_start..separator_start,
                font_size,
                font_runs,
                &mut layout,
            );
            self.shape_segment(
                text,
                separator_start..separator_end,
                font_size,
                font_runs,
                &mut layout,
            );
            paragraph_start = separator_end;
        }

        self.shape_segment(
            text,
            paragraph_start..text.len(),
            font_size,
            font_runs,
            &mut layout,
        );

        layout
    }

    fn shape_segment(
        &mut self,
        text: &str,
        range: Range<usize>,
        font_size: Pixels,
        font_runs: &[FontRun],
        layout: &mut LineLayout,
    ) {
        if range.is_empty() {
            return;
        }

        let segment_font_runs = clip_font_runs(font_runs, range.clone());
        let segment =
            self.layout_line_no_separators(&text[range.clone()], font_size, &segment_font_runs);

        let mut segment_runs = segment.runs;
        for run in &mut segment_runs {
            for glyph in &mut run.glyphs {
                glyph.index += range.start;
                glyph.position.x += layout.width;
            }
        }

        for mut run in segment_runs {
            if let Some(same_run) = layout
                .runs
                .last_mut()
                .filter(|last| last.font_id == run.font_id)
            {
                same_run.glyphs.append(&mut run.glyphs);
            } else {
                layout.runs.push(run);
            }
        }

        layout.width += segment.width;
        layout.ascent = layout.ascent.max(segment.ascent);
        layout.descent = layout.descent.max(segment.descent);
    }

    fn layout_line_no_separators(
        &mut self,
        text: &str,
        font_size: Pixels,
        font_runs: &[FontRun],
    ) -> LineLayout {
        let mut attrs_list = AttrsList::new(&Attrs::new());
        // an emoji the run's font and its fallbacks lack is drawn from the colour emoji font,
        // matched at its own weight and style, so that cosmic-text takes it as asked; a line
        // with no emoji in it neither looks for the font nor has its spans split for it
        let emoji = if may_ask_for_emoji(text) {
            self.look_up_emoji_font();
            self.emoji_font.as_ref().and_then(Option::as_ref)
        } else {
            None
        };
        let emoji_attrs = emoji
            .map(|(id, properties)| properties.attributes(*id, &properties.primary_family_name));
        let mut offs = 0;
        for run in font_runs {
            let run_end = offs + run.len;

            let Some(properties) = self.font_match_properties(run.font_id) else {
                offs = run_end;
                continue;
            };

            let primary_attrs = properties.attributes(run.font_id, &properties.primary_family_name);
            let mut fallback_attrs: SmallVec<[Attrs<'_>; 4]> = properties
                .fallback_chain
                .iter()
                .map(|(font_id, family_name)| properties.attributes(*font_id, family_name))
                .collect();
            // the emoji font's slot is the one after the chain's
            let emoji_slot = emoji.zip(emoji_attrs.as_ref()).map(|((id, _), attrs)| {
                fallback_attrs.push(attrs.clone());
                (fallback_attrs.len() - 1, *id)
            });

            let spans = if let Some(emoji) = emoji_slot {
                let loaded_fonts = &self.loaded_fonts;
                let covers = |id: FontId, ch: char| charmap_covers(loaded_fonts, id, ch);
                compute_emoji_run_spans(
                    text,
                    offs,
                    run.len,
                    run.font_id,
                    &properties.fallback_chain,
                    emoji,
                    &covers,
                )
            } else if properties.fallback_chain.is_empty() {
                let mut spans = SmallVec::<[RunSpan; 4]>::new();
                spans.push(RunSpan {
                    start: offs,
                    end: run_end,
                    slot: None,
                    font_id: run.font_id,
                });
                spans
            } else {
                let loaded_fonts = &self.loaded_fonts;
                let covers = |id: FontId, ch: char| charmap_covers(loaded_fonts, id, ch);
                compute_run_spans(
                    text,
                    offs,
                    run.len,
                    run.font_id,
                    &properties.fallback_chain,
                    &covers,
                )
            };

            for span in spans {
                let attrs = match span.slot {
                    None => &primary_attrs,
                    Some(ix) => &fallback_attrs[ix],
                };
                attrs_list.add_span(span.start..span.end, attrs);
            }
            offs = run_end;
        }

        let line = ShapeLine::new(
            &mut self.font_system,
            text,
            &attrs_list,
            cosmic_text::Shaping::Advanced,
            4,
        );
        let mut layout_lines = Vec::with_capacity(1);
        line.layout_to_buffer(
            &mut self.scratch,
            f32::from(font_size),
            None, // We do our own wrapping
            cosmic_text::Wrap::None,
            Ellipsize::None,
            None,
            &mut layout_lines,
            None,
            cosmic_text::Hinting::Disabled,
        );

        let Some(layout) = layout_lines.first() else {
            return LineLayout {
                font_size,
                width: Pixels::ZERO,
                ascent: Pixels::ZERO,
                descent: Pixels::ZERO,
                runs: Vec::new(),
                len: text.len(),
            };
        };

        let mut runs: Vec<ShapedRun> = Vec::new();
        for glyph in &layout.glyphs {
            let mut font_id = FontId(glyph.metadata);
            let mut loaded_font = self.loaded_font(font_id);
            if loaded_font.font.id() != glyph.font_id {
                match self.font_id_for_cosmic_id(glyph.font_id) {
                    std::result::Result::Ok(resolved_id) => {
                        font_id = resolved_id;
                        loaded_font = self.loaded_font(font_id);
                    }
                    Err(error) => {
                        log::warn!(
                            "failed to resolve cosmic font id {:?}: {error:#}",
                            glyph.font_id
                        );
                        continue;
                    }
                }
            }
            let is_emoji = loaded_font.is_known_emoji_font;

            // HACK: Prevent crash caused by variation selectors.
            if glyph.glyph_id == 3 && loaded_font.is_noto_color_emoji {
                continue;
            }

            let shaped_glyph = ShapedGlyph {
                id: GlyphId(glyph.glyph_id as u32),
                position: point(glyph.x.into(), glyph.y.into()),
                index: glyph.start,
                is_emoji,
            };

            if let Some(last_run) = runs
                .last_mut()
                .filter(|last_run| last_run.font_id == font_id)
            {
                last_run.glyphs.push(shaped_glyph);
            } else {
                runs.push(ShapedRun {
                    font_id,
                    glyphs: vec![shaped_glyph],
                });
            }
        }

        // a line has the ascent and descent of the fonts its runs asked for, as cosmic-text
        // reckons them: a glyph from another font, an emoji or a symbol of cosmic-text's own
        // fallback list, sits on that baseline and does not move it, so that a line with one
        // has the baseline of a line without, as the rows of a code editor have
        let (mut ascent, mut descent) = (layout.max_ascent, layout.max_descent);
        let asked = |id: FontId| {
            let id = self.loaded_font(id).font.id();
            font_runs
                .iter()
                .any(|run| self.loaded_font(run.font_id).font.id() == id)
        };
        if runs.iter().any(|run| !asked(run.font_id)) {
            let (text_ascent, text_descent) =
                font_runs
                    .iter()
                    .fold((0f32, 0f32), |(ascent, descent), run| {
                        let metrics = self.loaded_font(run.font_id).font.metrics();
                        let upem = f32::from(metrics.units_per_em);
                        (
                            ascent.max(f32::from(font_size) * (metrics.ascent / upem)),
                            descent.max(f32::from(font_size) * (-metrics.descent / upem)),
                        )
                    });
            if text_ascent + text_descent > 0. {
                (ascent, descent) = (text_ascent, text_descent);
            }
        }

        LineLayout {
            font_size,
            width: layout.w.into(),
            ascent: ascent.into(),
            descent: descent.into(),
            runs,
            len: text.len(),
        }
    }
}

#[inline(always)]
fn is_paragraph_separator(character: char) -> bool {
    unicode_bidi::bidi_class(character) == unicode_bidi::BidiClass::B
}

fn contains_paragraph_separator(text: &str) -> bool {
    if text
        .bytes()
        .any(|byte| matches!(byte, b'\n' | b'\r' | 0x1c | 0x1d | 0x1e))
    {
        return true;
    }

    !text.is_ascii() && text.chars().any(is_paragraph_separator)
}

fn clip_font_runs(font_runs: &[FontRun], range: Range<usize>) -> SmallVec<[FontRun; 4]> {
    let mut clipped = SmallVec::new();
    let mut offs = 0;
    for run in font_runs {
        let run_start = offs;
        offs += run.len;
        if offs <= range.start {
            continue;
        }
        if run_start >= range.end {
            break;
        }
        let start = run_start.max(range.start);
        let end = offs.min(range.end);
        if start < end {
            clipped.push(FontRun {
                len: end - start,
                font_id: run.font_id,
            });
        }
    }
    clipped
}

#[cfg(feature = "font-kit")]
fn find_best_match(
    font: &Font,
    candidates: &[FontId],
    state: &CosmicTextSystemState,
) -> Result<usize> {
    let candidate_properties = candidates
        .iter()
        .map(|font_id| {
            let database_id = state.loaded_font(*font_id).font.id();
            let face_info = state
                .font_system
                .db()
                .face(database_id)
                .context("font face not found in database")?;
            Ok(face_info_into_properties(face_info))
        })
        .collect::<Result<SmallVec<[_; 4]>>>()?;

    let ix =
        font_kit::matching::find_best_match(&candidate_properties, &font_into_properties(font))
            .context("requested font family contains no font matching the other parameters")?;

    Ok(ix)
}

#[cfg(not(feature = "font-kit"))]
fn find_best_match(
    font: &Font,
    candidates: &[FontId],
    state: &CosmicTextSystemState,
) -> Result<usize> {
    if candidates.is_empty() {
        anyhow::bail!("requested font family contains no font matching the other parameters");
    }
    if candidates.len() == 1 {
        return Ok(0);
    }

    let target_weight = font.weight.0;
    let target_italic = matches!(
        font.style,
        gpui::FontStyle::Italic | gpui::FontStyle::Oblique
    );

    let mut best_index = 0;
    let mut best_score = u32::MAX;

    for (index, font_id) in candidates.iter().enumerate() {
        let database_id = state.loaded_font(*font_id).font.id();
        let face_info = state
            .font_system
            .db()
            .face(database_id)
            .context("font face not found in database")?;

        let is_italic = matches!(
            face_info.style,
            cosmic_text::Style::Italic | cosmic_text::Style::Oblique
        );
        let style_penalty: u32 = if is_italic == target_italic { 0 } else { 1000 };
        let weight_diff = (face_info.weight.0 as i32 - target_weight as i32).unsigned_abs();
        let score = style_penalty + weight_diff;

        if score < best_score {
            best_score = score;
            best_index = index;
        }
    }

    Ok(best_index)
}

/// one contiguous slice of a `FontRun` that maps to a single slot. `slot` is
/// `None` for the primary font and `Some(ix)` for `fallback_chain[ix]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RunSpan {
    start: usize,
    end: usize,
    slot: Option<usize>,
    font_id: FontId,
}

/// walks `text[run_offset..run_offset + run_len]` and groups codepoints into
/// spans. inheriting codepoints stay in the current span so shaping clusters
/// like emoji zwj sequences and combining marks are not torn apart.
fn compute_run_spans(
    text: &str,
    run_offset: usize,
    run_len: usize,
    primary: FontId,
    fallback_chain: &[(FontId, SharedString)],
    covers: &impl Fn(FontId, char) -> bool,
) -> SmallVec<[RunSpan; 4]> {
    let mut spans = SmallVec::new();
    let run_end = run_offset + run_len;
    if run_end <= run_offset {
        return spans;
    }
    if fallback_chain.is_empty() {
        spans.push(RunSpan {
            start: run_offset,
            end: run_end,
            slot: None,
            font_id: primary,
        });
        return spans;
    }
    let run_text = &text[run_offset..run_end];
    let mut span_start = run_offset;
    let mut span_slot: Option<usize> = None;
    let mut span_font_id = primary;
    for (grapheme_idx, grapheme) in run_text.grapheme_indices(true) {
        let abs = run_offset + grapheme_idx;
        let ch = grapheme.chars().next().unwrap_or('\0');
        let next_slot = pick_covering_slot(ch, span_slot, primary, fallback_chain, covers);
        if next_slot == span_slot {
            continue;
        }
        if abs > span_start {
            spans.push(RunSpan {
                start: span_start,
                end: abs,
                slot: span_slot,
                font_id: span_font_id,
            });
        }
        span_start = abs;
        span_slot = next_slot;
        span_font_id = slot_font_id(next_slot, primary, fallback_chain);
    }
    if span_start < run_end {
        spans.push(RunSpan {
            start: span_start,
            end: run_end,
            slot: span_slot,
            font_id: span_font_id,
        });
    }
    spans
}

/// `compute_run_spans` with the colour emoji font `emoji` (its slot, its id) after the
/// fallback chain: a grapheme that asks to be drawn as an emoji, which neither the primary
/// font nor a font of the chain has, gets a span of the emoji font, and the text between
/// such graphemes is split as `compute_run_spans` splits it. Without this, cosmic-text's
/// own fallback list draws such an emoji from the first font it names that has one, a
/// monochrome symbol font on Linux.
fn compute_emoji_run_spans(
    text: &str,
    run_offset: usize,
    run_len: usize,
    primary: FontId,
    fallback_chain: &[(FontId, SharedString)],
    emoji: (usize, FontId),
    covers: &impl Fn(FontId, char) -> bool,
) -> SmallVec<[RunSpan; 4]> {
    let mut spans = SmallVec::new();
    let run_end = run_offset + run_len;
    let mut text_start = run_offset;
    let push_text = |spans: &mut SmallVec<[RunSpan; 4]>, start: usize, end: usize| {
        if start < end {
            spans.extend(compute_run_spans(
                text,
                start,
                end - start,
                primary,
                fallback_chain,
                covers,
            ));
        }
    };
    for (grapheme_idx, grapheme) in text[run_offset..run_end].grapheme_indices(true) {
        if !takes_emoji_font(grapheme, primary, fallback_chain, emoji.1, covers) {
            continue;
        }
        let start = run_offset + grapheme_idx;
        let end = start + grapheme.len();
        push_text(&mut spans, text_start, start);
        text_start = end;
        if let Some(last) = spans
            .last_mut()
            .filter(|last| last.slot == Some(emoji.0) && last.end == start)
        {
            last.end = end;
        } else {
            spans.push(RunSpan {
                start,
                end,
                slot: Some(emoji.0),
                font_id: emoji.1,
            });
        }
    }
    push_text(&mut spans, text_start, run_end);
    spans
}

/// Whether `grapheme` is drawn from the emoji font: it asks for emoji presentation, by its
/// first code point's `Emoji_Presentation`, or, after an emoji, by U+FE0F (VS16) or by being
/// a sequence of emoji, the emoji font has it, and the fonts before it do not. A sequence is
/// UTS #51's: a skin tone, a ZWJ, the keycap or a tag after the first code point, as `✌🏽`,
/// `❤‍🩹` or `1⃣`, which Chromium lays out as emoji with or without VS16; without this, a
/// monochrome font of cosmic-text's list that has each code point drew them apart. VS16 and
/// a sequence pass over the primary font, whose glyph for the code point would be its text
/// presentation; U+FE0E (VS15) asks for that.
fn takes_emoji_font(
    grapheme: &str,
    primary: FontId,
    fallback_chain: &[(FontId, SharedString)],
    emoji: FontId,
    covers: &impl Fn(FontId, char) -> bool,
) -> bool {
    let mut chars = grapheme.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if grapheme.len() == 1 || chars.clone().any(|ch| ch == '\u{FE0E}') {
        return false;
    }
    let asked = chars.any(|ch| {
        matches!(
            ch,
            '\u{FE0F}' | '\u{200D}' | '\u{20E3}' | '\u{1F3FB}'..='\u{1F3FF}' | '\u{E0020}'..='\u{E007F}'
        )
    });
    let presentation = match first.emoji_status() {
        EmojiStatus::EmojiPresentation
        | EmojiStatus::EmojiPresentationAndModifierBase
        | EmojiStatus::EmojiPresentationAndEmojiComponent
        | EmojiStatus::EmojiPresentationAndModifierAndEmojiComponent => true,
        _ => asked && first.is_emoji_char(),
    };
    presentation
        && (asked || !covers(primary, first))
        && !fallback_chain.iter().any(|(id, _)| covers(*id, first))
        && covers(emoji, first)
}

fn slot_font_id(
    slot: Option<usize>,
    primary: FontId,
    fallback_chain: &[(FontId, SharedString)],
) -> FontId {
    match slot {
        None => primary,
        Some(ix) => fallback_chain[ix].0,
    }
}

fn pick_covering_slot(
    ch: char,
    current: Option<usize>,
    primary: FontId,
    fallback_chain: &[(FontId, SharedString)],
    covers: &impl Fn(FontId, char) -> bool,
) -> Option<usize> {
    if (ch as u32) <= 0x7F {
        return None;
    }
    if covers(primary, ch) {
        return None;
    }
    let current_id = slot_font_id(current, primary, fallback_chain);
    if covers(current_id, ch) {
        return current;
    }

    fallback_chain
        .iter()
        .position(|(fb_id, _)| covers(*fb_id, ch))
}

fn charmap_covers(loaded_fonts: &[LoadedFont], id: FontId, ch: char) -> bool {
    loaded_fonts
        .get(id.0)
        .is_some_and(|loaded| loaded.font.as_swash().charmap().map(ch) != 0)
}

fn cosmic_font_features(features: &FontFeatures) -> Result<CosmicFontFeatures> {
    let mut result = CosmicFontFeatures::new();
    for feature in features.0.iter() {
        let name_bytes: [u8; 4] = feature
            .0
            .as_bytes()
            .try_into()
            .context("Incorrect feature flag format")?;

        let tag = cosmic_text::FeatureTag::new(&name_bytes);

        result.set(tag, feature.1);
    }
    Ok(result)
}

#[cfg(feature = "font-kit")]
fn font_into_properties(font: &gpui::Font) -> font_kit::properties::Properties {
    font_kit::properties::Properties {
        style: match font.style {
            gpui::FontStyle::Normal => font_kit::properties::Style::Normal,
            gpui::FontStyle::Italic => font_kit::properties::Style::Italic,
            gpui::FontStyle::Oblique => font_kit::properties::Style::Oblique,
        },
        weight: font_kit::properties::Weight(font.weight.0),
        stretch: Default::default(),
    }
}

#[cfg(feature = "font-kit")]
fn face_info_into_properties(
    face_info: &cosmic_text::fontdb::FaceInfo,
) -> font_kit::properties::Properties {
    font_kit::properties::Properties {
        style: match face_info.style {
            cosmic_text::Style::Normal => font_kit::properties::Style::Normal,
            cosmic_text::Style::Italic => font_kit::properties::Style::Italic,
            cosmic_text::Style::Oblique => font_kit::properties::Style::Oblique,
        },
        weight: font_kit::properties::Weight(face_info.weight.0.into()),
        stretch: match face_info.stretch {
            cosmic_text::Stretch::Condensed => font_kit::properties::Stretch::CONDENSED,
            cosmic_text::Stretch::Expanded => font_kit::properties::Stretch::EXPANDED,
            cosmic_text::Stretch::ExtraCondensed => font_kit::properties::Stretch::EXTRA_CONDENSED,
            cosmic_text::Stretch::ExtraExpanded => font_kit::properties::Stretch::EXTRA_EXPANDED,
            cosmic_text::Stretch::Normal => font_kit::properties::Stretch::NORMAL,
            cosmic_text::Stretch::SemiCondensed => font_kit::properties::Stretch::SEMI_CONDENSED,
            cosmic_text::Stretch::SemiExpanded => font_kit::properties::Stretch::SEMI_EXPANDED,
            cosmic_text::Stretch::UltraCondensed => font_kit::properties::Stretch::ULTRA_CONDENSED,
            cosmic_text::Stretch::UltraExpanded => font_kit::properties::Stretch::ULTRA_EXPANDED,
        },
    }
}

/// The colour emoji fonts, by PostScript name, in the order fontconfig's 60-generic.conf
/// prefers their families for `emoji` (a user's fontconfig setting is not read), with
/// Firefox's Twemoji Mozilla after Segoe UI Emoji: Noto Color Emoji, Apple Color Emoji,
/// Segoe UI Emoji, Twemoji Mozilla and JoyPixels. The families of that list whose colour
/// glyphs are SVG, which swash does not draw, are left out: Twitter Color Emoji, EmojiOne
/// Mozilla, Emoji Two, Emoji One, and EmojiOne Color.
const EMOJI_FONTS: &[&str] = &[
    "NotoColorEmoji",
    "AppleColorEmoji",
    "SegoeUIEmoji",
    "TwemojiMozilla",
    "JoyPixels",
];

/// Whether the font's glyphs are drawn as colour sprites: one of `EMOJI_FONTS` whose colour
/// glyphs swash draws. The emoji font an emoji is given (`look_up_emoji_font`) is such a
/// font; a build of one with only glyphs swash cannot draw, as a COLRv1 Noto Color Emoji,
/// is not, and its emoji go down cosmic-text's list as before.
fn is_emoji_font(postscript_name: &str, font: &CosmicTextFont) -> bool {
    EMOJI_FONTS.contains(&postscript_name) && draws_in_colour(font.as_swash())
}

/// Whether swash draws the font's glyphs in colour: bitmaps (`sbix`, or `CBDT` with `CBLC`)
/// or layers (`COLR` with `CPAL`). Of `COLR` swash reads the version 0 records only, so a
/// table with none, as a COLRv1 font has, is not drawn in colour; nor are SVG glyphs.
fn draws_in_colour(font: swash::FontRef<'_>) -> bool {
    let table = |tag: &[u8; 4]| font.table(swash::tag_from_bytes(tag));
    let layers = table(b"COLR")
        .and_then(|colr| colr.get(2..4))
        .is_some_and(|count| count != [0, 0]);
    table(b"sbix").is_some()
        || (table(b"CBDT").is_some() && table(b"CBLC").is_some())
        || (layers && table(b"CPAL").is_some())
}

/// Whether the font has a colour table of any kind, which makes it a font that is kept
/// though it has no `m`.
fn has_colour_tables(font: swash::FontRef<'_>) -> bool {
    [b"CBDT", b"sbix", b"COLR", b"SVG "]
        .into_iter()
        .any(|tag| font.table(swash::tag_from_bytes(tag)).is_some())
}

/// Whether a grapheme of `text` may ask for the emoji font (`takes_emoji_font`): it has an
/// emoji beyond ASCII, or VS16 or the keycap, so that a line with other text, an `é` or a
/// `…`, does not look for the font and has its spans as before.
fn may_ask_for_emoji(text: &str) -> bool {
    !text.is_ascii()
        && text.chars().any(|ch| {
            !ch.is_ascii() && (matches!(ch, '\u{FE0F}' | '\u{20E3}') || ch.is_emoji_char())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fid(i: usize) -> FontId {
        FontId(i)
    }

    fn chain(ids: &[usize]) -> SmallVec<[(FontId, SharedString); 4]> {
        ids.iter()
            .map(|&i| (fid(i), SharedString::from(format!("fb{i}"))))
            .collect()
    }

    fn span(start: usize, end: usize, slot: Option<usize>, font_id: FontId) -> RunSpan {
        RunSpan {
            start,
            end,
            slot,
            font_id,
        }
    }

    const IBM_PLEX: &[u8] =
        include_bytes!("../../../assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf");

    /// Every code point of `Bidi_Class=B`, each of which starts a new bidi
    /// paragraph and so can split one line into mixed-direction paragraphs.
    const SEPARATORS: &[char] = &[
        '\u{000a}', '\u{000d}', '\u{001c}', '\u{001d}', '\u{001e}', '\u{0085}', '\u{2029}',
    ];

    fn text_system() -> Result<CosmicTextSystem> {
        let text_system = CosmicTextSystem::new_without_system_fonts("IBM Plex Sans");
        text_system.add_fonts(vec![Cow::Borrowed(IBM_PLEX)])?;
        Ok(text_system)
    }

    fn layout_text(text_system: &CosmicTextSystem, text: &str) -> Result<LineLayout> {
        let font_id = text_system.font_id(&gpui::font("IBM Plex Sans"))?;
        let runs = [FontRun {
            len: text.len(),
            font_id,
        }];
        Ok(text_system.layout_line(text, gpui::px(14.0), &runs))
    }

    /// Mirrors the original crash: mixed-direction text reaching the shaper
    /// through `shape_text`, which only splits lines on `\n`.
    #[test]
    fn shape_text_with_mixed_direction_paragraphs() -> Result<()> {
        let platform_text_system = Arc::new(text_system()?);
        let text_system = Arc::new(gpui::TextSystem::new(platform_text_system));
        let window_text_system = gpui::WindowTextSystem::new(text_system);

        let text: SharedString = "first line\n\u{05d0}\u{001c}A".into();
        let runs = [gpui::TextRun {
            len: text.len(),
            font: gpui::font("IBM Plex Sans"),
            ..Default::default()
        }];

        let lines = window_text_system.shape_text(text, gpui::px(14.0), &runs, None, None)?;

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].len(), "\u{05d0}\u{001c}A".len());
        assert!(lines[1].width() > Pixels::ZERO);
        Ok(())
    }

    #[test]
    fn layout_line_with_mixed_direction_paragraphs() -> Result<()> {
        let text_system = text_system()?;

        for separator in SEPARATORS {
            for text in [
                format!("\u{05d0}{separator}A"),
                format!("A{separator}\u{05d0}"),
            ] {
                let layout = layout_text(&text_system, &text)?;

                assert_eq!(layout.len, text.len(), "{text:?}");
                assert!(layout.width > Pixels::ZERO, "{text:?}");
                assert!(
                    layout.runs.iter().any(|run| !run.glyphs.is_empty()),
                    "{text:?}"
                );
            }
        }

        Ok(())
    }

    #[test]
    fn layout_line_with_separators_at_line_edges() -> Result<()> {
        let text_system = text_system()?;

        for text in [
            "\u{001c}",
            "\u{001c}\u{001c}",
            "\u{001c}\u{05d0}",
            "\u{05d0}\u{001c}",
            "\u{05d0}\u{001c}\u{001c}A",
            "\u{001c}\u{05d0}\u{001c}A\u{001c}",
        ] {
            let layout = layout_text(&text_system, text)?;
            assert_eq!(layout.len, text.len(), "{text:?}");
        }

        Ok(())
    }

    /// Glyph indices must stay absolute and positions ordered across segment
    /// boundaries, otherwise cursor placement and hit testing desync. Uses
    /// single-direction text so visual order matches logical order.
    #[test]
    fn layout_line_keeps_indices_and_positions_ordered_across_paragraphs() -> Result<()> {
        let text_system = text_system()?;
        let text = "ab\u{001c}cd\u{2029}ef";
        let layout = layout_text(&text_system, text)?;

        let glyphs: Vec<_> = layout.runs.iter().flat_map(|run| &run.glyphs).collect();
        assert!(!glyphs.is_empty());

        for glyph in &glyphs {
            assert!(glyph.index < text.len(), "{:?}", glyph.index);
            assert!(text.is_char_boundary(glyph.index), "{:?}", glyph.index);
        }
        for pair in glyphs.windows(2) {
            assert!(pair[0].index < pair[1].index);
            assert!(pair[0].position.x <= pair[1].position.x);
        }

        // Every segment contributes width, so the whole line is wider than its
        // leading paragraph alone.
        assert!(layout.width > layout_text(&text_system, "ab")?.width);
        Ok(())
    }

    /// A font run boundary that does not line up with a paragraph boundary must
    /// still be clipped to the right segments.
    #[test]
    fn layout_line_with_font_run_straddling_a_separator() -> Result<()> {
        let text_system = text_system()?;
        let font_id = text_system.font_id(&gpui::font("IBM Plex Sans"))?;
        let text = "ab\u{001c}\u{05d0}\u{05d1}";

        // The run boundary falls inside the trailing RTL paragraph.
        let runs = [
            FontRun {
                len: "ab\u{001c}\u{05d0}".len(),
                font_id,
            },
            FontRun {
                len: "\u{05d1}".len(),
                font_id,
            },
        ];
        let layout = text_system.layout_line(text, gpui::px(14.0), &runs);

        assert_eq!(layout.len, text.len());
        assert!(layout.width > Pixels::ZERO);
        Ok(())
    }

    /// Lines with no separator take the fast path and must be shaped exactly as
    /// they were before paragraph splitting existed.
    #[test]
    fn layout_line_without_separators_takes_fast_path() -> Result<()> {
        let text_system = text_system()?;

        for text in [
            "hello world",
            "\u{05d0}\u{05d1}\u{05d2}",
            "mixed \u{05d0}\u{05d1}",
        ] {
            assert!(!contains_paragraph_separator(text), "{text:?}");
            let layout = layout_text(&text_system, text)?;
            assert_eq!(layout.len, text.len(), "{text:?}");
            assert!(layout.width > Pixels::ZERO, "{text:?}");
        }

        Ok(())
    }

    #[test]
    fn paragraph_separator_detection() {
        for separator in SEPARATORS {
            assert!(is_paragraph_separator(*separator), "{separator:?}");
            assert!(contains_paragraph_separator(&format!("a{separator}b")));
        }

        for text in [
            "",
            "plain ascii",
            "\u{05d0}",
            "tab\there",
            "emoji \u{1f600}",
        ] {
            assert!(!contains_paragraph_separator(text), "{text:?}");
        }
    }

    #[test]
    fn font_runs_are_clipped_to_segment() {
        let runs = [
            FontRun {
                len: 3,
                font_id: fid(1),
            },
            FontRun {
                len: 4,
                font_id: fid(2),
            },
        ];

        assert_eq!(clip_font_runs(&runs, 0..7).as_slice(), &runs);
        assert_eq!(
            clip_font_runs(&runs, 2..5).as_slice(),
            &[
                FontRun {
                    len: 1,
                    font_id: fid(1)
                },
                FontRun {
                    len: 2,
                    font_id: fid(2)
                },
            ]
        );
        assert_eq!(
            clip_font_runs(&runs, 3..7).as_slice(),
            &[FontRun {
                len: 4,
                font_id: fid(2)
            }]
        );
        assert!(clip_font_runs(&runs, 5..5).is_empty());
    }

    #[test]
    fn primary_wins_over_current_fallback_when_primary_covers() {
        let primary = fid(0);
        let fb = chain(&[1, 2]);
        let covers = |id: FontId, _: char| id == fid(0) || id == fid(1);
        assert_eq!(
            pick_covering_slot('a', Some(0), primary, &fb, &covers),
            None
        );
    }

    #[test]
    fn primary_preferred_over_fallback_when_both_cover() {
        let primary = fid(0);
        let fb = chain(&[1]);
        let covers = |_: FontId, _: char| true;
        assert_eq!(pick_covering_slot('a', None, primary, &fb, &covers), None);
    }

    #[test]
    fn falls_through_chain_in_order() {
        let primary = fid(0);
        let fb = chain(&[1, 2, 3]);
        // only fallback 2 at index 1 covers.
        let covers = |id: FontId, _: char| id == fid(2);
        assert_eq!(
            pick_covering_slot('字', None, primary, &fb, &covers),
            Some(1)
        );
    }

    #[test]
    fn no_coverage_returns_primary() {
        let primary = fid(0);
        let fb = chain(&[1, 2]);
        let covers = |_: FontId, _: char| false;
        // nothing covers. return `None` so the `cosmic-text` built in script
        // fallback can take over during shaping.
        assert_eq!(
            pick_covering_slot('\u{1F600}', Some(1), primary, &fb, &covers),
            None
        );
    }

    #[test]
    fn empty_chain_always_returns_primary() {
        let primary = fid(0);
        let fb: SmallVec<[(FontId, SharedString); 4]> = SmallVec::new();
        let covers = |_: FontId, _: char| false;
        assert_eq!(pick_covering_slot('a', None, primary, &fb, &covers), None);
    }

    #[test]
    fn slot_font_id_resolution() {
        let primary = fid(7);
        let fb = chain(&[10, 20]);
        assert_eq!(slot_font_id(None, primary, &fb), fid(7));
        assert_eq!(slot_font_id(Some(0), primary, &fb), fid(10));
        assert_eq!(slot_font_id(Some(1), primary, &fb), fid(20));
    }

    #[test]
    fn run_spans_with_no_chain_emit_one_primary_span() {
        let primary = fid(0);
        let fb: SmallVec<[(FontId, SharedString); 4]> = SmallVec::new();
        let covers = |_: FontId, _: char| false;
        let text = "hello";
        let spans = compute_run_spans(text, 0, text.len(), primary, &fb, &covers);
        assert_eq!(spans.as_slice(), &[span(0, text.len(), None, primary)]);
    }

    #[test]
    fn run_spans_use_byte_offsets_for_multibyte_chars() {
        let primary = fid(0);
        let fb = chain(&[1]);
        // primary covers ascii. fallback covers cjk.
        let covers = |id: FontId, ch: char| {
            if id == primary {
                ch.is_ascii()
            } else {
                !ch.is_ascii()
            }
        };
        let text = "a字b";
        let spans = compute_run_spans(text, 0, text.len(), primary, &fb, &covers);
        // '字' is 3 bytes so split is at 1 then 4.
        assert_eq!(
            spans.as_slice(),
            &[
                span(0, 1, None, primary),
                span(1, 4, Some(0), fid(1)),
                span(4, 5, None, primary),
            ]
        );
    }

    #[test]
    fn run_spans_respect_run_offset() {
        let primary = fid(0);
        let fb = chain(&[1]);
        let covers = |id: FontId, ch: char| {
            if id == primary {
                ch.is_ascii()
            } else {
                !ch.is_ascii()
            }
        };
        // outer text has a prefix that is not part of this run.
        let text = "xx字y";
        let run_offset = 2;
        let run_len = text.len() - run_offset;
        let spans = compute_run_spans(text, run_offset, run_len, primary, &fb, &covers);
        assert_eq!(
            spans.as_slice(),
            &[span(2, 5, Some(0), fid(1)), span(5, 6, None, primary)]
        );
    }

    #[test]
    fn run_spans_keep_combining_marks_with_base_in_fallback() {
        let primary = fid(0);
        let fb = chain(&[1]);
        // primary covers ascii only. fallback covers the base char.
        // combining mark must stay in the fallback span even when fallback
        // does not advertise coverage of it.
        let covers = |id: FontId, ch: char| {
            if id == primary {
                ch.is_ascii()
            } else {
                ch == '\u{0905}'
            }
        };
        // \u{0905} devanagari short a + \u{0902} candrabindu mark.
        let text = "\u{0905}\u{0902}";
        let spans = compute_run_spans(text, 0, text.len(), primary, &fb, &covers);
        assert_eq!(spans.as_slice(), &[span(0, text.len(), Some(0), fid(1))]);
    }

    #[test]
    fn run_spans_keep_zwj_inside_emoji_cluster() {
        let primary = fid(0);
        let fb = chain(&[1]);
        // only fallback covers the emoji codepoints. zwj must not split.
        let covers = |id: FontId, ch: char| id == fid(1) && ch != '\u{200D}';
        // family zwj sequence woman zwj girl.
        let text = "\u{1F469}\u{200D}\u{1F467}";
        let spans = compute_run_spans(text, 0, text.len(), primary, &fb, &covers);
        assert_eq!(spans.as_slice(), &[span(0, text.len(), Some(0), fid(1))]);
    }

    #[test]
    fn run_spans_collapse_adjacent_same_slot() {
        let primary = fid(0);
        let fb = chain(&[1]);
        let covers = |id: FontId, ch: char| {
            if id == primary {
                ch.is_ascii()
            } else {
                !ch.is_ascii()
            }
        };
        let text = "字字字";
        let spans = compute_run_spans(text, 0, text.len(), primary, &fb, &covers);
        assert_eq!(spans.as_slice(), &[span(0, text.len(), Some(0), fid(1))]);
    }

    #[test]
    fn run_spans_empty_run_returns_no_spans() {
        let primary = fid(0);
        let fb = chain(&[1]);
        let covers = |_: FontId, _: char| true;
        let spans = compute_run_spans("anything", 3, 0, primary, &fb, &covers);
        assert!(spans.is_empty());
    }

    /// The emoji font in these, `fid(9)`, has every emoji and the symbols beside them, as
    /// Noto Color Emoji has; the primary font has ASCII and `⚡`, `❤` and `↔`, as
    /// JetBrains Mono has `⚡` and `↔`.
    fn emoji_covers(id: FontId, ch: char) -> bool {
        match id.0 {
            0 => ch.is_ascii() || matches!(ch, '⚡' | '❤' | '↔'),
            9 => !ch.is_ascii() || matches!(ch, '#' | '0'..='9'),
            _ => false,
        }
    }

    fn emoji_spans(text: &str, fb: &[(FontId, SharedString)]) -> SmallVec<[RunSpan; 4]> {
        let emoji = (fb.len(), fid(9));
        compute_emoji_run_spans(text, 0, text.len(), fid(0), fb, emoji, &emoji_covers)
    }

    #[test]
    fn an_emoji_the_primary_lacks_takes_the_emoji_font() {
        let text = "\"🐈\",";
        assert_eq!(
            emoji_spans(text, &[]).as_slice(),
            &[
                span(0, 1, None, fid(0)),
                span(1, 5, Some(0), fid(9)),
                span(5, 7, None, fid(0)),
            ]
        );
    }

    #[test]
    fn emoji_side_by_side_and_a_zwj_sequence_are_one_span() {
        // cat, cat, woman zwj girl
        let text = "a🐈🐈\u{1F469}\u{200D}\u{1F467}";
        assert_eq!(
            emoji_spans(text, &[]).as_slice(),
            &[
                span(0, 1, None, fid(0)),
                span(1, text.len(), Some(0), fid(9))
            ]
        );
    }

    #[test]
    fn an_emoji_the_primary_has_stays_in_the_primary() {
        assert_eq!(
            emoji_spans("a⚡", &[]).as_slice(),
            &[span(0, 4, None, fid(0))]
        );
    }

    #[test]
    fn vs16_asks_for_the_emoji_font_over_the_primary() {
        let text = "❤\u{FE0F} #\u{FE0F}\u{20E3}";
        let heart = "❤\u{FE0F}".len();
        assert_eq!(
            emoji_spans(text, &[]).as_slice(),
            &[
                span(0, heart, Some(0), fid(9)),
                span(heart, heart + 1, None, fid(0)),
                span(heart + 1, text.len(), Some(0), fid(9)),
            ]
        );
    }

    #[test]
    fn vs15_and_a_symbol_of_text_presentation_are_not_emoji() {
        // the cat with VS15 asks for text; U+2194 has text presentation and U+263A too, which
        // the primary font lacks: they are left to cosmic-text's fallback, as before
        for text in ["🐈\u{FE0E}", "\u{2194}", "\u{263A}", "é"] {
            assert_eq!(
                emoji_spans(text, &[]).as_slice(),
                &[span(0, text.len(), None, fid(0))],
                "{text:?}"
            );
        }
    }

    #[test]
    fn a_fallback_of_the_callers_is_asked_before_the_emoji_font() {
        let fb = chain(&[1]);
        let covers = |id: FontId, ch: char| id == fid(1) || emoji_covers(id, ch);
        let text = "🐈";
        let spans = compute_emoji_run_spans(text, 0, text.len(), fid(0), &fb, (1, fid(9)), &covers);
        assert_eq!(spans.as_slice(), &[span(0, text.len(), Some(0), fid(1))]);
    }

    #[test]
    fn emoji_spans_respect_the_run_offset() {
        let text = "xx🐈y";
        let spans = compute_emoji_run_spans(
            text,
            2,
            text.len() - 2,
            fid(0),
            &[],
            (0, fid(9)),
            &emoji_covers,
        );
        assert_eq!(
            spans.as_slice(),
            &[span(2, 6, Some(0), fid(9)), span(6, 7, None, fid(0))]
        );
    }

    /// A text system with the machine's fonts and IBM Plex Sans, and whether the machine has
    /// Noto Color Emoji, without which the tests below have nothing to test and pass.
    fn system_text_system() -> Result<Option<CosmicTextSystem>> {
        let text_system = CosmicTextSystem::new("IBM Plex Sans");
        text_system.add_fonts(vec![Cow::Borrowed(IBM_PLEX)])?;
        if text_system
            .all_font_names()
            .iter()
            .any(|name| name == "Noto Color Emoji")
        {
            Ok(Some(text_system))
        } else {
            eprintln!("Noto Color Emoji is not installed: skipped");
            Ok(None)
        }
    }

    /// The font each glyph of `text` is drawn from, by its postscript name, and whether it is
    /// drawn as an emoji.
    fn glyph_fonts(
        text_system: &CosmicTextSystem,
        font: &Font,
        text: &str,
    ) -> Result<Vec<(String, bool)>> {
        let font_id = text_system.font_id(font)?;
        let runs = [FontRun {
            len: text.len(),
            font_id,
        }];
        let layout = text_system.layout_line(text, gpui::px(14.0), &runs);
        let state = text_system.0.read();
        Ok(layout
            .runs
            .iter()
            .flat_map(|run| {
                let id = state.loaded_font(run.font_id).font.id();
                let name = state
                    .font_system
                    .db()
                    .face(id)
                    .map(|face| face.post_script_name.clone())
                    .unwrap_or_default();
                run.glyphs
                    .iter()
                    .map(move |glyph| (name.clone(), glyph.is_emoji))
            })
            .collect())
    }

    #[test]
    fn an_emoji_takes_the_colour_font_before_symbols2() -> Result<()> {
        let Some(text_system) = system_text_system()? else {
            return Ok(());
        };
        let fonts = glyph_fonts(&text_system, &gpui::font("IBM Plex Sans"), "\"🐈\",")?;
        assert_eq!(
            fonts,
            [
                ("IBMPlexSans".to_string(), false),
                ("NotoColorEmoji".to_string(), true),
                ("IBMPlexSans".to_string(), false),
                ("IBMPlexSans".to_string(), false),
            ]
        );
        Ok(())
    }

    #[test]
    fn an_emoji_does_not_move_the_baseline() -> Result<()> {
        let Some(text_system) = system_text_system()? else {
            return Ok(());
        };
        let plain = layout_text(&text_system, "a")?;
        for text in ["a🐈", "🐈", "\"🐈😀\","] {
            let emoji = layout_text(&text_system, text)?;
            assert!(
                emoji
                    .runs
                    .iter()
                    .flat_map(|run| &run.glyphs)
                    .any(|glyph| glyph.is_emoji)
            );
            assert_eq!(
                (emoji.ascent, emoji.descent),
                (plain.ascent, plain.descent),
                "{text:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn an_emoji_font_without_an_m_is_kept() -> Result<()> {
        let Some(text_system) = system_text_system()? else {
            return Ok(());
        };
        let font = Font {
            fallbacks: Some(FontFallbacks::from_fonts(vec!["Noto Color Emoji".into()])),
            ..gpui::font("IBM Plex Sans")
        };
        let fonts = glyph_fonts(&text_system, &font, "🐈")?;
        assert!(
            text_system
                .all_font_names()
                .iter()
                .any(|name| name == "Noto Color Emoji"),
            "named as a fallback, Noto Color Emoji was taken out of the database"
        );
        assert_eq!(fonts, [("NotoColorEmoji".to_string(), true)]);
        Ok(())
    }

    /// The bytes of the machine's font of that PostScript name, if it has it.
    fn system_font(postscript_name: &str) -> Result<Option<Vec<u8>>> {
        let mut db = cosmic_text::fontdb::Database::new();
        db.load_system_fonts();
        let path = db.faces().find_map(|face| match &face.source {
            cosmic_text::fontdb::Source::File(path) if face.post_script_name == postscript_name => {
                Some(path.clone())
            }
            _ => None,
        });
        Ok(path.map(std::fs::read).transpose()?)
    }

    #[test]
    fn an_emoji_font_added_after_an_emoji_was_laid_out_is_found() -> Result<()> {
        let (Some(symbols), Some(emoji)) = (
            system_font("NotoSansSymbols2-Regular")?,
            system_font("NotoColorEmoji")?,
        ) else {
            eprintln!("Noto Sans Symbols2 or Noto Color Emoji is not installed: skipped");
            return Ok(());
        };
        let text_system = text_system()?;
        text_system.add_fonts(vec![Cow::Owned(symbols)])?;
        let font = gpui::font("IBM Plex Sans");
        assert_eq!(
            glyph_fonts(&text_system, &font, "🐈")?,
            [("NotoSansSymbols2-Regular".to_string(), false)]
        );
        text_system.add_fonts(vec![Cow::Owned(emoji)])?;
        assert_eq!(
            glyph_fonts(&text_system, &font, "🐈")?,
            [("NotoColorEmoji".to_string(), true)]
        );
        Ok(())
    }

    #[test]
    fn an_emoji_is_rasterised_in_colour() -> Result<()> {
        let Some(text_system) = system_text_system()? else {
            return Ok(());
        };
        let font_id = text_system.font_id(&gpui::font("IBM Plex Sans"))?;
        let text = "🐈";
        let layout = text_system.layout_line(
            text,
            gpui::px(14.0),
            &[FontRun {
                len: text.len(),
                font_id,
            }],
        );
        let run = layout
            .runs
            .first()
            .ok_or_else(|| anyhow::anyhow!("the cat was laid out in no run"))?;
        let params = RenderGlyphParams {
            font_id: run.font_id,
            glyph_id: run.glyphs[0].id,
            font_size: gpui::px(14.0),
            subpixel_variant: Default::default(),
            scale_factor: 1.0,
            is_emoji: true,
            subpixel_rendering: false,
            dilation: 0,
        };
        let bounds = text_system.glyph_raster_bounds(&params)?;
        let (size, bytes) = text_system.rasterize_glyph(&params, bounds)?;
        assert!(size.width.0 > 8 && size.height.0 > 8, "{size:?}");
        assert_eq!(bytes.len(), (size.width.0 * size.height.0 * 4) as usize);
        // BGRA: some opaque pixel is not grey
        assert!(
            bytes
                .chunks_exact(4)
                .any(|px| px[3] > 128 && (px[0].abs_diff(px[2]) > 40 || px[1].abs_diff(px[2]) > 40)),
            "the cat has no colour"
        );
        Ok(())
    }

    #[test]
    fn a_modifier_zwj_or_keycap_sequence_takes_the_emoji_font() {
        // victory hand and detective, of text presentation, with a skin tone; detective zwj
        // female sign; heart zwj fire, whose heart the primary has; a keycap without VS16
        for text in [
            "\u{270C}\u{1F3FD}",
            "\u{1F575}\u{1F3FD}",
            "\u{1F575}\u{200D}\u{2640}",
            "\u{2764}\u{200D}\u{1F525}",
            "1\u{20E3}",
        ] {
            assert_eq!(
                emoji_spans(text, &[]).as_slice(),
                &[span(0, text.len(), Some(0), fid(9))],
                "{text:?}"
            );
        }
        // a zwj in a script, after a letter that is no emoji, is not a sequence of emoji
        let text = "\u{915}\u{94D}\u{200D}";
        assert_eq!(
            emoji_spans(text, &[]).as_slice(),
            &[span(0, text.len(), None, fid(0))]
        );
    }

    #[test]
    fn a_sequence_of_emoji_without_vs16_is_one_colour_glyph() -> Result<()> {
        let Some(text_system) = system_text_system()? else {
            return Ok(());
        };
        let font = gpui::font("IBM Plex Sans");
        // eye zwj speech bubble and heart zwj adhesive bandage, whose parts Noto Sans Symbols2
        // has, were two monochrome glyphs of it; a keycap without VS16 was Noto Sans Symbols's
        // `1` and keycap; victory hand with a skin tone came out whole by cosmic-text's list
        for text in [
            "\u{1F441}\u{200D}\u{1F5E8}",
            "\u{2764}\u{200D}\u{1FA79}",
            "1\u{20E3}",
            "\u{270C}\u{1F3FD}",
        ] {
            assert_eq!(
                glyph_fonts(&text_system, &font, text)?,
                [("NotoColorEmoji".to_string(), true)],
                "{text:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn a_fallback_font_does_not_move_the_baseline() -> Result<()> {
        let Some(text_system) = system_text_system()? else {
            return Ok(());
        };
        let plain = layout_text(&text_system, "a")?;
        let primary = text_system.font_id(&gpui::font("IBM Plex Sans"))?;
        // a cat asking for text, from Noto Sans Symbols2, and a CJK ideograph
        let mut fell_back = false;
        for text in ["a\u{1F408}\u{FE0E}", "a\u{4E2D}"] {
            let layout = layout_text(&text_system, text)?;
            if layout.runs.iter().all(|run| run.font_id == primary) {
                eprintln!("{text:?} had no fallback font: skipped");
                continue;
            }
            fell_back = true;
            assert_eq!(
                (layout.ascent, layout.descent),
                (plain.ascent, plain.descent),
                "{text:?}"
            );
        }
        if !fell_back {
            eprintln!("no fallback font for either line: nothing tested");
        }
        Ok(())
    }

    /// `font` with its family named `family`: a name table of its own appended to the font,
    /// which the table directory points at instead.
    fn renamed(font: &[u8], family: &str) -> Vec<u8> {
        let be16 =
            |bytes: &[u8], at: usize| usize::from(u16::from_be_bytes([bytes[at], bytes[at + 1]]));
        let be32 = |bytes: &[u8], at: usize| {
            u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize
        };
        let record = (0..be16(font, 4))
            .map(|ix| 12 + 16 * ix)
            .find(|&record| &font[record..record + 4] == b"name")
            .expect("a name table");
        let name = &font[be32(font, record + 8)..][..be32(font, record + 12)];
        let storage = be16(name, 4);
        let mut table = name[..storage].to_vec();
        let mut strings = name[storage..].to_vec();
        let wide: Vec<u8> = family.encode_utf16().flat_map(u16::to_be_bytes).collect();
        let (wide_at, narrow_at) = (strings.len(), strings.len() + wide.len());
        strings.extend(&wide);
        strings.extend(family.as_bytes());
        for ix in 0..be16(name, 2) {
            let at = 6 + 12 * ix;
            if !matches!(be16(name, at + 6), 1 | 16) {
                continue;
            }
            let (len, offset) = if be16(name, at) == 1 {
                (family.len(), narrow_at)
            } else {
                (wide.len(), wide_at)
            };
            table[at + 8..at + 10].copy_from_slice(&(len as u16).to_be_bytes());
            table[at + 10..at + 12].copy_from_slice(&(offset as u16).to_be_bytes());
        }
        table.extend(strings);
        let mut out = font.to_vec();
        out.resize(out.len().next_multiple_of(4), 0);
        let at = out.len() as u32;
        out.extend(&table);
        out[record + 8..record + 12].copy_from_slice(&at.to_be_bytes());
        out[record + 12..record + 16].copy_from_slice(&(table.len() as u32).to_be_bytes());
        out
    }

    #[test]
    fn an_emoji_family_drawn_in_no_colour_is_neither_taken_nor_removed() -> Result<()> {
        let Some(symbols) = system_font("NotoSansSymbols2-Regular")? else {
            eprintln!("Noto Sans Symbols2 is not installed: skipped");
            return Ok(());
        };
        // as a Twemoji Mozilla with no COLR would be, or an emoji font of SVG glyphs: no `m`,
        // and no colour glyphs swash draws
        let text_system = text_system()?;
        text_system.add_fonts(vec![Cow::Owned(renamed(&symbols, "Twemoji Mozilla"))])?;
        assert!(
            text_system
                .all_font_names()
                .iter()
                .any(|name| name == "Twemoji Mozilla")
        );
        let fonts = glyph_fonts(&text_system, &gpui::font("IBM Plex Sans"), "a\u{1F408}")?;
        assert!(
            text_system
                .all_font_names()
                .iter()
                .any(|name| name == "Twemoji Mozilla"),
            "looking for the emoji font took the face out of the database"
        );
        assert!(fonts.iter().all(|(_, is_emoji)| !is_emoji), "{fonts:?}");
        Ok(())
    }

    #[test]
    fn an_outline_drawn_as_an_emoji_is_four_bytes_a_pixel() -> Result<()> {
        // an emoji font's glyph with no colour falls through to its outline, a mask of one
        // byte a pixel, which the colour atlas would take as four
        let text_system = text_system()?;
        let font_id = text_system.font_id(&gpui::font("IBM Plex Sans"))?;
        let glyph_id = text_system
            .glyph_for_char(font_id, 'm')
            .ok_or_else(|| anyhow::anyhow!("no m"))?;
        let params = RenderGlyphParams {
            font_id,
            glyph_id,
            font_size: gpui::px(14.0),
            subpixel_variant: Default::default(),
            scale_factor: 1.0,
            is_emoji: true,
            subpixel_rendering: false,
            dilation: 0,
        };
        let bounds = text_system.glyph_raster_bounds(&params)?;
        let (size, bytes) = text_system.rasterize_glyph(&params, bounds)?;
        assert_eq!(bytes.len(), (size.width.0 * size.height.0 * 4) as usize);
        Ok(())
    }

    /// A font of nothing but a table directory and these tables.
    fn font_of_tables(tables: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let mut tables = tables.to_vec();
        tables.sort_by_key(|(tag, _)| **tag);
        let header = 12 + 16 * tables.len();
        let mut directory = vec![0, 1, 0, 0];
        directory.extend((tables.len() as u16).to_be_bytes());
        directory.extend([0; 6]);
        let mut data = Vec::new();
        for (tag, table) in &tables {
            directory.extend(*tag);
            directory.extend([0; 4]);
            directory.extend(((header + data.len()) as u32).to_be_bytes());
            directory.extend((table.len() as u32).to_be_bytes());
            data.extend(*table);
            data.resize(data.len().next_multiple_of(4), 0);
        }
        directory.extend(data);
        directory
    }

    #[test]
    fn a_font_is_drawn_in_colour_from_the_tables_swash_draws() {
        // COLR version 0 with one base glyph record, and version 1 with none, as COLRv1 has
        let colr_v0: &[u8] = &[
            0, 0, 0, 1, 0, 0, 0, 14, 0, 0, 0, 20, 0, 1, 0, 5, 0, 0, 0, 1, 0, 6, 0, 0,
        ];
        let colr_v1: &[u8] = &[0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let table: &[u8] = &[0; 8];
        for (tables, colour) in [
            (vec![(b"COLR", colr_v0), (b"CPAL", table)], true),
            (vec![(b"COLR", colr_v1), (b"CPAL", table)], false),
            (vec![(b"COLR", colr_v0)], false),
            (vec![(b"CBDT", table), (b"CBLC", table)], true),
            (vec![(b"CBDT", table)], false),
            (vec![(b"sbix", table)], true),
            (vec![(b"SVG ", table)], false),
            (vec![(b"glyf", table)], false),
        ] {
            let bytes = font_of_tables(&tables);
            let font = swash::FontRef::from_index(&bytes, 0).expect("a font");
            let tags: Vec<_> = tables
                .iter()
                .map(|(tag, _)| String::from_utf8_lossy(*tag))
                .collect();
            assert_eq!(draws_in_colour(font), colour, "{tags:?}");
            assert!(has_colour_tables(font) || tags == ["glyf"], "{tags:?}");
        }
    }

    #[test]
    fn only_a_line_with_an_emoji_looks_for_the_emoji_font() {
        for text in [
            "a\u{1F408}",
            "\u{2764}\u{FE0F}",
            "1\u{20E3}",
            "\u{A9}",
            "\u{270C}\u{1F3FD}",
        ] {
            assert!(may_ask_for_emoji(text), "{text:?}");
        }
        for text in [
            "",
            "abc 123 #*",
            "\u{2026}",
            "\u{2192} \u{2014} \u{E9}",
            "\u{4E2D}\u{6587}",
        ] {
            assert!(!may_ask_for_emoji(text), "{text:?}");
        }
    }
}
