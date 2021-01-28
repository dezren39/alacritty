//! This module implements the functionality to show graphics in the terminal grid.
//!
//! ## Storage
//!
//! Graphics are stored in the grid as _attachments_, in the [`attachments`] field
//! of [`Graphics`].
//!
//! Each value in the [`attachments`] map is an instance of [`GraphicItem`]. These
//! instances has the necessary data to execute the graphics rendering shader
//! program to show the graphic: the texture in the GPU, its position, and its
//! dimensions.
//!
//! ## Phases
//!
//! Adding or removing graphics may need multiple calls to OpenGL functions, and we
//! need mutable access to the [`Term`] instance to update the grid after applying
//! the updates. However, only the result of `glGenTextures` (stored in
//! [`GraphicItem::texture`]) is required to update the grid.
//!
//! We have to minimize the duration of the lock on the [`Term`] instance, so the PTY
//! reader thread can keep processing input data while the display is being updated.
//! To reduce the lock time, the process is split in two phases:
//!
//! * *Prepare* phase.
//!
//!   Data from the [`pending`] and [`removed`] queues are processed.
//!
//!   For every action required to update the display this phase emits a
//!   [*graphics command*](GraphicsCommand).
//!
//! * *Draw* phase.
//!
//!   It takes those commands and invokes the required OpenGL functions.
//!
//!   The lock on [`Term`] is released before executing the *draw* phase. The only
//!   OpenGL function executed during the *prepare* phase is `glGenTextures`.
//!
//! ### Prepare phase
//!
//! The *prepare* phase is done in three steps.
//!
//! First, it takes the [`GraphicItem`] instances in the [`removed`] field, and
//! builds a [`DeleteTextures`] command with the texture names in those items.
//!
//! Then, it attaches the [`GraphicData`] instances found in the [`pending`] field.
//! The attach process is described below.
//!
//! Finally, it collects the visible graphics in the current display, and emits a
//! [`Render`] command to show them.
//!
//! After this process, the fields [`removed`] and [`pending`] are empty, and the
//! [`attachments`] field is updated with the new graphics.
//!
//! #### Attaching graphics to the grid.
//!
//! Every line in the grid can be associated with only one texture. This is needed
//! to simplify the render process, but we have to make some extra steps in the
//! process to add a new graphic if it overlaps with an existing one.
//!
//! When the new graphic does not overlap with anything else, the process is
//! performed in two actions:
//!
//! 1. Create a [`GraphicItem`] instance with the data needed to render the graphic,
//!    and insert it in the [`attachments`] map.
//!
//! 2. Generate a new texture (`glGenTextures`), and emits an [`InitTexture`] command
//!    to upload the pixels in the *draw* phase.
//!
//! If the new graphic overlaps, it has to be split in multiple parts, and emits
//! [`ResizeTexture`] and [`BlitGraphic`] commands for the overlapping regions.
//!
//! For example, the following grid has a graphic from the point `(2, 3)` to
//! `(5, 5)`:
//!
//! ```notrust
//! --------------
//! --------------       '-' are empty cells.
//! -xxxx---------       'x' are cells occupied by a graphic.
//! -xxxx---------
//! -xxxx---------
//! --------------
//! --------------
//! --------------
//! ```
//!
//! Then, we add a graphic from `(8,2)` to `(10,7)`:
//!
//! ```notrust
//! --------------
//! -------aaa----
//! -xxxx--bbb----
//! -xxxx--bbb----
//! -xxxx--bbb----
//! -------ccc----
//! -------ccc----
//! --------------
//! ```
//!
//! The process is performed in the following actions:
//!
//! 1. The region above the existing graphic (`a` character in the previous grid) is
//!    added as a new graphic.
//!
//! 2. The overlapping region (with the `b` character) is merged with the existing
//!    graphic.
//!
//!    The new graphic is not within the bounds of the texture, so the process emits
//!    a [`ResizeTexture`] command. Thus, the texture bounds will be from column 2
//!    to column 10.
//!
//!    Then, it emits a [`BlitGraphic`] command to copy the pixels of the region
//!    with the `b` characters to the new texture.
//!
//! 3. Finally, the region below the existing graphic (with the `c` character) is
//!    added as a new graphic.
//!
//! Another example is to put a new graphic on the same region of an existing one
//! (in an image viewer or a similar application). In this case, we only need to
//! update the pixels of the existing texture, so the only emitted command is
//! [`BlitGraphic`].
//!
//! ### Draw phase
//!
//! The *draw* phase is executed after the lock on [`Term`] is released. It takes the
//! commands emitted in the *prepare* phase, and executes them to update the display.
//!
//! See [`GraphicsCommand`] documentation for more details.
//!
//! [`BlitGraphic`]: GraphicsCommand::BlitGraphic
//! [`DeleteTextures`]: GraphicsCommand::DeleteTextures
//! [`Graphics`]: alacritty_terminal::graphics::Graphics
//! [`InitTexture`]: GraphicsCommand::InitTexture
//! [`Render`]: GraphicsCommand::Render
//! [`ResizeTexture`]: GraphicsCommand::ResizeTexture
//! [`Term`]: alacritty_terminal::term::Term
//! [`attachments`]: alacritty_terminal::graphics::Graphics#structfield.attachments
//! [`pending`]: alacritty_terminal::graphics::Graphics#structfield.pending
//! [`removed`]: alacritty_terminal::graphics::Graphics#structfield.removed

use std::mem;

use alacritty_terminal::graphics::{ColorType, GraphicData, Graphics, GraphicsLine};
use alacritty_terminal::index::Column;
use alacritty_terminal::term::SizeInfo;

use log::trace;
use serde::{Deserialize, Serialize};

use crate::gl;
use crate::gl::types::*;
use crate::renderer;

mod draw;
mod prepare;
mod shader;

/// Type for texture names generated in the GPU.
#[derive(Serialize, Deserialize, Eq, PartialEq, Clone, Debug)]
pub struct TextureName(GLuint);

// In debug mode, check if the inner value was set to zero, so we can detect if
// the associated texture was deleted from the GPU.
#[cfg(debug_assertions)]
impl Drop for TextureName {
    fn drop(&mut self) {
        if self.0 != 0 {
            log::error!("Texture {} was not deleted.", self.0);
        }
    }
}

/// Graphic items, attached to a grid at a position specified by a
/// `GraphicsLine` instance.
///
/// This type contains the necessary data to draw a graphic in the
/// viewport. It is generated during the *prepare* phase.
#[derive(Serialize, Deserialize, PartialEq, Clone, Debug)]
pub struct GraphicItem {
    /// Texture in the GPU where the graphic pixels are stored.
    texture: TextureName,

    /// Column where the graphic is attached.
    column: Column,

    /// Last line where the graphic can be seen.
    bottom: GraphicsLine,

    /// Cell height at the moment graphic was created.
    ///
    /// Used to scale it if the user increases or decreases the font size.
    cell_height: f32,

    /// Width in pixels of the graphic.
    width: usize,

    /// Height in pixels of the graphic.
    height: usize,
}

impl GraphicItem {
    /// Ignore the inner value to avoid the warning of the `Drop`
    /// implementation.
    ///
    /// It should be used only when we know that the texture will not be
    /// released explicitly (for instance, when application exits).
    #[cfg(debug_assertions)]
    pub fn forget_texture(&mut self) {
        self.texture.0 = 0;
    }
}

/// Data for the `ResizeTexture` graphics command.
#[derive(Debug, PartialEq)]
pub struct ResizeTextureData {
    /// Texture to store the graphic.
    target_texture: GLuint,

    /// Graphic item to read the current texture pixels.
    source: GraphicItem,

    /// Offset in the x direction to copy the source texture.
    source_offset: GLint,

    /// Width in pixels of the new texture.
    width: usize,

    /// Height in pixels of the new texture.
    height: usize,
}

/// Data for the `BlitGraphic` graphics command.
#[derive(Debug, PartialEq)]
pub struct BlitGraphicData {
    /// Texture where the pixels will be copied.
    texture: GLuint,

    /// Offset in the x direction to copy the pixels.
    offset_x: usize,

    /// Offset in the y direction to copy the pixels.
    offset_y: usize,

    /// Width of the image defined by the pixels data.
    width: usize,

    /// Height of the image defined by the pixels data.
    height: usize,

    /// Format of every pixel.
    color_type: ColorType,

    /// Pixels data.
    pixels: Vec<u8>,
}

/// Data for the `Render` graphics command.
#[derive(Debug, PartialEq)]
pub struct RenderData {
    /// Vertices for the `glDrawArrays` call.
    pub vertices: Vec<shader::Vertex>,

    /// Offset to compute the position the graphics from the `GraphicsLine`.
    pub graphics_line_offset: f32,

    /// Width in pixels of the viewport.
    pub view_width: f32,

    /// Height in pixels of the viewport.
    pub view_height: f32,

    /// Width in pixels of a single grid cell.
    pub cell_width: f32,

    /// Height in pixels of a single grid cell.
    pub cell_height: f32,
}

/// Commands generated during the *prepare* phase.
#[derive(Debug, PartialEq)]
pub enum GraphicsCommand {
    /// Delete the textures in the array, in a single call to
    /// `glDeleteTextures`.
    DeleteTextures(Vec<GLuint>),

    /// Configure a new texture and copy the pixels in the `GraphicData`
    /// instance.
    InitTexture(GLuint, GraphicData),

    /// Configure a new texture and copy the pixels of another one. Then,
    /// delete the old texture.
    ///
    /// See [`run_resize_texture`](draw::run_resize_texture) for more details.
    ResizeTexture(ResizeTextureData),

    /// Transfer pixels from the CPU memory to a texture.
    BlitGraphic(BlitGraphicData),

    /// Use the graphics rendering shader program to show the graphics
    /// in the display.
    Render(RenderData),
}

#[derive(Debug)]
pub struct GraphicsRenderer {
    program: shader::GraphicsShaderProgram,
}

impl GraphicsRenderer {
    pub fn new() -> Result<GraphicsRenderer, renderer::Error> {
        let program = shader::GraphicsShaderProgram::new()?;
        Ok(GraphicsRenderer { program })
    }

    /// Run the *prepare* phase, and return the commands for the *draw* phase.
    ///
    /// If there is no graphics in the grid, return `None`.
    #[inline]
    pub fn prepare(
        &self,
        graphics: &mut Graphics<GraphicItem>,
        display_offset: usize,
        size_info: &SizeInfo,
    ) -> Option<Vec<GraphicsCommand>> {
        let attachments_is_empty = graphics.attachments.is_empty();
        let pending_is_empty = graphics.pending.is_empty();
        let removed_is_empty = graphics.removed.is_empty();

        // If there are no graphics in the grid, return as soon as possible.
        if attachments_is_empty && pending_is_empty && removed_is_empty {
            return None;
        }

        let mut commands = Vec::new();

        // Collect textures to be deleted.
        if !removed_is_empty {
            commands.push(prepare::delete(mem::take(&mut graphics.removed)));
        }

        // Prepare textures that need to be initialized in the GPU.
        if !pending_is_empty {
            for data in mem::take(&mut graphics.pending) {
                prepare::attach_graphics(graphics, size_info, &mut commands, data, || {
                    let mut texture: GLuint = 0;
                    unsafe { gl::GenTextures(1, &mut texture) };
                    trace!(target: "graphics", "Texture generated: {}", texture);
                    texture
                });
            }
        }

        // Collect graphics to draw on the display. They could be added
        // after processing the `pending` field, so we can't reuse the
        // `attachments_is_empty` variable.
        if !graphics.attachments.is_empty() {
            prepare::render_commands(graphics, size_info, display_offset, &mut commands);
        }

        Some(commands)
    }

    /// Run the *draw* phase, to show graphics in the grid.
    pub fn draw(&self, commands: Vec<GraphicsCommand>) {
        for command in commands {
            match command {
                GraphicsCommand::DeleteTextures(textures) => draw::run_delete_textures(textures),
                GraphicsCommand::InitTexture(texture, data) => {
                    draw::run_init_texture(texture, data)
                },
                GraphicsCommand::ResizeTexture(data) => draw::run_resize_texture(data),
                GraphicsCommand::BlitGraphic(data) => draw::run_blit_graphic(data),
                GraphicsCommand::Render(data) => draw::run_render(&self.program, data),
            }
        }
    }
}
