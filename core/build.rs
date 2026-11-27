//! Compiles libopus (vendor/opus: the reference C implementation, BSD; vendor/README.md says what
//! was taken and why) into the crate with the `cc` crate. The build machine needs only the C
//! compiler the target already has (MSVC, the NDK's clang, Xcode's clang): no cmake, autotools,
//! pkg-config or system libopus.
//!
//! What goes in: the single-stream encoder and decoder, SILK and CELT both (a browser peer may send
//! any mode; the decoder must take all of it). What stays out: multistream / projection, custom
//! modes, QEXT, the float API with its speech / music analysis, and the deep-learning parts (DRED,
//! deep PLC, OSCE: dnn/ is not even vendored). All of it -Os; the linker's section GC drops what
//! the wrapper never reaches.
//!
//! FIXED_POINT on every target. armeabi-v7a is the slow phones, and libopus's ARMv7 tuning (the
//! EDSP inline asm, the NEON fixed-point kernels) is for this build; arm64 could take float, but
//! float measured 12 KB bigger there (273,040 vs 260,912 bytes of .so, the float API and its
//! analysis included: 1.6.1 does not build float without them) and needs libm, for what is mostly
//! a better speech / music decision (VOIP assumes speech). One arithmetic everywhere also means the
//! desktop tests run what the phones run: libopus keeps its NEON kernels bit-exact with the C.
//!
//! ARM SIMD: NEON intrinsics, presumed (no run-time detection, no external assembly), the macros
//! upstream's configure / meson set on aarch64. arm64 has NEON by definition. On armeabi-v7a the
//! Rust target leaves NEON off (vfpv3-d16) and this C requires it: every armv7 Android phone in use
//! has it; one without would fault on the first frame. On NEON: the CELT inner products and pitch
//! correlation kernel, SILK's LPC gain, warped autocorrelation and plain NSQ. Still C: SILK's
//! delayed-decision NSQ (without run-time detection silk/arm/NSQ_del_dec_arm.h reaches its NEON
//! version only through OPUS_ARM_PRESUME_NEON, the external-asm macro, which in fixed point also
//! wants celt_pitch_xcorr_neon from the armv7 .S: an encoder-speed option for armv7, not a size one).

use std::env;

const OPUS: &str = "vendor/opus";

/// celt_sources.mk: CELT_SOURCES.
const CELT: &[&str] = &[
    "bands", "celt", "celt_encoder", "celt_decoder", "cwrs", "entcode", "entdec", "entenc",
    "kiss_fft", "laplace", "mathops", "mdct", "modes", "pitch", "celt_lpc", "quant_bands", "rate",
    "vq",
];

/// silk_sources.mk: SILK_SOURCES.
const SILK: &[&str] = &[
    "CNG", "code_signs", "init_decoder", "decode_core", "decode_frame", "decode_parameters",
    "decode_indices", "decode_pulses", "decoder_set_fs", "dec_API", "enc_API", "encode_indices",
    "encode_pulses", "gain_quant", "interpolate", "LP_variable_cutoff", "NLSF_decode", "NSQ",
    "NSQ_del_dec", "PLC", "shell_coder", "tables_gain", "tables_LTP", "tables_NLSF_CB_NB_MB",
    "tables_NLSF_CB_WB", "tables_other", "tables_pitch_lag", "tables_pulses_per_block", "VAD",
    "control_audio_bandwidth", "quant_LTP_gains", "VQ_WMat_EC", "HP_variable_cutoff",
    "NLSF_encode", "NLSF_VQ", "NLSF_unpack", "NLSF_del_dec_quant", "process_NLSFs",
    "stereo_LR_to_MS", "stereo_MS_to_LR", "check_control_input", "control_SNR", "init_encoder",
    "control_codec", "A2NLSF", "ana_filt_bank_1", "biquad_alt", "bwexpander_32", "bwexpander",
    "debug", "decode_pitch", "inner_prod_aligned", "lin2log", "log2lin", "LPC_analysis_filter",
    "LPC_inv_pred_gain", "table_LSF_cos", "NLSF2A", "NLSF_stabilize", "NLSF_VQ_weights_laroia",
    "pitch_est_tables", "resampler", "resampler_down2_3", "resampler_down2",
    "resampler_private_AR2", "resampler_private_down_FIR", "resampler_private_IIR_FIR",
    "resampler_private_up2_HQ", "resampler_rom", "sigm_Q15", "sort", "sum_sqr_shift",
    "stereo_decode_pred", "stereo_encode_pred", "stereo_find_predictor", "stereo_quant_pred",
    "LPC_fit",
];

/// silk_sources.mk: SILK_SOURCES_FIXED.
const SILK_FIXED: &[&str] = &[
    "LTP_analysis_filter_FIX", "LTP_scale_ctrl_FIX", "corrMatrix_FIX", "encode_frame_FIX",
    "find_LPC_FIX", "find_LTP_FIX", "find_pitch_lags_FIX", "find_pred_coefs_FIX",
    "noise_shape_analysis_FIX", "process_gains_FIX", "regularize_correlations_FIX",
    "residual_energy16_FIX", "residual_energy_FIX", "warped_autocorrelation_FIX",
    "apply_sine_window_FIX", "autocorr_FIX", "burg_modified_FIX", "k2a_FIX", "k2a_Q16_FIX",
    "pitch_analysis_core_FIX", "vector_ops_FIX", "schur64_FIX", "schur_FIX",
];

/// opus_sources.mk: OPUS_SOURCES minus multistream / projection / mapping_matrix (never called).
/// The repacketizer stays: the encoder uses it for multi-frame packets and padding.
const OPUS_SRC: &[&str] = &["opus", "opus_decoder", "opus_encoder", "extensions", "repacketizer"];

/// CELT_SOURCES_ARM_NEON_INTR, SILK_SOURCES_ARM_NEON_INTR, SILK_SOURCES_FIXED_ARM_NEON_INTR.
const NEON: &[&str] = &[
    "celt/arm/celt_neon_intr", "celt/arm/pitch_neon_intr", "silk/arm/biquad_alt_neon_intr",
    "silk/arm/LPC_inv_pred_gain_neon_intr", "silk/arm/NSQ_del_dec_neon_intr", "silk/arm/NSQ_neon",
    "silk/fixed/arm/warped_autocorrelation_FIX_neon_intr",
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={OPUS}");

    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let msvc = env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");

    let mut cc = cc::Build::new();
    // Paths stay relative to the package (a build script runs there): __FILE__ in libopus's
    // hardening checks then reads "vendor/opus/...", not this machine's path. The root is on the
    // path too: the SILK ARM headers include "celt/arm/armcpu.h" (automake's -I$(srcdir)).
    cc.include(OPUS)
        .include(format!("{OPUS}/include"))
        .include(format!("{OPUS}/celt"))
        .include(format!("{OPUS}/silk"))
        .include(format!("{OPUS}/silk/fixed"))
        // -Os whatever the profile (also in test builds). -Oz measured 17 KB smaller on arm64
        // and 12 KB on armv7, at the price of the codec's speed on the slow phones.
        .opt_level_str("s")
        .warnings(false)
        .extra_warnings(false)
        .define("OPUS_BUILD", None)
        // Nothing of libopus is exported from the library; only the crate's C face is.
        .define("OPUS_EXPORT", Some(""))
        .define("FIXED_POINT", None)
        // 24-bit internal resolution: upstream's fixed-point default since 1.6.
        .define("ENABLE_RES24", None)
        .define("DISABLE_FLOAT_API", None)
        // Cheap run-time checks of libopus's own invariants (upstream's default): a broken one
        // aborts instead of running on into undefined behaviour on a peer's bytes. Measured
        // cost: 19-21 KB of .so, 14 KB of it the 170 assertion messages.
        .define("ENABLE_HARDENING", None);
    if msvc {
        cc.define("USE_ALLOCA", None);
    } else {
        cc.define("VAR_ARRAYS", None).flag("-fvisibility=hidden");
    }

    let mut files: Vec<String> = Vec::new();
    files.extend(CELT.iter().map(|f| format!("{OPUS}/celt/{f}.c")));
    files.extend(SILK.iter().map(|f| format!("{OPUS}/silk/{f}.c")));
    files.extend(SILK_FIXED.iter().map(|f| format!("{OPUS}/silk/fixed/{f}.c")));
    files.extend(OPUS_SRC.iter().map(|f| format!("{OPUS}/src/{f}.c")));

    if arch == "aarch64" || arch == "arm" {
        cc.define("OPUS_ARM_MAY_HAVE_NEON_INTR", None).define("OPUS_ARM_PRESUME_NEON_INTR", None);
        if arch == "aarch64" {
            cc.define("OPUS_ARM_PRESUME_AARCH64_NEON_INTR", None);
        } else {
            // The EDSP / media inline asm does the fixed-point multiplies (ARMv7-A, Thumb-2 too).
            cc.flag("-mfpu=neon")
                .define("OPUS_ARM_INLINE_ASM", None)
                .define("OPUS_ARM_INLINE_EDSP", None)
                .define("OPUS_ARM_INLINE_MEDIA", None);
        }
        files.extend(NEON.iter().map(|f| format!("{OPUS}/{f}.c")));
    }

    cc.files(&files).compile("opus");
}
