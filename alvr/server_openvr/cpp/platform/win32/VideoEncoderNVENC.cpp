#include "VideoEncoderNVENC.h"

#include "GreyProbe.h"
#include "NvCodecUtils.h"

#include "alvr_server/Logger.h"
#include "alvr_server/Utils.h"
#include "alvr_server/bindings.h"

VideoEncoderNVENC::VideoEncoderNVENC(std::shared_ptr<CD3DRender> pD3DRender, int width, int height)
    : m_pD3DRender(pD3DRender)
    , m_codec(Settings_Instance()->m_codec)
    , m_refreshRate(Settings_Instance()->m_refreshRate)
    , m_renderWidth(width)
    , m_renderHeight(height)
    , m_bitrateInMBits(30) { }

VideoEncoderNVENC::~VideoEncoderNVENC() { }

void VideoEncoderNVENC::Initialize() {
    //
    // Initialize Encoder
    //

    NV_ENC_BUFFER_FORMAT format
        = Settings_Instance()->m_enableHdr ? NV_ENC_BUFFER_FORMAT_NV12 : NV_ENC_BUFFER_FORMAT_ABGR;

    if (Settings_Instance()->m_use10bitEncoder) {
        format = Settings_Instance()->m_enableHdr ? NV_ENC_BUFFER_FORMAT_YUV420_10BIT
                                                  : NV_ENC_BUFFER_FORMAT_ABGR10;
    }

    Debug(
        "Initializing CNvEncoder. Width=%d Height=%d Format=%d\n",
        m_renderWidth,
        m_renderHeight,
        format
    );

    try {
        m_NvNecoder = std::make_shared<NvEncoderD3D11>(
            m_pD3DRender->GetDevice(), m_renderWidth, m_renderHeight, format, 0
        );
    } catch (NVENCException e) {
        throw MakeException(
            "NvEnc NvEncoderD3D11 failed. Code=%d %hs\n", e.getErrorCode(), e.what()
        );
    }

    NV_ENC_INITIALIZE_PARAMS initializeParams = { NV_ENC_INITIALIZE_PARAMS_VER };
    NV_ENC_CONFIG encodeConfig = { NV_ENC_CONFIG_VER };
    initializeParams.encodeConfig = &encodeConfig;

    FillEncodeConfig(
        initializeParams,
        m_refreshRate,
        m_renderWidth,
        m_renderHeight,
        m_bitrateInMBits * 1'000'000L
    );
    try {
        m_NvNecoder->CreateEncoder(&initializeParams);
    } catch (NVENCException e) {
        if (e.getErrorCode() == NV_ENC_ERR_INVALID_PARAM) {
            throw MakeException(
                "This GPU does not support H.265 encoding. (NvEncoderCuda NV_ENC_ERR_INVALID_PARAM)"
            );
        }
        // Long-term references are a request, not a promise: a GPU or a preset may refuse the
        // configuration. Retry once without them rather than losing the session — the encoder then
        // behaves exactly as it did before any of this, which is a working stream with a broken
        // reference chain per lost frame rather than no stream at all.
        if (!m_ltrSupported) {
            throw MakeException("NvEnc CreateEncoder failed. Code=%d %hs", e.getErrorCode(), e.what());
        }
        Warn(
            "NVENC refused long-term references (%hs); retrying without them. A lost frame will then "
            "break the reference chain until a keyframe, rather than being routed around.",
            e.what()
        );
        m_ltrAllowed = false;
        NV_ENC_INITIALIZE_PARAMS retryParams = { NV_ENC_INITIALIZE_PARAMS_VER };
        NV_ENC_CONFIG retryConfig = { NV_ENC_CONFIG_VER };
        retryParams.encodeConfig = &retryConfig;
        FillEncodeConfig(
            retryParams,
            m_refreshRate,
            m_renderWidth,
            m_renderHeight,
            m_bitrateInMBits * 1'000'000L
        );
        try {
            m_NvNecoder->CreateEncoder(&retryParams);
        } catch (NVENCException e2) {
            throw MakeException(
                "NvEnc CreateEncoder failed. Code=%d %hs", e2.getErrorCode(), e2.what()
            );
        }
    }

    Debug("CNvEncoder is successfully initialized.\n");
}

void VideoEncoderNVENC::Shutdown() {
    std::vector<std::vector<uint8_t>> vPacket;
    if (m_NvNecoder)
        m_NvNecoder->EndEncode(vPacket);

    for (std::vector<uint8_t>& packet : vPacket) {
        if (fpOut) {
            fpOut.write(reinterpret_cast<char*>(packet.data()), packet.size());
        }
    }
    if (m_NvNecoder) {
        m_NvNecoder->DestroyEncoder();
        m_NvNecoder.reset();
    }

    Debug("CNvEncoder::Shutdown\n");

    if (fpOut) {
        fpOut.close();
    }
}

void VideoEncoderNVENC::Transmit(
    ID3D11Texture2D* pTexture, uint64_t presentationTime, uint64_t targetTimestampNs, bool insertIDR
) {
    auto params = GetDynamicEncoderParams();
    if (params.updated) {
        m_bitrateInMBits = params.bitrate_bps / 1'000'000;
        NV_ENC_INITIALIZE_PARAMS initializeParams = { NV_ENC_INITIALIZE_PARAMS_VER };
        NV_ENC_CONFIG encodeConfig = { NV_ENC_CONFIG_VER };
        initializeParams.encodeConfig = &encodeConfig;
        FillEncodeConfig(
            initializeParams,
            params.framerate,
            m_renderWidth,
            m_renderHeight,
            m_bitrateInMBits * 1'000'000L
        );
        NV_ENC_RECONFIGURE_PARAMS reconfigureParams = { NV_ENC_RECONFIGURE_PARAMS_VER };
        reconfigureParams.reInitEncodeParams = initializeParams;
        m_NvNecoder->Reconfigure(&reconfigureParams);

        // A reconfigure can reset the encoder's picture buffer, and a long-term reference that is no
        // longer there is worse than none: the frame that names it cannot be decoded. Forgetting
        // which frames are marked means the next unconfirmed frame rebuilds the chain instead of
        // referencing something that may not exist. It costs a keyframe per parameter change and
        // nothing else.
        for (int slot = 0; slot < LTR_SLOTS; slot++) {
            m_ltrFrameIndex[slot] = 0;
        }
    }

    // -----------------------------------------------------------------------------------------
    // Loss stops poisoning the stream. (The reference decision, per frame.)
    //
    // A P-frame references its predecessor. If that predecessor never reached the client, every
    // frame behind it decodes to a plausible-looking, entirely wrong picture — which is why the
    // client's trust gate held, and asked for a keyframe, and why a single lost frame cost ~140
    // frames of black on hardware.
    //
    // The client now tells us which frames it **decoded** (`Feedback::Ack`), so the encoder can
    // reference only those: mark each frame as a long-term reference, and when the previous frame is
    // unconfirmed, encode against the newest confirmed one instead. Nothing then depends on a frame
    // the client does not have. The cost is that references are older — a compression cost, not a
    // correctness one — and that is the whole trade.
    //
    // Three decisions, in order of preference:
    //   1. The previous frame is confirmed: ordinary P-frame, reference it. This is the common case.
    //   2. The previous frame is not confirmed, but a confirmed frame is still an LTR: encode
    //      against that LTR alone. No keyframe, no burst, and the client can decode it.
    //   3. Nothing confirmed is available: a keyframe is the only thing that can rebuild the chain.
    //      When intra refresh is enabled a *sweep* is preferred to an IDR — it spreads the same cost
    //      over several frames instead of emitting one frame several times the size, which is
    //      exactly the burst that fills the queue we are trying to drain.
    //
    // `client_ack_valid == 0` (no acknowledgement yet, or a client that does not acknowledge) means
    // no frame is confirmed, so the decision falls through to the keyframe path — the behaviour that
    // existed before this, unchanged.
    // -----------------------------------------------------------------------------------------
    const unsigned long long frameIndex = GetFrameSequence();
    const bool ackValid = params.client_ack_valid != 0;
    const unsigned long long ackedFrame = params.client_acked_frame;

    // Was this exact frame decoded by the client? A cursor is not enough to answer it: a client that
    // skipped a frame still acknowledges later ones. The mask covers the 64 frames behind the newest
    // acknowledgement, which is many round trips of slack.
    auto wasAcked = [&](unsigned long long index) -> bool {
        if (!ackValid || index == 0 || index > ackedFrame) {
            return false;
        }
        const unsigned long long delta = ackedFrame - index;
        if (delta >= 64) {
            return false;
        }
        return ((params.client_acked_recent_mask >> delta) & 1ULL) != 0;
    };

    // The newest long-term reference slot whose frame the client has confirmed, if any.
    int confirmSlot = -1;
    if (m_ltrSupported) {
        for (int slot = 0; slot < LTR_SLOTS; slot++) {
            if (wasAcked(m_ltrFrameIndex[slot])
                && (confirmSlot < 0 || m_ltrFrameIndex[slot] > m_ltrFrameIndex[confirmSlot])) {
                confirmSlot = slot;
            }
        }
    }

    // Reference this frame against the previous frame (the common case, best compression), or
    // against a confirmed older frame (a small compression cost, and nothing depends on a frame the
    // client does not have), or not at all — in which case the chain is rebuilt.
    const bool usePrevious = !insertIDR && wasAcked(m_lastEncodedFrameIndex);
    const bool useConfirmedLtr = !insertIDR && !usePrevious && confirmSlot >= 0;
    unsigned long long chainRoot = 0;
    if (insertIDR) {
        chainRoot = 0; // a keyframe is its own root; the client needs no reference for it
    } else if (usePrevious) {
        chainRoot = m_lastEncodedFrameIndex;
    } else if (useConfirmedLtr) {
        chainRoot = m_ltrFrameIndex[confirmSlot];
    } else {
        // Nothing confirmed: the chain is rebuilt. `chainRoot` stays 0 — "the sender does not say" —
        // so the client treats the frame as unproven rather than trusting it on the strength of a
        // keyframe flag that a lost datagram can invalidate anyway.
        insertIDR = true;
    }

    SetFrameChainRoot(chainRoot);

    // -----------------------------------------------------------------------------------------
    // The datagram copy. Unchanged, and still probed: see the grey-frame note below.
    // -----------------------------------------------------------------------------------------
    std::vector<std::vector<uint8_t>> vPacket;

    const NvEncInputFrame* encoderInputFrame = m_NvNecoder->GetNextInputFrame();

    ID3D11Texture2D* pInputTexture
        = reinterpret_cast<ID3D11Texture2D*>(encoderInputFrame->inputPtr);

    // --- grey-frame instrumentation: does CopyResource actually have matching
    // formats? ID3D11DeviceContext::CopyResource returns void and requires the
    // source and destination formats to agree; a mismatch is a silent no-op that
    // only the D3D11 debug layer would mention, and a release driver build has it
    // off. Print the numbers the runtime reports rather than trusting the enum.
    // See /workspace/VD_RE/51-grey-frame-ruled-out.md
    {
        D3D11_TEXTURE2D_DESC srcDesc{}, dstDesc{};
        pTexture->GetDesc(&srcDesc);
        pInputTexture->GetDesc(&dstDesc);
        static bool formatsLogged = false;
        if (!formatsLogged) {
            formatsLogged = true;
            Info(
                "NVENCPROBE: copy source %ux%u fmt=%d  ->  nvenc input buffer %ux%u fmt=%d  "
                "(match=%d)  ring buffers=%u",
                srcDesc.Width,
                srcDesc.Height,
                (int)srcDesc.Format,
                dstDesc.Width,
                dstDesc.Height,
                (int)dstDesc.Format,
                (int)(srcDesc.Format == dstDesc.Format),
                m_NvNecoder->GetEncoderBufferCount()
            );
        }
    }

    m_pD3DRender->GetContext()->CopyResource(pInputTexture, pTexture);

    // The buffer NVENC is about to read, sampled at the moment it reads it. This is
    // the one link never measured: every other probe sampled inside FrameRender.
    // Stage "encin" = the encoder's actual input, after the copy.
    greyprobe::Probe::Instance().Sample(
        m_pD3DRender->GetDevice(),
        m_pD3DRender->GetContext(),
        pInputTexture,
        targetTimestampNs,
        0,
        0,
        "encin"
    );

    NV_ENC_PIC_PARAMS picParams = {};
    if (insertIDR) {
        // A sweep rebuilds the chain *progressively*, which needs a picture buffer to refresh from.
        // The first frame of a session has none — the decoder has not even seen the parameter sets —
        // so it is always a real IDR, whatever the settings say.
        if (m_intraRefreshSweepFrames > 0 && m_lastEncodedFrameIndex != 0) {
            // Rebuild the chain with a sweep rather than an IDR where the encoder supports it: the
            // same recovery, spread over several frames instead of one frame several times the size.
            // A keyframe-sized burst on a link that is already behind is the congestion that lost the
            // frame in the first place.
            m_intraRefreshFramesLeft = m_intraRefreshSweepFrames;
            m_sweepsStarted++;
        } else {
            Debug("Inserting IDR frame.\n");
            picParams.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR;
        }
    }
    if (m_intraRefreshFramesLeft > 0) {
        m_intraRefreshFramesLeft--;
        switch (m_codec) {
        case ALVR_CODEC_H264:
            picParams.codecPicParams.h264PicParams.forceIntraRefreshWithFrameCnt
                = (uint32_t)m_intraRefreshSweepFrames;
            break;
        case ALVR_CODEC_HEVC:
            picParams.codecPicParams.hevcPicParams.forceIntraRefreshWithFrameCnt
                = (uint32_t)m_intraRefreshSweepFrames;
            break;
        default:
            break;
        }
    }

    // Mark this frame as a long-term reference and, when the chain is being carried by one, use it.
    // The LTR is the mechanism that makes "reference only confirmed frames" possible at all: the
    // encoder is not required to run the chain through the frames in between.
    if (m_ltrSupported && (m_codec == ALVR_CODEC_H264 || m_codec == ALVR_CODEC_HEVC)) {
        const uint32_t slot = (uint32_t)(frameIndex % (unsigned long long)LTR_SLOTS);
        if (m_codec == ALVR_CODEC_H264) {
            picParams.codecPicParams.h264PicParams.ltrMarkFrame = 1;
            picParams.codecPicParams.h264PicParams.ltrMarkFrameIdx = slot;
            if (useConfirmedLtr) {
                picParams.codecPicParams.h264PicParams.ltrUseFrames = 1;
                picParams.codecPicParams.h264PicParams.ltrUseFrameBitmap
                    = 1u << (uint32_t)confirmSlot;
            }
        } else {
            picParams.codecPicParams.hevcPicParams.ltrMarkFrame = 1;
            picParams.codecPicParams.hevcPicParams.ltrMarkFrameIdx = slot;
            if (useConfirmedLtr) {
                picParams.codecPicParams.hevcPicParams.ltrUseFrames = 1;
                picParams.codecPicParams.hevcPicParams.ltrUseFrameBitmap
                    = 1u << (uint32_t)confirmSlot;
            }
        }
        // The slot now holds this frame. It is not confirmed yet — the client has not seen it — and
        // it will only ever be referenced if an acknowledgement for this index arrives.
        m_ltrFrameIndex[slot] = frameIndex;
    }

    if (!m_loggedFirstRecovery && !usePrevious) {
        m_loggedFirstRecovery = true;
        Info(
            "NVENC reference: frame %llu encoded against %s (client confirmed %llu of %llu, "
            "valid=%d, ltr=%d)",
            frameIndex,
            insertIDR ? "a keyframe" : "an older confirmed frame",
            ackedFrame,
            frameIndex,
            (int)ackValid,
            (int)m_ltrSupported
        );
    }

    m_NvNecoder->EncodeFrame(vPacket, &picParams);

    m_lastEncodedFrameIndex = frameIndex;
    if (usePrevious) {
        m_framesReferencingPrevious++;
    } else if (!insertIDR) {
        m_framesReferencingConfirmed++;
    } else {
        m_framesForcedKey++;
    }

    for (std::vector<uint8_t>& packet : vPacket) {
        uint8_t* buf = packet.data();
        int len = (int)packet.size();

        // NVENC's AV1 encoding includes a bunch of IVF wrapping,
        // so we need to strip it down to just the OBUs
        if (m_codec == ALVR_CODEC_AV1) {
            const uint8_t ivf_magic[4] = { 0x44, 0x4B, 0x49, 0x46 };
            if (len >= 4 && !memcmp(buf, ivf_magic, 4)) {
                buf += 32;
                len -= 32;
            }
            if (len <= 12) {
                continue;
            }
            buf += 12; // skip past the IVF packet size header thing
            len -= 12;
        }

        if (len <= 0) {
            continue;
        }

        if (fpOut) {
            fpOut.write(reinterpret_cast<char*>(buf), len);
        }

        ParseFrameNals(m_codec, buf, len, targetTimestampNs, insertIDR);
    }
}

void VideoEncoderNVENC::FillEncodeConfig(
    NV_ENC_INITIALIZE_PARAMS& initializeParams,
    int refreshRate,
    int renderWidth,
    int renderHeight,
    uint64_t bitrate_bps
) {
    auto& encodeConfig = *initializeParams.encodeConfig;

    GUID encoderGUID;
    switch (m_codec) {
    case ALVR_CODEC_H264:
        encoderGUID = NV_ENC_CODEC_H264_GUID;
        break;
    case ALVR_CODEC_HEVC:
        encoderGUID = NV_ENC_CODEC_HEVC_GUID;
        break;
    case ALVR_CODEC_AV1:
        encoderGUID = NV_ENC_CODEC_AV1_GUID;
        break;
    }

    GUID qualityPreset;
    // See recommended NVENC settings for low-latency encoding.
    // https://docs.nvidia.com/video-technologies/video-codec-sdk/nvenc-video-encoder-api-prog-guide/#recommended-nvenc-settings
    switch (Settings_Instance()->m_nvencQualityPreset) {
    case 7:
        qualityPreset = NV_ENC_PRESET_P7_GUID;
        break;
    case 6:
        qualityPreset = NV_ENC_PRESET_P6_GUID;
        break;
    case 5:
        qualityPreset = NV_ENC_PRESET_P5_GUID;
        break;
    case 4:
        qualityPreset = NV_ENC_PRESET_P4_GUID;
        break;
    case 3:
        qualityPreset = NV_ENC_PRESET_P3_GUID;
        break;
    case 2:
        qualityPreset = NV_ENC_PRESET_P2_GUID;
        break;
    case 1:
    default:
        qualityPreset = NV_ENC_PRESET_P1_GUID;
        break;
    }

    NV_ENC_TUNING_INFO tuningPreset
        = static_cast<NV_ENC_TUNING_INFO>(Settings_Instance()->m_nvencTuningPreset);

    m_NvNecoder->CreateDefaultEncoderParams(
        &initializeParams, encoderGUID, qualityPreset, tuningPreset
    );

    initializeParams.encodeWidth = initializeParams.darWidth = renderWidth;
    initializeParams.encodeHeight = initializeParams.darHeight = renderHeight;
    initializeParams.frameRateNum = refreshRate;
    initializeParams.frameRateDen = 1;

    if (Settings_Instance()->m_nvencRefreshRate != -1) {
        initializeParams.frameRateNum = Settings_Instance()->m_nvencRefreshRate;
    }

    initializeParams.enableWeightedPrediction
        = Settings_Instance()->m_nvencEnableWeightedPrediction;

    // 16 is recommended when using reference frame invalidation. But it has caused bad visual
    // quality. Now, use 0 (use default).
    uint32_t maxNumRefFrames = 0;
    uint32_t gopLength = NVENC_INFINITE_GOPLENGTH;

    if (Settings_Instance()->m_nvencMaxNumRefFrames != -1) {
        maxNumRefFrames = Settings_Instance()->m_nvencMaxNumRefFrames;
    }
    if (Settings_Instance()->m_nvencGopLength != -1) {
        gopLength = Settings_Instance()->m_nvencGopLength;
    }

    switch (m_codec) {
    case ALVR_CODEC_H264: {
        auto& config = encodeConfig.encodeCodecConfig.h264Config;
        config.repeatSPSPPS = 1;
        config.enableIntraRefresh = Settings_Instance()->m_nvencEnableIntraRefresh;

        if (Settings_Instance()->m_nvencIntraRefreshPeriod != -1) {
            config.intraRefreshPeriod = Settings_Instance()->m_nvencIntraRefreshPeriod;
        }
        if (Settings_Instance()->m_nvencIntraRefreshCount != -1) {
            config.intraRefreshCnt = Settings_Instance()->m_nvencIntraRefreshCount;
        }

        switch (Settings_Instance()->m_entropyCoding) {
        case ALVR_CABAC:
            config.entropyCodingMode = NV_ENC_H264_ENTROPY_CODING_MODE_CABAC;
            break;
        case ALVR_CAVLC:
            config.entropyCodingMode = NV_ENC_H264_ENTROPY_CODING_MODE_CAVLC;
            break;
        }

        config.maxNumRefFrames = maxNumRefFrames;
        config.idrPeriod = gopLength;

        if (Settings_Instance()->m_fillerData) {
            config.enableFillerDataInsertion = Settings_Instance()->m_rateControlMode == ALVR_CBR;
        }

        config.h264VUIParameters.videoSignalTypePresentFlag = 1;
        config.h264VUIParameters.videoFormat = NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
        config.h264VUIParameters.videoFullRangeFlag = 1;
        config.h264VUIParameters.colourDescriptionPresentFlag = 1;
        if (Settings_Instance()->m_enableHdr) {
            config.h264VUIParameters.colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES_BT2020;
            config.h264VUIParameters.transferCharacteristics
                = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SRGB;
            config.h264VUIParameters.colourMatrix = NV_ENC_VUI_MATRIX_COEFFS_BT2020_NCL;
        } else {
            config.h264VUIParameters.colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES_BT709;
            config.h264VUIParameters.transferCharacteristics
                = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SRGB;
            config.h264VUIParameters.colourMatrix = NV_ENC_VUI_MATRIX_COEFFS_BT709;
        }
    } break;
    case ALVR_CODEC_HEVC: {
        auto& config = encodeConfig.encodeCodecConfig.hevcConfig;
        config.repeatSPSPPS = 1;
        config.enableIntraRefresh = Settings_Instance()->m_nvencEnableIntraRefresh;

        if (Settings_Instance()->m_nvencIntraRefreshPeriod != -1) {
            config.intraRefreshPeriod = Settings_Instance()->m_nvencIntraRefreshPeriod;
        }
        if (Settings_Instance()->m_nvencIntraRefreshCount != -1) {
            config.intraRefreshCnt = Settings_Instance()->m_nvencIntraRefreshCount;
        }

        config.maxNumRefFramesInDPB = maxNumRefFrames;
        config.idrPeriod = gopLength;

        if (Settings_Instance()->m_use10bitEncoder) {
            encodeConfig.encodeCodecConfig.hevcConfig.pixelBitDepthMinus8 = 2;
        }

        if (Settings_Instance()->m_fillerData) {
            config.enableFillerDataInsertion = Settings_Instance()->m_rateControlMode == ALVR_CBR;
        }

        config.hevcVUIParameters.videoSignalTypePresentFlag = 1;
        config.hevcVUIParameters.videoFormat = NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
        config.hevcVUIParameters.videoFullRangeFlag = 1;
        config.hevcVUIParameters.colourDescriptionPresentFlag = 1;
        if (Settings_Instance()->m_enableHdr) {
            config.hevcVUIParameters.colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES_BT2020;
            config.hevcVUIParameters.transferCharacteristics
                = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SRGB;
            config.hevcVUIParameters.colourMatrix = NV_ENC_VUI_MATRIX_COEFFS_BT2020_NCL;
        } else {
            config.hevcVUIParameters.colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES_BT709;
            config.hevcVUIParameters.transferCharacteristics
                = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SRGB;
            config.hevcVUIParameters.colourMatrix = NV_ENC_VUI_MATRIX_COEFFS_BT709;
        }
    } break;
    case ALVR_CODEC_AV1: {
        auto& config = encodeConfig.encodeCodecConfig.av1Config;
        config.repeatSeqHdr = 1;
        config.enableIntraRefresh = Settings_Instance()->m_nvencEnableIntraRefresh;

        if (Settings_Instance()->m_nvencIntraRefreshPeriod != -1) {
            config.intraRefreshPeriod = Settings_Instance()->m_nvencIntraRefreshPeriod;
        }
        if (Settings_Instance()->m_nvencIntraRefreshCount != -1) {
            config.intraRefreshCnt = Settings_Instance()->m_nvencIntraRefreshCount;
        }

        config.maxNumRefFramesInDPB = maxNumRefFrames;
        config.idrPeriod = gopLength;

        if (Settings_Instance()->m_use10bitEncoder) {
            config.pixelBitDepthMinus8 = 2;
        }

        if (Settings_Instance()->m_fillerData) {
            config.enableBitstreamPadding = Settings_Instance()->m_rateControlMode == ALVR_CBR;
        }

        config.chromaFormatIDC = 1; // 4:2:0, 4:4:4 currently not supported
        config.colorRange = 1;
        if (Settings_Instance()->m_enableHdr) {
            config.colorPrimaries = NV_ENC_VUI_COLOR_PRIMARIES_BT2020;
            config.transferCharacteristics = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SRGB;
            config.matrixCoefficients = NV_ENC_VUI_MATRIX_COEFFS_BT2020_NCL;
        } else {
            config.colorPrimaries = NV_ENC_VUI_COLOR_PRIMARIES_BT709;
            config.transferCharacteristics = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SRGB;
            config.matrixCoefficients = NV_ENC_VUI_MATRIX_COEFFS_BT709;
        }
    } break;
    }

    // Disable automatic IDR insertion by NVENC. We need to manually insert IDR when packet is
    // dropped if don't use reference frame invalidation.
    encodeConfig.gopLength = gopLength;
    encodeConfig.frameIntervalP = 1;

    if (Settings_Instance()->m_nvencPFrameStrategy != -1) {
        encodeConfig.frameIntervalP = Settings_Instance()->m_nvencPFrameStrategy;
    }

    // -----------------------------------------------------------------------------------------
    // Long-term references: how "reference only frames the client confirmed" is implemented.
    //
    // NVENC's P-frame references its predecessor. To reference an *older* frame instead, the frame
    // has to have been marked as a long-term reference, and the current picture has to be told to
    // use it (`ltrUseFrames` + a bitmap). LTR Per Picture mode is the documented-preferred mode and
    // is what this uses: `ltrTrustMode = 0`, and each picture marked with `ltrMarkFrame = 1` as it
    // is encoded. See the reference decision in Transmit for how the choice is made.
    //
    // Requires no B-frames (`frameIntervalP == 1`), which this pipeline already assumes, and is
    // unavailable for AV1 — for AV1 `m_ltrSupported` stays false and the encoder falls back to
    // keyframes, which is the previous behaviour.
    // -----------------------------------------------------------------------------------------
    m_ltrSupported = m_ltrAllowed && encodeConfig.frameIntervalP == 1
        && (m_codec == ALVR_CODEC_H264 || m_codec == ALVR_CODEC_HEVC);
    if (m_ltrSupported) {
        if (m_codec == ALVR_CODEC_H264) {
            encodeConfig.encodeCodecConfig.h264Config.enableLTR = 1;
            encodeConfig.encodeCodecConfig.h264Config.ltrNumFrames = LTR_SLOTS;
            encodeConfig.encodeCodecConfig.h264Config.ltrTrustMode = 0;
        } else {
            encodeConfig.encodeCodecConfig.hevcConfig.enableLTR = 1;
            encodeConfig.encodeCodecConfig.hevcConfig.ltrNumFrames = LTR_SLOTS;
            encodeConfig.encodeCodecConfig.hevcConfig.ltrTrustMode = 0;
        }
    }

    // -----------------------------------------------------------------------------------------
    // Rolling intra refresh: how the chain is rebuilt when it does have to be rebuilt.
    //
    // A keyframe is one frame several times the size of every other frame. On a link that is already
    // behind, that burst is the congestion that lost the frame in the first place — so recovery
    // spreads the same cost over a sweep of frames instead. The sweep length is the setting's own
    // intra-refresh length, which is what it is documented as meaning.
    // -----------------------------------------------------------------------------------------
    m_intraRefreshSweepFrames = 0;
    if (Settings_Instance()->m_nvencEnableIntraRefresh) {
        const long long count = Settings_Instance()->m_nvencIntraRefreshCount;
        const long long period = Settings_Instance()->m_nvencIntraRefreshPeriod;
        long long sweep = 8;
        if (count > 0) {
            sweep = count;
        } else if (period > 0) {
            sweep = period / 8;
        }
        if (sweep < 1) {
            sweep = 1;
        }
        if (sweep > 64) {
            sweep = 64;
        }
        m_intraRefreshSweepFrames = (int)sweep;
    }

    switch (Settings_Instance()->m_rateControlMode) {
    case ALVR_CBR:
        encodeConfig.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR;
        break;
    case ALVR_VBR:
        encodeConfig.rcParams.rateControlMode = NV_ENC_PARAMS_RC_VBR;
        break;
    }
    encodeConfig.rcParams.multiPass
        = static_cast<NV_ENC_MULTI_PASS>(Settings_Instance()->m_nvencMultiPass);
    encodeConfig.rcParams.lowDelayKeyFrameScale = 1;

    if (Settings_Instance()->m_nvencLowDelayKeyFrameScale != -1) {
        encodeConfig.rcParams.lowDelayKeyFrameScale
            = Settings_Instance()->m_nvencLowDelayKeyFrameScale;
    }

    uint32_t maxFrameSize = static_cast<uint32_t>(bitrate_bps / refreshRate);
    Debug("VideoEncoderNVENC: maxFrameSize=%d bits\n", maxFrameSize);
    encodeConfig.rcParams.vbvBufferSize = maxFrameSize * 1.1;
    encodeConfig.rcParams.vbvInitialDelay = maxFrameSize * 1.1;
    encodeConfig.rcParams.maxBitRate = static_cast<uint32_t>(bitrate_bps);
    encodeConfig.rcParams.averageBitRate = static_cast<uint32_t>(bitrate_bps);
    if (Settings_Instance()->m_nvencAdaptiveQuantizationMode == SpatialAQ) {
        encodeConfig.rcParams.enableAQ = 1;
    } else if (Settings_Instance()->m_nvencAdaptiveQuantizationMode == TemporalAQ) {
        encodeConfig.rcParams.enableTemporalAQ = 1;
    }

    if (Settings_Instance()->m_nvencRateControlMode != -1) {
        encodeConfig.rcParams.rateControlMode
            = (NV_ENC_PARAMS_RC_MODE)Settings_Instance()->m_nvencRateControlMode;
    }
    if (Settings_Instance()->m_nvencRcBufferSize != -1) {
        encodeConfig.rcParams.vbvBufferSize = Settings_Instance()->m_nvencRcBufferSize;
    }
    if (Settings_Instance()->m_nvencRcInitialDelay != -1) {
        encodeConfig.rcParams.vbvInitialDelay = Settings_Instance()->m_nvencRcInitialDelay;
    }
    if (Settings_Instance()->m_nvencRcMaxBitrate != -1) {
        encodeConfig.rcParams.maxBitRate = Settings_Instance()->m_nvencRcMaxBitrate;
    }
    if (Settings_Instance()->m_nvencRcAverageBitrate != -1) {
        encodeConfig.rcParams.averageBitRate = Settings_Instance()->m_nvencRcAverageBitrate;
    }
}
