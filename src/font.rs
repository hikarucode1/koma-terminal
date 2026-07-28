//! Font loading, glyph rasterisation (swash) and a shelf-packed alpha atlas.
//!
//! Faces are ordered: regular, bold, then fallbacks. A character is rendered by
//! the first face whose charmap covers it, which is what makes CJK text work
//! when the primary monospace font has no kana/kanji.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use swash::FontRef;
use swash::scale::{Render, ScaleContext, Source as ScaleSource, StrikeWith};
use swash::zeno::Format;

/// Preferred monospace families, best first. macOS names lead.
const MONO_FAMILIES: &[&str] = &[
    "SF Mono",
    "Menlo",
    "Monaco",
    "JetBrains Mono",
    "Hack",
    "Fira Code",
    "DejaVu Sans Mono",
    "Liberation Mono",
    "Noto Sans Mono",
    "Ubuntu Mono",
    "Consolas",
];

/// Fallback families for characters the primary font lacks (CJK first).
const FALLBACK_FAMILIES: &[&str] = &[
    "Hiragino Sans",
    "Hiragino Kaku Gothic ProN",
    "Noto Sans CJK JP",
    "Noto Sans JP",
    "Source Han Sans JP",
    "IPAGothic",
    "Apple Color Emoji",
    "Noto Color Emoji",
    "Noto Emoji",
    "Apple Symbols",
    "Symbola",
    "DejaVu Sans",
];

#[derive(Clone, Copy)]
pub struct GlyphInfo {
    /// u0, v0, u1, v1 in atlas texture space (0..1).
    pub uv: [f32; 4],
    /// Offset from the pen position to the top-left of the bitmap, in pixels.
    pub left: f32,
    pub top: f32,
    pub w: f32,
    pub h: f32,
}

struct Face {
    data: Arc<Vec<u8>>,
    index: u32,
}

impl Face {
    fn font(&self) -> Option<FontRef<'_>> {
        FontRef::from_index(&self.data, self.index as usize)
    }
}

pub struct Atlas {
    pub size: u32,
    pub data: Vec<u8>,
    cursor_x: u32,
    cursor_y: u32,
    row_h: u32,
    /// Rows touched since the last GPU upload, as `[y0, y1)`.
    pub dirty: Option<(u32, u32)>,
}

impl Atlas {
    fn new(size: u32) -> Self {
        Atlas {
            size,
            data: vec![0; (size * size) as usize],
            cursor_x: 0,
            cursor_y: 0,
            row_h: 0,
            dirty: Some((0, size)),
        }
    }

    /// Copies an alpha bitmap into the atlas, returning its pixel rect.
    fn insert(&mut self, w: u32, h: u32, src: &[u8]) -> Option<(u32, u32)> {
        if w == 0 || h == 0 {
            return Some((0, 0));
        }
        if w > self.size || h > self.size {
            return None;
        }
        if self.cursor_x + w > self.size {
            // Advance to the next shelf.
            self.cursor_x = 0;
            self.cursor_y += self.row_h + 1;
            self.row_h = 0;
        }
        if self.cursor_y + h > self.size {
            return None; // atlas full
        }
        let (x, y) = (self.cursor_x, self.cursor_y);
        for row in 0..h {
            let d = ((y + row) * self.size + x) as usize;
            let s = (row * w) as usize;
            self.data[d..d + w as usize].copy_from_slice(&src[s..s + w as usize]);
        }
        self.cursor_x += w + 1;
        self.row_h = self.row_h.max(h);

        self.dirty = Some(match self.dirty {
            Some((y0, y1)) => (y0.min(y), y1.max(y + h)),
            None => (y, y + h),
        });
        Some((x, y))
    }
}

pub struct FontSet {
    faces: Vec<Face>,
    bold: Option<usize>,
    size_px: f32,
    pub cell_w: f32,
    pub cell_h: f32,
    pub ascent: f32,
    pub underline_offset: f32,
    ctx: ScaleContext,
    cache: HashMap<(char, bool), Option<GlyphInfo>>,
    /// Which face covers a given char, resolved once.
    face_for: HashMap<char, Option<usize>>,
    pub atlas: Atlas,
}

fn load_family(db: &fontdb::Database, families: &[&str], bold: bool) -> Option<Face> {
    let weight = if bold { fontdb::Weight::BOLD } else { fontdb::Weight::NORMAL };
    for name in families {
        let q = fontdb::Query {
            families: &[fontdb::Family::Name(name)],
            weight,
            stretch: fontdb::Stretch::Normal,
            style: fontdb::Style::Normal,
        };
        if let Some(id) = db.query(&q) {
            if let Some(face) = db.with_face_data(id, |data, index| Face {
                data: Arc::new(data.to_vec()),
                index,
            }) {
                return Some(face);
            }
        }
    }
    None
}

impl FontSet {
    pub fn new(size_px: f32) -> Result<Self> {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();

        let regular = load_family(&db, MONO_FAMILIES, false)
            .or_else(|| {
                // Last resort: any face the system reports as monospaced.
                let id = db
                    .faces()
                    .find(|f| f.monospaced)
                    .map(|f| f.id)?;
                db.with_face_data(id, |data, index| Face { data: Arc::new(data.to_vec()), index })
            })
            .context("no monospace font found on this system")?;

        let mut faces = vec![regular];
        let bold = load_family(&db, MONO_FAMILIES, true).map(|f| {
            faces.push(f);
            faces.len() - 1
        });
        for name in FALLBACK_FAMILIES {
            if let Some(f) = load_family(&db, &[name], false) {
                faces.push(f);
            }
        }

        let (cell_w, cell_h, ascent, underline_offset) = {
            let font = faces[0].font().context("primary font failed to parse")?;
            let m = font.metrics(&[]).scale(size_px);
            let gm = font.glyph_metrics(&[]).scale(size_px);
            // Monospace: every glyph shares 'M's advance.
            let gid = font.charmap().map('M');
            let mut adv = gm.advance_width(gid);
            if adv <= 0.0 {
                adv = m.average_width.max(size_px * 0.6);
            }
            let h = (m.ascent + m.descent + m.leading).ceil().max(1.0);
            (adv.ceil().max(1.0), h, m.ascent.round(), m.underline_offset)
        };

        Ok(FontSet {
            faces,
            bold,
            size_px,
            cell_w,
            cell_h,
            ascent,
            underline_offset,
            ctx: ScaleContext::new(),
            cache: HashMap::new(),
            face_for: HashMap::new(),
            atlas: Atlas::new(2048),
        })
    }

    /// Rebuilds at a new pixel size, dropping every cached glyph.
    pub fn set_size(&mut self, size_px: f32) {
        if (size_px - self.size_px).abs() < 0.01 {
            return;
        }
        self.size_px = size_px;
        self.cache.clear();
        self.atlas = Atlas::new(self.atlas.size);
        if let Some(font) = self.faces[0].font() {
            let m = font.metrics(&[]).scale(size_px);
            let gm = font.glyph_metrics(&[]).scale(size_px);
            let gid = font.charmap().map('M');
            let mut adv = gm.advance_width(gid);
            if adv <= 0.0 {
                adv = m.average_width.max(size_px * 0.6);
            }
            self.cell_w = adv.ceil().max(1.0);
            self.cell_h = (m.ascent + m.descent + m.leading).ceil().max(1.0);
            self.ascent = m.ascent.round();
            self.underline_offset = m.underline_offset;
        }
    }

    /// Index of the face that can draw `c`, preferring `bold` when asked.
    fn resolve_face(&mut self, c: char, bold: bool) -> Option<usize> {
        if bold {
            if let Some(bi) = self.bold {
                if self.faces[bi].font().map(|f| f.charmap().map(c)).unwrap_or(0) != 0 {
                    return Some(bi);
                }
            }
        }
        if let Some(&cached) = self.face_for.get(&c) {
            return cached;
        }
        let found = self.faces.iter().enumerate().find_map(|(i, face)| {
            let gid = face.font()?.charmap().map(c);
            (gid != 0).then_some(i)
        });
        self.face_for.insert(c, found);
        found
    }

    /// Rasterises `c` if needed and returns its atlas placement.
    /// `None` means "nothing to draw" (blank, or no font covers it).
    pub fn glyph(&mut self, c: char, bold: bool) -> Option<GlyphInfo> {
        if c == ' ' || c == '\0' {
            return None;
        }
        if let Some(&hit) = self.cache.get(&(c, bold)) {
            return hit;
        }

        let face_idx = self.resolve_face(c, bold);
        let info = face_idx.and_then(|fi| {
            // Split the borrow so `ctx`/`atlas` stay mutable alongside `faces`.
            let FontSet { faces, ctx, atlas, size_px, .. } = self;
            let font = faces[fi].font()?;
            let gid = font.charmap().map(c);
            if gid == 0 {
                return None;
            }
            let mut scaler = ctx.builder(font).size(*size_px).hint(true).build();
            let img = Render::new(&[
                ScaleSource::ColorOutline(0),
                ScaleSource::Outline,
                ScaleSource::Bitmap(StrikeWith::BestFit),
            ])
            .format(Format::Alpha)
            .render(&mut scaler, gid)?;

            let (w, h) = (img.placement.width, img.placement.height);
            if w == 0 || h == 0 {
                return None;
            }
            let (x, y) = atlas.insert(w, h, &img.data)?;
            let s = atlas.size as f32;
            Some(GlyphInfo {
                uv: [x as f32 / s, y as f32 / s, (x + w) as f32 / s, (y + h) as f32 / s],
                left: img.placement.left as f32,
                top: img.placement.top as f32,
                w: w as f32,
                h: h as f32,
            })
        });

        self.cache.insert((c, bold), info);
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_a_monospace_face_with_sane_metrics() {
        let f = FontSet::new(26.0).expect("no usable system font");
        assert!(f.cell_w > 1.0 && f.cell_w < 100.0, "cell_w = {}", f.cell_w);
        assert!(f.cell_h > f.cell_w, "a monospace cell should be taller than wide");
        assert!(f.ascent > 0.0 && f.ascent <= f.cell_h);
    }

    #[test]
    fn rasterises_ascii_into_the_atlas() {
        let mut f = FontSet::new(26.0).unwrap();
        let g = f.glyph('A', false).expect("'A' must rasterise");
        assert!(g.w > 0.0 && g.h > 0.0);
        // uv must stay inside the atlas.
        assert!(g.uv.iter().all(|v| (0.0..=1.0).contains(v)), "uv = {:?}", g.uv);
        assert!(g.uv[2] > g.uv[0] && g.uv[3] > g.uv[1]);
        // Some ink actually landed in the atlas.
        assert!(f.atlas.data.iter().any(|&px| px > 0));
    }

    #[test]
    fn blanks_produce_no_glyph() {
        let mut f = FontSet::new(26.0).unwrap();
        assert!(f.glyph(' ', false).is_none());
    }

    #[test]
    fn repeated_lookups_hit_the_cache() {
        let mut f = FontSet::new(26.0).unwrap();
        let a = f.glyph('W', false).unwrap();
        let b = f.glyph('W', false).unwrap();
        assert_eq!(a.uv, b.uv, "the second lookup must not re-pack the glyph");
    }

    #[test]
    fn changing_size_rebuilds_metrics_and_cache() {
        let mut f = FontSet::new(13.0).unwrap();
        let small = f.cell_w;
        f.glyph('A', false).unwrap();
        f.set_size(26.0);
        assert!(f.cell_w > small, "a bigger size must widen the cell");
        let g = f.glyph('A', false).expect("glyph must re-rasterise after a resize");
        assert!(g.w > 0.0);
    }

    #[test]
    fn atlas_reports_only_the_rows_it_touched() {
        let mut f = FontSet::new(26.0).unwrap();
        f.atlas.dirty = None;
        f.glyph('Z', false).unwrap();
        let (y0, y1) = f.atlas.dirty.expect("inserting a glyph must mark rows dirty");
        assert!(y1 > y0 && y1 <= f.atlas.size);
    }
    #[test]
    fn fallback_covers_cjk_and_box_drawing() {
        // The primary monospace face rarely has kana/kanji, so these only
        // resolve if the fallback chain is working.
        let mut f = FontSet::new(26.0).unwrap();
        for c in ['あ', '漢', '→', '│', '█'] {
            let g = f.glyph(c, false).unwrap_or_else(|| panic!("no glyph for {c:?}"));
            assert!(g.w > 0.0 && g.h > 0.0, "{c:?} rasterised empty");
        }
    }
}
