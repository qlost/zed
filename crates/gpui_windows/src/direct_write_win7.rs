// Windows 7 / DirectWrite 1.0 text backend.
//
// This is a stripped-down, Win7-compatible implementation of `PlatformTextSystem` that only relies
// on DirectWrite 1.0 APIs (the base `IDWriteFactory`/`IDWriteFontCollection`/`IDWriteFontFace`),
// which are guaranteed present on Win7 without any Platform Update. It shares the DWrite-1.0-safe
// helpers from `direct_write` (the Win10+ backend) but cannot reuse its state machine or text
// renderer, which are tied to `IDWriteFactory5` / `IDWriteFontSet` (Win10-only).
//
// Accepted degradation compared to the Win10 backend:
// - monochrome / grayscale text only (no color emoji glyphs)
// - no custom (in-memory) fonts: `add_fonts` is a no-op
// - no font fallback chains: `FontFallbacks` are ignored
//
// Font selection walks the system font collection via `IDWriteFontCollection::GetFontFamily` /
// `IDWriteFont::CreateFontFace` instead of the Win10-only `IDWriteFontSet`/`IDWriteFontSetBuilder`.
// Deliberately avoids the DWrite 1.1 interfaces (`IDWriteFactory1`, `IDWriteFontCollection1`,
// `IDWriteFontFace1`): on some Win7 setups a QueryInterface for `IDWriteFontCollection1` on the
// system font collection crashes, so this module stays on base (DWrite 1.0) interfaces only.

use std::{
    borrow::Cow,
    ffi::c_void,
    mem::ManuallyDrop,
};

use anyhow::{Context, Result};
use collections::HashMap;
use gpui_util::ResultExt;
use parking_lot::{RwLock, RwLockUpgradableReadGuard};
use windows::{
    Win32::{
        Foundation::*,
        Globalization::GetUserDefaultLocaleName,
        Graphics::DirectWrite::*,
        System::SystemServices::LOCALE_NAME_MAX_LENGTH,
    },
    core::*,
};

use crate::direct_write::{
    apply_font_features, font_style_to_dwrite, font_weight_to_dwrite, get_name,
    get_system_subpixel_rendering, get_system_ui_font_name,
};
use crate::directx_devices::DirectXDevices;
use gpui::*;

#[derive(Debug)]
struct FontInfo {
    font_family_h: HSTRING,
    font_face: IDWriteFontFace,
    weight: DWRITE_FONT_WEIGHT,
    style: DWRITE_FONT_STYLE,
    features: IDWriteTypography,
    font_collection: IDWriteFontCollection,
}

pub(crate) struct DirectWriteTextSystem {
    components: DirectWriteComponents,
    state: RwLock<DirectWriteState>,
}

struct DirectWriteComponents {
    locale: HSTRING,
    factory: IDWriteFactory,
    text_renderer: TextRendererWrapper,
    system_ui_font_name: SharedString,
    system_subpixel_rendering: bool,
}

struct DirectWriteState {
    system_font_collection: IDWriteFontCollection,
    fonts: Vec<FontInfo>,
    font_to_font_id: HashMap<Font, FontId>,
    font_info_cache: HashMap<usize, FontId>,
    layout_line_scratch: Vec<u16>,
}

impl DirectWriteTextSystem {
    pub(crate) fn new(_directx_devices: &DirectXDevices) -> Result<Self> {
        let factory: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };
        let mut locale = [0u16; LOCALE_NAME_MAX_LENGTH as usize];
        unsafe { GetUserDefaultLocaleName(&mut locale) };
        let locale = HSTRING::from_wide(&locale);
        let text_renderer = TextRendererWrapper::new();

        let system_subpixel_rendering = get_system_subpixel_rendering();
        let system_ui_font_name = get_system_ui_font_name();
        let components = DirectWriteComponents {
            locale,
            factory,
            text_renderer,
            system_ui_font_name,
            system_subpixel_rendering,
        };

        let system_font_collection = unsafe {
            let mut result: Option<IDWriteFontCollection> = None;
            components
                .factory
                .GetSystemFontCollection(&mut result, true)?;
            result.context("Failed to get system font collection")?
        };

        Ok(Self {
            components,
            state: RwLock::new(DirectWriteState {
                system_font_collection,
                fonts: Vec::new(),
                font_to_font_id: HashMap::default(),
                font_info_cache: HashMap::default(),
                layout_line_scratch: Vec::new(),
            }),
        })
    }

    pub(crate) fn handle_gpu_lost(&self, directx_devices: &DirectXDevices) -> Result<()> {
        self.state.write().handle_gpu_lost(directx_devices)
    }
}

impl PlatformTextSystem for DirectWriteTextSystem {
    fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        self.state.write().add_fonts(&self.components, fonts)
    }

    fn all_font_names(&self) -> Vec<String> {
        self.state.read().all_font_names(&self.components)
    }

    fn font_id(&self, font: &Font) -> Result<FontId> {
        let lock = self.state.upgradable_read();
        if let Some(font_id) = lock.font_to_font_id.get(font) {
            Ok(*font_id)
        } else {
            RwLockUpgradableReadGuard::upgrade(lock)
                .select_and_cache_font(&self.components, font)
                .with_context(|| format!("Failed to select font: {:?}", font))
        }
    }

    fn font_metrics(&self, font_id: FontId) -> FontMetrics {
        self.state.read().font_metrics(font_id)
    }

    fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
        self.state.read().get_typographic_bounds(font_id, glyph_id)
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> anyhow::Result<Size<f32>> {
        self.state.read().get_advance(font_id, glyph_id)
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        self.state.read().glyph_for_char(font_id, ch)
    }

    fn glyph_raster_bounds(
        &self,
        params: &RenderGlyphParams,
    ) -> anyhow::Result<Bounds<DevicePixels>> {
        self.state.read().raster_bounds(&self.components, params)
    }

    fn rasterize_glyph(
        &self,
        params: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> anyhow::Result<(Size<DevicePixels>, Vec<u8>)> {
        self.state
            .read()
            .rasterize_glyph(&self.components, params, raster_bounds)
    }

    fn layout_line(&self, text: &str, font_size: Pixels, runs: &[FontRun]) -> LineLayout {
        self.state
            .write()
            .layout_line(&self.components, text, font_size, runs)
            .log_err()
            .unwrap_or(LineLayout {
                font_size,
                ..Default::default()
            })
    }

    fn recommended_rendering_mode(
        &self,
        _font_id: FontId,
        _font_size: Pixels,
    ) -> TextRenderingMode {
        if self.components.system_subpixel_rendering {
            TextRenderingMode::Subpixel
        } else {
            TextRenderingMode::Grayscale
        }
    }
}

impl DirectWriteState {
    fn select_and_cache_font(
        &mut self,
        components: &DirectWriteComponents,
        font: &Font,
    ) -> Option<FontId> {
        let select_font = |this: &mut DirectWriteState, font: &Font| -> Option<FontId> {
            let info = unsafe {
                this.make_font_from_font_collection(
                    font,
                    &this.system_font_collection,
                    &components.factory,
                    &components.locale,
                    &components.system_ui_font_name,
                )
            }?;

            let font_id = FontId(this.fonts.len());
            let font_face_key = info.font_face.cast::<IUnknown>().unwrap().as_raw().addr();
            this.fonts.push(info);
            this.font_info_cache.insert(font_face_key, font_id);
            Some(font_id)
        };

        let mut font_id = select_font(self, font);
        if font_id.is_none() {
            // try updating system fonts and reselect
            let mut collection: Option<IDWriteFontCollection> = None;
            let font_collection_updated = unsafe {
                components
                    .factory
                    .GetSystemFontCollection(&mut collection, true)
            }
            .log_err()
            .is_some();
            if font_collection_updated && let Some(collection) = collection {
                self.system_font_collection = collection;
            }
            font_id = select_font(self, font);
        };
        let font_id = font_id?;
        self.font_to_font_id.insert(font.clone(), font_id);
        Some(font_id)
    }

    fn add_fonts(
        &mut self,
        _components: &DirectWriteComponents,
        _fonts: Vec<Cow<'static, [u8]>>,
    ) -> Result<()> {
        // Win7: custom (in-memory) fonts require `IDWriteInMemoryFontFileLoader`
        // (Win10 Creators Update+), so loading additional fonts is not supported.
        Ok(())
    }

    unsafe fn generate_font_features(
        factory: &IDWriteFactory,
        font_features: &FontFeatures,
    ) -> Result<IDWriteTypography> {
        let direct_write_features = unsafe { factory.CreateTypography()? };
        apply_font_features(&direct_write_features, font_features)?;
        Ok(direct_write_features)
    }

    unsafe fn make_font_from_font_collection(
        &self,
        &Font {
            ref family,
            ref features,
            weight,
            style,
            ..
        }: &Font,
        collection: &IDWriteFontCollection,
        factory: &IDWriteFactory,
        locale: &HSTRING,
        system_ui_font_name: &SharedString,
    ) -> Option<FontInfo> {
        const SYSTEM_UI_FONT_NAME: &str = ".SystemUIFont";
        let family: &str = if family == SYSTEM_UI_FONT_NAME {
            system_ui_font_name.as_str()
        } else {
            family.as_str()
        };
        let font_family_h = HSTRING::from(family);
        let requested_weight = font_weight_to_dwrite(weight);
        let requested_style = font_style_to_dwrite(style);

        let family_count = unsafe { collection.GetFontFamilyCount() };
        for family_index in 0..family_count {
            let Some(font_family) = unsafe { collection.GetFontFamily(family_index) }.log_err()
            else {
                continue;
            };
            let Some(localized_family_name) =
                unsafe { font_family.GetFamilyNames() }.log_err()
            else {
                continue;
            };
            let Some(family_name) = get_name(localized_family_name, locale).log_err() else {
                continue;
            };
            if family_name != *family {
                continue;
            }

            // Pick the font in this family closest to the requested weight/style. Prefer an exact
            // style match; among candidates, minimise the weight distance.
            let font_count = unsafe { font_family.GetFontCount() };
            let mut best: Option<(i32, IDWriteFont)> = None;
            for font_index in 0..font_count {
                // Force the base `IDWriteFont` interface so that only DWrite 1.0 methods
                // (`GetWeight`/`GetStyle`/`CreateFontFace`) are ever called, avoiding any Win8+
                // vtable slots on a Win7 font object.
                let Some(font) = unsafe { font_family.GetFont(font_index) }
                    .log_err()?
                    .cast::<IDWriteFont>()
                    .log_err()
                else {
                    continue;
                };
                let font_weight = unsafe { font.GetWeight() };
                let font_style = unsafe { font.GetStyle() };
                let style_penalty = if font_style == requested_style { 0 } else { 1_000_000 };
                let weight_distance = (font_weight.0 - requested_weight.0).abs();
                let score = style_penalty + weight_distance;
                if best.as_ref().map_or(true, |(best_score, _)| score < *best_score) {
                    best = Some((score, font));
                }
            }

            let font = best?.1;
            let font_face = unsafe { font.CreateFontFace() }.log_err()?;
            let direct_write_features =
                unsafe { Self::generate_font_features(factory, features) }.log_err()?;
            return Some(FontInfo {
                font_family_h,
                font_face,
                weight: unsafe { font.GetWeight() },
                style: unsafe { font.GetStyle() },
                features: direct_write_features,
                font_collection: collection.clone(),
            });
        }
        None
    }

    fn layout_line(
        &mut self,
        components: &DirectWriteComponents,
        text: &str,
        font_size: Pixels,
        font_runs: &[FontRun],
    ) -> Result<LineLayout> {
        if font_runs.is_empty() {
            return Ok(LineLayout {
                font_size,
                ..Default::default()
            });
        }
        unsafe {
            self.layout_line_scratch.clear();
            self.layout_line_scratch.extend(text.encode_utf16());
            let text_wide = &*self.layout_line_scratch;

            let mut utf8_offset = 0usize;
            let mut utf16_offset = 0u32;
            let text_layout = {
                let first_run = &font_runs[0];
                let font_info = &self.fonts[first_run.font_id.0];
                let collection = &font_info.font_collection;
                let format: IDWriteTextFormat = components.factory.CreateTextFormat(
                    &font_info.font_family_h,
                    collection,
                    font_info.weight,
                    font_info.style,
                    DWRITE_FONT_STRETCH_NORMAL,
                    font_size.as_f32(),
                    &components.locale,
                )?;

                let layout = components.factory.CreateTextLayout(
                    text_wide,
                    &format,
                    f32::INFINITY,
                    f32::INFINITY,
                )?;
                let current_text = &text[utf8_offset..(utf8_offset + first_run.len)];
                utf8_offset += first_run.len;
                let current_text_utf16_length = current_text.encode_utf16().count() as u32;
                let text_range = DWRITE_TEXT_RANGE {
                    startPosition: utf16_offset,
                    length: current_text_utf16_length,
                };
                layout.SetTypography(&font_info.features, text_range)?;
                utf16_offset += current_text_utf16_length;

                layout
            };

            let (ascent, descent) = {
                let mut first_metrics = [DWRITE_LINE_METRICS::default(); 4];
                let mut line_count = 0u32;
                text_layout.GetLineMetrics(Some(&mut first_metrics), &mut line_count)?;
                (
                    px(first_metrics[0].baseline),
                    px(first_metrics[0].height - first_metrics[0].baseline),
                )
            };
            let mut break_ligatures = true;
            for run in &font_runs[1..] {
                let font_info = &self.fonts[run.font_id.0];
                let current_text = &text[utf8_offset..(utf8_offset + run.len)];
                utf8_offset += run.len;
                let current_text_utf16_length = current_text.encode_utf16().count() as u32;

                let collection = &font_info.font_collection;
                let text_range = DWRITE_TEXT_RANGE {
                    startPosition: utf16_offset,
                    length: current_text_utf16_length,
                };
                utf16_offset += current_text_utf16_length;
                text_layout.SetFontCollection(collection, text_range)?;
                text_layout.SetFontFamilyName(&font_info.font_family_h, text_range)?;
                let font_size = if break_ligatures {
                    font_size.as_f32().next_up()
                } else {
                    font_size.as_f32()
                };
                text_layout.SetFontSize(font_size, text_range)?;
                text_layout.SetFontStyle(font_info.style, text_range)?;
                text_layout.SetFontWeight(font_info.weight, text_range)?;
                text_layout.SetTypography(&font_info.features, text_range)?;

                break_ligatures = !break_ligatures;
            }

            let mut runs = Vec::new();
            let mut renderer_context = RendererContext {
                text_system: self,
                index_converter: StringIndexConverter::new(text),
                runs: &mut runs,
                width: 0.0,
            };
            text_layout.Draw(
                Some((&raw mut renderer_context).cast::<c_void>().cast_const()),
                &components.text_renderer.0,
                0.0,
                0.0,
            )?;
            let width = px(renderer_context.width);

            Ok(LineLayout {
                font_size,
                width,
                ascent,
                descent,
                runs,
                len: text.len(),
            })
        }
    }

    fn font_metrics(&self, font_id: FontId) -> FontMetrics {
        unsafe {
            let font_info = &self.fonts[font_id.0];
            let mut metrics = std::mem::zeroed();
            font_info.font_face.GetMetrics(&mut metrics);

            FontMetrics {
                units_per_em: metrics.designUnitsPerEm as _,
                ascent: metrics.ascent as _,
                descent: -(metrics.descent as f32),
                line_gap: metrics.lineGap as _,
                underline_position: metrics.underlinePosition as _,
                underline_thickness: metrics.underlineThickness as _,
                cap_height: metrics.capHeight as _,
                x_height: metrics.xHeight as _,
                // Base `DWRITE_FONT_METRICS` has no glyph-box fields (those are only in
                // `DWRITE_FONT_METRICS1`, a DWrite 1.1 struct), so approximate the bounding box from
                // the em square / ascent+descent. Good enough for cursor/selection hit-testing.
                bounding_box: Bounds {
                    origin: Point {
                        x: 0.0,
                        y: -(metrics.ascent as f32),
                    },
                    size: Size {
                        width: metrics.designUnitsPerEm as f32,
                        height: (metrics.ascent as f32 + metrics.descent as f32),
                    },
                },
            }
        }
    }

    fn create_glyph_run_analysis(
        &self,
        components: &DirectWriteComponents,
        params: &RenderGlyphParams,
    ) -> Result<IDWriteGlyphRunAnalysis> {
        let font = &self.fonts[params.font_id.0];
        let glyph_id = [params.glyph_id.0 as u16];
        let advance = [0.0];
        let offset = [DWRITE_GLYPH_OFFSET::default()];
        let glyph_run = DWRITE_GLYPH_RUN {
            fontFace: ManuallyDrop::new(Some(unsafe { std::ptr::read(&font.font_face) })),
            fontEmSize: params.font_size.as_f32(),
            glyphCount: 1,
            glyphIndices: glyph_id.as_ptr(),
            glyphAdvances: advance.as_ptr(),
            glyphOffsets: offset.as_ptr(),
            isSideways: BOOL(0),
            bidiLevel: 0,
        };
        let transform = DWRITE_MATRIX {
            m11: params.scale_factor,
            m12: 0.0,
            m21: 0.0,
            m22: params.scale_factor,
            dx: 0.0,
            dy: 0.0,
        };
        let baseline_origin_x =
            params.subpixel_variant.x as f32 / SUBPIXEL_VARIANTS_X as f32 / params.scale_factor;
        let baseline_origin_y = params.subpixel_variant.y as f32
            / gpui::SUBPIXEL_VARIANTS_Y as f32
            / params.scale_factor;

        // DWrite 1.0 only offers the base `DWRITE_RENDERING_MODE` (no grid-fit/antialias knobs), and
        // `raster_bounds`/`rasterize_monochrome` key the alpha-texture type off `subpixel_rendering`,
        // so pick the rendering mode that matches: a ClearType (3x1) mode for subpixel, a 1x1
        // antialiased mode otherwise.
        let rendering_mode = if params.subpixel_rendering {
            DWRITE_RENDERING_MODE_CLEARTYPE_NATURAL
        } else {
            DWRITE_RENDERING_MODE_NATURAL
        };

        let glyph_analysis = unsafe {
            components.factory.CreateGlyphRunAnalysis(
                &glyph_run,
                1.0, // pixels per DIP; scaling is applied via the transform
                Some(&transform),
                rendering_mode,
                DWRITE_MEASURING_MODE_NATURAL,
                baseline_origin_x,
                baseline_origin_y,
            )
        }?;
        Ok(glyph_analysis)
    }

    fn raster_bounds(
        &self,
        components: &DirectWriteComponents,
        params: &RenderGlyphParams,
    ) -> Result<Bounds<DevicePixels>> {
        let glyph_analysis = self.create_glyph_run_analysis(components, params)?;

        let texture_type = if params.subpixel_rendering {
            DWRITE_TEXTURE_CLEARTYPE_3x1
        } else {
            DWRITE_TEXTURE_ALIASED_1x1
        };

        let bounds = unsafe { glyph_analysis.GetAlphaTextureBounds(texture_type)? };

        if bounds.right < bounds.left {
            Ok(Bounds {
                origin: point(0.into(), 0.into()),
                size: size(0.into(), 0.into()),
            })
        } else {
            Ok(Bounds {
                origin: point(bounds.left.into(), bounds.top.into()),
                size: size(
                    (bounds.right - bounds.left).into(),
                    (bounds.bottom - bounds.top).into(),
                ),
            })
        }
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        let font_info = &self.fonts[font_id.0];
        let codepoints = ch as u32;
        let mut glyph_indices = 0u16;
        unsafe {
            font_info
                .font_face
                .GetGlyphIndices(&raw const codepoints, 1, &raw mut glyph_indices)
                .log_err()
        }
        .map(|_| GlyphId(glyph_indices as u32))
    }

    fn rasterize_glyph(
        &self,
        components: &DirectWriteComponents,
        params: &RenderGlyphParams,
        glyph_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        if glyph_bounds.size.width.0 == 0 || glyph_bounds.size.height.0 == 0 {
            anyhow::bail!("glyph bounds are empty");
        }

        // Win7: no color (emoji) rasterization; every glyph is rendered as monochrome.
        let bitmap_data = self.rasterize_monochrome(components, params, glyph_bounds)?;
        if params.is_emoji {
            let bitmap_data = bitmap_data
                .into_iter()
                .flat_map(|pixel| [0, 0, 0, pixel])
                .collect::<Vec<_>>();
            Ok((glyph_bounds.size, bitmap_data))
        } else {
            Ok((glyph_bounds.size, bitmap_data))
        }
    }

    fn rasterize_monochrome(
        &self,
        components: &DirectWriteComponents,
        params: &RenderGlyphParams,
        glyph_bounds: Bounds<DevicePixels>,
    ) -> Result<Vec<u8>> {
        let glyph_analysis = self.create_glyph_run_analysis(components, params)?;
        if !params.subpixel_rendering {
            let mut bitmap_data =
                vec![0u8; glyph_bounds.size.width.0 as usize * glyph_bounds.size.height.0 as usize];
            unsafe {
                glyph_analysis.CreateAlphaTexture(
                    DWRITE_TEXTURE_ALIASED_1x1,
                    &RECT {
                        left: glyph_bounds.origin.x.0,
                        top: glyph_bounds.origin.y.0,
                        right: glyph_bounds.size.width.0 + glyph_bounds.origin.x.0,
                        bottom: glyph_bounds.size.height.0 + glyph_bounds.origin.y.0,
                    },
                    &mut bitmap_data,
                )?;
            }

            return Ok(bitmap_data);
        }

        let width = glyph_bounds.size.width.0 as usize;
        let height = glyph_bounds.size.height.0 as usize;
        let pixel_count = width * height;

        let mut bitmap_data = vec![0u8; pixel_count * 4];

        unsafe {
            glyph_analysis.CreateAlphaTexture(
                DWRITE_TEXTURE_CLEARTYPE_3x1,
                &RECT {
                    left: glyph_bounds.origin.x.0,
                    top: glyph_bounds.origin.y.0,
                    right: glyph_bounds.size.width.0 + glyph_bounds.origin.x.0,
                    bottom: glyph_bounds.size.height.0 + glyph_bounds.origin.y.0,
                },
                &mut bitmap_data[..pixel_count * 3],
            )?;
        }

        // The output buffer expects RGBA data, so pad the alpha channel with zeros.
        for pixel_ix in (0..pixel_count).rev() {
            let src = pixel_ix * 3;
            let dst = pixel_ix * 4;
            (
                bitmap_data[dst],
                bitmap_data[dst + 1],
                bitmap_data[dst + 2],
                bitmap_data[dst + 3],
            ) = (
                bitmap_data[src],
                bitmap_data[src + 1],
                bitmap_data[src + 2],
                0,
            );
        }

        Ok(bitmap_data)
    }

    fn get_typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
        unsafe {
            let font = &self.fonts[font_id.0].font_face;
            let glyph_indices = [glyph_id.0 as u16];
            let mut metrics = [DWRITE_GLYPH_METRICS::default()];
            font.GetDesignGlyphMetrics(glyph_indices.as_ptr(), 1, metrics.as_mut_ptr(), false)?;

            let metrics = &metrics[0];
            let advance_width = metrics.advanceWidth as i32;
            let advance_height = metrics.advanceHeight as i32;
            let left_side_bearing = metrics.leftSideBearing;
            let right_side_bearing = metrics.rightSideBearing;
            let top_side_bearing = metrics.topSideBearing;
            let bottom_side_bearing = metrics.bottomSideBearing;
            let vertical_origin_y = metrics.verticalOriginY;

            let y_offset = vertical_origin_y + bottom_side_bearing - advance_height;
            let width = advance_width - (left_side_bearing + right_side_bearing);
            let height = advance_height - (top_side_bearing + bottom_side_bearing);

            Ok(Bounds {
                origin: Point {
                    x: left_side_bearing as f32,
                    y: y_offset as f32,
                },
                size: Size {
                    width: width as f32,
                    height: height as f32,
                },
            })
        }
    }

    fn get_advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        unsafe {
            let font = &self.fonts[font_id.0].font_face;
            let glyph_indices = [glyph_id.0 as u16];
            let mut metrics = [DWRITE_GLYPH_METRICS::default()];
            font.GetDesignGlyphMetrics(glyph_indices.as_ptr(), 1, metrics.as_mut_ptr(), false)?;

            let metrics = &metrics[0];

            Ok(Size {
                width: metrics.advanceWidth as f32,
                height: 0.0,
            })
        }
    }

    fn all_font_names(&self, components: &DirectWriteComponents) -> Vec<String> {
        get_font_names_from_collection(&self.system_font_collection, &components.locale)
    }

    fn handle_gpu_lost(&mut self, _directx_devices: &DirectXDevices) -> Result<()> {
        // Win7: text rasterization is CPU-only (monochrome), so no GPU state to rebuild.
        Ok(())
    }
}

/// Enumerates family names of a base (DWrite 1.0) font collection. Mirrors the Win10 helper of the
/// same name, which operates on `IDWriteFontCollection1`.
fn get_font_names_from_collection(
    collection: &IDWriteFontCollection,
    locale: &HSTRING,
) -> Vec<String> {
    unsafe {
        let mut result = Vec::new();
        let family_count = collection.GetFontFamilyCount();
        for index in 0..family_count {
            let Some(font_family) = collection.GetFontFamily(index).log_err() else {
                continue;
            };
            let Some(localized_family_name) = font_family.GetFamilyNames().log_err() else {
                continue;
            };
            let Some(family_name) = get_name(localized_family_name, locale).log_err() else {
                continue;
            };
            result.push(family_name);
        }

        result
    }
}

pub(crate) struct TextRendererWrapper(IDWriteTextRenderer);

impl TextRendererWrapper {
    pub(crate) fn new() -> Self {
        let inner = TextRenderer::new();
        TextRendererWrapper(inner.into())
    }
}

#[implement(IDWriteTextRenderer)]
struct TextRenderer {}

impl TextRenderer {
    fn new() -> Self {
        TextRenderer {}
    }
}

struct RendererContext<'t, 'a, 'b> {
    text_system: &'t mut DirectWriteState,
    index_converter: StringIndexConverter<'a>,
    runs: &'b mut Vec<ShapedRun>,
    width: f32,
}

#[derive(Debug)]
struct ClusterAnalyzer<'t> {
    utf16_idx: usize,
    glyph_idx: usize,
    glyph_count: usize,
    cluster_map: &'t [u16],
}

impl<'t> ClusterAnalyzer<'t> {
    fn new(cluster_map: &'t [u16], glyph_count: usize) -> Self {
        ClusterAnalyzer {
            utf16_idx: 0,
            glyph_idx: 0,
            glyph_count,
            cluster_map,
        }
    }
}

impl Iterator for ClusterAnalyzer<'_> {
    type Item = (usize, usize);

    fn next(&mut self) -> Option<(usize, usize)> {
        if self.utf16_idx >= self.cluster_map.len() {
            return None; // No more clusters
        }
        let start_utf16_idx = self.utf16_idx;
        let current_glyph = self.cluster_map[start_utf16_idx] as usize;

        // Find the end of current cluster (where glyph index changes)
        let mut end_utf16_idx = start_utf16_idx + 1;
        while end_utf16_idx < self.cluster_map.len()
            && self.cluster_map[end_utf16_idx] as usize == current_glyph
        {
            end_utf16_idx += 1;
        }

        let utf16_len = end_utf16_idx - start_utf16_idx;

        // Calculate glyph count for this cluster
        let next_glyph = if end_utf16_idx < self.cluster_map.len() {
            self.cluster_map[end_utf16_idx] as usize
        } else {
            self.glyph_count
        };

        let glyph_count = next_glyph - current_glyph;

        // Update state for next call
        self.utf16_idx = end_utf16_idx;
        self.glyph_idx = next_glyph;

        Some((utf16_len, glyph_count))
    }
}

#[allow(non_snake_case)]
impl IDWritePixelSnapping_Impl for TextRenderer_Impl {
    fn IsPixelSnappingDisabled(
        &self,
        _clientdrawingcontext: *const ::core::ffi::c_void,
    ) -> windows::core::Result<BOOL> {
        Ok(BOOL(0))
    }

    fn GetCurrentTransform(
        &self,
        _clientdrawingcontext: *const ::core::ffi::c_void,
        transform: *mut DWRITE_MATRIX,
    ) -> windows::core::Result<()> {
        unsafe {
            *transform = DWRITE_MATRIX {
                m11: 1.0,
                m12: 0.0,
                m21: 0.0,
                m22: 1.0,
                dx: 0.0,
                dy: 0.0,
            };
        }
        Ok(())
    }

    fn GetPixelsPerDip(
        &self,
        _clientdrawingcontext: *const ::core::ffi::c_void,
    ) -> windows::core::Result<f32> {
        Ok(1.0)
    }
}

#[allow(non_snake_case)]
impl IDWriteTextRenderer_Impl for TextRenderer_Impl {
    fn DrawGlyphRun(
        &self,
        clientdrawingcontext: *const ::core::ffi::c_void,
        _baselineoriginx: f32,
        _baselineoriginy: f32,
        _measuringmode: DWRITE_MEASURING_MODE,
        glyphrun: *const DWRITE_GLYPH_RUN,
        glyphrundescription: *const DWRITE_GLYPH_RUN_DESCRIPTION,
        _clientdrawingeffect: windows::core::Ref<windows::core::IUnknown>,
    ) -> windows::core::Result<()> {
        let glyphrun = unsafe { &*glyphrun };
        let glyph_count = glyphrun.glyphCount as usize;
        if glyph_count == 0 {
            return Ok(());
        }
        let desc = unsafe { &*glyphrundescription };
        let context = unsafe { &mut *(clientdrawingcontext.cast::<RendererContext>().cast_mut()) };
        let Some(font_face) = glyphrun.fontFace.as_ref() else {
            return Ok(());
        };
        let font_face_key = font_face.cast::<IUnknown>().unwrap().as_raw().addr();
        let font_id = match context.text_system.font_info_cache.get(&font_face_key) {
            Some(&font_id) => font_id,
            // On Win7 we cannot recover the family/weight/style from an unrecognized font face
            // (unlike Win10's `IDWriteFontFace3`), so skip runs served from a font face we did not
            // create (e.g. symbol substitution).
            None => {
                log::debug!("Win7 text: skipping glyph run for unrecognized font face");
                return Ok(());
            }
        };

        let glyph_ids = unsafe {
            slice_from_nullable(
                glyphrun.glyphIndices,
                glyph_count,
                "DirectWrite returned a null glyph indices array",
            )?
        };
        let glyph_advances = unsafe {
            slice_from_nullable(
                glyphrun.glyphAdvances,
                glyph_count,
                "DirectWrite returned a null glyph advances array",
            )?
        };
        let glyph_offsets = unsafe {
            slice_from_nullable(
                glyphrun.glyphOffsets,
                glyph_count,
                "DirectWrite returned a null glyph offsets array",
            )?
        };
        let cluster_map = unsafe {
            slice_from_nullable(
                desc.clusterMap,
                desc.stringLength as usize,
                "DirectWrite returned a null cluster map",
            )?
        };

        let cluster_analyzer = ClusterAnalyzer::new(cluster_map, glyph_count);
        let mut utf16_idx = desc.textPosition as usize;
        let mut glyph_idx = 0;
        let mut glyphs = Vec::with_capacity(glyph_count);
        for (cluster_utf16_len, cluster_glyph_count) in cluster_analyzer {
            context.index_converter.advance_to_utf16_ix(utf16_idx);
            utf16_idx += cluster_utf16_len;
            for (cluster_glyph_idx, glyph_id) in glyph_ids
                [glyph_idx..(glyph_idx + cluster_glyph_count)]
                .iter()
                .enumerate()
            {
                let id = GlyphId(*glyph_id as u32);
                let this_glyph_idx = glyph_idx + cluster_glyph_idx;
                glyphs.push(ShapedGlyph {
                    id,
                    position: point(
                        px(context.width + glyph_offsets[this_glyph_idx].advanceOffset),
                        px(-glyph_offsets[this_glyph_idx].ascenderOffset),
                    ),
                    index: context.index_converter.utf8_ix,
                    is_emoji: false,
                });
                context.width += glyph_advances[this_glyph_idx];
            }
            glyph_idx += cluster_glyph_count;
        }
        context.runs.push(ShapedRun { font_id, glyphs });
        Ok(())
    }

    fn DrawUnderline(
        &self,
        _clientdrawingcontext: *const ::core::ffi::c_void,
        _baselineoriginx: f32,
        _baselineoriginy: f32,
        _underline: *const DWRITE_UNDERLINE,
        _clientdrawingeffect: windows::core::Ref<windows::core::IUnknown>,
    ) -> windows::core::Result<()> {
        Err(windows::core::Error::new(
            E_NOTIMPL,
            "DrawUnderline unimplemented",
        ))
    }

    fn DrawStrikethrough(
        &self,
        _clientdrawingcontext: *const ::core::ffi::c_void,
        _baselineoriginx: f32,
        _baselineoriginy: f32,
        _strikethrough: *const DWRITE_STRIKETHROUGH,
        _clientdrawingeffect: windows::core::Ref<windows::core::IUnknown>,
    ) -> windows::core::Result<()> {
        Err(windows::core::Error::new(
            E_NOTIMPL,
            "DrawStrikethrough unimplemented",
        ))
    }

    fn DrawInlineObject(
        &self,
        _clientdrawingcontext: *const ::core::ffi::c_void,
        _originx: f32,
        _originy: f32,
        _inlineobject: windows::core::Ref<IDWriteInlineObject>,
        _issideways: BOOL,
        _isrighttoleft: BOOL,
        _clientdrawingeffect: windows::core::Ref<windows::core::IUnknown>,
    ) -> windows::core::Result<()> {
        Err(windows::core::Error::new(
            E_NOTIMPL,
            "DrawInlineObject unimplemented",
        ))
    }
}

/// Interprets an optional DirectWrite array pointer as a slice, treating a
/// null pointer with a zero length as an empty slice. A null pointer with a
/// nonzero length fails with `null_error_message`.
///
/// # Safety
///
/// When `ptr` is non-null, the caller must guarantee that it points to a valid
/// array of at least `len` elements that outlives the returned slice.
unsafe fn slice_from_nullable<'a, T>(
    ptr: *const T,
    len: usize,
    null_error_message: &str,
) -> windows::core::Result<&'a [T]> {
    if ptr.is_null() {
        if len != 0 {
            return Err(Error::new(E_INVALIDARG, null_error_message));
        }
        Ok(&[])
    } else {
        Ok(unsafe { std::slice::from_raw_parts(ptr, len) })
    }
}

struct StringIndexConverter<'a> {
    text: &'a str,
    utf8_ix: usize,
    utf16_ix: usize,
}

impl<'a> StringIndexConverter<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            utf8_ix: 0,
            utf16_ix: 0,
        }
    }

    fn advance_to_utf16_ix(&mut self, utf16_target: usize) {
        for (ix, c) in self.text[self.utf8_ix..].char_indices() {
            if self.utf16_ix >= utf16_target {
                self.utf8_ix += ix;
                return;
            }
            self.utf16_ix += c.len_utf16();
        }
        self.utf8_ix = self.text.len();
    }
}