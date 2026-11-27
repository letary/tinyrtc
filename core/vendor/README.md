# vendor

Crates.io crates carried with a patch, wired in `core/Cargo.toml` under `[patch.crates-io]`; and
libopus's C sources, which `core/build.rs` compiles.

## str0m-rust-crypto 0.7.0

Upstream: crates.io `str0m-rust-crypto` 0.7.0 (str0m git `5f2183ce7c67117cd1960dfeb11bed11fc188b66`,
`crypto/rust-crypto`). The copy is the published crate (its normalized `Cargo.toml`, `README.md`,
`src/`) with the patch below; every changed spot says `PATCH (tinyrtc)`.

**Why.** The crate enables dimpl's `rcgen` feature to make the self-signed DTLS certificate, and
in dimpl 0.9 `rcgen` turns on dimpl's `aws-lc-rs` feature (rcgen with its `aws_lc_rs` backend). So
the "pure Rust" backend links aws-lc (C). It is not only the certificate: with `aws-lc-rs` on,
dimpl's `Config::build` picks its aws-lc crypto provider over the RustCrypto one, so the DTLS
handshake ran on aws-lc too. A feature edge cannot be turned off downstream, and not calling the
code does not remove it, so the fix has to be in this crate's manifest. dimpl itself needs no
patch: without `rcgen` it builds with `rust-crypto` alone (only its `certificate` module goes).

**What.**

- `Cargo.toml`: dimpl features `["rust-crypto", "rcgen"]` become `["rust-crypto"]`; p256 gains
  `getrandom` (the key and the serial come from the OS generator through
  `elliptic_curve::Generate`; dimpl's tree has it on already, this keeps the crate standalone).
- `src/cert.rs` (new): the certificate rcgen made, on p256 / ecdsa / sha2 (all already deps) and
  hand-written DER: X.509 v3, ECDSA P-256 key, ecdsa-with-SHA256 self-signature,
  `O=DTLS, CN=DTLS Peer` as subject and issuer, a random 16-byte positive serial (Firefox wants
  unique serials, str0m#517), valid from now for 365 days, no extensions (rcgen wrote none for a
  non-CA certificate without SANs). The private key is PKCS#8 DER around a SEC1 key with its
  public key, as rcgen / OpenSSL write it.
- `src/dtls.rs`: `generate_certificate` calls `cert::generate_self_signed` instead of
  `dimpl::certificate::generate_self_signed_certificate`.
- `src/lib.rs`: `mod cert;`.

The SDP fingerprint is unchanged: str0m takes SHA-256 of the certificate DER itself.
`core`'s test `dtls_cert_is_canonical_x509` checks what the loopback cannot (self-signature over
the canonical TBS, key pairing, serial, validity).

**Re-applying on a str0m bump.** Copy the new published `str0m-rust-crypto` from
`~/.cargo/registry/src/index.crates.io-*/` over this folder (minus `Cargo.lock`,
`Cargo.toml.orig`, `.cargo_vcs_info.json`, `.cargo-ok`), redo the four points, then check that
`cargo tree -e normal -i aws-lc-sys --target aarch64-linux-android` finds nothing. The patch
can be dropped once upstream generates the certificate without rcgen (or dimpl's `rcgen` stops
implying `aws-lc-rs`); `diff -ru <registry copy> vendor/str0m-rust-crypto` is the patch to send.

## opus 1.6.1

Upstream: the libopus 1.6.1 release, `https://downloads.xiph.org/releases/opus/opus-1.6.1.tar.gz`,
SHA-256 `6ffcb593207be92584df15b32466ed64bbec99109f007c82205f0194572411a1` (the release's
`SHA256SUMS.txt`). BSD-3-Clause (`opus/COPYING`). No patch: every file here is the tarball's, byte
for byte.

**Why the sources, compiled by `build.rs` with `cc`.** Every libopus binding on crates.io builds
it some other way: cmake (`audiopus_sys`, `opusic-sys`, `opus-head-sys`, `libopus_sys`,
`opus-static-sys`, `opus-codec`), autotools (`opus-sys`, `opus-embedded-sys`), pkg-config and a
system libopus (`media-codec-opus-sys`), or a prebuilt it downloads (`shiguredo_opus`); several
build DRED / OSCE by default. `unsafe-libopus` is libopus 1.3.1 through c2rust: float only, no
SIMD. With the sources and `cc`, the build machine needs only the target's C compiler, and
`build.rs` names every source and define: what is compiled, FIXED_POINT, NEON, -Os (its header says
what and why).

**What was taken.** `include/`, `celt/`, `silk/`, `src/`, `COPYING`, `AUTHORS`, `README`,
`package_version`. Left out, as nothing here compiles them: `dnn/` (DRED, deep PLC, OSCE: 23 MB of
weights and code, off on purpose); the x86 and MIPS SIMD dirs and NE10 (`*_ne10.*`); the ARM
external assembly (`celt/arm/*.s`, `*-gnu.S`, `arm2gnu.pl`, `armopts.s.in`); the tests and demo
programs (`tests/`, `celt/tests`, `silk/tests`, `src/opus_demo.c`, `opus_compare.c`,
`qext_compare.c`, `repacketizer_demo.c`, `celt/opus_custom_demo.c`, `celt/mini_kfft.c`); the build
systems (autotools with its `*.mk` lists, CMake, meson) and the release's other documents (`doc/`,
`NEWS`, `ChangeLog`, `INSTALL`). Kept and not compiled now: the float SILK, the float API's
analysis, multistream / projection: a `build.rs` edit away.

**Bumping.** Fetch the new tarball and `SHA256SUMS.txt` from `downloads.xiph.org/releases/opus/`,
check the hash, replace the kept parts with the new release's and leave out the same ones. Then
hold `build.rs` against the release: its source lists against `celt_sources.mk`,
`silk_sources.mk`, `opus_sources.mk`; its defines against `configure.ac`'s defaults
(`AC_ARG_ENABLE`: anything new that is on by default, such as `ENABLE_RES24` in 1.6). Then
`cargo test --release` and the size build (README).
