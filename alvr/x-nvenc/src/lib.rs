//! x-nvenc — GemLink's NVENC codec plug-in (Phase 1).
//!
//! Talks to the NVENC runtime (`nvEncodeAPI64.dll`, shipped with the GPU
//! driver) directly: open session on the capture's D3D11 device, register our
//! GPU-resident BGRA8 textures, map, encode, and pull the bitstream — all
//! without a single CPU pixel copy.
//!
//! ABI: the structs below were transcribed against the official Video Codec
//! SDK 13.1 headers and validated with a compiled probe (sizes and offsets
//! asserted at compile time). All `unsafe` lives in this module.

#![allow(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::ffi::c_void;

// ---------------------------------------------------------------------------
// Constants (measured against SDK 13.1)

const NVENCAPI_VERSION: u32 = 0x0100_000D; // major 13 | minor 1 << 24
const NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER: u32 = 0x7101_000D;
const NV_ENCODE_API_FUNCTION_LIST_VER: u32 = 0x7102_000D;
const NV_ENC_INITIALIZE_PARAMS_VER: u32 = 0xF107_000D;
const NV_ENC_REGISTER_RESOURCE_VER: u32 = 0x7105_000D;
const NV_ENC_MAP_INPUT_RESOURCE_VER: u32 = 0x7104_000D;
const NV_ENC_CREATE_BITSTREAM_BUFFER_VER: u32 = 0x7101_000D;
const NV_ENC_PIC_PARAMS_VER: u32 = 0xF107_000D;
const NV_ENC_LOCK_BITSTREAM_VER: u32 = 0xF102_000D;

const NV_ENC_DEVICE_TYPE_DIRECTX: u32 = 0x0;
const NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX: u32 = 0x0;
/// Matches DXGI_FORMAT_B8G8R8A8_UNORM — our DDA/pool texture format.
const NV_ENC_BUFFER_FORMAT_ARGB: u32 = 0x0100_0000;
const NV_ENC_PIC_STRUCT_FRAME: u32 = 0x1;
const NV_ENC_PIC_FLAG_FORCEIDR: u32 = 0x2;
const NV_ENC_PIC_FLAG_OUTPUT_SPSPPS: u32 = 0x4;
const NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY: u32 = 0x3;
const NV_ENC_BUFFER_USAGE_INPUT_IMAGE: u32 = 0x0;

const CODEC_HEVC_GUID: [u8; 16] = [
    0x88, 0xDC, 0x0C, 0x79, 0x22, 0x45, 0x7B, 0x4D, 0x94, 0x25, 0xBD, 0xA9, 0x97, 0x5F, 0x76, 0x03,
];
const PRESET_P4_GUID: [u8; 16] = [
    0x26, 0xB8, 0xA7, 0x90, 0x06, 0xDF, 0x62, 0x48, 0xB9, 0xD2, 0xCD, 0x6D, 0x73, 0xA0, 0x86, 0x81,
];

const NV_OK_OR_AGAIN: i32 = 0; // NV_ENC_SUCCESS

// ---------------------------------------------------------------------------
// ABI structs — layout asserted against the compiled probe.

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Guid {
    pub bytes: [u8; 16],
}

#[repr(C)]
pub struct OpenEncodeSessionExParams {
    pub version: u32,
    pub device_type: u32,
    pub device: *mut c_void,
    pub reserved: *mut c_void,
    pub api_version: u32,
    pub reserved1: [u32; 253],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
pub struct InitializeParams {
    pub version: u32,
    pub encode_guid: Guid,
    pub preset_guid: Guid,
    pub encode_width: u32,
    pub encode_height: u32,
    pub dar_width: u32,
    pub dar_height: u32,
    pub frame_rate_num: u32,
    pub frame_rate_den: u32,
    pub enable_encode_async: u32,
    pub enable_ptd: u32,
    /// reportSliceOffsets / subFrameWrite / extMEHints / MEOnly /
    /// weightedPrediction / splitEncodeMode:4 / outputInVidmem /
    /// reconFrameOutput / outputStats / uniDirB / reserved — all zero.
    pub bitfields: u32,
    pub priv_data_size: u32,
    pub reserved: u32,
    pub priv_data: *mut c_void,
    /// NULL → driver uses the preset configuration verbatim.
    pub encode_config: *mut c_void,
    pub max_encode_width: u32,
    pub max_encode_height: u32,
    pub max_me_hint_counts_per_block: [u32; 8],
    pub tuning_info: u32,
    pub buffer_format: u32,
    pub num_state_buffers: u32,
    pub output_stats_level: u32,
    pub reserved1: [u32; 284],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
pub struct RegisterResource {
    pub version: u32,
    pub resource_type: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub sub_resource_index: u32,
    pub resource_to_register: *mut c_void,
    pub registered_resource: *mut c_void,
    pub buffer_format: u32,
    pub buffer_usage: u32,
    pub p_input_fence_point: *mut c_void,
    pub chroma_offset: [u32; 2],
    pub chroma_offset_in: [u32; 2],
    pub reserved1: [u32; 244],
    pub reserved2: [*mut c_void; 61],
}

#[repr(C)]
pub struct MapInputResource {
    pub version: u32,
    pub sub_resource_index: u32,
    pub input_resource: *mut c_void,
    pub registered_resource: *mut c_void,
    pub mapped_resource: *mut c_void,
    pub mapped_buffer_fmt: u32,
    pub reserved1: [u32; 251],
    pub reserved2: [*mut c_void; 63],
}

#[repr(C)]
pub struct CreateBitstreamBuffer {
    pub version: u32,
    pub reserved: u32,
    pub _pad: *mut c_void,
    pub bitstream_buffer: *mut c_void,
    pub bitstream_buffer_ptr: *mut c_void,
    pub reserved1: [u32; 58],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
pub struct PicParams {
    pub version: u32,
    pub input_width: u32,
    pub input_height: u32,
    pub input_pitch: u32,
    pub encode_pic_flags: u32,
    pub frame_idx: u32,
    _pad: [u32; 4],
    pub input_buffer: *mut c_void,
    pub output_bitstream: *mut c_void,
    pub completion_event: *mut c_void,
    pub buffer_fmt: u32,
    pub picture_struct: u32,
    pub picture_type: u32,
    /// codec-specific union — zeroed (driver defaults).
    pub codec_pic_params: [u32; 821],
}

#[repr(C)]
pub struct LockBitstream {
    pub version: u32,
    /// doNotWait / ltrFrame / getRCStats — all zero.
    pub bitfields: u32,
    pub output_bitstream: *mut c_void,
    pub slice_offsets: *mut c_void,
    pub frame_idx: u32,
    pub hw_encode_status: u32,
    pub num_slices: u32,
    pub bitstream_size_in_bytes: u32,
    pub _pad1: u32,
    pub _pad2: u32,
    pub _pad3: *mut c_void,
    pub bitstream_buffer_ptr: *mut c_void,
    pub picture_type: u32,
    pub picture_struct: u32,
    _tail: [u32; 368],
}

// Compile-time layout verification against the compiled probe measurements.
const _: () = {
    macro_rules! assert_size {
        ($t:ty, $bytes:expr) => {
            assert!(core::mem::size_of::<$t>() == $bytes);
        };
    }
    assert_size!(OpenEncodeSessionExParams, 1552);
    assert_size!(InitializeParams, 1800);
    assert_size!(RegisterResource, 1536);
    assert_size!(MapInputResource, 1544);
    assert_size!(CreateBitstreamBuffer, 776);
    assert_size!(PicParams, 3360);
    assert_size!(LockBitstream, 1544);
};

// ---------------------------------------------------------------------------
// Function table — SDK 13.1 slot order; unused slots stay null.

#[repr(C)]
pub struct FunctionList {
    pub version: u32,
    pub reserved: u32,
    pub nv_enc_open_encode_session: *mut c_void,
    pub nv_enc_get_encode_guid_count: *mut c_void,
    pub nv_enc_get_encode_profile_guid_count: *mut c_void,
    pub nv_enc_get_encode_profile_guids: *mut c_void,
    pub nv_enc_get_encode_guids: *mut c_void,
    pub nv_enc_get_input_format_count: *mut c_void,
    pub nv_enc_get_input_formats: *mut c_void,
    pub nv_enc_get_encode_caps: *mut c_void,
    pub nv_enc_get_encode_preset_count: *mut c_void,
    pub nv_enc_get_encode_preset_guids: *mut c_void,
    pub nv_enc_get_encode_preset_config: *mut c_void,
    pub nv_enc_initialize_encoder: Option<NvEncInitializeEncoderFn>,
    pub nv_enc_create_input_buffer: *mut c_void,
    pub nv_enc_destroy_input_buffer: *mut c_void,
    pub nv_enc_create_bitstream_buffer: Option<NvEncCreateBitstreamBufferFn>,
    pub nv_enc_destroy_bitstream_buffer: Option<NvEncDestroyBitstreamBufferFn>,
    pub nv_enc_encode_picture: Option<NvEncEncodePictureFn>,
    pub nv_enc_lock_bitstream: Option<NvEncLockBitstreamFn>,
    pub nv_enc_unlock_bitstream: Option<NvEncUnlockBitstreamFn>,
    pub nv_enc_lock_input_buffer: *mut c_void,
    pub nv_enc_unlock_input_buffer: *mut c_void,
    pub nv_enc_get_encode_stats: *mut c_void,
    pub nv_enc_get_sequence_params: *mut c_void,
    pub nv_enc_register_async_event: *mut c_void,
    pub nv_enc_unregister_async_event: *mut c_void,
    pub nv_enc_map_input_resource: Option<NvEncMapInputResourceFn>,
    pub nv_enc_unmap_input_resource: Option<NvEncUnmapInputResourceFn>,
    pub nv_enc_destroy_encoder: Option<NvEncDestroyEncoderFn>,
    pub nv_enc_invalidate_ref_frames: *mut c_void,
    pub nv_enc_open_encode_session_ex: Option<NvEncOpenEncodeSessionExFn>,
    pub nv_enc_register_resource: Option<NvEncRegisterResourceFn>,
    pub nv_enc_unregister_resource: Option<NvEncUnregisterResourceFn>,
    pub nv_enc_reconfigure_encoder: *mut c_void,
    pub reserved1: *mut c_void,
    pub nv_enc_create_mv_buffer: *mut c_void,
    pub nv_enc_destroy_mv_buffer: *mut c_void,
    pub nv_enc_run_motion_estimation_only: *mut c_void,
    pub nv_enc_get_encode_preset_config_ex: *mut c_void,
    pub nv_enc_get_sequence_param_ex: *mut c_void,
    pub nv_enc_restore_encoder_state: *mut c_void,
    pub nv_enc_lookahead_picture: *mut c_void,
    pub reserved2: [*mut c_void; 275],
}

type Status = i32;
pub type NvEncOpenEncodeSessionExFn =
    unsafe extern "system" fn(*mut OpenEncodeSessionExParams, *mut *mut c_void) -> Status;
pub type NvEncInitializeEncoderFn =
    unsafe extern "system" fn(*mut c_void, *mut InitializeParams) -> Status;
pub type NvEncCreateBitstreamBufferFn =
    unsafe extern "system" fn(*mut c_void, *mut CreateBitstreamBuffer) -> Status;
pub type NvEncDestroyBitstreamBufferFn =
    unsafe extern "system" fn(*mut c_void, *mut c_void) -> Status;
pub type NvEncEncodePictureFn = unsafe extern "system" fn(*mut c_void, *mut PicParams) -> Status;
pub type NvEncLockBitstreamFn =
    unsafe extern "system" fn(*mut c_void, *mut LockBitstream) -> Status;
pub type NvEncUnlockBitstreamFn = unsafe extern "system" fn(*mut c_void, *mut c_void) -> Status;
pub type NvEncMapInputResourceFn =
    unsafe extern "system" fn(*mut c_void, *mut MapInputResource) -> Status;
pub type NvEncUnmapInputResourceFn = unsafe extern "system" fn(*mut c_void, *mut c_void) -> Status;
pub type NvEncDestroyEncoderFn = unsafe extern "system" fn(*mut c_void) -> Status;
pub type NvEncRegisterResourceFn =
    unsafe extern "system" fn(*mut c_void, *mut RegisterResource) -> Status;
pub type NvEncUnregisterResourceFn = unsafe extern "system" fn(*mut c_void, *mut c_void) -> Status;

unsafe extern "system" {
    #[link_name = "NvEncodeAPICreateInstance"]
    fn nv_enc_api_create_instance(function_list: *mut FunctionList) -> Status;
}

fn load_runtime() -> Result<*mut FunctionList, String> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    }
    unsafe {
        let name: Vec<u16> = "nvEncodeAPI64.dll\0".encode_utf16().collect();
        let module = LoadLibraryW(name.as_ptr());
        if module.is_null() {
            return Err("nvEncodeAPI64.dll not found (is the NVIDIA driver installed?)".into());
        }
        let create = GetProcAddress(module, b"NvEncodeAPICreateInstance\0".as_ptr());
        if create.is_null() {
            return Err("NvEncodeAPICreateInstance not found in runtime".into());
        }
        let list = Box::leak(Box::new(
            // zeroed with version set; the runtime fills every slot
            FunctionList {
                version: NV_ENCODE_API_FUNCTION_LIST_VER,
                reserved: 0,
                nv_enc_open_encode_session: std::ptr::null_mut(),
                nv_enc_get_encode_guid_count: std::ptr::null_mut(),
                nv_enc_get_encode_profile_guid_count: std::ptr::null_mut(),
                nv_enc_get_encode_profile_guids: std::ptr::null_mut(),
                nv_enc_get_encode_guids: std::ptr::null_mut(),
                nv_enc_get_input_format_count: std::ptr::null_mut(),
                nv_enc_get_input_formats: std::ptr::null_mut(),
                nv_enc_get_encode_caps: std::ptr::null_mut(),
                nv_enc_get_encode_preset_count: std::ptr::null_mut(),
                nv_enc_get_encode_preset_guids: std::ptr::null_mut(),
                nv_enc_get_encode_preset_config: std::ptr::null_mut(),
                nv_enc_initialize_encoder: None,
                nv_enc_create_input_buffer: std::ptr::null_mut(),
                nv_enc_destroy_input_buffer: std::ptr::null_mut(),
                nv_enc_create_bitstream_buffer: None,
                nv_enc_destroy_bitstream_buffer: None,
                nv_enc_encode_picture: None,
                nv_enc_lock_bitstream: None,
                nv_enc_unlock_bitstream: None,
                nv_enc_lock_input_buffer: std::ptr::null_mut(),
                nv_enc_unlock_input_buffer: std::ptr::null_mut(),
                nv_enc_get_encode_stats: std::ptr::null_mut(),
                nv_enc_get_sequence_params: std::ptr::null_mut(),
                nv_enc_register_async_event: std::ptr::null_mut(),
                nv_enc_unregister_async_event: std::ptr::null_mut(),
                nv_enc_map_input_resource: None,
                nv_enc_unmap_input_resource: None,
                nv_enc_destroy_encoder: None,
                nv_enc_invalidate_ref_frames: std::ptr::null_mut(),
                nv_enc_open_encode_session_ex: None,
                nv_enc_register_resource: None,
                nv_enc_unregister_resource: None,
                nv_enc_reconfigure_encoder: std::ptr::null_mut(),
                reserved1: std::ptr::null_mut(),
                nv_enc_create_mv_buffer: std::ptr::null_mut(),
                nv_enc_destroy_mv_buffer: std::ptr::null_mut(),
                nv_enc_run_motion_estimation_only: std::ptr::null_mut(),
                nv_enc_get_encode_preset_config_ex: std::ptr::null_mut(),
                nv_enc_get_sequence_param_ex: std::ptr::null_mut(),
                nv_enc_restore_encoder_state: std::ptr::null_mut(),
                nv_enc_lookahead_picture: std::ptr::null_mut(),
                reserved2: [std::ptr::null_mut(); 275],
            },
        ));
        let create_fn: unsafe extern "system" fn(*mut FunctionList) -> Status =
            std::mem::transmute(create);
        let status = create_fn(list);
        if status != NV_OK_OR_AGAIN {
            return Err(format!("NvEncodeAPICreateInstance failed: {status}"));
        }
        Ok(list)
    }
}

// ---------------------------------------------------------------------------
// Encoder — session lifecycle + per-frame register/map/encode on D3D11 textures

pub struct NvEncoder {
    list: *mut FunctionList,
    session: *mut c_void,
    bitstream: *mut c_void,
    width: u32,
    height: u32,
    registered: Vec<(*mut c_void, *mut c_void)>,
    frame_idx: u32,
}

impl NvEncoder {
    /// `device` is the capture pipeline's ID3D11Device (as raw pointer).
    pub fn new(device: *mut c_void, width: u32, height: u32, fps: u32) -> Result<Self, String> {
        let list = load_runtime()?;
        unsafe {
            let mut session: *mut c_void = std::ptr::null_mut();
            let mut open = OpenEncodeSessionExParams {
                version: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
                device_type: NV_ENC_DEVICE_TYPE_DIRECTX,
                device,
                reserved: std::ptr::null_mut(),
                api_version: NVENCAPI_VERSION,
                reserved1: [0; 253],
                reserved2: [std::ptr::null_mut(); 64],
            };
            let open_fn: NvEncOpenEncodeSessionExFn =
                (*list).nv_enc_open_encode_session_ex.expect("slot");
            let status = open_fn(&mut open, &mut session);
            if status != NV_OK_OR_AGAIN {
                return Err(format!("OpenEncodeSessionEx failed: {status}"));
            }

            let mut init = InitializeParams {
                version: NV_ENC_INITIALIZE_PARAMS_VER,
                encode_guid: Guid {
                    bytes: CODEC_HEVC_GUID,
                },
                preset_guid: Guid {
                    bytes: PRESET_P4_GUID,
                },
                encode_width: width,
                encode_height: height,
                dar_width: width,
                dar_height: height,
                frame_rate_num: fps,
                frame_rate_den: 1,
                enable_encode_async: 0,
                enable_ptd: 1,
                bitfields: 0,
                priv_data_size: 0,
                reserved: 0,
                priv_data: std::ptr::null_mut(),
                encode_config: std::ptr::null_mut(), // preset defaults
                max_encode_width: width,
                max_encode_height: height,
                max_me_hint_counts_per_block: [0; 8],
                tuning_info: NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                buffer_format: NV_ENC_BUFFER_FORMAT_ARGB,
                num_state_buffers: 0,
                output_stats_level: 0,
                reserved1: [0; 284],
                reserved2: [std::ptr::null_mut(); 64],
            };
            let init_fn: NvEncInitializeEncoderFn =
                (*list).nv_enc_initialize_encoder.expect("slot");
            let status = init_fn(session, &mut init);
            if status != NV_OK_OR_AGAIN {
                return Err(format!("InitializeEncoder failed: {status}"));
            }

            let mut bitstream = CreateBitstreamBuffer {
                version: NV_ENC_CREATE_BITSTREAM_BUFFER_VER,
                reserved: 0,
                _pad: std::ptr::null_mut(),
                bitstream_buffer: std::ptr::null_mut(),
                bitstream_buffer_ptr: std::ptr::null_mut(),
                reserved1: [0; 58],
                reserved2: [std::ptr::null_mut(); 64],
            };
            let bs_fn: NvEncCreateBitstreamBufferFn =
                (*list).nv_enc_create_bitstream_buffer.expect("slot");
            let status = bs_fn(session, &mut bitstream);
            if status != NV_OK_OR_AGAIN {
                return Err(format!("CreateBitstreamBuffer failed: {status}"));
            }

            Ok(Self {
                list,
                session,
                bitstream: bitstream.bitstream_buffer,
                width,
                height,
                registered: Vec::new(),
                frame_idx: 0,
            })
        }
    }

    /// Register a GPU texture once; subsequent frames reuse the registration.
    fn register(&mut self, texture: *mut c_void, pitch: u32) -> Result<*mut c_void, String> {
        if let Some((_, reg)) = self.registered.iter().find(|(t, _)| *t == texture) {
            return Ok(*reg);
        }
        unsafe {
            let mut rr = RegisterResource {
                version: NV_ENC_REGISTER_RESOURCE_VER,
                resource_type: NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX,
                width: self.width,
                height: self.height,
                pitch,
                sub_resource_index: 0,
                resource_to_register: texture,
                registered_resource: std::ptr::null_mut(),
                buffer_format: NV_ENC_BUFFER_FORMAT_ARGB,
                buffer_usage: NV_ENC_BUFFER_USAGE_INPUT_IMAGE,
                p_input_fence_point: std::ptr::null_mut(),
                chroma_offset: [0; 2],
                chroma_offset_in: [0; 2],
                reserved1: [0; 244],
                reserved2: [std::ptr::null_mut(); 61],
            };
            let reg_fn: NvEncRegisterResourceFn =
                (*self.list).nv_enc_register_resource.expect("slot");
            let status = reg_fn(self.session, &mut rr);
            if status != NV_OK_OR_AGAIN {
                return Err(format!("RegisterResource failed: {status}"));
            }
            self.registered.push((texture, rr.registered_resource));
            Ok(rr.registered_resource)
        }
    }

    /// Encode one GPU-resident frame; returns the encoded bitstream bytes.
    pub fn encode(&mut self, texture: *mut c_void, pitch: u32) -> Result<Vec<u8>, String> {
        unsafe {
            let registered = self.register(texture, pitch)?;

            let mut map = MapInputResource {
                version: NV_ENC_MAP_INPUT_RESOURCE_VER,
                sub_resource_index: 0,
                input_resource: texture,
                registered_resource: registered,
                mapped_resource: std::ptr::null_mut(),
                mapped_buffer_fmt: 0,
                reserved1: [0; 251],
                reserved2: [std::ptr::null_mut(); 63],
            };
            let map_fn: NvEncMapInputResourceFn =
                (*self.list).nv_enc_map_input_resource.expect("slot");
            if map_fn(self.session, &mut map) != NV_OK_OR_AGAIN {
                return Err("MapInputResource failed".into());
            }

            let mut pic = PicParams {
                version: NV_ENC_PIC_PARAMS_VER,
                input_width: self.width,
                input_height: self.height,
                input_pitch: pitch,
                encode_pic_flags: if self.frame_idx == 0 {
                    NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS
                } else {
                    0
                },
                frame_idx: self.frame_idx,
                _pad: [0; 4],
                input_buffer: map.mapped_resource,
                output_bitstream: self.bitstream,
                completion_event: std::ptr::null_mut(),
                buffer_fmt: NV_ENC_BUFFER_FORMAT_ARGB,
                picture_struct: NV_ENC_PIC_STRUCT_FRAME,
                picture_type: 0,
                codec_pic_params: [0; 821],
            };
            let enc_fn: NvEncEncodePictureFn = (*self.list).nv_enc_encode_picture.expect("slot");
            let status = enc_fn(self.session, &mut pic);
            let _ = {
                let unmap_fn: NvEncUnmapInputResourceFn =
                    (*self.list).nv_enc_unmap_input_resource.expect("slot");
                unmap_fn(self.session, map.mapped_resource)
            };
            if status != NV_OK_OR_AGAIN {
                return Err(format!("EncodePicture failed: {status}"));
            }

            let mut lock = LockBitstream {
                version: NV_ENC_LOCK_BITSTREAM_VER,
                bitfields: 0,
                output_bitstream: self.bitstream,
                slice_offsets: std::ptr::null_mut(),
                frame_idx: 0,
                hw_encode_status: 0,
                num_slices: 0,
                bitstream_size_in_bytes: 0,
                _pad1: 0,
                _pad2: 0,
                _pad3: std::ptr::null_mut(),
                bitstream_buffer_ptr: std::ptr::null_mut(),
                picture_type: 0,
                picture_struct: 0,
                _tail: [0; 368],
            };
            let lock_fn: NvEncLockBitstreamFn = (*self.list).nv_enc_lock_bitstream.expect("slot");
            if lock_fn(self.session, &mut lock) != NV_OK_OR_AGAIN {
                return Err("LockBitstream failed".into());
            }
            let bytes = std::slice::from_raw_parts(
                lock.bitstream_buffer_ptr as *const u8,
                lock.bitstream_size_in_bytes as usize,
            )
            .to_vec();
            let unlock_fn: NvEncUnlockBitstreamFn =
                (*self.list).nv_enc_unlock_bitstream.expect("slot");
            unlock_fn(self.session, self.bitstream);

            self.frame_idx += 1;
            Ok(bytes)
        }
    }

    pub fn frames_encoded(&self) -> u32 {
        self.frame_idx
    }
}

impl Drop for NvEncoder {
    fn drop(&mut self) {
        unsafe {
            let unreg_fn: NvEncUnregisterResourceFn =
                (*self.list).nv_enc_unregister_resource.expect("slot");
            for (_, reg) in &self.registered {
                let _ = unreg_fn(self.session, *reg);
            }
            let destroy_bs_fn: NvEncDestroyBitstreamBufferFn =
                (*self.list).nv_enc_destroy_bitstream_buffer.expect("slot");
            let _ = destroy_bs_fn(self.session, self.bitstream);
            let destroy_fn: NvEncDestroyEncoderFn =
                (*self.list).nv_enc_destroy_encoder.expect("slot");
            let _ = destroy_fn(self.session);
        }
    }
}

/// Encoded-frame statistics for the bench (schema-friendly).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EncodeStats {
    pub frames: u32,
    pub mean_encode_ms: f64,
    pub p95_encode_ms: f64,
    pub max_encode_ms: f64,
    pub mean_bitstream_bytes: f64,
}

pub fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[idx - 1]
}
