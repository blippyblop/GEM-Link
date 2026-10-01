//! Real NVENC video source for pcsim (Windows only).
//!
//! This is `nvenc_probe`'s synthetic "feeder" path packaged for the sim rig: a
//! standalone D3D11 device feeds NVENC textures directly, no desktop capture.
//! That deliberately decouples two risks — "is real encoded video flowing into
//! the sim" and "does DDA work on this box" — and it is the right shape anyway,
//! because the encoder must be fed **VR-resolution** frames, not the desktop's.
//!
//! The resolution is a parameter, never a constant: render resolution is
//! negotiable and must be allowed to change (ADR-0008 / the sim's contract).

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use x_nvenc::NvEncoder;

pub struct Feeder {
    /// Kept alive: the encoder holds a reference to this device.
    _device: ID3D11Device,
    pool: Vec<ID3D11Texture2D>,
    encoder: NvEncoder,
    pitch: u32,
    idx: usize,
    submitted: u64,
}

impl Feeder {
    pub fn new(width: u32, height: u32, fps: u32) -> Result<Self, String> {
        let (device, _context) = standalone_device()?;

        // BGRA8 for now: the Frame's 10-bit support is a known mystery (the
        // firmware suggests it can, but it does not appear to display it), so
        // stay 8-bit until everything else is done.
        let pool = create_pool(&device, width, height)?;

        let encoder = NvEncoder::new(unsafe { device.as_raw() }, width, height, fps)
            .map_err(|e| format!("nvenc session: {e}"))?;

        Ok(Self {
            _device: device,
            pool,
            encoder,
            pitch: width * 4,
            idx: 0,
            submitted: 0,
        })
    }

    /// Encode one frame and return its bitstream.
    pub fn encode_next(&mut self) -> Result<Vec<u8>, String> {
        let texture = &self.pool[self.idx % self.pool.len()];
        self.idx += 1;
        self.submitted += 1;
        self.encoder
            .encode(texture.as_raw(), self.pitch)
            .map_err(|e| format!("nvenc encode: {e}"))
    }

    pub fn submitted(&self) -> u64 {
        self.submitted
    }
}

fn standalone_device() -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
    unsafe {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            None,
            windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            None,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .map_err(|e| format!("D3D11CreateDevice failed: {e}"))?;
        Ok((device.ok_or("no device")?, context.ok_or("no context")?))
    }
}

fn create_pool(
    device: &ID3D11Device,
    w: u32,
    h: u32,
) -> Result<Vec<ID3D11Texture2D>, String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut pool = Vec::new();
    for _ in 0..2 {
        let mut tex: Option<ID3D11Texture2D> = None;
        unsafe {
            device
                .CreateTexture2D(&desc, None, Some(&mut tex))
                .map_err(|e| format!("CreateTexture2D failed: {e}"))?;
        }
        pool.push(tex.ok_or("CreateTexture2D returned no texture")?);
    }
    Ok(pool)
}
