//! Certificates with the dates a test wants, as PEM. A copy of a real one
//! with other dates no longer matches its signature: it serves checks made
//! before connecting, a client whose certificate the server doesn't ask
//! for, or a server whose certificate a client refuses for its dates (they
//! are checked before the signature).

use std::time::{SystemTime, UNIX_EPOCH};

/// The first certificate of `pem`, valid from `not_before` to `not_after`.
pub fn with_validity(pem: &[u8], not_before: SystemTime, not_after: SystemTime) -> String {
    let der = first_certificate(pem);
    let (tbs, signature) = split(contents(&der));
    let mut fields = Vec::new();
    let mut rest = contents(tbs);
    while !rest.is_empty() {
        let (field, after) = split(rest);
        fields.push(field.to_vec());
        rest = after;
    }
    // After the version ([0], when there is one), serialNumber, signature
    // and issuer.
    let at = if fields[0][0] == 0xa0 { 4 } else { 3 };
    fields[at] = validity(not_before, not_after);
    to_pem(&tlv(0x30, &[tlv(0x30, &fields.concat()), signature.to_vec()].concat()))
}

/// A certificate made up, valid from `not_before` to `not_after`: well
/// formed, its key and signature made up too.
pub fn made_up(not_before: SystemTime, not_after: SystemTime) -> String {
    let name = tlv(0x30, &tlv(0x31, &tlv(0x30, &[tlv(0x06, &[0x55, 0x04, 0x03]), tlv(0x0c, b"made up")].concat())));
    // ecdsa-with-SHA256, and a P-256 key.
    let algorithm = tlv(0x30, &tlv(0x06, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02]));
    let key_type = tlv(0x30, &[tlv(0x06, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01]), tlv(0x06, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07])].concat());
    let key = tlv(0x30, &[key_type, tlv(0x03, &[[0, 4].as_slice(), &[7; 64]].concat())].concat());
    let fields = [tlv(0xa0, &tlv(0x02, &[2])), tlv(0x02, &[0x01, 0x23]), algorithm.clone(), name.clone(), validity(not_before, not_after), name, key];
    to_pem(&tlv(0x30, &[tlv(0x30, &fields.concat()), algorithm, tlv(0x03, &[0, 0x30, 0x00])].concat()))
}

/// `der` as a PEM certificate.
pub fn to_pem(der: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut text = String::new();
    for chunk in der.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | (u32::from(b) << (16 - 8 * i)));
        for i in 0..4 {
            text.push(if i <= chunk.len() { ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    let lines: Vec<&str> = text.as_bytes().chunks(64).map(|l| std::str::from_utf8(l).unwrap()).collect();
    format!("-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n", lines.join("\n"))
}

/// The DER of the first certificate of `pem`.
fn first_certificate(pem: &[u8]) -> Vec<u8> {
    let text = std::str::from_utf8(pem).expect("PEM");
    let body = text.split("-----BEGIN CERTIFICATE-----").nth(1).and_then(|b| b.split("-----END CERTIFICATE-----").next()).expect("a PEM certificate");
    let (mut out, mut acc, mut bits) = (Vec::new(), 0u32, 0);
    for c in body.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=') {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            _ => 63,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// A DER element.
fn tlv(tag: u8, contents: &[u8]) -> Vec<u8> {
    let len = contents.len();
    let mut out = vec![tag];
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes: Vec<u8> = len.to_be_bytes().into_iter().skip_while(|&b| b == 0).collect();
        out.push(0x80 | bytes.len() as u8);
        out.extend(bytes);
    }
    out.extend_from_slice(contents);
    out
}

/// The DER element `data` starts with, whole, and what follows it.
fn split(data: &[u8]) -> (&[u8], &[u8]) {
    let (header, len) = match data[1] {
        n if n < 0x80 => (2, usize::from(n)),
        n => {
            let count = usize::from(n & 0x7f);
            (2 + count, data[2..2 + count].iter().fold(0, |l, &b| (l << 8) | usize::from(b)))
        }
    };
    data.split_at(header + len)
}

/// The contents of the DER element `element`.
fn contents(element: &[u8]) -> &[u8] {
    let header = if element[1] < 0x80 { 2 } else { 2 + usize::from(element[1] & 0x7f) };
    &element[header..]
}

/// A Validity: UTCTime for the years 1950 to 2049, GeneralizedTime for
/// others, as RFC 5280 wants them.
fn validity(not_before: SystemTime, not_after: SystemTime) -> Vec<u8> {
    let time = |t: SystemTime| {
        let secs = t.duration_since(UNIX_EPOCH).expect("a time after 1970").as_secs();
        let (days, in_day) = (secs / 86400, secs % 86400);
        // Howard Hinnant's civil_from_days.
        let z = days + 719_468;
        let (era, doe) = (z / 146_097, z % 146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let (day, month) = (doy - (153 * mp + 2) / 5 + 1, if mp < 10 { mp + 3 } else { mp - 9 });
        let year = yoe + era * 400 + u64::from(month <= 2);
        let rest = format!("{month:02}{day:02}{:02}{:02}{:02}Z", in_day / 3600, in_day % 3600 / 60, in_day % 60);
        if (1950..2050).contains(&year) { tlv(0x17, format!("{:02}{rest}", year % 100).as_bytes()) } else { tlv(0x18, format!("{year:04}{rest}").as_bytes()) }
    };
    tlv(0x30, &[time(not_before), time(not_after)].concat())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn pem_round_trip() {
        for der in [vec![0x30, 0x00], vec![0x30, 0x01, 0x05], vec![0x30, 0x02, 0x05, 0x00], (0..=255).collect::<Vec<u8>>()] {
            assert_eq!(first_certificate(to_pem(&der).as_bytes()), der);
        }
        assert!(to_pem(&[0x30, 0x00]).contains("\nMAA=\n"));
    }

    #[test]
    fn dates_as_rfc_5280_wants_them() {
        let at = |secs: u64| UNIX_EPOCH + Duration::from_secs(secs);
        // 2026-10-01 12:00:00 and 2126-09-16 23:34:14.
        let v = validity(at(1_790_856_000), at(4_945_275_254));
        assert_eq!(&v[2..], &[tlv(0x17, b"261001120000Z"), tlv(0x18, b"21260916233414Z")].concat()[..]);
        let made = made_up(at(0), at(1_790_856_000));
        let copy = with_validity(made.as_bytes(), at(1_790_856_000), at(4_945_275_254));
        assert!(first_certificate(copy.as_bytes()).windows(v.len()).any(|w| w == v));
    }
}
