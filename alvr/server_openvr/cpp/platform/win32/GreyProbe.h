#pragma once

// GreyProbe — dump what each stage of GemLink's frame path actually contains.
//
// Why this exists: the open question is which hand-off turns the compositor's
// perfectly good frame into the flat mid-grey that reaches the client. Numbers
// were tried first and they misled (see VD_RE/50-grey-frame-experiments.md, E7):
// a probe that averaged bytes looked plausible while disagreeing with the client.
// So this writes **images**, one per stage per frame, which cannot be argued with.
//
// Stages, in pipeline order:
//   src    the compositor's own texture            (OvrDirectModeComponent::CopyTexture)
//   comp   the composition render target            (end of the layer draws)
//   ffr    the foveation pass output                (after m_ffr->Render)
//   yuv    the YUV/NV12 convert output              (after m_yuvPipeline->Render)
//   final  m_pStagingTexture, what NVENC is handed  (before RenderFrame returns)
//
// Off unless GEMLINK_GREYDIR names a directory. PNGs are written with stored
// (uncompressed) deflate blocks so there is no zlib dependency to add; they are
// large but open anywhere.
//
//   GEMLINK_GREYDIR   directory for images (and implies the CSV is written there)
//   GEMLINK_GREYMAX   max images to write, total (default 120)

#include "alvr_server/openvr_driver_wrap.h"

#include <algorithm>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

namespace greyprobe {

inline int bytes_per_pixel(DXGI_FORMAT f) {
    switch (f) {
    case DXGI_FORMAT_R32G32B32A32_FLOAT:
    case DXGI_FORMAT_R32G32B32A32_TYPELESS:
        return 16;
    case DXGI_FORMAT_R16G16B16A16_FLOAT:
    case DXGI_FORMAT_R16G16B16A16_TYPELESS:
        return 8;
    case DXGI_FORMAT_R10G10B10A2_TYPELESS:
    case DXGI_FORMAT_R10G10B10A2_UNORM:
        return 4;
    case DXGI_FORMAT_NV12:
        return 1; // luma plane only; chroma is interleaved below it
    default:
        return 4; // the 8-bit RGBA/BGRA family SteamVR apps use
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

inline uint32_t crc32_of(uint32_t crc, const uint8_t *p, size_t n) {
    static uint32_t table[256];
    static bool ready = false;
    if (!ready) {
        for (uint32_t i = 0; i < 256; i++) {
            uint32_t c = i;
            for (int k = 0; k < 8; k++) {
                c = (c & 1) ? (0xEDB88320u ^ (c >> 1)) : (c >> 1);
            }
            table[i] = c;
        }
        ready = true;
    }
    crc ^= 0xFFFFFFFFu;
    for (size_t i = 0; i < n; i++) {
        crc = table[(crc ^ p[i]) & 0xFF] ^ (crc >> 8);
    }
    return crc ^ 0xFFFFFFFFu;
}

// Minimal PNG writer: 8-bit RGB, filter 0 on every row, stored deflate blocks.
// Valid PNG, no compression. ~4 bytes/px on disk, which is fine for a handful of
// diagnostic frames and costs no build dependency.
inline bool write_png(const std::string &path, int w, int h, const std::vector<uint8_t> &rgb) {
    if (w <= 0 || h <= 0 || rgb.size() < size_t(w) * 3 * size_t(h))
        return false;
    std::FILE *f = std::fopen(path.c_str(), "wb");
    if (!f)
        return false;

    auto be32 = [](uint8_t *o, uint32_t v) {
        o[0] = uint8_t(v >> 24);
        o[1] = uint8_t(v >> 16);
        o[2] = uint8_t(v >> 8);
        o[3] = uint8_t(v);
    };
    auto chunk = [&](const char *type, const uint8_t *data, uint32_t len) {
        uint8_t hdr[4];
        be32(hdr, len);
        std::fwrite(hdr, 1, 4, f);
        std::fwrite(type, 1, 4, f);
        if (len)
            std::fwrite(data, 1, len, f);
        uint32_t crc = crc32_of(0, reinterpret_cast<const uint8_t *>(type), 4);
        if (len)
            crc = crc32_of(crc, data, len);
        uint8_t c[4];
        be32(c, crc);
        std::fwrite(c, 1, 4, f);
    };

    static const uint8_t signature[8] = { 0x89, 'P', 'N', 'G', '\r', '\n', 0x1A, '\n' };
    std::fwrite(signature, 1, 8, f);

    uint8_t ihdr[13];
    be32(ihdr, uint32_t(w));
    be32(ihdr + 4, uint32_t(h));
    ihdr[8] = 8;
    ihdr[9] = 2;
    ihdr[10] = 0;
    ihdr[11] = 0;
    ihdr[12] = 0;
    chunk("IHDR", ihdr, 13);

    const size_t stride = size_t(w) * 3;
    std::vector<uint8_t> raw((stride + 1) * size_t(h));
    for (int y = 0; y < h; y++) {
        raw[(stride + 1) * size_t(y)] = 0;
        std::memcpy(&raw[(stride + 1) * size_t(y) + 1], &rgb[stride * size_t(y)], stride);
    }

    std::vector<uint8_t> z;
    z.push_back(0x78);
    z.push_back(0x01);
    size_t pos = 0;
    while (pos < raw.size()) {
        size_t n = std::min<size_t>(65535, raw.size() - pos);
        const bool last = (pos + n) >= raw.size();
        z.push_back(last ? 1 : 0);
        z.push_back(uint8_t(n & 0xFF));
        z.push_back(uint8_t((n >> 8) & 0xFF));
        z.push_back(uint8_t((~n) & 0xFF));
        z.push_back(uint8_t(((~n) >> 8) & 0xFF));
        z.insert(z.end(), raw.begin() + pos, raw.begin() + pos + n);
        pos += n;
    }
    uint32_t a = 1;
    uint32_t b = 0;
    for (uint8_t v : raw) {
        a = (a + v) % 65521;
        b = (b + a) % 65521;
    }
    const uint32_t adler = (b << 16) | a;
    z.push_back(uint8_t(adler >> 24));
    z.push_back(uint8_t((adler >> 16) & 0xFF));
    z.push_back(uint8_t((adler >> 8) & 0xFF));
    z.push_back(uint8_t(adler & 0xFF));
    chunk("IDAT", z.data(), uint32_t(z.size()));
    chunk("IEND", nullptr, 0);

    std::fclose(f);
    return true;
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
        if (desc.Width == 0 || desc.Height == 0)
            return;

        DXGI_FORMAT format = concrete(desc.Format);
        const int bpp = bytes_per_pixel(format);

        // Whole texture, capped. A centre crop is not good enough here: the
        // composition target is two eyes wide, so its centre is the seam between
        // them and would say nothing about either eye.
        const UINT w = std::min<UINT>(desc.Width, 2048);
        const UINT h = std::min<UINT>(desc.Height, 2048);
        if (bpp < 1 || bpp > 16)
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
                Warn("greyprobe: staging texture failed (%ux%u fmt=%d), disabling", w, h, int(format));
                m_enabled = false;
                return;
            }
            m_w = w;
            m_h = h;
            m_format = format;
        }

        D3D11_BOX box{};
        box.left = 0;
        box.top = 0;
        box.front = 0;
        box.right = w;
        box.bottom = h;
        box.back = 1;
        context->CopySubresourceRegion(m_staging.Get(), 0, 0, 0, 0, texture, 0, &box);

        D3D11_MAPPED_SUBRESOURCE mapped{};
        if (FAILED(context->Map(m_staging.Get(), 0, D3D11_MAP_READ, 0, &mapped)))
            return;
        const uint8_t *base = static_cast<const uint8_t *>(mapped.pData);
        const UINT rowBytes = std::min<UINT>(w * UINT(bpp), mapped.RowPitch);

        // --- statistics over the whole (capped) texture -----------------------
        uint64_t sum = 0;
        uint64_t sumSq = 0;
        uint64_t n = 0;
        uint8_t lo = 255;
        uint8_t hi = 0;
        for (UINT y = 0; y < h; y++) {
            const uint8_t *row = base + size_t(y) * mapped.RowPitch;
            for (UINT x = 0; x < rowBytes; x++) {
                const uint8_t v = row[x];
                sum += v;
                sumSq += uint64_t(v) * v;
                n++;
                lo = std::min(lo, v);
                hi = std::max(hi, v);
            }
        }
        if (n == 0) {
            context->Unmap(m_staging.Get(), 0);
            return;
        }
        const double mean = double(sum) / n;
        const double variance = double(sumSq) / n - mean * mean;
        const double stddev = variance > 0.0 ? std::sqrt(variance) : 0.0;

        // --- an image, downsampled to a fixed width ---------------------------
        std::string imagePath;
        if (!m_dir.empty() && m_written < m_max) {
            const int outW = 384;
            const int outH = std::max(1, int(h) * outW / std::max(1, int(w)));
            std::vector<uint8_t> rgb(size_t(outW) * size_t(outH) * 3);
            for (int oy = 0; oy < outH; oy++) {
                const UINT y0 = UINT(oy) * h / UINT(outH);
                const UINT y1 = std::max<UINT>(y0 + 1, UINT(oy + 1) * h / UINT(outH));
                for (int ox = 0; ox < outW; ox++) {
                    const UINT x0 = UINT(ox) * w / UINT(outW);
                    const UINT x1 = std::max<UINT>(x0 + 1, UINT(ox + 1) * w / UINT(outW));
                    uint32_t acc[3] = { 0, 0, 0 };
                    uint32_t cnt = 0;
                    for (UINT y = y0; y < y1; y++) {
                        const uint8_t *row = base + size_t(y) * mapped.RowPitch;
                        for (UINT x = x0; x < x1; x++) {
                            const UINT o = x * UINT(bpp);
                            if (o + UINT(bpp) > rowBytes)
                                continue;
                            if (bpp >= 3) {
                                acc[0] += row[o];
                                acc[1] += row[o + 1];
                                acc[2] += row[o + 2];
                            } else {
                                acc[0] += row[o];
                                acc[1] += row[o];
                                acc[2] += row[o];
                            }
                            cnt++;
                        }
                    }
                    if (cnt == 0)
                        cnt = 1;
                    const size_t so = (size_t(oy) * size_t(outW) + size_t(ox)) * 3;
                    rgb[so] = uint8_t(acc[0] / cnt);
                    rgb[so + 1] = uint8_t(acc[1] / cnt);
                    rgb[so + 2] = uint8_t(acc[2] / cnt);
                }
            }
            imagePath = m_dir + "/" + std::to_string(frameIndex) + "_" + stage + ".png";
            if (write_png(imagePath, outW, outH, rgb))
                m_written++;
            else
                imagePath.clear();
        }

        context->Unmap(m_staging.Get(), 0);

        if (m_file) {
            if (!m_headerWritten) {
                std::fprintf(
                    m_file,
                    "stage,frameIndex,layer,eye,w,h,texW,texH,format,bpp,mean,std,min,max,image\n"
                );
                m_headerWritten = true;
            }
            std::fprintf(
                m_file,
                "%s,%llu,%d,%d,%u,%u,%u,%u,%d,%d,%.3f,%.3f,%u,%u,%s\n",
                stage,
                static_cast<unsigned long long>(frameIndex),
                layer,
                eye,
                w,
                h,
                desc.Width,
                desc.Height,
                int(format),
                bpp,
                mean,
                stddev,
                lo,
                hi,
                imagePath.empty() ? "" : imagePath.c_str()
            );
            std::fflush(m_file);
        }
    }

private:
    Probe() {
        const char *dir = std::getenv("GEMLINK_GREYDIR");
        if (dir && *dir)
            m_dir = dir;
        if (const char *m = std::getenv("GEMLINK_GREYMAX"))
            m_max = size_t(std::max(0, std::atoi(m)));
        if (m_dir.empty())
            return;
        // Images need a directory that exists; create nothing, fail loudly.
        m_file = std::fopen((m_dir + "/greyprobe.csv").c_str(), "w");
        if (!m_file) {
            Warn("greyprobe: cannot write to %s", m_dir.c_str());
            return;
        }
        m_enabled = true;
        Debug("greyprobe: dumping stage images into %s (max %zu)", m_dir.c_str(), m_max);
    }

    bool m_enabled = false;
    bool m_headerWritten = false;
    std::string m_dir;
    size_t m_max = 120;
    size_t m_written = 0;
    std::FILE *m_file = nullptr;
    ComPtr<ID3D11Texture2D> m_staging;
    UINT m_w = 0;
    UINT m_h = 0;
    DXGI_FORMAT m_format = DXGI_FORMAT_UNKNOWN;
};

} // namespace greyprobe
