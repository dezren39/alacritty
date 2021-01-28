//! Functions to create graphics commands in the *prepare* phase.
//!
//! See the documentation of the `renderer::graphics` module for more details.

use std::cmp::{max, min};
use std::mem;

use alacritty_terminal::graphics::{GraphicData, Graphics, GraphicsLine, ROWS_PER_GRAPHIC};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::SizeInfo;

use crate::gl::types::*;

use super::{
    shader, BlitGraphicData, GraphicItem, GraphicsCommand, RenderData, ResizeTextureData,
    TextureName,
};

/// Take the graphic items in the `removed` queue and prepare a
/// `DeleteTextures` command to release their resources.
pub(super) fn delete(items: Vec<GraphicItem>) -> GraphicsCommand {
    let mut textures = Vec::with_capacity(items.len());
    for mut item in items {
        // Reset the inner value of TextureName, so the Drop implementation
        // (in debug mode) can verify that the texture was deleted.
        textures.push(mem::take(&mut item.texture.0));
    }

    GraphicsCommand::DeleteTextures(textures)
}

/// Attach a new graphic to a grid.
///
/// If the new graphic does not overlap with any other graphic, the process to
/// attach it is the following:
///
/// Generate new texture (`glGenTextures`) to build a `GraphicItem` instance.
/// Attach the item to the grid (`graphics` parameter of the function).
/// Emit a `InitTexture` command to upload the pixels in the *draw* phase.
///
/// If the new graphic overlaps with any existing graphics, the function splits
/// the new graphic in multiple parts. The textures of the existing graphics are
/// called _surfaces_ in this function.
///
/// If the region of the new graphic is not within the bounds of the surface, the
/// function generates a new texture and emits a `ResizeTexture` command to
/// initialize it. Then, update the attachment in the grid to use the new texture.
///
/// A `BlitGraphic` command is emitted to transfer the pixels in the overlapping
/// region to the surface.
///
/// The call to `glGenTextures` is done in the `texture_counter` closure. The reason
/// to receive values from a closure is to make it easier to test.
pub(super) fn attach_graphics<T>(
    graphics: &mut Graphics<GraphicItem>,
    size_info: &SizeInfo,
    commands: &mut Vec<GraphicsCommand>,
    mut new_graphic: GraphicData,
    mut texture_generator: T,
) where
    T: FnMut() -> GLuint,
{
    // Compute the last line to contain the new graphic.
    let new_graphic_bottom = new_graphic.line
        + f32::ceil(new_graphic.height as f32 / size_info.cell_height()) as isize
        - 1;

    // Find existing graphics that overlaps with the new graphic.
    let surface_lines: Vec<_> = graphics
        .attachments
        .range(new_graphic.line - ROWS_PER_GRAPHIC as isize..=new_graphic_bottom)
        .filter(|(&key, surface)| key <= new_graphic_bottom && new_graphic.line <= surface.bottom)
        .map(|(&key, _)| key)
        .collect();

    for surface_line in surface_lines {
        // Split the upper region of the new graphic if there is something
        // above the existing texture.

        let upper_lines = surface_line.0 - new_graphic.line.0;
        if upper_lines > 0 {
            let top = new_graphic.line;
            let height = upper_lines as usize * size_info.cell_height() as usize;

            let mut pixels = new_graphic.pixels.split_off(
                height * new_graphic.width as usize * new_graphic.color_type.bytes_per_pixel(),
            );

            // Update data of the new graphic.
            mem::swap(&mut pixels, &mut new_graphic.pixels);
            new_graphic.height -= height;
            new_graphic.line = new_graphic.line + upper_lines;

            let upper_data = GraphicData {
                column: new_graphic.column,
                line: top,
                width: new_graphic.width,
                height,
                color_type: new_graphic.color_type,
                pixels,
                resize: None,
            };

            let texture = texture_generator();
            let upper_item = GraphicItem {
                texture: TextureName(texture),
                column: upper_data.column,
                bottom: top + upper_lines - 1,
                cell_height: size_info.cell_height(),
                width: upper_data.width,
                height: upper_data.height,
            };

            graphics.attach(top, upper_item);
            commands.push(GraphicsCommand::InitTexture(texture, upper_data));
        }

        // Copy the overlapping region to the existing texture.

        let surface = &graphics.attachments[&surface_line];
        let mut surface_height = surface.height;

        let blit_tex;
        let blit_offset_x;

        // We have to resize the existing texture if the new graphic is not
        // within its bounds, or the cell_height is different.
        let cw = size_info.cell_width();

        let surface_right = cw.mul_add(surface.column.0 as f32, surface.width as f32) as usize;
        let new_graphic_right =
            cw.mul_add(new_graphic.column.0 as f32, new_graphic.width as f32) as usize;

        let surface_bottom = surface.bottom;

        // The surface is resized is any of the following conditions is
        // satisfied:
        //
        // - The left column of the surface is greater than the left column of the graphic.
        // - The right side of the surface is less than the right side of the graphic.
        // - The surface does not cover completely the last line in the grid.
        let need_resize = surface.column > new_graphic.column
            || surface_right < new_graphic_right
            || surface.height % size_info.cell_height() as usize != 0;

        if need_resize {
            let surface = graphics.attachments.remove(&surface_line).unwrap();
            blit_tex = texture_generator();

            let resize_column = min(surface.column.0, new_graphic.column.0);
            let resize_right = max(surface_right, new_graphic_right);

            let source_offset = (surface.column.0 - resize_column) as GLint * cw as GLint;
            blit_offset_x = (new_graphic.column.0 - resize_column) * cw as usize;

            let resize_width = resize_right - cw as usize * resize_column;

            // After a resize, the last pixels row of the surface should be at
            // the bottom of the last row in the grid.
            surface_height =
                ((surface_bottom.0 + 1 - surface_line.0) as f32 * size_info.cell_height()) as usize;

            // Attach the resized texture.
            let new_surface = GraphicItem {
                texture: TextureName(blit_tex),
                column: Column(resize_column as usize),
                bottom: surface.bottom,
                cell_height: size_info.cell_height(),
                width: resize_width,
                height: surface_height,
            };

            graphics.attach(surface_line, new_surface);

            commands.push(GraphicsCommand::ResizeTexture(ResizeTextureData {
                target_texture: blit_tex,
                source: surface,
                source_offset,
                width: resize_width,
                height: surface_height,
            }));
        } else {
            blit_tex = surface.texture.0;
            blit_offset_x = ((new_graphic.column - surface.column).0 as f32 * cw) as usize;
        }

        let blit_offset_y =
            ((new_graphic.line.0 - surface_line.0) as f32 * size_info.cell_height()) as usize;

        let mut pixels;
        let mut blit_height = surface_height - blit_offset_y;
        if blit_height >= new_graphic.height {
            blit_height = new_graphic.height;
            new_graphic.height = 0;
            pixels = mem::take(&mut new_graphic.pixels);
        } else {
            new_graphic.height -= blit_height;
            new_graphic.line = surface_bottom + 1;
            pixels = new_graphic.pixels.split_off(
                blit_height as usize
                    * new_graphic.width as usize
                    * new_graphic.color_type.bytes_per_pixel(),
            );
            mem::swap(&mut pixels, &mut new_graphic.pixels);
        }

        commands.push(GraphicsCommand::BlitGraphic(BlitGraphicData {
            texture: blit_tex,
            offset_x: blit_offset_x,
            offset_y: blit_offset_y,
            width: new_graphic.width,
            height: blit_height,
            color_type: new_graphic.color_type,
            pixels,
        }));

        if new_graphic.height == 0 {
            return;
        }
    }

    if new_graphic.height > 0 {
        // The new graphic (or its lower region) does not overlap with any existing
        // item in the grid, so we only need to create a new texture and emit a
        // command to initialize it.
        let texture = texture_generator();

        let graphics_item = GraphicItem {
            texture: TextureName(texture),
            column: new_graphic.column,
            bottom: new_graphic_bottom,
            cell_height: size_info.cell_height(),
            width: new_graphic.width,
            height: new_graphic.height,
        };

        graphics.attach(new_graphic.line, graphics_item);
        commands.push(GraphicsCommand::InitTexture(texture, new_graphic));
    }
}

/// Generate the vertices required to render the graphics visible in the current
/// viewport, and emit a `Render` command with all of them.
///
/// If no graphics are visible, it does not emit any command.
pub(super) fn render_commands(
    graphics: &Graphics<GraphicItem>,
    size_info: &SizeInfo,
    display_offset: usize,
    commands: &mut Vec<GraphicsCommand>,
) {
    use super::shader::VertexSide::{BottomLeft, BottomRight, TopLeft, TopRight};

    let mut vertices = Vec::new();

    let visible_range = graphics.visible_range(size_info, display_offset);

    let display_top = GraphicsLine::new(graphics, Line(0)) - display_offset as isize;

    for (graphics_line, graphics_item) in graphics.attachments.range(visible_range) {
        // Check if the bottom side of the graphic is within the viewport.
        if display_top > graphics_item.bottom {
            continue;
        }

        // 2 triangles to render the graphic.

        vertices.reserve(6);

        let vertex = shader::Vertex {
            texture_id: graphics_item.texture.0,
            sides: TopLeft,
            column: graphics_item.column.0 as GLuint,
            graphics_line: graphics_line.0 as GLuint,
            height: graphics_item.height as u16,
            width: graphics_item.width as u16,
            base_cell_height: graphics_item.cell_height,
        };

        vertices.push(vertex);

        for &sides in &[TopRight, BottomLeft, TopRight, BottomRight, BottomLeft] {
            vertices.push(shader::Vertex { sides, ..vertex });
        }
    }

    if !vertices.is_empty() {
        let data = RenderData {
            vertices,
            graphics_line_offset: (graphics.base_position() - display_offset as isize) as f32,
            view_width: size_info.width() - size_info.padding_x() * 2.,
            view_height: size_info.height() - size_info.padding_y() * 2.,
            cell_width: size_info.cell_width(),
            cell_height: size_info.cell_height(),
        };

        commands.push(GraphicsCommand::Render(data));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::graphics::*;

    #[test]
    fn attach_with_overlapping_graphics() {
        // This test put multiple graphics in the grid, in positions that can
        // trigger the different cases of `attach_graphics`.

        use GraphicsCommand::{BlitGraphic, InitTexture, ResizeTexture};

        const CELL_WIDTH: usize = 2;
        const CELL_HEIGHT: usize = 4;

        let mut pending_graphics = vec![];

        // Non-overlapping graphic, lines 2..12.
        pending_graphics.push(GraphicData::with_size(
            ColorType::RGB,
            Column(10),
            GraphicsLine(2),
            CELL_WIDTH * 5,
            CELL_HEIGHT * 10 - 3,
        ));

        // Non-overlapping graphic, line 15..35.
        pending_graphics.push(GraphicData::with_size(
            ColorType::RGB,
            Column(5),
            GraphicsLine(15),
            CELL_WIDTH * 30,
            CELL_HEIGHT * 20,
        ));

        // Overlapping graphic, line 0..5.
        pending_graphics.push(GraphicData::with_size(
            ColorType::RGB,
            Column(0),
            GraphicsLine(0),
            CELL_WIDTH * 5,
            CELL_HEIGHT * 5,
        ));

        // Overlapping graphic, line 10..20.
        pending_graphics.push(GraphicData::with_size(
            ColorType::RGB,
            Column(2),
            GraphicsLine(10),
            CELL_WIDTH * 30,
            CELL_HEIGHT * 10,
        ));

        // Overlapping graphic, line 25..30.
        pending_graphics.push(GraphicData::with_size(
            ColorType::RGBA,
            Column(15),
            GraphicsLine(25),
            CELL_WIDTH * 5,
            CELL_HEIGHT * 5,
        ));

        // Attach the pending graphics to the grid.

        let size_info =
            SizeInfo::new(1000., 1000., CELL_WIDTH as f32, CELL_HEIGHT as f32, 0., 0., false);

        let mut texture_counter = 10..;
        let mut commands = Vec::new();
        let mut graphics = Graphics::default();

        for graphic_data in pending_graphics {
            attach_graphics(&mut graphics, &size_info, &mut commands, graphic_data, || {
                texture_counter.next().unwrap()
            });
        }

        // Verify the emitted commands.
        //
        // The first two graphics only need an InitTexture command.

        let mut commands = commands.into_iter();

        assert_eq!(
            commands.next(),
            Some(InitTexture(
                10,
                GraphicData::with_size(
                    ColorType::RGB,
                    Column(10),
                    GraphicsLine(2),
                    CELL_WIDTH * 5,
                    CELL_HEIGHT * 10 - 3
                )
            ))
        );

        assert_eq!(
            commands.next(),
            Some(InitTexture(
                11,
                GraphicData::with_size(
                    ColorType::RGB,
                    Column(5),
                    GraphicsLine(15),
                    CELL_WIDTH * 30,
                    CELL_HEIGHT * 20
                )
            ))
        );

        // The third graphic is split in two textures. The lower part is merged
        // with the first graphic, which needs to be resized.

        assert_eq!(
            commands.next(),
            Some(InitTexture(
                12,
                GraphicData::with_size(
                    ColorType::RGB,
                    Column(0),
                    GraphicsLine(0),
                    CELL_WIDTH * 5,
                    CELL_HEIGHT * 2
                )
            ))
        );

        assert_eq!(
            commands.next(),
            Some(ResizeTexture(ResizeTextureData {
                target_texture: 13,
                source: GraphicItem {
                    texture: TextureName(10),
                    column: Column(10),
                    bottom: GraphicsLine(11),
                    cell_height: CELL_HEIGHT as f32,
                    width: CELL_WIDTH * 5,
                    height: CELL_HEIGHT * 10 - 3,
                },
                source_offset: CELL_WIDTH as GLint * 10,
                width: CELL_WIDTH * 15,
                height: CELL_HEIGHT * 10,
            }))
        );

        assert_eq!(
            commands.next(),
            Some(BlitGraphic(BlitGraphicData {
                texture: 13,
                offset_x: 0,
                offset_y: 0,
                width: CELL_WIDTH * 5,
                height: CELL_HEIGHT * 3,
                color_type: ColorType::RGB,
                pixels: vec![0; (CELL_WIDTH as usize * 5) * (CELL_HEIGHT as usize * 3) * 3],
            }))
        );

        // The fourth graphic overlaps two existing textures.

        assert_eq!(
            commands.next(),
            Some(ResizeTexture(ResizeTextureData {
                target_texture: 14,
                source: GraphicItem {
                    texture: TextureName(13),
                    column: Column(0),
                    bottom: GraphicsLine(11),
                    cell_height: CELL_HEIGHT as f32,
                    width: CELL_WIDTH * 15,
                    height: CELL_HEIGHT * 10,
                },
                source_offset: 0,
                width: CELL_WIDTH * 32,
                height: CELL_HEIGHT * 10,
            }))
        );

        assert_eq!(
            commands.next(),
            Some(BlitGraphic(BlitGraphicData {
                texture: 14,
                offset_x: CELL_WIDTH * 2,
                offset_y: CELL_HEIGHT * 8,
                width: CELL_WIDTH * 30,
                height: CELL_HEIGHT * 2,
                color_type: ColorType::RGB,
                pixels: vec![0; (CELL_WIDTH as usize * 30) * (CELL_HEIGHT as usize * 2) * 3],
            }))
        );

        assert_eq!(
            commands.next(),
            Some(InitTexture(
                15,
                GraphicData::with_size(
                    ColorType::RGB,
                    Column(2),
                    GraphicsLine(12),
                    CELL_WIDTH * 30,
                    CELL_HEIGHT * 3
                )
            ))
        );

        assert_eq!(
            commands.next(),
            Some(ResizeTexture(ResizeTextureData {
                target_texture: 16,
                source: GraphicItem {
                    texture: TextureName(11),
                    column: Column(5),
                    bottom: GraphicsLine(34),
                    cell_height: CELL_HEIGHT as f32,
                    width: CELL_WIDTH * 30,
                    height: CELL_HEIGHT * 20,
                },
                source_offset: 6,
                width: CELL_WIDTH * 33,
                height: CELL_HEIGHT * 20,
            }))
        );

        assert_eq!(
            commands.next(),
            Some(BlitGraphic(BlitGraphicData {
                texture: 16,
                offset_x: 0,
                offset_y: 0,
                width: CELL_WIDTH * 30,
                height: CELL_HEIGHT * 5,
                color_type: ColorType::RGB,
                pixels: vec![0; (CELL_WIDTH as usize * 30) * (CELL_HEIGHT as usize * 5) * 3],
            }))
        );

        // The last graphic doesn't need to resize any existing texture.
        assert_eq!(
            commands.next(),
            Some(BlitGraphic(BlitGraphicData {
                texture: 16,
                offset_x: CELL_WIDTH * 13,
                offset_y: CELL_HEIGHT * 10,
                width: CELL_WIDTH * 5,
                height: CELL_HEIGHT * 5,
                color_type: ColorType::RGBA,
                pixels: vec![0; (CELL_WIDTH as usize * 5) * (CELL_HEIGHT as usize * 5) * 4],
            }))
        );

        // No more commands.
        assert_eq!(commands.next(), None);

        // Check attached graphics.

        assert_eq!(
            graphics.attachments.remove(&GraphicsLine(0)),
            Some(GraphicItem {
                texture: TextureName(12),
                column: Column(0),
                bottom: GraphicsLine(1),
                cell_height: CELL_HEIGHT as f32,
                width: CELL_WIDTH * 5,
                height: CELL_HEIGHT * 2,
            })
        );

        assert_eq!(
            graphics.attachments.remove(&GraphicsLine(2)),
            Some(GraphicItem {
                texture: TextureName(14),
                column: Column(0),
                bottom: GraphicsLine(11),
                cell_height: CELL_HEIGHT as f32,
                width: CELL_WIDTH * 32,
                height: CELL_HEIGHT * 10,
            })
        );

        assert_eq!(
            graphics.attachments.remove(&GraphicsLine(12)),
            Some(GraphicItem {
                texture: TextureName(15),
                column: Column(2),
                bottom: GraphicsLine(14),
                cell_height: CELL_HEIGHT as f32,
                width: CELL_WIDTH * 30,
                height: CELL_HEIGHT * 3,
            })
        );

        assert_eq!(
            graphics.attachments.remove(&GraphicsLine(15)),
            Some(GraphicItem {
                texture: TextureName(16),
                column: Column(2),
                bottom: GraphicsLine(34),
                cell_height: CELL_HEIGHT as f32,
                width: CELL_WIDTH * 33,
                height: CELL_HEIGHT * 20,
            })
        );

        assert!(graphics.attachments.is_empty());

        assert_eq!(texture_counter.next(), Some(17));
    }
}
