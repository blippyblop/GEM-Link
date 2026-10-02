#pragma once

// GreyProbe — fingerprint the compositor's OWN output texture.
//
// Why this exists: the open question is whether a flat-grey frame reaching the
// client came from upstream (SteamVR / the scene app) or was manufactured inside
// GemLink. Everything GemLink does to a frame happens *after* CopyTexture()
// resolves the app's ID3D11Texture2D from the handle map, so sampling the texture
// there — and nowhere else — draws exactly that line.
//
// It is a measurement, not a fix. It is off unless GEMLINK_GREYPROBE names a CSV
// path, and when off it costs one pointer compare per present.
//
// One row per (present, layer, eye):
//   frameIndex,layer,eye,w,h,format,bpp,mean,std,min,max
// `mean` near 128 with `std` near 0 is a flat mid-grey surface. A structured
// scene gives std in the tens.
//
// See VD_RE/50-grey-frame-experiments.md (E1).

#include "alvr_server/openvr_driver_wrap.h"

#include <algorithm>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>

namespace greyprobe {

inline int bytes_per_pixel(DXGI_FORMAT f) {
    switch (f) {
    case DXGI_FORMAT_R32G8X24_TYPELESS:
    case DXGI_FORMAT_R32G32B32A32_FLOAT:
    case DXGI_FORMAT_R32G32B32A32_TYPELESS:
        return 8;
    case DXGI_FORMAT_R16G16B16A16_FLOAT:
    case DXGI_FORMAT_R16G16B16A16_TYPELESS:
        return 8;
    case DXGI_FORMAT_R10G10B10A2_TYPELESS:
    case DXGI_FORMAT_R10G10B10A2_UNORM:
        return 4;
    case DXGI_FORMAT_NV12:
    case DXGI_FORMAT_NV11:
        return 1; // luma plane only; the chroma plane is interleaved below it
    default:
        return 4; // 8-bit RGBA/BGRA family, which is what SteamVR apps use
    }
}

// TYPELESS formats cannot be mapped, so pick a concrete one for the staging copy.
inline DXGI_FORMAT concrete(DXGI_FORMAT f) {
    switch (f) {
    case DXGI_FORMAT_R8G8B8A8_TYPELESS:
        return DXGI_FORMAT_R8G8B8A8_UNORM;
    case DXGI_FORMAT_B8G8R8A8_TYPELESS:
        return DXGI_FORMAT_B8G8R8A8_UNORM;
    case DXGI_FORMAT_R10G10B10A2_TYPELESS:
        return DXGI_FORMAT_R10G10B10A2_UNORM;
    case DXGI_FORMAT_R16G16B16A16_TYPELESS:
        return DXGI_FORMAT_R16G16B16A16_FLOAT;
    default:
        return f;
    }
}

class Probe {
public:
    static Probe &Instance() {
        static Probe instance;
        return instance;
    }

    bool Enabled() const {
        return m_enabled;
    }

    void Sample(
        ID3D11Device *device,
        ID3D11DeviceContext *context,
        ID3D11Texture2D *texture,
        uint64_t frameIndex,
        int layer,
        int eye,
        const char *stage
    ) {
        if (!m_enabled || !texture || !device || !context)
            return;

        D3D11_TEXTURE2D_DESC desc{};
        texture->GetDesc(&desc);

        const UINT w = std::min<UINT>(desc.Width, 256);
        const UINT h = std::min<UINT>(desc.Height, 256);
        DXGI_FORMAT format = concrete(desc.Format);
        const int bpp = bytes_per_pixel(format);
        if (w == 0 || h == 0 || bpp < 1 || bpp > 8)
            return;

        if (!m_staging || m_w != w || m_h != h || m_format != format) {
            m_staging.Reset();
            D3D11_TEXTURE2D_DESC sd{};
            sd.Width = w;
            sd.Height = h;
            sd.MipLevels = 1;
            sd.ArraySize = 1;
            sd.Format = format;
            sd.SampleDesc.Count = 1;
            sd.Usage = D3D11_USAGE_STAGING;
            sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
            if (FAILED(device->CreateTexture2D(&sd, nullptr, &m_staging))) {
                Warn("greyprobe: staging texture creation failed, disabling");
                m_enabled = false;
                return;
            }
            m_w = w;
            m_h = h;
            m_format = format;
            std::fprintf(
                m_file,
                "stage,frameIndex,layer,eye,w,h,texW,texH,format,bpp,mean,std,min,max\n"
            );
        }

        // Sample the middle of the texture. A corner can legitimately be flat (or
        // hidden-area) while the frame is not, which would fake a positive.
        D3D11_BOX box{};
        box.left = (desc.Width - w) / 2;
        box.top = (desc.Height - h) / 2;
        box.front = 0;
        box.right = box.left + w;
        box.bottom = box.top + h;
        box.back = 1;
        context->CopySubresourceRegion(m_staging.Get(), 0, 0, 0, 0, texture, 0, &box);

        D3D11_MAPPED_SUBRESOURCE mapped{};
        if (FAILED(context->Map(m_staging.Get(), 0, D3D11_MAP_READ, 0, &mapped)))
            return;

        const uint8_t *base = static_cast<const uint8_t *>(mapped.pData);
        uint64_t sum = 0;
        uint64_t sumSq = 0;
        uint64_t n = 0;
        uint8_t lo = 255;
        uint8_t hi = 0;
        // Never read past the row pitch: NV12 and small textures have less than
        // width*bpp of usable bytes per row.
        const UINT rowBytes = std::min<UINT>(w * static_cast<UINT>(bpp), mapped.RowPitch);
        for (UINT y = 0; y < h; ++y) {
            const uint8_t *row = base + static_cast<size_t>(y) * mapped.RowPitch;
            for (UINT x = 0; x < rowBytes; ++x) {
                const uint8_t v = row[x];
                sum += v;
                sumSq += static_cast<uint64_t>(v) * v;
                ++n;
                lo = std::min(lo, v);
                hi = std::max(hi, v);
            }
        }
        context->Unmap(m_staging.Get(), 0);
        if (n == 0)
            return;

        const double mean = static_cast<double>(sum) / n;
        const double variance = static_cast<double>(sumSq) / n - mean * mean;
        const double stddev = variance > 0.0 ? std::sqrt(variance) : 0.0;

        std::fprintf(
            m_file,
            "%s,%llu,%d,%d,%u,%u,%u,%u,%d,%d,%.3f,%.3f,%u,%u\n",
            stage,
            static_cast<unsigned long long>(frameIndex),
            layer,
            eye,
            w,
            h,
            desc.Width,
            desc.Height,
            static_cast<int>(format),
            bpp,
            mean,
            stddev,
            lo,
            hi
        );
        std::fflush(m_file);
    }

private:
    Probe() {
        const char *path = std::getenv("GEMLINK_GREYPROBE");
        if (!path || !*path)
            return;
        m_file = std::fopen(path, "a");
        if (!m_file) {
            Warn("greyprobe: cannot open %s", path);
            return;
        }
        m_enabled = true;
        Debug("greyprobe: sampling compositor output textures -> %s", path);
    }

    bool m_enabled = false;
    std::FILE *m_file = nullptr;
    ComPtr<ID3D11Texture2D> m_staging;
    UINT m_w = 0;
    UINT m_h = 0;
    DXGI_FORMAT m_format = DXGI_FORMAT_UNKNOWN;
};

} // namespace greyprobe
