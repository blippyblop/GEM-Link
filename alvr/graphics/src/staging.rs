use super::{GraphicsContext, NativeFrame, ck};
use crate::GL_TEXTURE_EXTERNAL_OES;
use alvr_common::glam::{IVec2, UVec2};
use glow::{self as gl, HasContext};
use std::{cell::Cell, rc::Rc};

fn create_program(
    gl: &gl::Context,
    vertex_shader_source: &str,
    fragment_shader_source: &str,
) -> gl::Program {
    unsafe {
        let vertex_shader = ck!(gl.create_shader(gl::VERTEX_SHADER).unwrap());
        ck!(gl.shader_source(vertex_shader, vertex_shader_source));
        ck!(gl.compile_shader(vertex_shader));
        if !gl.get_shader_compile_status(vertex_shader) {
            panic!(
                "Failed to compile vertex shader: {}",
                gl.get_shader_info_log(vertex_shader)
            );
        }

        let fragment_shader = ck!(gl.create_shader(gl::FRAGMENT_SHADER).unwrap());
        ck!(gl.shader_source(fragment_shader, fragment_shader_source));
        ck!(gl.compile_shader(fragment_shader));
        if !gl.get_shader_compile_status(fragment_shader) {
            panic!(
                "Failed to compile fragment shader: {}",
                gl.get_shader_info_log(fragment_shader)
            );
        }

        let program = ck!(gl.create_program().unwrap());
        ck!(gl.attach_shader(program, vertex_shader));
        ck!(gl.attach_shader(program, fragment_shader));
        ck!(gl.link_program(program));
        if !gl.get_program_link_status(program) {
            panic!(
                "Failed to link program: {}",
                gl.get_program_info_log(program)
            );
        }

        ck!(gl.delete_shader(vertex_shader));
        ck!(gl.delete_shader(fragment_shader));

        program
    }
}

pub struct StagingRenderer {
    context: Rc<GraphicsContext>,
    program: gl::Program,
    view_idx_uloc: gl::UniformLocation,
    surface_texture: gl::Texture,
    framebuffers: [gl::Framebuffer; 2],
    viewport_size: IVec2,
    /// So a renderer that cannot import frames says so once rather than sixty times a second.
    dma_buf_failure_reported: Cell<bool>,
}

impl StagingRenderer {
    pub fn new(
        context: Rc<GraphicsContext>,
        staging_textures: [gl::Texture; 2],
        view_resolution: UVec2,
        fix_limited_range: bool,
    ) -> Self {
        let gl = &context.gl_context;
        context.make_current();

        // Add #defines into the shader after the first line
        let mut frag_lines: Vec<&str> = include_str!("../resources/staging_fragment.glsl")
            .lines()
            .collect();
        if fix_limited_range {
            frag_lines.insert(1, "#line 0 1\n#define FIX_LIMITED_RANGE");
        }
        let frag_str = frag_lines.join("\n");

        let program = create_program(
            gl,
            include_str!("../resources/staging_vertex.glsl"),
            frag_str.as_str(),
        );

        unsafe {
            // This is an external surface and storage should not be initialized
            let surface_texture = ck!(gl.create_texture().unwrap());

            // **The staging textures are cleared to a no-signal grey, not left black.**
            //
            // They are what the renderer samples when the decoder has nothing new — deliberately, so
            // a held frame stays on screen. But before the *first* frame, and after a stream reset,
            // "nothing new" means an uninitialised texture: a driver is free to hand back anything,
            // and on every driver that means zeros. A black screen is indistinguishable from a
            // crashed streamer, and the one thing the display path must never do is look dead while
            // the stream is alive. Grey reads as "no signal yet", which is what it is.
            let mut framebuffers = vec![];
            for tex in staging_textures {
                let framebuffer = ck!(gl.create_framebuffer().unwrap());
                ck!(gl.bind_framebuffer(gl::DRAW_FRAMEBUFFER, Some(framebuffer)));
                ck!(gl.framebuffer_texture_2d(
                    gl::DRAW_FRAMEBUFFER,
                    gl::COLOR_ATTACHMENT0,
                    gl::TEXTURE_2D,
                    Some(tex),
                    0,
                ));

                ck!(gl.clear_color(0.5, 0.5, 0.5, 1.0));
                ck!(gl.clear(gl::COLOR_BUFFER_BIT));

                framebuffers.push(framebuffer);
            }

            ck!(gl.bind_framebuffer(gl::FRAMEBUFFER, None));

            let view_idx_uloc = ck!(gl.get_uniform_location(program, "view_idx")).unwrap();

            Self {
                context,
                program,
                surface_texture,
                view_idx_uloc,
                framebuffers: framebuffers.try_into().unwrap(),
                viewport_size: view_resolution.as_ivec2(),
                dma_buf_failure_reported: Cell::new(false),
            }
        }
    }

    /// Draw `frame` into both eyes' staging textures.
    ///
    /// Every branch that *fails* says so once and then keeps the previous picture. That is the
    /// deliberate choice: a held frame is a stutter the user can see coming out of, and a frame
    /// imported with the wrong stride is a sheared image that reads as a shader bug. The one thing
    /// that must not happen is a silent black screen.
    pub fn render(&self, frame: NativeFrame) {
        let gl = &self.context.gl_context;
        self.context.make_current();

        let draw = || unsafe {
            ck!(gl.use_program(Some(self.program)));

            ck!(gl.viewport(0, 0, self.viewport_size.x, self.viewport_size.y));
            ck!(gl.disable(gl::SCISSOR_TEST));
            ck!(gl.disable(gl::STENCIL_TEST));

            for (i, framebuffer) in self.framebuffers.iter().enumerate() {
                ck!(gl.bind_framebuffer(gl::DRAW_FRAMEBUFFER, Some(*framebuffer)));

                ck!(gl.active_texture(gl::TEXTURE0));
                ck!(gl.bind_texture(GL_TEXTURE_EXTERNAL_OES, Some(self.surface_texture)));
                ck!(gl.bind_sampler(0, None));
                ck!(gl.uniform_1_i32(Some(&self.view_idx_uloc), i as i32));
                ck!(gl.draw_arrays(gl::TRIANGLE_STRIP, 0, 4));
            }
        };

        match frame {
            NativeFrame::None => {}
            NativeFrame::HardwareBuffer(address) => unsafe {
                // SAFETY: the address was produced by the decoder as an `AHardwareBuffer *` and is
                // kept alive by the client for the duration of the frame.
                self.context.render_ahardwarebuffer_using_texture(
                    address as *const std::ffi::c_void,
                    self.surface_texture,
                    draw,
                );
            },
            NativeFrame::DmaBuf(dma_buf) => {
                // SAFETY: the fds belong to the decoder that produced this frame and stay open for
                // the duration of the call.
                let drawn = unsafe {
                    self.context
                        .render_dma_buf_using_texture(&dma_buf, self.surface_texture, draw)
                };
                if !drawn && !self.dma_buf_failure_reported.get() {
                    self.dma_buf_failure_reported.set(true);
                    alvr_common::error!(
                        "could not import a {}x{} dma-buf from the decoder; the picture will be \
                         static. This is the client's renderer, not the stream.\
                         {}",
                        dma_buf.width,
                        dma_buf.height,
                        if self.context.supports_dma_buf_import() {
                            ""
                        } else {
                            " The EGL display has no EGL_EXT_image_dma_buf_import."
                        }
                    );
                }
            }
        }
    }
}

impl Drop for StagingRenderer {
    fn drop(&mut self) {
        let gl = &self.context.gl_context;
        self.context.make_current();

        unsafe {
            ck!(gl.delete_program(self.program));
            ck!(gl.delete_texture(self.surface_texture));
            for framebuffer in &self.framebuffers {
                ck!(gl.delete_framebuffer(*framebuffer));
            }
        }
    }
}
