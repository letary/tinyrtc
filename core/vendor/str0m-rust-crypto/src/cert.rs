//! Self-signed DTLS certificate on RustCrypto alone (PATCH, tinyrtc; see `core/vendor/README.md`).
//!
//! Stands in for `dimpl::certificate::generate_self_signed_certificate`, which is rcgen on aws-lc.
//! The certificate is the one that function made: X.509 v3, an ECDSA P-256 key, an
//! ecdsa-with-SHA256 self-signature, subject = issuer = `O=DTLS, CN=DTLS Peer`, a random 16-byte
//! serial (Firefox wants serials unique across processes and machines: str0m#517), valid from now
//! for 365 days, no extensions. The key is PKCS#8 DER, as rcgen's `serialize_der`; it is the
//! format dimpl's key loader tries first. The DER is written by hand: it is fixed but for the key,
//! the serial, the dates and the signature.

use std::time::{SystemTime, UNIX_EPOCH};

use p256::ecdsa::signature::hazmat::PrehashSigner;
use p256::ecdsa::{Signature, SigningKey};
use p256::elliptic_curve::Generate;
use sha2::{Digest, Sha256};
use str0m_proto::crypto::dtls::DtlsCert;

// Whole DER elements: tag, length, body.
const OID_EC_PUBLIC_KEY: &[u8] = &[0x06, 0x07, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01]; // 1.2.840.10045.2.1
const OID_P256: &[u8] = &[0x06, 0x08, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07]; // 1.2.840.10045.3.1.7
const OID_ECDSA_SHA256: &[u8] = &[0x06, 0x08, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02]; // 1.2.840.10045.4.3.2
const OID_O: &[u8] = &[0x06, 0x03, 0x55, 0x04, 0x0A]; // 2.5.4.10
const OID_CN: &[u8] = &[0x06, 0x03, 0x55, 0x04, 0x03]; // 2.5.4.3
const VERSION_V3: &[u8] = &[0xA0, 0x03, 0x02, 0x01, 0x02]; // [0] EXPLICIT INTEGER 2
const INTEGER_0: &[u8] = &[0x02, 0x01, 0x00];
const INTEGER_1: &[u8] = &[0x02, 0x01, 0x01];

const SEQUENCE: u8 = 0x30;
const SET: u8 = 0x31;
const INTEGER: u8 = 0x02;
const BIT_STRING: u8 = 0x03;
const OCTET_STRING: u8 = 0x04;
const UTF8_STRING: u8 = 0x0C;
const UTC_TIME: u8 = 0x17;
const GENERALIZED_TIME: u8 = 0x18;
const CONTEXT_1: u8 = 0xA1;

const DAY: u64 = 86_400;

pub(crate) fn generate_self_signed() -> Option<DtlsCert> {
    let key = SigningKey::try_generate().ok()?;
    let mut serial = <[u8; 16]>::try_generate().ok()?;
    // Positive, and no leading zero byte to strip: the INTEGER is minimal as it stands.
    serial[0] = (serial[0] & 0x7F) | 0x40;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();

    // 04 || X || Y, in a BIT STRING with no unused bits.
    let point = key.verifying_key().to_sec1_point(false);
    let public = tlv(BIT_STRING, &[&[0], point.as_bytes()]);
    let key_algorithm = tlv(SEQUENCE, &[OID_EC_PUBLIC_KEY, OID_P256]);
    let signature_algorithm = tlv(SEQUENCE, &[OID_ECDSA_SHA256]);
    let o = tlv(SEQUENCE, &[OID_O, &tlv(UTF8_STRING, &[b"DTLS"])]);
    let cn = tlv(SEQUENCE, &[OID_CN, &tlv(UTF8_STRING, &[b"DTLS Peer"])]);
    let name = tlv(SEQUENCE, &[&tlv(SET, &[&o]), &tlv(SET, &[&cn])]);

    let tbs = tlv(
        SEQUENCE,
        &[
            VERSION_V3,
            &tlv(INTEGER, &[&serial]),
            &signature_algorithm,
            &name,
            &tlv(SEQUENCE, &[&time(now), &time(now + 365 * DAY)]),
            &name,
            &tlv(SEQUENCE, &[&key_algorithm, &public]),
        ],
    );
    let signature: Signature = key.sign_prehash(&Sha256::digest(&tbs)).ok()?;
    let signature = signature.to_der();
    let certificate = tlv(
        SEQUENCE,
        &[
            &tbs,
            &signature_algorithm,
            &tlv(BIT_STRING, &[&[0], signature.as_bytes()]),
        ],
    );

    // PrivateKeyInfo (RFC 5208) around an ECPrivateKey (RFC 5915) with its public key.
    let ec_private_key = tlv(
        SEQUENCE,
        &[
            INTEGER_1,
            &tlv(OCTET_STRING, &[&key.to_bytes()]),
            &tlv(CONTEXT_1, &[&public]),
        ],
    );
    let private_key = tlv(
        SEQUENCE,
        &[
            INTEGER_0,
            &key_algorithm,
            &tlv(OCTET_STRING, &[&ec_private_key]),
        ],
    );

    Some(DtlsCert {
        certificate,
        private_key,
    })
}

/// One DER element: the tag, the definite length, then the parts back to back.
fn tlv(tag: u8, parts: &[&[u8]]) -> Vec<u8> {
    let len: usize = parts.iter().map(|p| p.len()).sum();
    let mut out = Vec::with_capacity(len + 4);
    out.push(tag);
    match len {
        0..0x80 => out.push(len as u8),
        0x80..0x100 => out.extend_from_slice(&[0x81, len as u8]),
        _ => out.extend_from_slice(&[0x82, (len >> 8) as u8, len as u8]),
    }
    for p in parts {
        out.extend_from_slice(p);
    }
    out
}

/// A UTC time as RFC 5280 4.1.2.5 wants it: UTCTime through 2049, GeneralizedTime after.
fn time(unix: u64) -> Vec<u8> {
    // Days since 1970-01-01 to a civil date: H. Hinnant's `civil_from_days`, for days >= 0.
    let z = unix / DAY + 719_468;
    let (era, doe) = (z / 146_097, z % 146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    let secs = unix % DAY;

    let (tag, year_digits) = if year < 2050 {
        (UTC_TIME, 2)
    } else {
        (GENERALIZED_TIME, 4)
    };
    let mut text = Vec::with_capacity(15);
    let mut put = |v: u64, n: u32| {
        for i in (0..n).rev() {
            text.push(b'0' + (v / 10u64.pow(i) % 10) as u8);
        }
    };
    put(year, year_digits);
    for v in [month, day, secs / 3600, secs / 60 % 60, secs % 60] {
        put(v, 2);
    }
    text.push(b'Z');
    tlv(tag, &[&text])
}
