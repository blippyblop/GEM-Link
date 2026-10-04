#pragma once

#include "NvEncoderD3D11.h"
#include "VideoEncoder.h"
#include "shared/d3drender.h"
#include <memory>

enum AdaptiveQuantizationMode { SpatialAQ = 1, TemporalAQ = 2 };

/// How many long-term reference slots the encoder keeps.
///
/// Four is two round trips at 90 Hz: enough that a confirmed frame survives a lost
/// acknowledgement and a burst of unusable frames, small enough that marking every frame as an LTR
/// costs the rate controller almost nothing. NVENC treats this as a ceiling and may keep fewer
/// (`NV_ENC_CONFIG_HEVC::ltrNumFrames` is documented as guidance rather than a promise), which is
/// why the code does not assume a slot exists merely because it is in range.
const int LTR_SLOTS = 4;

/// The least time between two forced rebuilds of the reference chain, in milliseconds.
///
/// The caller's own IDR scheduler has used 100 ms since it was written; the encoder's recovery path
/// had no limit at all, and the first live run with it produced a keyframe for every frame — 77 KB
/// pictures at 44 Mbps on a link whose receiver could read a fraction of that. A rebuild is a
/// response to congestion, so it must not be able to become congestion.
const unsigned long long RECOVERY_MIN_INTERVAL_MS = 100;

// Video encoder for NVIDIA NvEnc.
class VideoEncoderNVENC : public VideoEncoder {
public:
    VideoEncoderNVENC(std::shared_ptr<CD3DRender> pD3DRender, int width, int height);
    ~VideoEncoderNVENC();

    void Initialize();
    void Shutdown();

    void Transmit(
        ID3D11Texture2D* pTexture,
        uint64_t presentationTime,
        uint64_t targetTimestampNs,
        bool insertIDR
    );

private:
    void FillEncodeConfig(
        NV_ENC_INITIALIZE_PARAMS& initializeParams,
        int refreshRate,
        int renderWidth,
        int renderHeight,
        uint64_t bitrate_bps
    );

    /// Whether a forced rebuild of the reference chain is allowed right now, and the record that it
    /// happened. See `RECOVERY_MIN_INTERVAL_MS`.
    bool recoveryAllowed();

    std::ofstream fpOut;
    std::shared_ptr<NvEncoder> m_NvNecoder;

    std::shared_ptr<CD3DRender> m_pD3DRender;

    int m_codec;
    int m_refreshRate;
    int m_renderWidth;
    int m_renderHeight;
    int m_bitrateInMBits;

    // ---------------------------------------------------------------------------------------
    // Reference management. See "loss stops poisoning the stream" in the .cpp.
    // ---------------------------------------------------------------------------------------

    /// Whether the encoder was created with long-term reference support. If the GPU or the preset
    /// refused the configuration the session is re-created without it, and this stays false — the
    /// encoder then produces ordinary P-frames and every unconfirmed frame is a reference the client
    /// may not have, which is the behaviour that existed before any of this.
    bool m_ltrSupported = false;
    /// Whether LTR **may** be requested at all. Cleared if the encoder refuses the configuration, so
    /// the retry does not simply ask for the same rejected thing again.
    bool m_ltrAllowed = true;
    /// The frame index of each LTR slot, or 0 when the slot holds nothing. `ltrNumFrames` of them.
    unsigned long long m_ltrFrameIndex[LTR_SLOTS] = {};
    /// The frame index of the last picture this encoder emitted, so an ordinary P-frame can name
    /// what it references.
    unsigned long long m_lastEncodedFrameIndex = 0;
    /// Frames left in a forced intra-refresh sweep that is rebuilding the chain, and how long a
    /// sweep to start when one is needed.
    int m_intraRefreshFramesLeft = 0;
    int m_intraRefreshSweepFrames = 0;
    /// Counters, because a reference decision that is only visible in the picture is one nobody can
    /// debug: how many frames referenced the previous frame, an older confirmed frame, or a
    /// keyframe, and how many sweeps were started.
    unsigned long long m_framesReferencingPrevious = 0;
    unsigned long long m_framesReferencingConfirmed = 0;
    unsigned long long m_framesForcedKey = 0;
    unsigned long long m_sweepsStarted = 0;
    /// When the last forced rebuild happened, in milliseconds since the process started.
    ///
    /// Keyframes are rate-limited for the same reason the caller has always rate-limited them
    /// (`IDRScheduler::MIN_IDR_FRAME_INTERVAL`): a burst of them is congestion, and here the burst
    /// would be a response to congestion, which is how a feedback loop is built.
    unsigned long long m_lastRebuildMs = 0;
    /// One log line per session on the first reference decision that is not the ordinary one.
    bool m_loggedFirstRecovery = false;
};
