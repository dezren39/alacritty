//! This module implements the logic to manage graphic items included in a
//! `Grid` instance.

pub mod osc1337;
pub mod sixel;

use std::cmp::min;
use std::collections::BTreeMap;
use std::mem;
use std::ops::{Add, Bound, RangeBounds, RangeInclusive, Sub};

use image::DynamicImage;
use serde::{Deserialize, Serialize};

use crate::index::{Column, Line};
use crate::term::color::Rgb;
use crate::term::SizeInfo;

/// Pixels rows in a single graphic item.
pub const ROWS_PER_GRAPHIC: usize = 1000;

/// Max allowed dimensions (width, height) for the graphic, in pixels.
const MAX_GRAPHIC_DIMENSIONS: (usize, usize) = (4096, 4096);

/// Specifies the format of the pixel data.
#[derive(Serialize, Deserialize, Eq, PartialEq, Clone, Debug, Copy)]
pub enum ColorType {
    /// 3 bytes per pixel (red, green, blue).
    RGB,

    /// 4 bytes per pixel (red, green, blue, alpha).
    RGBA,
}

impl ColorType {
    /// Number of bytes to define a single pixel.
    #[inline]
    pub fn bytes_per_pixel(&self) -> usize {
        match *self {
            ColorType::RGB => 3,
            ColorType::RGBA => 4,
        }
    }
}

/// Unit to specify a dimension to resize the graphic.
#[derive(Serialize, Deserialize, Eq, PartialEq, Clone, Copy, Debug)]
pub enum ResizeParameter {
    /// Dimension is computed from the original graphic dimensions.
    Auto,

    /// Size is specified in number of grid cells.
    Cells(u32),

    /// Size is specified in number pixels.
    Pixels(u32),

    /// Size is specified in a percent of the window.
    WindowPercent(u32),
}

/// Dimensions to resize a graphic.
#[derive(Serialize, Deserialize, Eq, PartialEq, Clone, Copy, Debug)]
pub struct ResizeCommand {
    pub width: ResizeParameter,

    pub height: ResizeParameter,

    pub preserve_aspect_ratio: bool,
}

/// Defines a single graphic read from the PTY.
#[derive(Serialize, Deserialize, Eq, PartialEq, Clone, Debug)]
pub struct GraphicData {
    /// Column in the grid where the graphic should be attached.
    pub column: Column,

    /// Line in the grid where the graphic should be attached.
    pub line: GraphicsLine,

    /// Width, in pixels, of the graphic.
    pub width: usize,

    /// Height, in pixels, of the graphic.
    pub height: usize,

    /// Color type of the pixels.
    pub color_type: ColorType,

    /// Pixels data.
    pub pixels: Vec<u8>,

    /// Render graphic in a different size.
    pub resize: Option<ResizeCommand>,
}

impl GraphicData {
    /// Creates an empty graphic. Used for testing in both `alacritty` and
    /// `alacritty_terminal` crates.
    pub fn with_size(
        color_type: ColorType,
        column: Column,
        line: GraphicsLine,
        width: usize,
        height: usize,
    ) -> GraphicData {
        GraphicData {
            column,
            line,
            width,
            height,
            color_type,
            pixels: vec![0; width * height * color_type.bytes_per_pixel()],
            resize: None,
        }
    }

    /// Create an instance from [`image::DynamicImage`].
    pub fn from_dynamic_image(column: Column, line: GraphicsLine, image: DynamicImage) -> Self {
        let color_type;
        let width;
        let height;
        let pixels;

        match image {
            DynamicImage::ImageRgb8(image) => {
                color_type = ColorType::RGB;
                width = image.width() as usize;
                height = image.height() as usize;
                pixels = image.into_raw();
            },

            DynamicImage::ImageRgba8(image) => {
                color_type = ColorType::RGBA;
                width = image.width() as usize;
                height = image.height() as usize;
                pixels = image.into_raw();
            },

            _ => {
                // Non-RGB image. Convert it to RGBA.
                let image = image.into_rgba8();
                color_type = ColorType::RGBA;
                width = image.width() as usize;
                height = image.height() as usize;
                pixels = image.into_raw();
            },
        }

        GraphicData { column, line, width, height, color_type, pixels, resize: None }
    }

    /// Resize the graphic according to the dimensions in the `resize` field.
    pub fn resized(
        self,
        cell_width: usize,
        cell_height: usize,
        view_width: usize,
        view_height: usize,
    ) -> Option<Self> {
        let resize = match self.resize {
            Some(resize) => resize,
            None => return Some(self),
        };

        if (resize.width == ResizeParameter::Auto && resize.height == ResizeParameter::Auto)
            || self.height == 0
            || self.width == 0
        {
            return Some(self);
        }

        let mut width = match resize.width {
            ResizeParameter::Auto => 1,
            ResizeParameter::Pixels(n) => n as usize,
            ResizeParameter::Cells(n) => n as usize * cell_width,
            ResizeParameter::WindowPercent(n) => n as usize * view_width / 100,
        };

        let mut height = match resize.height {
            ResizeParameter::Auto => 1,
            ResizeParameter::Pixels(n) => n as usize,
            ResizeParameter::Cells(n) => n as usize * cell_height,
            ResizeParameter::WindowPercent(n) => n as usize * view_height / 100,
        };

        if width == 0 || height == 0 {
            return None;
        }

        // Compute "auto" dimensions.
        if resize.width == ResizeParameter::Auto {
            width = self.width * height / self.height;
        }

        if resize.height == ResizeParameter::Auto {
            height = self.height * width / self.width;
        }

        // Limit size to MAX_GRAPHIC_DIMENSIONS.
        width = min(width, MAX_GRAPHIC_DIMENSIONS.0);
        height = min(height, MAX_GRAPHIC_DIMENSIONS.1);

        log::trace!(
            target: "graphics",
            "Resize new graphic to width={}, height={}",
            width,
            height,
        );

        // Create a new DynamicImage to resize the graphic.
        let dynimage = match self.color_type {
            ColorType::RGB => {
                let buffer =
                    image::RgbImage::from_raw(self.width as u32, self.height as u32, self.pixels)?;
                DynamicImage::ImageRgb8(buffer)
            },

            ColorType::RGBA => {
                let buffer =
                    image::RgbaImage::from_raw(self.width as u32, self.height as u32, self.pixels)?;
                DynamicImage::ImageRgba8(buffer)
            },
        };

        // Finally, use `resize` or `resize_exact` to make the new image.
        let width = width as u32;
        let height = height as u32;
        let filter = image::imageops::FilterType::Triangle;

        let new_image = if resize.preserve_aspect_ratio {
            dynimage.resize(width, height, filter)
        } else {
            dynimage.resize_exact(width, height, filter)
        };

        Some(Self::from_dynamic_image(self.column, self.line, new_image))
    }
}

/// Line relative to the `base_position` field of the `Graphics` type.
///
/// Its inner value requires a signed integer because it can be negative when
/// the window is resized.
#[derive(Serialize, Deserialize, Debug, Copy, Clone, Eq, PartialEq, Default, Ord, PartialOrd)]
pub struct GraphicsLine(pub isize);

impl GraphicsLine {
    /// Compute the `GraphicsLine` equivalent of a line in the grid.
    #[inline]
    pub fn new<G>(graphics: &Graphics<G>, line: Line) -> GraphicsLine {
        GraphicsLine(graphics.base_position + line.0 as isize)
    }

    /// Map a `Range*<Line>` to `Range*<GraphicsLine>`.
    pub fn range<R, G>(graphics: &Graphics<G>, range: R) -> impl RangeBounds<GraphicsLine>
    where
        R: RangeBounds<Line>,
    {
        let map = |bound: Bound<&Line>| match bound {
            Bound::Included(&b) => Bound::Included(GraphicsLine::new(graphics, b)),
            Bound::Excluded(&b) => Bound::Excluded(GraphicsLine::new(graphics, b)),
            Bound::Unbounded => Bound::Unbounded,
        };

        (map(range.start_bound()), map(range.end_bound()))
    }
}

impl Add<isize> for GraphicsLine {
    type Output = Self;

    #[inline]
    fn add(self, rhs: isize) -> Self {
        GraphicsLine(self.0 + rhs)
    }
}

impl Sub<isize> for GraphicsLine {
    type Output = Self;

    #[inline]
    fn sub(self, rhs: isize) -> Self {
        GraphicsLine(self.0 - rhs)
    }
}

/// Storage for graphics attached to a grid.
///
/// Graphics read from PTY are added to the `pending` queue.
///
/// The display should collect items in the queue, move them the GPU (or any other
/// presentation system), and attach them to a specific `GraphicsLine` instance.
///
/// The type used to track graphics in the display is generic, and we don't make
/// any requirements for it. The display is free to use whatever works better.
///
/// `base_position` is used to compute the `GraphicsLine` for a line in the grid.
/// It is updated when the grid is scrolled up, or resized.
#[derive(Serialize, Deserialize, Eq, PartialEq, Clone, Debug)]
pub struct Graphics<G> {
    /// Base value used to compute the position of the graphics in the grid.
    #[serde(skip)]
    base_position: isize,

    /// Graphics attached to a line in the grid.
    ///
    /// These items require data generated by the display (like the texture
    /// name), so in this module we don't assume anything about the values.
    #[serde(skip)]
    pub attachments: BTreeMap<GraphicsLine, G>,

    /// Graphics read from the PTY, and ready to be attached to the display.
    pub pending: Vec<GraphicData>,

    /// Graphics removed from the grid. The display is responsible to release
    /// the resources used by them.
    pub removed: Vec<G>,

    /// Shared palette for Sixel graphics.
    pub sixel_shared_palette: Option<Vec<Rgb>>,
}

// A manual implementation for `Default` is required because, if we use the
// `#[derive]` attribute, the compiler requires the constraint `G: Default`.
//
// We should not make assumptions about the instances for `G` (it may be
// impossible to make a default value), so we skip the automatic `Default.
impl<G> Default for Graphics<G> {
    fn default() -> Self {
        Graphics {
            base_position: 0,
            attachments: BTreeMap::new(),
            pending: Vec::new(),
            removed: Vec::new(),
            sixel_shared_palette: None,
        }
    }
}

impl<G> Graphics<G> {
    /// Attach a graphics item to a specific `GraphicsLine`. The line should
    /// not have any previous attachment.
    #[inline]
    pub fn attach(&mut self, top: GraphicsLine, item: G) {
        let _previous_item = self.attachments.insert(top, item);
        debug_assert!(_previous_item.is_none());
    }

    /// Remove all graphics.
    ///
    /// Attached graphics are moved to the `removed` field. Pending graphics
    /// are discarded.
    pub fn clear(&mut self) {
        let attached_len = self.attachments.len();
        if attached_len > 0 {
            self.removed.reserve(attached_len);
            for (_, item) in mem::take(&mut self.attachments) {
                self.removed.push(item);
            }
        }

        self.pending.clear();
        self.sixel_shared_palette = None;
        self.base_position = 0;
    }

    /// Remove attachments in the given range.
    ///
    /// The graphics are moved to the `removed` queue, so the display can
    /// release the resources used by them.
    #[inline]
    pub fn clear_range<R>(&mut self, range: R)
    where
        R: RangeBounds<Line>,
    {
        if self.attachments.is_empty() {
            return;
        }

        self.remove_attachments(GraphicsLine::range(self, range));
    }

    /// Compute a range of `GraphicsLine` instances with graphics items that
    /// may be visible in the display.
    ///
    /// The caller must check the height of the graphic to determine if they
    /// are really visible.
    ///
    /// The range includes lines above the current grid, because some graphics
    /// may start before the top line, but extend multiple lines.
    pub fn visible_range(
        &self,
        size_info: &SizeInfo,
        display_offset: usize,
    ) -> RangeInclusive<GraphicsLine> {
        // We assume the worst case (`cell_height = 1px`) even when it is
        // mostly impossible to be there.
        //
        // A simple optimization is to ignore useless cases (for example,
        // `cell_height < 6px`), and reduce the range with a smaller range,
        // like `ROWS_PER_GRAPHIC / 6`. However, it is unclear if, in a real
        // scenario, there is any improvement.
        let top = self.base_position - ROWS_PER_GRAPHIC as isize - display_offset as isize;

        let bottom = self.base_position + size_info.screen_lines().0 as isize;
        let bottom = bottom - display_offset as isize;

        GraphicsLine(top)..=GraphicsLine(bottom)
    }

    /// Get the current line offset.
    #[inline]
    pub fn base_position(&self) -> isize {
        self.base_position
    }

    /// Update the `base_position` field, and remove graphics that are now out
    /// of scrolling history.
    #[inline]
    pub fn move_base_position(&mut self, positions: isize, history_size: usize) {
        self.base_position += positions;

        // Remove graphics out of scrolling history.
        if history_size > 0 && !self.attachments.is_empty() {
            let limit = GraphicsLine(self.base_position - history_size as isize);
            self.remove_attachments(..limit);
        }
    }

    /// Move attachments in the given range to the `removed` queue.
    fn remove_attachments<R>(&mut self, range: R)
    where
        R: RangeBounds<GraphicsLine>,
    {
        let keys: Vec<_> = self.attachments.range(range).map(|(&k, _)| k).collect();
        for key in keys {
            if let Some(item) = self.attachments.remove(&key) {
                self.removed.push(item);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_visible_lines() {
        let size_info = SizeInfo::new(1000., 1000., 10., 10., 0., 0., false);
        let max_height_lines = ROWS_PER_GRAPHIC as isize / size_info.cell_height() as isize;

        let mut graphics = Graphics::<char>::default();
        graphics.move_base_position(5, 0);

        assert_eq!(
            graphics.visible_range(&size_info, 0),
            GraphicsLine(5 - ROWS_PER_GRAPHIC as isize)..=GraphicsLine(105)
        );

        graphics.move_base_position(max_height_lines + 1, 0);
        assert_eq!(
            graphics.visible_range(&size_info, 5),
            GraphicsLine(max_height_lines + 1 - ROWS_PER_GRAPHIC as isize)
                ..=GraphicsLine(max_height_lines + 100 + 1)
        );
    }
}
