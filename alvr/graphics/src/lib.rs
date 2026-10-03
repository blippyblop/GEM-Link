mod lobby;
mod staging;
mod stream;

pub use lobby::*;
pub use stream::*;

use alvr_common::{
    DeviceMotion, Fov, Pose,
    glam::{Mat4, UVec2, Vec4},
};
use glow::{self as gl, HasContext};
use khronos_egl as egl;
use std::{ffi::c_void, ptr};
use wgpu::{
    Device, Extent3d, Instance, Queue, Texture, TextureDescriptor, TextureDimension, TextureFormat,
    TextureUsages, TextureView,
};

pub const SDR_FORMAT: TextureFormat = TextureFormat::Rgba8Unorm;
pub const SDR_FORMAT_GL: u32 = gl::RGBA8;
pub const GL_TEXTURE_EXTERNAL_OES: u32 = 0x8D65;
pub const MAX_PUSH_CONSTANTS_SIZE: u32 = 128;

// `EGL_EXT_image_dma_buf_import`. A Linux GPU stack imports a decoded frame by *file descriptor* —
// the same memory the decoder wrote, with no copy — where Android imports an `AHardwareBuffer`.
// The attribute list is: size, format, then one (fd, offset, pitch) triple per plane.
pub const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
pub const EGL_LINUX_DRM_FOURCC_EXT: u32 = 0x3271;
pub const EGL_DMA_BUF_PLANE0_FD_EXT: u32 = 0x3272;
pub const EGL_DMA_BUF_PLANE0_OFFSET_EXT: u32 = 0x3273;
pub const EGL_DMA_BUF_PLANE0_PITCH_EXT: u32 = 0x3274;
pub const EGL_DMA_BUF_PLANE1_FD_EXT: u32 = 0x3275;
pub const EGL_DMA_BUF_PLANE1_OFFSET_EXT: u32 = 0x3276;
pub const EGL_DMA_BUF_PLANE1_PITCH_EXT: u32 = 0x3277;
pub const EGL_DMA_BUF_PLANE2_FD_EXT: u32 = 0x3278;
pub const EGL_DMA_BUF_PLANE2_OFFSET_EXT: u32 = 0x3279;
pub const EGL_DMA_BUF_PLANE2_PITCH_EXT: u32 = 0x327A;

/// `DRM_FORMAT_NV12` / `NV21`.
///
/// V4L2 and DRM spell these the same way *on purpose*: the encodings are identical (a full-size Y
/// plane followed by an interleaved chroma plane), so a translation table would be a place for a
/// typo to become an image artifact. That is also why the mapping below can be an identity — and
/// why anything else must fail rather than be approximated.
pub const DRM_FORMAT_NV12: u32 = u32::from_le_bytes(*b"NV12");
pub const DRM_FORMAT_NV21: u32 = u32::from_le_bytes(*b"NV21");

/// Translate a V4L2 capture fourcc into the DRM format the EGL importer wants, or `None` if this
/// renderer has no path for it.
///
/// `None` is the honest answer for a tiled or YUV-3-plane format: importing one needs
/// `EGL_EXT_image_dma_buf_import_modifiers` and a modifier the decoder must supply, and a guess
/// here produces a picture that is *wrong* rather than absent. The caller logs it and shows the
/// previous frame.
pub fn drm_fourcc_from_v4l2(v4l2_fourcc: u32) -> Option<u32> {
    match v4l2_fourcc {
        DRM_FORMAT_NV12 | DRM_FORMAT_NV21 => Some(v4l2_fourcc),
        _ => None,
    }
}

/// The extension name as the EGL display spells it in `eglQueryString(EGL_EXTENSIONS)`.
pub const DMA_BUF_IMPORT_EXTENSION: &str = "EGL_EXT_image_dma_buf_import";

/// A decoded frame as a Linux graphics stack receives it.
///
/// The fd names the memory and the layout says how it is arranged inside it. Both are required:
/// a hardware decoder pads each plane to its own alignment, so a renderer that derives its stride
/// from the width produces a picture that shears progressively across the image — a symptom that
/// reads as a shader bug rather than as a wrong number.
#[derive(Debug, Clone, Copy)]
pub struct DmaBufFrame {
    pub width: u32,
    pub height: u32,
    pub drm_fourcc: u32,
    pub planes: u32,
    pub fds: [i32; 2],
    pub offsets: [u32; 2],
    pub strides: [u32; 2],
}

/// What the renderer has to draw this frame, in whatever form the platform takes.
///
/// Handles are held as addresses rather than raw pointers so that a frame can cross a thread
/// boundary — the decoder produces it, the render thread consumes it, and a `*mut c_void` in here
/// would make that illegal for a reason that has nothing to do with either handle. They are cast
/// back at the one call that needs a pointer.
#[derive(Debug, Clone, Copy)]
pub enum NativeFrame {
    /// Nothing this frame: keep showing the previous image. A frame with no importable handle ends
    /// up here, and whatever produced it has already said so in the log.
    None,
    /// Android: an `AHardwareBuffer *`, as an address.
    HardwareBuffer(usize),
    /// Linux: dma-buf file descriptors.
    DmaBuf(DmaBufFrame),
}

type CreateImageFn = unsafe extern "C" fn(
    egl::EGLDisplay,
    egl::EGLContext,
    egl::Enum,
    egl::EGLClientBuffer,
    *const egl::Int,
) -> egl::EGLImage;
type DestroyImageFn = unsafe extern "C" fn(egl::EGLDisplay, egl::EGLImage) -> egl::Boolean;
type GetNativeClientBufferFn = unsafe extern "C" fn(*const c_void) -> egl::EGLClientBuffer;
type ImageTargetTexture2DFn = unsafe extern "C" fn(egl::Enum, egl::EGLImage);

pub struct HandData {
    pub grip_motion: Option<DeviceMotion>,
    pub detached_grip_motion: Option<DeviceMotion>,
    pub skeleton_joints: Option<[Pose; 26]>,
}

pub fn check_error(gl: &gl::Context, message_context: &str) {
    let err = unsafe { gl.get_error() };
    if err != glow::NO_ERROR {
        alvr_common::error!("gl error {message_context} -> {err}");
        std::process::abort();
    }
}

macro_rules! ck {
    ($gl_ctx:ident.$($gl_cmd:tt)*) => {{
        let res = $gl_ctx.$($gl_cmd)*;

        #[cfg(debug_assertions)]
        crate::check_error(&$gl_ctx, &format!("{}:{}: {}", file!(), line!(), stringify!($($gl_cmd)*)));

        res
    }};
}
pub(crate) use ck;

fn projection_from_fov(fov: Fov) -> Mat4 {
    const NEAR: f32 = 0.1;

    let tanl = f32::tan(fov.left);
    let tanr = f32::tan(fov.right);
    let tanu = f32::tan(fov.up);
    let tand = f32::tan(fov.down);
    let a = 2.0 / (tanr - tanl);
    let b = 2.0 / (tanu - tand);
    let c = (tanr + tanl) / (tanr - tanl);
    let d = (tanu + tand) / (tanu - tand);

    // note: for wgpu compatibility, the b and d components should be flipped. Maybe a bug in the
    // viewport handling in wgpu?
    Mat4::from_cols(
        Vec4::new(a, 0.0, c, 0.0),
        Vec4::new(0.0, -b, -d, 0.0),
        Vec4::new(0.0, 0.0, -1.0, -NEAR),
        Vec4::new(0.0, 0.0, -1.0, 0.0),
    )
    .transpose()
}

pub fn choose_swapchain_format(supported_formats: &[u32], enable_hdr: bool) -> u32 {
    // Priority-sorted list of swapchain formats we'll accept--
    let mut app_supported_swapchain_formats = vec![gl::SRGB8_ALPHA8, gl::RGBA8];

    // float16 is required for HDR output. However, float16 swapchains
    // have a high perf cost, so only use these if HDR is enabled.
    if enable_hdr {
        app_supported_swapchain_formats.insert(0, gl::RGBA16F);
    }

    for format in app_supported_swapchain_formats {
        if supported_formats.contains(&format) {
            return format;
        }
    }

    // If we can't enumerate, default to a required format
    gl::RGBA8
}

pub fn gl_format_to_wgpu(format: u32) -> TextureFormat {
    match format {
        gl::SRGB8_ALPHA8 => TextureFormat::Rgba8UnormSrgb,
        gl::RGBA8 => TextureFormat::Rgba8Unorm,
        gl::RGBA16F => TextureFormat::Rgba16Float,
        _ => panic!("Unsupported GL format: {format}"),
    }
}

pub fn create_texture(device: &Device, resolution: UVec2, format: TextureFormat) -> Texture {
    device.create_texture(&TextureDescriptor {
        label: None,
        size: Extent3d {
            width: resolution.x,
            height: resolution.y,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format,
        usage: TextureUsages::COPY_DST | TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    })
}

#[cfg(not(any(target_os = "macos", target_os = "ios")))]
fn create_texture_from_gles(
    device: &Device,
    texture: u32,
    resolution: UVec2,
    format: TextureFormat,
) -> Texture {
    use std::num::NonZeroU32;
    use wgpu::{
        TextureUses,
        hal::{self, MemoryFlags, api},
    };

    let size = Extent3d {
        width: resolution.x,
        height: resolution.y,
        depth_or_array_layers: 1,
    };

    unsafe {
        let hal_texture = device.as_hal::<api::Gles, _, _>(|device| {
            device.unwrap().texture_from_raw(
                NonZeroU32::new(texture).unwrap(),
                &hal::TextureDescriptor {
                    label: None,
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: TextureDimension::D2,
                    format,
                    usage: TextureUses::COLOR_TARGET,
                    memory_flags: MemoryFlags::empty(),
                    view_formats: vec![],
                },
                Some(Box::new(|| ())),
            )
        });

        device.create_texture_from_hal::<api::Gles>(
            hal_texture,
            &TextureDescriptor {
                label: None,
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: TextureDimension::D2,
                format,
                usage: TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            },
        )
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn create_texture_from_gles(_: &Device, _: u32, _: UVec2, _: TextureFormat) -> Texture {
    unimplemented!()
}

// This is used to convert OpenXR swapchains to wgpu
pub fn create_gl_swapchain(
    device: &Device,
    gl_textures: &[u32],
    resolution: UVec2,
    format: TextureFormat,
) -> Vec<TextureView> {
    gl_textures
        .iter()
        .map(|gl_tex| {
            create_texture_from_gles(device, *gl_tex, resolution, format)
                .create_view(&Default::default())
        })
        .collect()
}

pub struct GraphicsContext {
    _instance: Instance,

    #[cfg(not(any(windows, target_os = "macos", target_os = "ios")))]
    adapter: wgpu::Adapter,

    device: Device,
    queue: Queue,
    pub egl_display: egl::Display,
    pub egl_config: egl::Config,
    pub egl_context: egl::Context,
    pub gl_context: gl::Context,

    #[cfg(not(any(windows, target_os = "macos", target_os = "ios")))]
    dummy_surface: egl::Surface,

    create_image: CreateImageFn,
    destroy_image: DestroyImageFn,
    get_native_client_buffer: GetNativeClientBufferFn,
    image_target_texture_2d: ImageTargetTexture2DFn,

    /// Whether this display advertises `EGL_EXT_image_dma_buf_import`. Asked once, at start-up:
    /// see [`GraphicsContext::supports_dma_buf_import`] for why it is worth a field.
    dma_buf_import: bool,
}

impl GraphicsContext {
    #[cfg(not(any(windows, target_os = "macos", target_os = "ios")))]
    pub fn new_gl() -> Self {
        use std::mem;
        use wgpu::{
            Backends, DeviceDescriptor, Features, InstanceDescriptor, InstanceFlags, Limits,
            MemoryHints, Trace, hal::api,
        };

        const CREATE_IMAGE_FN_STR: &str = "eglCreateImageKHR";
        const DESTROY_IMAGE_FN_STR: &str = "eglDestroyImageKHR";
        const GET_NATIVE_CLIENT_BUFFER_FN_STR: &str = "eglGetNativeClientBufferANDROID";
        const IMAGE_TARGET_TEXTURE_2D_FN_STR: &str = "glEGLImageTargetTexture2DOES";

        let flags = if cfg!(debug_assertions) {
            InstanceFlags::DEBUG | InstanceFlags::VALIDATION
        } else {
            InstanceFlags::empty()
        };

        let instance = Instance::new(&InstanceDescriptor {
            backends: Backends::GL,
            flags,
            ..Default::default()
        });

        let adapter = instance.enumerate_adapters(Backends::GL).remove(0);
        let (device, queue) = pollster::block_on(adapter.request_device(&DeviceDescriptor {
            label: None,
            required_features: Features::PUSH_CONSTANTS,
            required_limits: Limits {
                max_push_constant_size: MAX_PUSH_CONSTANTS_SIZE,
                ..adapter.limits()
            },
            memory_hints: MemoryHints::Performance,
            trace: Trace::Off,
        }))
        .unwrap();

        let raw_instance = unsafe { instance.as_hal::<api::Gles>() }.unwrap();

        let egl_display = raw_instance.raw_display();
        let egl_config = raw_instance.egl_config();

        let (
            egl_context,
            gl_context,
            dummy_surface,
            create_image,
            destroy_image,
            get_native_client_buffer,
            image_target_texture_2d,
            dma_buf_import,
        ) = unsafe {
            adapter.as_hal::<api::Gles, _, _>(|raw_adapter| {
                let adapter_context = raw_adapter.unwrap().adapter_context();
                let egl_instance = adapter_context.egl_instance().unwrap();

                let egl_context = egl::Context::from_ptr(adapter_context.raw_context());

                const PBUFFER_ATTRIBS: [i32; 5] = [egl::WIDTH, 16, egl::HEIGHT, 16, egl::NONE];
                let dummy_surface = egl_instance
                    .create_pbuffer_surface(egl_display, egl_config, &PBUFFER_ATTRIBS)
                    .unwrap();

                egl_instance
                    .make_current(
                        egl_display,
                        Some(dummy_surface),
                        Some(dummy_surface),
                        Some(egl_context),
                    )
                    .unwrap();

                let gl_context = gl::Context::from_loader_function(|fn_name| {
                    egl_instance
                        .get_proc_address(fn_name)
                        .map_or(ptr::null(), |f| f as *const c_void)
                });

                let get_fn_ptr = |fn_name| {
                    egl_instance
                        .get_proc_address(fn_name)
                        .map_or(ptr::null(), |f| f as *const c_void)
                };

                let create_image: CreateImageFn = mem::transmute(get_fn_ptr(CREATE_IMAGE_FN_STR));
                let destroy_image: DestroyImageFn =
                    mem::transmute(get_fn_ptr(DESTROY_IMAGE_FN_STR));
                let get_native_client_buffer: GetNativeClientBufferFn =
                    mem::transmute(get_fn_ptr(GET_NATIVE_CLIENT_BUFFER_FN_STR));
                let image_target_texture_2d: ImageTargetTexture2DFn =
                    mem::transmute(get_fn_ptr(IMAGE_TARGET_TEXTURE_2D_FN_STR));

                // Asked here, once, because the alternative is asking per frame and then having to
                // decide what to do about the answer mid-render. A display without it can still
                // run the client; it just cannot show a Linux decoder's frames, and the one place
                // that is knowable is start-up.
                let dma_buf_import = egl_instance
                    .query_string(Some(egl_display), egl::EXTENSIONS)
                    .map(|extensions| {
                        extensions
                            .to_string_lossy()
                            .contains(DMA_BUF_IMPORT_EXTENSION)
                    })
                    .unwrap_or(false);

                (
                    egl_context,
                    gl_context,
                    dummy_surface,
                    create_image,
                    destroy_image,
                    get_native_client_buffer,
                    image_target_texture_2d,
                    dma_buf_import,
                )
            })
        };

        Self {
            _instance: instance,
            adapter,
            device,
            queue,
            egl_display,
            egl_config,
            egl_context,
            gl_context,
            dummy_surface,
            create_image,
            destroy_image,
            get_native_client_buffer,
            image_target_texture_2d,
            dma_buf_import,
        }
    }

    #[cfg(any(windows, target_os = "macos", target_os = "ios"))]
    pub fn new_gl() -> Self {
        unimplemented!()
    }

    pub fn make_current(&self) {
        #[cfg(not(any(windows, target_os = "macos", target_os = "ios")))]
        unsafe {
            self.adapter
                .as_hal::<wgpu::hal::api::Gles, _, _>(|raw_adapter| {
                    let egl_instance = raw_adapter
                        .unwrap()
                        .adapter_context()
                        .egl_instance()
                        .unwrap();

                    egl_instance
                        .make_current(
                            self.egl_display,
                            Some(self.dummy_surface),
                            Some(self.dummy_surface),
                            Some(self.egl_context),
                        )
                        .unwrap();
                })
        };
    }

    /// # Safety
    /// `buffer` must be a valid AHardwareBuffer.
    /// `texture` must be a valid GL texture.
    pub unsafe fn render_ahardwarebuffer_using_texture(
        &self,
        buffer: *const c_void,
        texture: gl::Texture,
        render_cb: impl FnOnce(),
    ) {
        const EGL_NATIVE_BUFFER_ANDROID: u32 = 0x3140;

        if !buffer.is_null() {
            let client_buffer = unsafe { (self.get_native_client_buffer)(buffer) };
            check_error(&self.gl_context, "get_native_client_buffer");

            let image = unsafe {
                (self.create_image)(
                    self.egl_display.as_ptr(),
                    egl::NO_CONTEXT,
                    EGL_NATIVE_BUFFER_ANDROID,
                    client_buffer,
                    ptr::null(),
                )
            };
            check_error(&self.gl_context, "create_image");

            unsafe {
                self.gl_context
                    .bind_texture(GL_TEXTURE_EXTERNAL_OES, Some(texture))
            };
            check_error(&self.gl_context, "bind texture OES");

            unsafe { (self.image_target_texture_2d)(GL_TEXTURE_EXTERNAL_OES, image) };
            check_error(&self.gl_context, "image_target_texture_2d");

            render_cb();

            unsafe { (self.destroy_image)(self.egl_display.as_ptr(), image) };
            check_error(&self.gl_context, "destroy_image");
        }
    }

    /// Whether this display can import a decoded frame by file descriptor.
    ///
    /// Asked once, at start-up, and **not** treated as a nicety: a client whose renderer cannot
    /// import a dma-buf shows nothing at all, and the whole point of asking here is to say that in
    /// one line at start-up rather than to present a black screen forever with no explanation.
    pub fn supports_dma_buf_import(&self) -> bool {
        self.dma_buf_import
    }

    /// Import a decoded frame that lives in a dma-buf, and run `render_cb` with it bound to
    /// `texture`.
    ///
    /// Returns `false` without drawing if the display has no `EGL_EXT_image_dma_buf_import`, if the
    /// pixel format has no import path, or if the driver refuses the image — the three ways this can
    /// legitimately fail. It never falls back to drawing *something*: a wrong picture is worse than
    /// a held one, and it is much harder to diagnose.
    ///
    /// # Safety
    /// `frame.fds` must be open dma-buf file descriptors owned by the caller; they are only read.
    pub unsafe fn render_dma_buf_using_texture(
        &self,
        frame: &DmaBufFrame,
        texture: gl::Texture,
        render_cb: impl FnOnce(),
    ) -> bool {
        if !self.dma_buf_import || frame.fds[0] < 0 {
            return false;
        }

        let mut attribs: Vec<egl::Int> = vec![
            egl::WIDTH,
            frame.width as egl::Int,
            egl::HEIGHT,
            frame.height as egl::Int,
            // `EGL_LINUX_DRM_FOURCC_EXT` is *signed* in the spec's prototype; the fourccs are
            // ASCII and always positive, so the cast is exact.
            EGL_LINUX_DRM_FOURCC_EXT as egl::Int,
            frame.drm_fourcc as egl::Int,
        ];

        // Plane 0's fd/offset/pitch tokens are contiguous, and so are plane 1's. That is not
        // accidental — it is how the extension was written, and it is what the loop below relies
        // on. Listed explicitly rather than computed so a reader does not have to know it.
        const PLANE_TOKENS: [[u32; 3]; 2] = [
            [
                EGL_DMA_BUF_PLANE0_FD_EXT,
                EGL_DMA_BUF_PLANE0_OFFSET_EXT,
                EGL_DMA_BUF_PLANE0_PITCH_EXT,
            ],
            [
                EGL_DMA_BUF_PLANE1_FD_EXT,
                EGL_DMA_BUF_PLANE1_OFFSET_EXT,
                EGL_DMA_BUF_PLANE1_PITCH_EXT,
            ],
        ];

        for (plane, tokens) in PLANE_TOKENS
            .iter()
            .enumerate()
            .take(frame.planes.clamp(1, 2) as usize)
        {
            if frame.fds[plane] < 0 {
                return false;
            }
            attribs.extend([
                tokens[0] as egl::Int,
                frame.fds[plane],
                tokens[1] as egl::Int,
                frame.offsets[plane] as egl::Int,
                tokens[2] as egl::Int,
                frame.strides[plane] as egl::Int,
            ]);
        }
        attribs.push(egl::NONE);

        // SAFETY: the attribute list is well-formed for `EGL_LINUX_DMA_BUF_EXT`, and every fd in it
        // is open for the duration of the call (the caller owns them).
        let image = unsafe {
            (self.create_image)(
                self.egl_display.as_ptr(),
                egl::NO_CONTEXT,
                EGL_LINUX_DMA_BUF_EXT,
                ptr::null_mut(),
                attribs.as_ptr(),
            )
        };
        if image == egl::NO_IMAGE {
            alvr_common::warn!(
                "eglCreateImageKHR refused a {}x{} dma-buf (fourcc {:#x}, {} plane(s))",
                frame.width,
                frame.height,
                frame.drm_fourcc,
                frame.planes
            );
            return false;
        }

        unsafe {
            self.gl_context
                .bind_texture(GL_TEXTURE_EXTERNAL_OES, Some(texture))
        };

        // SAFETY: `image` is a valid EGLImage owned by this call, and the target matches the one the
        // image was created for.
        unsafe { (self.image_target_texture_2d)(GL_TEXTURE_EXTERNAL_OES, image) };

        render_cb();

        // SAFETY: the image is destroyed exactly once, after the draw that used it.
        unsafe { (self.destroy_image)(self.egl_display.as_ptr(), image) };

        true
    }
}

#[cfg(not(windows))]
impl Default for GraphicsContext {
    fn default() -> Self {
        Self::new_gl()
    }
}
