//! Bounded JPEG pixel crops used only as corroborating visual evidence.
//!
//! This module deliberately does not assign a crop to a field. Callers must
//! establish the document, form, record and field geometry before asking for a
//! crop. It returns only a boolean change signal and never returns pixels.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;
use std::io::Cursor;
use zune_jpeg::JpegDecoder;

const MAX_DIMENSION: u32 = 1920;
const MAX_CROP_DIMENSION: u32 = 96;
const MAX_CROP_PIXELS: usize = (MAX_CROP_DIMENSION * MAX_CROP_DIMENSION) as usize;
const PIXEL_DELTA: u8 = 22;

pub(super) struct Frame {
    width: u32,
    height: u32,
    channels: u8,
    pixels: Vec<u8>,
}

pub(super) struct Crop {
    width: u32,
    height: u32,
    channels: u8,
    pixels: Vec<u8>,
}

impl Frame {
    /// Decode a viewport JPEG after validating its transport metadata.
    pub(super) fn decode(image: &Value) -> Option<Self> {
        if image.get("scope")?.as_str()? != "viewport"
            || image.get("mime_type")?.as_str()? != "image/jpeg"
            || image.get("css_pixel_scale")?.as_f64()? != 1.0
        {
            return None;
        }
        let width = image.get("width")?.as_u64()?.try_into().ok()?;
        let height = image.get("height")?.as_u64()?.try_into().ok()?;
        if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
            return None;
        }
        let encoded = image.get("data")?.as_str()?;
        if encoded.len() > 512 * 1024 || encoded.is_empty() {
            return None;
        }
        let bytes = STANDARD.decode(encoded).ok()?;
        if bytes.len() > 512 * 1024 {
            return None;
        }
        let mut decoder = JpegDecoder::new(Cursor::new(bytes));
        decoder.decode_headers().ok()?;
        let dimensions = decoder.dimensions()?;
        if dimensions != (width as usize, height as usize) {
            return None;
        }
        let pixels = decoder.decode().ok()?;
        let pixel_count = (width as usize).checked_mul(height as usize)?;
        let channels: u8 = match pixels.len() {
            n if n == pixel_count => 1,
            n if n == pixel_count.checked_mul(3)? => 3,
            n if n == pixel_count.checked_mul(4)? => 4,
            _ => return None,
        };
        Some(Self {
            width,
            height,
            channels,
            pixels,
        })
    }

    pub(super) fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Extract a small viewport crop. Coordinates are device pixels and must
    /// already have been checked against the field's stable layout rectangle.
    pub(super) fn crop(&self, x: u32, y: u32, width: u32, height: u32) -> Option<Crop> {
        if width == 0
            || height == 0
            || width > MAX_CROP_DIMENSION
            || height > MAX_CROP_DIMENSION
            || (width as usize).checked_mul(height as usize)? > MAX_CROP_PIXELS
            || x.checked_add(width)? > self.width
            || y.checked_add(height)? > self.height
        {
            return None;
        }
        let channels = self.channels as usize;
        let row_bytes = (self.width as usize).checked_mul(channels)?;
        let crop_row = (width as usize).checked_mul(channels)?;
        let mut pixels = Vec::with_capacity((height as usize) * crop_row);
        for row in y..y + height {
            let start = (row as usize)
                .checked_mul(row_bytes)?
                .checked_add((x as usize).checked_mul(channels)?)?;
            pixels.extend_from_slice(self.pixels.get(start..start + crop_row)?);
        }
        Some(Crop {
            width,
            height,
            channels: self.channels,
            pixels,
        })
    }
}

impl Crop {
    fn luma(&self, index: usize) -> u8 {
        let offset = index * self.channels as usize;
        match self.channels {
            1 => self.pixels[offset],
            _ => {
                let r = self.pixels[offset] as u16;
                let g = self.pixels[offset + 1] as u16;
                let b = self.pixels[offset + 2] as u16;
                ((r * 77 + g * 150 + b * 29) / 256) as u8
            }
        }
    }

    fn different_at(&self, i: usize, other: &Self, j: usize) -> bool {
        if self.luma(i).abs_diff(other.luma(j)) >= PIXEL_DELTA {
            return true;
        }
        let rgb = |crop: &Self, index: usize, channel: usize| {
            crop.pixels
                [index * crop.channels as usize + if crop.channels == 1 { 0 } else { channel }]
        };
        (0..3).any(|channel| rgb(self, i, channel).abs_diff(rgb(other, j, channel)) >= 48)
    }

    fn novel_at(&self, i: usize, other: &Self) -> bool {
        let width = self.width as usize;
        let (x, y) = (i % width, i / width);
        (y.saturating_sub(1)..=(y + 1).min(self.height as usize - 1)).all(|v| {
            (x.saturating_sub(1)..=(x + 1).min(width - 1))
                .all(|u| self.different_at(i, other, v * width + u))
        })
    }

    /// Return true only for a compact, non-uniform changed structure.
    /// Broad fills, theme changes and JPEG ringing are intentionally ignored.
    pub(super) fn changed_icon(&self, after: &Self) -> bool {
        if self.width != after.width
            || self.height != after.height
            || self.channels == 0
            || after.channels == 0
        {
            return false;
        }
        let count = (self.width as usize) * self.height as usize;
        let mut changed = Vec::new();
        for index in 0..count {
            // A fractional layout shift or another JPEG block alignment
            // can move an existing edge by a pixel. Require locally new paint.
            if self.different_at(index, after, index)
                && (self.novel_at(index, after) || after.novel_at(index, self))
            {
                changed.push(index);
            }
        }
        let total = count as f32;
        if changed.len() < 4 || changed.len() as f32 / total > 0.35 {
            return false;
        }
        let width = self.width as usize;
        let min_x = changed.iter().map(|i| i % width).min().unwrap_or(0);
        let max_x = changed.iter().map(|i| i % width).max().unwrap_or(0);
        let min_y = changed.iter().map(|i| i / width).min().unwrap_or(0);
        let max_y = changed.iter().map(|i| i / width).max().unwrap_or(0);
        let box_area = (max_x - min_x + 1) * (max_y - min_y + 1);
        if box_area as f32 / total > 0.65 || max_x - min_x == 0 || max_y - min_y == 0 {
            return false;
        }
        // Require enough changed pixels inside the compact box to reject a
        // single JPEG edge and enough empty perimeter to reject a flat fill.
        changed.len() * 100 / box_area >= 8 && changed.len() * 100 / count <= 35
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn crop(width: u32, height: u32, fill: u8, points: &[(usize, u8)]) -> Crop {
        let mut pixels = vec![fill; (width * height) as usize];
        for &(index, value) in points {
            pixels[index] = value;
        }
        Crop {
            width,
            height,
            channels: 1,
            pixels,
        }
    }
    #[test]
    fn compact_change_is_evidence() {
        let before = crop(16, 16, 120, &[]);
        let after = crop(
            16,
            16,
            120,
            &[(85, 0), (86, 0), (101, 0), (102, 0), (117, 0), (118, 0)],
        );
        assert!(before.changed_icon(&after));
    }
    #[test]
    fn flat_and_broad_changes_are_not_icons() {
        let before = crop(16, 16, 120, &[]);
        let flat = crop(16, 16, 0, &[(0, 0)]);
        assert!(!before.changed_icon(&flat));
        let broad_points: Vec<_> = (0..90).map(|i| (i, 0)).collect();
        let broad = crop(16, 16, 0, &broad_points);
        assert!(!before.changed_icon(&broad));
    }
    #[test]
    fn one_pixel_reflow_is_not_a_new_glyph() {
        let glyph = [(85, 0), (86, 0), (101, 0), (102, 0), (117, 0), (118, 0)];
        let shifted = glyph.map(|(i, c)| (i + 1, c));
        assert!(!crop(16, 16, 220, &glyph).changed_icon(&crop(16, 16, 220, &shifted)));
    }

    #[test]
    fn crop_bounds_are_bounded() {
        let frame = Frame {
            width: 32,
            height: 32,
            channels: 1,
            pixels: vec![0; 1024],
        };
        assert!(frame.crop(0, 0, 96, 1).is_none());
        assert!(frame.crop(31, 31, 2, 2).is_none());
        assert_eq!(frame.crop(4, 5, 8, 9).unwrap().width, 8);
    }
}
