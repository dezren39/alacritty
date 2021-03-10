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

use alacritty_terminal::graphics::{ColorType, GraphicData, GraphicId, UpdateQueues};
use alacritty_terminal::term::SizeInfo;

use log::trace;
use serde::{Deserialize, Serialize};

use crate::gl;
use crate::gl::types::*;
use crate::renderer;

use std::collections::HashMap;

mod draw;
mod shader;

pub use draw::RenderList;

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
pub struct GraphicTexture {
    /// Texture in the GPU where the graphic pixels are stored.
    texture: TextureName,

    /// Cell height at the moment graphic was created.
    ///
    /// Used to scale it if the user increases or decreases the font size.
    cell_height: f32,

    /// Width in pixels of the graphic.
    width: u16,

    /// Height in pixels of the graphic.
    height: u16,
}

#[derive(Debug)]
pub struct GraphicsRenderer {
    /// Program in the GPU to render graphics.
    program: shader::GraphicsShaderProgram,

    /// Collection to associate graphic identifiers with their textures.
    graphic_textures: HashMap<GraphicId, GraphicTexture>,
}

impl GraphicsRenderer {
    pub fn new() -> Result<GraphicsRenderer, renderer::Error> {
        let program = shader::GraphicsShaderProgram::new()?;
        Ok(GraphicsRenderer { program, graphic_textures: HashMap::default() })
    }

    /// Run the required actions to apply changes for the graphics in the grid.
    #[inline]
    pub fn run_updates(&mut self, update_queues: UpdateQueues, size_info: &SizeInfo) {
        self.remove_graphics(update_queues.remove_queue);
        self.upload_pending_graphics(update_queues.pending, size_info);
    }

    /// Release resources used by removed graphics.
    fn remove_graphics(&mut self, removed_ids: Vec<GraphicId>) {
        let mut textures = Vec::with_capacity(removed_ids.len());
        for id in removed_ids {
            if let Some(mut graphic_texture) = self.graphic_textures.remove(&id) {
                // Reset the inner value of TextureName, so the Drop implementation
                // (in debug mode) can verify that the texture was deleted.
                textures.push(mem::take(&mut graphic_texture.texture.0));
            }
        }

        trace!(target: "graphics", "Call glDeleteTextures with {} items", textures.len());

        unsafe {
            gl::DeleteTextures(textures.len() as GLint, textures.as_ptr());
        }
    }

    /// Create new textures in the GPU, and upload the pixels to them.
    fn upload_pending_graphics(&mut self, graphics: Vec<GraphicData>, size_info: &SizeInfo) {
        for graphic in graphics {
            let mut texture = 0;

            unsafe {
                gl::GenTextures(1, &mut texture);
                trace!(target: "graphics", "Texture generated: {}", texture);

                gl::BindTexture(gl::TEXTURE_2D, texture);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAX_LEVEL, 0);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as GLint);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as GLint);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as GLint);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as GLint);

                let pixel_format = match graphic.color_type {
                    ColorType::Rgb => gl::RGB,
                    ColorType::Rgba => gl::RGBA,
                };

                gl::TexImage2D(
                    gl::TEXTURE_2D,
                    0,
                    gl::RGBA as GLint,
                    graphic.width as GLint,
                    graphic.height as GLint,
                    0,
                    pixel_format,
                    gl::UNSIGNED_BYTE,
                    graphic.pixels.as_ptr().cast(),
                );

                gl::BindTexture(gl::TEXTURE_2D, 0);
            }

            let graphic_texture = GraphicTexture {
                texture: TextureName(texture),
                cell_height: size_info.cell_height(),
                width: graphic.width as u16,
                height: graphic.height as u16,
            };

            self.graphic_textures.insert(graphic.id, graphic_texture);
        }
    }

    /// Draw graphics in the display.
    #[inline]
    pub fn draw(&mut self, render_list: RenderList, size_info: &SizeInfo) {
        if !render_list.is_empty() {
            render_list.draw(self, size_info);
        }
    }
}
