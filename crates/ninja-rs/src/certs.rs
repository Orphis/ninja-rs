//! The dates of the X.509 certificates TLS uses (RFC 5280), read from their
//! DER before anything connects: a server refusing an expired client
//! certificate may only close the connection, and one about to expire can
//! still be renewed. Nothing else is read, and signatures aren't checked:
//! TLS does that.

use std::time::{Duration, SystemTime};

/// How long before a certificate expires its use is warned about.
pub const EXPIRY_WARNING: Duration = Duration::from_secs(14 * 24 * 3600);

/// When a certificate is valid: its notBefore and notAfter, in seconds
/// since the Unix epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Validity {
    pub not_before: i64,
    pub not_after: i64,
}

/// The DER of the `CERTIFICATE` sections of PEM `data`, in order.
pub fn pem_certificates(data: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let mut certs = Vec::new();
    let mut body: Option<Vec<u8>> = None;
    for line in data.split(|&b| b == b'\n').map(<[u8]>::trim_ascii) {
        match &mut body {
            None => {
                if line == b"-----BEGIN CERTIFICATE-----" {
                    body = Some(Vec::new());
                }
            }
            Some(b) if line == b"-----END CERTIFICATE-----" => {
                certs.push(base64(b).map_err(|e| format!("PEM certificate {}: {e}", certs.len() + 1))?);
                body = None;
            }
            Some(b) => b.extend_from_slice(line),
        }
    }
    if body.is_some() {
        return Err(format!("PEM certificate {} has no END line", certs.len() + 1));
    }
    Ok(certs)
}

/// Decodes base64, whitespace ignored.
fn base64(text: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut acc, mut bits, mut padded) = (0u32, 0, false);
    for &c in text {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                padded = true;
                continue;
            }
            c if c.is_ascii_whitespace() => continue,
            c => return Err(format!("'{}' isn't base64", c.escape_ascii())),
        };
        if padded {
            return Err("base64 after its padding".into());
        }
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

const INTEGER: u8 = 0x02;
const SEQUENCE: u8 = 0x30;
const UTC_TIME: u8 = 0x17;
const GENERALIZED_TIME: u8 = 0x18;
/// `[0] EXPLICIT`: the version of a tbsCertificate.
const VERSION: u8 = 0xa0;

/// The DER element `data` starts with: its tag, its contents, and what
/// follows it.
fn element(data: &[u8]) -> Result<(u8, &[u8], &[u8]), String> {
    let truncated = || "truncated DER".to_string();
    let (&tag, rest) = data.split_first().ok_or_else(truncated)?;
    if tag & 0x1f == 0x1f {
        return Err(format!("DER tag {tag:#04x} of several bytes"));
    }
    let (&first, rest) = rest.split_first().ok_or_else(truncated)?;
    let (len, rest) = match first {
        n if n < 0x80 => (usize::from(n), rest),
        0x80 => return Err("an indefinite length (BER, not DER)".into()),
        n => {
            let count = usize::from(n & 0x7f);
            if count > 4 {
                return Err(format!("a DER length of {count} bytes"));
            }
            let bytes = rest.get(..count).ok_or_else(truncated)?;
            (bytes.iter().fold(0usize, |l, &b| (l << 8) | usize::from(b)), &rest[count..])
        }
    };
    let contents = rest.get(..len).ok_or_else(truncated)?;
    Ok((tag, contents, &rest[len..]))
}

/// The DER element `data` starts with, which must be a `tag`: `what`.
fn expect<'a>(data: &'a [u8], tag: u8, what: &str) -> Result<(&'a [u8], &'a [u8]), String> {
    match element(data)? {
        (t, contents, rest) if t == tag => Ok((contents, rest)),
        (t, ..) => Err(format!("{what}: DER tag {t:#04x}, not {tag:#04x}")),
    }
}

/// A certificate's validity, from its DER: Certificate, tbsCertificate,
/// its version, serialNumber, signature and issuer, then validity.
pub fn validity(der: &[u8]) -> Result<Validity, String> {
    let (cert, rest) = expect(der, SEQUENCE, "the certificate")?;
    if !rest.is_empty() {
        return Err("data after the certificate".into());
    }
    let (mut tbs, _) = expect(cert, SEQUENCE, "tbsCertificate")?;
    if tbs.first() == Some(&VERSION) {
        tbs = element(tbs)?.2;
    }
    for (tag, what) in [(INTEGER, "serialNumber"), (SEQUENCE, "signature"), (SEQUENCE, "issuer")] {
        tbs = expect(tbs, tag, what)?.1;
    }
    let (dates, _) = expect(tbs, SEQUENCE, "validity")?;
    let (not_before, rest) = time(dates).map_err(|e| format!("notBefore: {e}"))?;
    let (not_after, rest) = time(rest).map_err(|e| format!("notAfter: {e}"))?;
    if !rest.is_empty() {
        return Err("validity: more than two dates".into());
    }
    Ok(Validity { not_before, not_after })
}

/// A time of RFC 5280, in seconds since the epoch, and what follows it:
/// UTCTime (`YYMMDDHHMMSSZ`, years from 1950 to 2049) or GeneralizedTime
/// (`YYYYMMDDHHMMSSZ`).
fn time(data: &[u8]) -> Result<(i64, &[u8]), String> {
    let (tag, text, rest) = element(data)?;
    let digits = match tag {
        UTC_TIME => 12,
        GENERALIZED_TIME => 14,
        t => return Err(format!("DER tag {t:#04x} isn't a time")),
    };
    let bad = || format!("'{}' isn't a time of RFC 5280", text.escape_ascii());
    let Some(text) = text.strip_suffix(b"Z").filter(|t| t.len() == digits && t.iter().all(u8::is_ascii_digit)) else {
        return Err(bad());
    };
    let number = |range: std::ops::Range<usize>| text[range].iter().fold(0i64, |n, &d| n * 10 + i64::from(d - b'0'));
    let (year, at) = match tag {
        UTC_TIME => {
            let yy = number(0..2);
            (if yy >= 50 { 1900 + yy } else { 2000 + yy }, 2)
        }
        _ => (number(0..4), 4),
    };
    let field = |i: usize| number(at + 2 * i..at + 2 * i + 2);
    let (month, day, hour, minute, second) = (field(0), field(1), field(2), field(3), field(4));
    let days = days_from_civil(year, month, day);
    if !(1..=12).contains(&month) || civil_from_days(days) != (year, month, day) || hour > 23 || minute > 59 || second > 59 {
        return Err(bad());
    }
    Ok((days * 86400 + hour * 3600 + minute * 60 + second, rest))
}

/// Days since 1970-01-01 of a date of the proleptic Gregorian calendar
/// (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

/// The date `days` after 1970-01-01: year, month, day.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (day, month) = (doy - (153 * mp + 2) / 5 + 1, if mp < 10 { mp + 3 } else { mp - 9 });
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

/// Seconds since the epoch as `2026-10-01 12:00 UTC`.
pub fn format_time(secs: i64) -> String {
    let (year, month, day) = civil_from_days(secs.div_euclid(86400));
    let in_day = secs.rem_euclid(86400);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02} UTC", in_day / 3600, in_day % 3600 / 60)
}

/// Seconds since the epoch of `t`.
fn unix(t: SystemTime) -> i64 {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX),
    }
}

/// Checks the dates of the certificates (DER) of one file at `now`: `kind`
/// says what they are (`client`, `CA`), `source` where they come from
/// (`--tls_certificate=PATH`). One that expired, isn't valid yet or can't be
/// read is an error; one expiring within `EXPIRY_WARNING` is a warning
/// (about the first to expire, once per file).
pub fn check(kind: &str, source: &str, certs: &[Vec<u8>], now: SystemTime) -> Result<Option<String>, String> {
    let name = |i: usize| {
        if certs.len() == 1 { format!("the {kind} certificate ({source})") } else { format!("{kind} certificate {} of {} ({source})", i + 1, certs.len()) }
    };
    let now = unix(now);
    let mut soonest: Option<(usize, i64)> = None;
    for (i, der) in certs.iter().enumerate() {
        let v = validity(der).map_err(|e| format!("{} can't be read: {e}", name(i)))?;
        if now > v.not_after {
            return Err(format!("{} expired on {}", name(i), format_time(v.not_after)));
        }
        if now < v.not_before {
            return Err(format!("{} is not valid until {} (is the clock right? it says {})", name(i), format_time(v.not_before), format_time(now)));
        }
        if v.not_after - now < EXPIRY_WARNING.as_secs() as i64 && soonest.is_none_or(|(_, t)| v.not_after < t) {
            soonest = Some((i, v.not_after));
        }
    }
    Ok(soonest.map(|(i, not_after)| {
        let left = match (not_after - now) / 86400 {
            0 => "in less than a day".to_string(),
            1 => "in 1 day".to_string(),
            n => format!("in {n} days"),
        };
        format!("{} expires on {}, {left}", name(i), format_time(not_after))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A DER element.
    fn tlv(tag: u8, contents: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        match contents.len() {
            n if n < 0x80 => out.push(n as u8),
            n if n < 0x100 => out.extend([0x81, n as u8]),
            n => out.extend([0x82, (n >> 8) as u8, n as u8]),
        }
        out.extend_from_slice(contents);
        out
    }

    /// A certificate with these dates (UTCTime or GeneralizedTime, as
    /// their tags say), and fields as X.509 orders them.
    fn certificate(not_before: (u8, &str), not_after: (u8, &str), version: bool) -> Vec<u8> {
        let name = tlv(SEQUENCE, &tlv(0x31, &tlv(SEQUENCE, &[tlv(0x06, &[0x55, 0x04, 0x03]), tlv(0x0c, b"test")].concat())));
        let algorithm = tlv(SEQUENCE, &tlv(0x06, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02]));
        let dates = tlv(SEQUENCE, &[tlv(not_before.0, not_before.1.as_bytes()), tlv(not_after.0, not_after.1.as_bytes())].concat());
        let mut tbs = if version { tlv(VERSION, &tlv(INTEGER, &[2])) } else { Vec::new() };
        for part in [tlv(INTEGER, &[0x01, 0x23]), algorithm.clone(), name.clone(), dates, name, tlv(SEQUENCE, &[0; 40])] {
            tbs.extend(part);
        }
        tlv(SEQUENCE, &[tlv(SEQUENCE, &tbs), algorithm, tlv(0x03, &[0, 1, 2, 3])].concat())
    }

    fn at(date: &str) -> i64 {
        let (d, t) = date.split_once(' ').unwrap();
        let p: Vec<i64> = d.split('-').chain(t.split(':')).map(|n| n.parse().unwrap()).collect();
        days_from_civil(p[0], p[1], p[2]) * 86400 + p[3] * 3600 + p[4] * 60 + p[5]
    }

    #[test]
    fn utc_and_generalized_times() {
        let v = validity(&certificate((UTC_TIME, "261001120000Z"), (GENERALIZED_TIME, "21260916233414Z"), true)).unwrap();
        assert_eq!(v, Validity { not_before: at("2026-10-01 12:00:00"), not_after: at("2126-09-16 23:34:14") });
        assert_eq!(format_time(v.not_before), "2026-10-01 12:00 UTC");
        assert_eq!(format_time(v.not_after), "2126-09-16 23:34 UTC");
        // Version 1 certificates have no version field.
        let v = validity(&certificate((UTC_TIME, "700101000000Z"), (UTC_TIME, "000229000001Z"), false)).unwrap();
        assert_eq!((v.not_before, v.not_after), (0, at("2000-02-29 00:00:01")));
        assert_eq!(format_time(-1), "1969-12-31 23:59 UTC");
    }

    /// UTCTime's years from 50 are 19xx, below 20xx (RFC 5280, 4.1.2.5.1).
    #[test]
    fn utc_time_years_from_1950_to_2049() {
        let year = |yy: &str| {
            let v = validity(&certificate((UTC_TIME, &format!("{yy}0101000000Z")), (UTC_TIME, "491231235959Z"), true)).unwrap();
            civil_from_days(v.not_before.div_euclid(86400)).0
        };
        assert_eq!((year("50"), year("99"), year("00"), year("49")), (1950, 1999, 2000, 2049));
        let v = validity(&certificate((UTC_TIME, "500101000000Z"), (UTC_TIME, "491231235959Z"), true)).unwrap();
        assert_eq!(format_time(v.not_after), "2049-12-31 23:59 UTC");
    }

    #[test]
    fn malformed_certificates() {
        let good = certificate((UTC_TIME, "261001120000Z"), (UTC_TIME, "271001120000Z"), true);
        assert!(validity(&good).is_ok());
        for (what, der) in [
            ("empty", vec![]),
            ("truncated", good[..good.len() - 1].to_vec()),
            ("trailing data", [good.clone(), vec![0]].concat()),
            ("not a sequence", tlv(INTEGER, &[1])),
            ("an indefinite length", [vec![SEQUENCE, 0x80], good[2..].to_vec()].concat()),
            ("a length too long", vec![SEQUENCE, 0x85, 1, 0, 0, 0, 0]),
            ("no Z", certificate((UTC_TIME, "261001120000"), (UTC_TIME, "271001120000Z"), true)),
            ("an offset", certificate((UTC_TIME, "2610011200+0100"), (UTC_TIME, "271001120000Z"), true)),
            ("no seconds", certificate((UTC_TIME, "2610011200Z"), (UTC_TIME, "271001120000Z"), true)),
            ("a fraction", certificate((GENERALIZED_TIME, "20261001120000.5Z"), (UTC_TIME, "271001120000Z"), true)),
            ("February 30th", certificate((UTC_TIME, "260230120000Z"), (UTC_TIME, "271001120000Z"), true)),
            ("month 13", certificate((UTC_TIME, "261301120000Z"), (UTC_TIME, "271001120000Z"), true)),
            ("24 o'clock", certificate((UTC_TIME, "261001240000Z"), (UTC_TIME, "271001120000Z"), true)),
            ("not digits", certificate((UTC_TIME, "26100112000xZ"), (UTC_TIME, "271001120000Z"), true)),
            ("not a time", certificate((0x0c, "261001120000Z"), (UTC_TIME, "271001120000Z"), true)),
        ] {
            assert!(validity(&der).is_err(), "{what}: {:?}", validity(&der));
        }
        // PEM: a section without its end, or that isn't base64.
        assert!(pem_certificates(b"-----BEGIN CERTIFICATE-----\nMIIB\n").is_err());
        assert!(pem_certificates(b"-----BEGIN CERTIFICATE-----\nMI*B\n-----END CERTIFICATE-----\n").is_err());
        assert!(pem_certificates(b"-----BEGIN CERTIFICATE-----\nMA==MA==\n-----END CERTIFICATE-----\n").is_err());
    }

    #[test]
    fn pem_sections() {
        let pem = "junk\r\n-----BEGIN CERTIFICATE-----\r\nMAMC\r\nAQE=\r\n-----END CERTIFICATE-----\r\n-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n\
                   -----BEGIN CERTIFICATE-----\nMAA=\n-----END CERTIFICATE-----\n";
        assert_eq!(pem_certificates(pem.as_bytes()).unwrap(), [vec![0x30, 0x03, 0x02, 0x01, 0x01], vec![0x30, 0x00]]);
        assert!(pem_certificates(b"no certificate").unwrap().is_empty());
    }

    #[test]
    fn checks_dates() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(at("2026-10-15 10:00:00") as u64);
        let cert = |from: &str, to: &str| certificate((GENERALIZED_TIME, from), (GENERALIZED_TIME, to), true);
        let year = cert("20261001000000Z", "20271001000000Z");
        let soon = cert("20261001000000Z", "20261018120000Z");
        let sooner = cert("20261001000000Z", "20261016000000Z");
        let expired = cert("20250101000000Z", "20261001120000Z");
        let future = cert("20270101000000Z", "20280101000000Z");
        let check = |certs: &[&Vec<u8>]| check("CA", "--tls_certificate=/ca.pem", &certs.iter().map(|c| (*c).clone()).collect::<Vec<_>>(), now);
        assert_eq!(check(&[&year]), Ok(None));
        assert_eq!(check(&[&soon]), Ok(Some("the CA certificate (--tls_certificate=/ca.pem) expires on 2026-10-18 12:00 UTC, in 3 days".into())));
        // The first of a bundle to expire.
        assert_eq!(
            check(&[&year, &soon, &sooner]),
            Ok(Some("CA certificate 3 of 3 (--tls_certificate=/ca.pem) expires on 2026-10-16 00:00 UTC, in less than a day".into()))
        );
        assert_eq!(check(&[&year, &expired, &soon]), Err("CA certificate 2 of 3 (--tls_certificate=/ca.pem) expired on 2026-10-01 12:00 UTC".into()));
        let early = check(&[&future]).unwrap_err();
        assert_eq!(
            early,
            "the CA certificate (--tls_certificate=/ca.pem) is not valid until 2027-01-01 00:00 UTC (is the clock right? it says 2026-10-15 10:00 UTC)"
        );
        let err = check(&[&year, &vec![0x30, 0x01]]).unwrap_err();
        assert!(err.starts_with("CA certificate 2 of 2 (--tls_certificate=/ca.pem) can't be read: "), "{err}");
        // On the threshold: not yet.
        let exactly = SystemTime::UNIX_EPOCH + Duration::from_secs(at("2026-10-18 12:00:00") as u64) - EXPIRY_WARNING;
        assert_eq!(super::check("CA", "x", std::slice::from_ref(&soon), exactly), Ok(None));
        assert!(super::check("CA", "x", &[soon], exactly + Duration::from_secs(1)).unwrap().is_some());
    }
}
