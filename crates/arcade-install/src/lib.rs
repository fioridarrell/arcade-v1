//! Check an Arcade Windows installer payload before anything is unpacked.
//!
//! The release workflow appends the release zip to `setup.exe` with
//! [`append_payload`]. The installer (scripts/windows/Verify-Payload.ps1) and
//! the release check (scripts/release/payload_trailer.py) use the same trailer.
//! [`extract_verified_payload`] returns the zip members only after the SHA-256
//! matches the hash embedded in the installer. A mismatch writes nothing.
//!
//! Trailer, immediately before the Authenticode certificate table when the file
//! is signed, otherwise at the end of the file. osslsigncode and signtool pad
//! the certificate table to an 8-byte boundary, so up to 7 bytes may sit
//! between the trailer and the certificate:
//!
//! ```text
//! [NSIS stub][payload zip][payload length, u64 little-endian][magic ARCDPAY1][0-7 pad][certificate]
//! ```
//!
//! The expected SHA-256 is compiled into the stub as UTF-16
//! `ARCADE_PAYLOAD_SHA256=` plus 64 hex characters (the version resource, which
//! Authenticode covers). Append the payload before signing. Signing afterwards
//! covers the stub, the payload and the trailer.

use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::path::{Component, Path};

/// Eight bytes at the end of the trailer. Not a secret; it only locates the payload.
pub const TRAILER_MAGIC: &[u8; 8] = b"ARCDPAY1";
/// Text in front of the 64 hex characters stored in the installer's version resource.
pub const EMBEDDED_HASH_MARKER: &str = "ARCADE_PAYLOAD_SHA256=";
const FOOTER_LEN: usize = 16;

#[derive(Debug, PartialEq, Eq)]
pub enum PayloadError {
    TooSmall,
    BadMagic,
    BadLength,
    AlreadySigned,
    EmptyPayload,
    BadHash,
    /// The payload's SHA-256 is not the hash embedded in the installer.
    Mismatch {
        expected: String,
        actual: String,
    },
    BadPe,
    Embedded(String),
    Zip(String),
    UnsafePath(String),
    Io(String),
}

impl std::fmt::Display for PayloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let detail = match self {
            PayloadError::TooSmall => "The installer is too small to carry a payload.".to_string(),
            PayloadError::BadMagic => {
                "This installer has no Arcade payload trailer (ARCDPAY1).".to_string()
            }
            PayloadError::BadLength => "The installer payload length is not valid.".to_string(),
            PayloadError::AlreadySigned => {
                "The installer stub is already Authenticode-signed. Append the payload before signing."
                    .to_string()
            }
            PayloadError::EmptyPayload => "The payload is empty.".to_string(),
            PayloadError::BadHash => "The embedded SHA-256 is not 64 hex characters.".to_string(),
            PayloadError::Mismatch { expected, actual } => format!(
                "Arcade payload SHA-256 does not match the hash embedded in this installer. Expected {expected}, got {actual}."
            ),
            PayloadError::BadPe => "This installer is not a readable PE file.".to_string(),
            PayloadError::Embedded(msg) => msg.clone(),
            PayloadError::Zip(msg) => format!("The payload zip could not be read ({msg})."),
            PayloadError::UnsafePath(name) => {
                format!("The payload zip contains an unsafe path ({name}).")
            }
            PayloadError::Io(msg) => format!("The payload could not be written ({msg})."),
        };
        write!(
            f,
            "{detail} Nothing was extracted and nothing was installed."
        )
    }
}

impl std::error::Error for PayloadError {}

fn io_err(err: io::Error) -> PayloadError {
    PayloadError::Io(err.to_string())
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn normalize_hash(expected: &str) -> Result<String, PayloadError> {
    let hash = expected.trim().to_ascii_lowercase();
    if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(hash)
    } else {
        Err(PayloadError::BadHash)
    }
}

/// Append `payload` to an unsigned installer stub. Sign the result afterwards.
pub fn append_payload(stub: &[u8], payload: &[u8]) -> Result<Vec<u8>, PayloadError> {
    if payload.is_empty() {
        return Err(PayloadError::EmptyPayload);
    }
    if security_directory(stub)?.is_some() {
        return Err(PayloadError::AlreadySigned);
    }
    let mut out = Vec::with_capacity(stub.len() + payload.len() + FOOTER_LEN);
    out.extend_from_slice(stub);
    out.extend_from_slice(payload);
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(TRAILER_MAGIC);
    Ok(out)
}

/// `(file offset, size)` of the Authenticode certificate table.
///
/// `Ok(None)` for a file that is not PE, or a PE with no certificate.
/// `Err(BadPe)` when the file starts with `MZ` but the header cannot be read.
fn security_directory(image: &[u8]) -> Result<Option<(usize, usize)>, PayloadError> {
    if image.len() < 0x40 || image[0] != b'M' || image[1] != b'Z' {
        return Ok(None);
    }
    let e_lfanew = u32_at(image, 0x3C).ok_or(PayloadError::BadPe)? as usize;
    if e_lfanew.saturating_add(24) > image.len() || &image[e_lfanew..e_lfanew + 4] != b"PE\0\0" {
        return Err(PayloadError::BadPe);
    }
    let coff = e_lfanew + 4;
    let opt_size = u16_at(image, coff + 16).ok_or(PayloadError::BadPe)? as usize;
    let opt = coff + 20;
    let magic = u16_at(image, opt).ok_or(PayloadError::BadPe)?;
    let (num_off, dd_off) = match magic {
        0x10B => (92, 96),
        0x20B => (108, 112),
        _ => return Err(PayloadError::BadPe),
    };
    if opt + dd_off > image.len() || opt_size < dd_off {
        return Err(PayloadError::BadPe);
    }
    let num = u32_at(image, opt + num_off).ok_or(PayloadError::BadPe)?;
    if num <= 4 {
        return Ok(None);
    }
    let entry = opt + dd_off + 4 * 8;
    let off = u32_at(image, entry).ok_or(PayloadError::BadPe)? as usize;
    let size = u32_at(image, entry + 4).ok_or(PayloadError::BadPe)? as usize;
    if off == 0 || size == 0 {
        return Ok(None);
    }
    if off > image.len() || size > image.len() - off {
        return Err(PayloadError::BadPe);
    }
    Ok(Some((off, size)))
}

fn content_end(image: &[u8]) -> Result<usize, PayloadError> {
    Ok(security_directory(image)?
        .map(|(off, _)| off)
        .unwrap_or(image.len()))
}

/// Payload bytes and the index where they start.
pub fn split_payload(image: &[u8]) -> Result<(usize, &[u8]), PayloadError> {
    let end = content_end(image)?;
    if end < FOOTER_LEN {
        return Err(PayloadError::TooSmall);
    }
    let mut magic_at = None;
    for pad in 0..8 {
        let at = match end.checked_sub(pad + 8) {
            Some(at) if at >= 8 => at,
            _ => break,
        };
        if &image[at..at + 8] == TRAILER_MAGIC {
            magic_at = Some(at);
            break;
        }
    }
    let magic_at = magic_at.ok_or(PayloadError::BadMagic)?;
    let len_at = magic_at - 8;
    let payload_len = u64::from_le_bytes(image[len_at..len_at + 8].try_into().unwrap()) as usize;
    if payload_len == 0 || payload_len > len_at {
        return Err(PayloadError::BadLength);
    }
    let start = len_at - payload_len;
    Ok((start, &image[start..len_at]))
}

/// SHA-256 carried in the installer stub, before the payload. Copies of the
/// marker inside the payload are ignored. Two different hashes is an error.
pub fn embedded_payload_sha256(image: &[u8]) -> Result<String, PayloadError> {
    let region = match split_payload(image) {
        Ok((start, _)) => &image[..start],
        Err(PayloadError::BadMagic) => image,
        Err(other) => return Err(other),
    };
    let found = hashes_in(region);
    match found.len() {
        1 => Ok(found[0].clone()),
        n => Err(PayloadError::Embedded(format!(
            "Expected one embedded payload SHA-256, found {n}."
        ))),
    }
}

fn hashes_in(region: &[u8]) -> Vec<String> {
    let mut found = Vec::new();
    let mut push = |raw: &str| {
        let hash = raw.trim().to_ascii_lowercase();
        if hash.len() >= 64 {
            let hash = hash[..64].to_string();
            if hash.bytes().all(|b| b.is_ascii_hexdigit()) && !found.contains(&hash) {
                found.push(hash);
            }
        }
    };
    let marker_u: Vec<u8> = EMBEDDED_HASH_MARKER
        .encode_utf16()
        .flat_map(|c| c.to_le_bytes())
        .collect();
    let mut from = 0;
    while let Some(rel) = find_subslice(&region[from..], &marker_u) {
        let i = from + rel + marker_u.len();
        let end = (i + 128).min(region.len());
        if let Ok(text) = decode_utf16(&region[i..end]) {
            push(&text);
        }
        from = (from + rel + 2).min(region.len());
    }
    let marker_a = EMBEDDED_HASH_MARKER.as_bytes();
    from = 0;
    while let Some(rel) = find_subslice(&region[from..], marker_a) {
        let i = from + rel + marker_a.len();
        let end = (i + 64).min(region.len());
        push(&String::from_utf8_lossy(&region[i..end]));
        from = (from + rel + 1).min(region.len());
    }
    found
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn decode_utf16(bytes: &[u8]) -> Result<String, ()> {
    if bytes.len() < 2 {
        return Err(());
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16(&units).map_err(|_| ())
}

/// The payload, if its SHA-256 is `expected`. Does not write anything.
pub fn verify_payload<'a>(image: &'a [u8], expected: &str) -> Result<&'a [u8], PayloadError> {
    let expected = normalize_hash(expected)?;
    let (_, payload) = split_payload(image)?;
    let actual = sha256_hex(payload);
    if actual != expected {
        return Err(PayloadError::Mismatch { expected, actual });
    }
    Ok(payload)
}

/// Unpack a stored (method 0) zip payload into `dest` only when its SHA-256 matches.
///
/// On failure `dest` is not created. The Windows installer does the same check
/// and then uses Expand-Archive, which also accepts deflate. These tests use a
/// stored zip so the unpack itself needs no extra crates.
pub fn extract_verified_payload(
    image: &[u8],
    expected: &str,
    dest: &Path,
) -> Result<(), PayloadError> {
    let payload = verify_payload(image, expected)?;
    let files = read_stored_zip(payload)?;
    // Reject a bad path before creating `dest`, so a mismatch or a hostile
    // member leaves the destination absent.
    let planned: Vec<(std::path::PathBuf, String, Vec<u8>)> = files
        .into_iter()
        .map(|(name, data)| safe_join(dest, &name).map(|path| (path, name, data)))
        .collect::<Result<_, _>>()?;
    if dest.exists() {
        return Err(PayloadError::Io(format!(
            "{} already exists",
            dest.display()
        )));
    }
    fs::create_dir_all(dest).map_err(io_err)?;
    for (path, name, data) in planned {
        if name.ends_with('/') {
            fs::create_dir_all(&path).map_err(io_err)?;
            continue;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(io_err)?;
        }
        if let Err(err) = fs::write(&path, data) {
            let _ = fs::remove_dir_all(dest);
            return Err(io_err(err));
        }
    }
    Ok(())
}

fn safe_join(dest: &Path, name: &str) -> Result<std::path::PathBuf, PayloadError> {
    let name = name.replace('\\', "/");
    if name.is_empty() {
        return Err(PayloadError::UnsafePath(name));
    }
    let mut out = dest.to_path_buf();
    for component in Path::new(&name).components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => return Err(PayloadError::UnsafePath(name)),
        }
    }
    Ok(out)
}

/// Stored-zip members as `(name, bytes)`. The central directory is not trusted
/// for the bytes; each local header is checked and then skipped past.
fn read_stored_zip(blob: &[u8]) -> Result<Vec<(String, Vec<u8>)>, PayloadError> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= blob.len() {
        let sig = u32_at(blob, i).ok_or_else(|| PayloadError::Zip("truncated".into()))?;
        if sig != 0x0403_4b50 {
            break;
        }
        if i + 30 > blob.len() {
            return Err(PayloadError::Zip("truncated local header".into()));
        }
        let method = u16_at(blob, i + 8).unwrap();
        let comp_size = u32_at(blob, i + 18).unwrap() as usize;
        let name_len = u16_at(blob, i + 26).unwrap() as usize;
        let extra_len = u16_at(blob, i + 28).unwrap() as usize;
        let name_at = i + 30;
        let data_at = name_at + name_len + extra_len;
        let data_end = data_at + comp_size;
        if data_end > blob.len() {
            return Err(PayloadError::Zip("truncated member".into()));
        }
        let name = String::from_utf8_lossy(&blob[name_at..name_at + name_len]).into_owned();
        if method != 0 {
            return Err(PayloadError::Zip(format!("{name} is compressed")));
        }
        out.push((name, blob[data_at..data_end].to_vec()));
        i = data_end;
    }
    if out.is_empty() {
        return Err(PayloadError::Zip("no files".into()));
    }
    Ok(out)
}

fn u16_at(buf: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(buf.get(off..off + 2)?.try_into().ok()?))
}

fn u32_at(buf: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(buf.get(off..off + 4)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let t = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("arcade-install-{t}-{n}"));
            fs::create_dir_all(&path).unwrap();
            Scratch(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
        !crc
    }

    fn store_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut local = Vec::new();
        let mut central = Vec::new();
        for (name, data) in files {
            let name_b = name.as_bytes();
            let offset = local.len() as u32;
            let crc = crc32(data);
            local.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            local.extend_from_slice(&20u16.to_le_bytes());
            local.extend_from_slice(&0u16.to_le_bytes());
            local.extend_from_slice(&0u16.to_le_bytes());
            local.extend_from_slice(&0u16.to_le_bytes());
            local.extend_from_slice(&0u16.to_le_bytes());
            local.extend_from_slice(&crc.to_le_bytes());
            local.extend_from_slice(&(data.len() as u32).to_le_bytes());
            local.extend_from_slice(&(data.len() as u32).to_le_bytes());
            local.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
            local.extend_from_slice(&0u16.to_le_bytes());
            local.extend_from_slice(name_b);
            local.extend_from_slice(data);

            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name_b);
        }
        let cd_off = local.len() as u32;
        let cd_len = central.len() as u32;
        let mut out = local;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&cd_len.to_le_bytes());
        out.extend_from_slice(&cd_off.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    fn utf16(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(|c| c.to_le_bytes()).collect()
    }

    fn marked_stub(hash: &str) -> Vec<u8> {
        let mut stub = b"installer-stub-".to_vec();
        stub.extend(utf16(&format!("{EMBEDDED_HASH_MARKER}{hash}")));
        stub
    }

    /// Unsigned PE header plus the embedded-hash marker after it, so a later
    /// patch of the certificate directory does not cover the marker.
    fn pe_sized_stub(hash: &str) -> Vec<u8> {
        let mut stub = pe32_header(0, 0);
        stub.extend(utf16(&format!("{EMBEDDED_HASH_MARKER}{hash}")));
        stub.extend_from_slice(b"-tail");
        stub
    }

    fn installer_with(payload: &[u8]) -> (Vec<u8>, String) {
        let hash = sha256_hex(payload);
        let image = append_payload(&marked_stub(&hash), payload).unwrap();
        (image, hash)
    }

    #[test]
    fn good_payload_is_accepted_and_extracted() {
        let payload = store_zip(&[("bin/hello.txt", b"hello from arcade")]);
        let (image, hash) = installer_with(&payload);
        let scratch = Scratch::new();
        let dest = scratch.0.join("out");
        extract_verified_payload(&image, &hash, &dest).unwrap();
        let got = fs::read(dest.join("bin/hello.txt")).unwrap();
        assert_eq!(got, b"hello from arcade");
        assert_eq!(embedded_payload_sha256(&image).unwrap(), hash);
    }

    #[test]
    fn tampered_payload_is_refused_and_nothing_is_extracted() {
        let payload = store_zip(&[("bin/hello.txt", b"hello from arcade")]);
        let (mut image, hash) = installer_with(&payload);
        let start = split_payload(&image).unwrap().0;
        image[start] ^= 0xff;
        let scratch = Scratch::new();
        let dest = scratch.0.join("out");
        let err = extract_verified_payload(&image, &hash, &dest).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Nothing was extracted"), "{msg}");
        assert!(msg.contains(&hash), "{msg}");
        assert!(msg.contains("got "), "{msg}");
        assert!(!dest.exists(), "a mismatch must not create the destination");
        assert!(fs::read_dir(&scratch.0).unwrap().next().is_none());
    }

    #[test]
    fn uppercase_expected_hash_still_matches() {
        let payload = store_zip(&[("a.txt", b"abc")]);
        let (image, hash) = installer_with(&payload);
        let scratch = Scratch::new();
        let dest = scratch.0.join("out");
        extract_verified_payload(&image, &hash.to_ascii_uppercase(), &dest).unwrap();
        assert_eq!(fs::read(dest.join("a.txt")).unwrap(), b"abc");
    }

    #[test]
    fn signed_overlay_padding_still_refuses_a_tampered_payload() {
        let payload = store_zip(&[("bin/arcaded.exe", b"program bytes")]);
        let hash = sha256_hex(&payload);
        // The stub is longer than the PE header so patching the certificate
        // directory does not overwrite the marker or the payload.
        let mut stub = pe_sized_stub(&hash);
        let mut unsigned = append_payload(&stub, &payload).unwrap();
        if unsigned.len() % 8 == 0 {
            stub.push(b'Q');
            unsigned = append_payload(&stub, &payload).unwrap();
        }
        let pad = (8 - (unsigned.len() % 8)) % 8;
        assert!(pad > 0, "this case has to exercise the alignment gap");
        let mut signed = unsigned;
        signed.extend(std::iter::repeat(0u8).take(pad));
        let cert_off = signed.len();
        let cert = b"FAKE-AUTHENTICODE-CERT";
        signed.extend_from_slice(cert);
        let header = pe32_header(cert_off as u32, cert.len() as u32);
        assert!(signed.len() > header.len());
        signed[..header.len()].copy_from_slice(&header);

        let scratch = Scratch::new();
        let dest = scratch.0.join("good");
        extract_verified_payload(&signed, &hash, &dest).unwrap();
        assert_eq!(
            fs::read(dest.join("bin/arcaded.exe")).unwrap(),
            b"program bytes"
        );

        let start = split_payload(&signed).unwrap().0;
        signed[start + 10] ^= 0xff;
        let bad = scratch.0.join("bad");
        let err = extract_verified_payload(&signed, &hash, &bad).unwrap_err();
        assert!(matches!(err, PayloadError::Mismatch { .. }), "{err}");
        assert!(!bad.exists());
    }

    #[test]
    fn embedded_hash_ignores_a_copy_inside_the_payload() {
        let hash = sha256_hex(b"not the real payload");
        let mut payload = b"decoy-".to_vec();
        payload.extend(utf16(&format!("{EMBEDDED_HASH_MARKER}{hash}")));
        payload.extend_from_slice(b"-tail");
        let real = sha256_hex(&payload);
        let image = append_payload(&marked_stub(&real), &payload).unwrap();
        assert_eq!(embedded_payload_sha256(&image).unwrap(), real);
        assert_ne!(real, hash);
    }

    #[test]
    fn append_refuses_a_stub_that_is_already_signed() {
        let payload = b"payload";
        let hash = sha256_hex(payload);
        let unsigned = append_payload(&pe_sized_stub(&hash), payload).unwrap();
        let mut signed = unsigned.clone();
        signed.extend_from_slice(&[0, 0, 0]);
        let cert_off = signed.len();
        signed.extend_from_slice(b"CERT!!");
        let header = pe32_header(cert_off as u32, 6);
        signed[..header.len()].copy_from_slice(&header);
        let err = append_payload(&signed, b"more").unwrap_err();
        assert_eq!(err, PayloadError::AlreadySigned);
    }

    #[test]
    fn unsafe_zip_path_is_refused_before_writing() {
        let payload = store_zip(&[("../outside.txt", b"nope")]);
        let (image, hash) = installer_with(&payload);
        let scratch = Scratch::new();
        let dest = scratch.0.join("out");
        let err = extract_verified_payload(&image, &hash, &dest).unwrap_err();
        assert!(matches!(err, PayloadError::UnsafePath(_)), "{err}");
        assert!(!dest.exists());
    }

    #[test]
    fn installer_script_checks_the_hash_before_it_unpacks() {
        let root = repo_root();
        let nsi = fs::read_to_string(root.join("scripts/windows/setup.nsi")).unwrap();
        let ps1 = fs::read_to_string(root.join("scripts/windows/Verify-Payload.ps1")).unwrap();
        assert!(
            !nsi.contains("File /r"),
            "the package must not be unpacked by File /r"
        );
        let verify_at = nsi
            .find("Verify-Payload.ps1")
            .expect("installer runs the verifier");
        let install_at = nsi
            .find("install.ps1")
            .expect("installer still runs install.ps1");
        assert!(verify_at < install_at, "verify before install.ps1");
        assert!(nsi.contains("Nothing was extracted"));
        assert!(nsi.contains(EMBEDDED_HASH_MARKER));
        // The header comment names Expand-Archive. Match the calls, not that sentence.
        let hash_at = ps1
            .find("$sha.ComputeHash")
            .expect("verifier hashes the payload");
        let expand_at = ps1
            .find("Expand-Archive -LiteralPath")
            .expect("verifier unpacks after the check");
        assert!(hash_at < expand_at, "hash before Expand-Archive");
        assert!(ps1.contains("ARCDPAY1"));
        assert!(ps1.contains("Nothing was extracted and nothing was installed."));
    }

    #[test]
    fn python_trailer_matches_rust_and_release_check() {
        let payload = store_zip(&[("bin/hello.txt", b"checked")]);
        let (image, hash) = installer_with(&payload);
        let from_python = python_append(&marked_stub(&hash), &payload);
        assert_eq!(from_python, image);

        let scratch = Scratch::new();
        let installer = scratch.0.join("setup.exe");
        fs::write(&installer, &image).unwrap();
        let status = Command::new("python3")
            .arg(repo_root().join("scripts/release/payload_trailer.py"))
            .args(["check", "--installer"])
            .arg(&installer)
            .args(["--expected", &hash])
            .status()
            .unwrap();
        assert!(status.success(), "python check rejected a good installer");

        let mut tampered = image.clone();
        let start = split_payload(&tampered).unwrap().0;
        tampered[start] ^= 0xff;
        let bad = scratch.0.join("bad.exe");
        fs::write(&bad, &tampered).unwrap();
        let output = Command::new("python3")
            .arg(repo_root().join("scripts/release/payload_trailer.py"))
            .args(["check", "--installer"])
            .arg(&bad)
            .args(["--expected", &hash])
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("Nothing was extracted"), "{stderr}");
        assert!(stderr.contains(&hash), "{stderr}");

        write_fake_release(&scratch.0, &image, &payload, &hash);
        let ok = Command::new("python3")
            .arg(repo_root().join("scripts/release/check_release_assets.py"))
            .arg(&scratch.0)
            .env("WINDOWS_SIGNING_REQUIRED", "false")
            .env("LINUX_MINISIGN_REQUIRED", "false")
            .env("LINUX_GPG_REQUIRED", "false")
            .status()
            .unwrap();
        assert!(ok.success(), "release check rejected a consistent tree");

        let mut broken = fs::read(&scratch.0.join("arcade-v1-windows-x86_64-setup.exe")).unwrap();
        let start = split_payload(&broken).unwrap().0;
        broken[start] ^= 0xff;
        fs::write(
            scratch.0.join("arcade-v1-windows-x86_64-setup.exe"),
            &broken,
        )
        .unwrap();
        // The published file hash must follow the tampered bytes, so the failure
        // is the embedded payload check and not the sidecar of setup.exe itself.
        let setup_hash = sha256_hex(&broken);
        fs::write(
            scratch.0.join("arcade-v1-windows-x86_64-setup.exe.sha256"),
            format!("{setup_hash}  arcade-v1-windows-x86_64-setup.exe\n"),
        )
        .unwrap();
        let binaries =
            fs::read_to_string(scratch.0.join("arcade-v1-windows-x86_64-binaries.sha256")).unwrap();
        let binaries = binaries
            .lines()
            .map(|line| {
                if line.ends_with("arcade-v1-windows-x86_64-setup.exe") {
                    format!("{setup_hash}  arcade-v1-windows-x86_64-setup.exe")
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(
            scratch.0.join("arcade-v1-windows-x86_64-binaries.sha256"),
            binaries,
        )
        .unwrap();
        let output = Command::new("python3")
            .arg(repo_root().join("scripts/release/check_release_assets.py"))
            .arg(&scratch.0)
            .env("WINDOWS_SIGNING_REQUIRED", "false")
            .env("LINUX_MINISIGN_REQUIRED", "false")
            .env("LINUX_GPG_REQUIRED", "false")
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "tampered payload must fail the release check"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("refusing to publish"), "{stderr}");
    }

    fn python_append(stub: &[u8], payload: &[u8]) -> Vec<u8> {
        let scratch = Scratch::new();
        let stub_path = scratch.0.join("stub");
        let payload_path = scratch.0.join("payload");
        let out = scratch.0.join("out");
        fs::write(&stub_path, stub).unwrap();
        fs::write(&payload_path, payload).unwrap();
        let status = Command::new("python3")
            .arg(repo_root().join("scripts/release/payload_trailer.py"))
            .args(["append", "--stub"])
            .arg(&stub_path)
            .arg("--payload")
            .arg(&payload_path)
            .arg("--out")
            .arg(&out)
            .status()
            .unwrap();
        assert!(status.success(), "python append failed");
        fs::read(out).unwrap()
    }

    fn write_fake_release(dir: &Path, setup: &[u8], payload: &[u8], payload_hash: &str) {
        let setup_hash = sha256_hex(setup);
        fs::write(dir.join("arcade-v1-windows-x86_64-setup.exe"), setup).unwrap();
        fs::write(
            dir.join("arcade-v1-windows-x86_64-setup.exe.sha256"),
            format!("{setup_hash}  arcade-v1-windows-x86_64-setup.exe\n"),
        )
        .unwrap();
        fs::write(
            dir.join("arcade-v1-windows-x86_64-payload.sha256"),
            format!("{payload_hash}  arcade-v1-windows-x86_64-payload\n"),
        )
        .unwrap();
        let arcaded = b"arcaded-bytes";
        let arcade = b"arcade-bytes";
        let desktop = b"desktop-bytes";
        let zip = store_zip(&[
            ("arcade-v1-windows-x86_64/bin/arcaded.exe", arcaded),
            ("arcade-v1-windows-x86_64/bin/arcade.exe", arcade),
            ("arcade-v1-windows-x86_64/bin/arcade-desktop.exe", desktop),
            ("arcade-v1-windows-x86_64/Install.exe", setup),
        ]);
        fs::write(dir.join("arcade-v1-windows-x86_64.zip"), &zip).unwrap();
        let zip_hash = sha256_hex(&zip);
        fs::write(
            dir.join("arcade-v1-windows-x86_64.zip.sha256"),
            format!("{zip_hash}  arcade-v1-windows-x86_64.zip\n"),
        )
        .unwrap();
        let lines = format!(
            "{}  arcaded.exe\n{}  arcade.exe\n{}  arcade-desktop.exe\n{}  Install.exe\n{}  arcade-v1-windows-x86_64-setup.exe\n",
            sha256_hex(arcaded),
            sha256_hex(arcade),
            sha256_hex(desktop),
            setup_hash,
            setup_hash
        );
        fs::write(dir.join("arcade-v1-windows-x86_64-binaries.sha256"), lines).unwrap();
        for arch in ["x86_64", "aarch64"] {
            let name = format!("arcade-v1-linux-{arch}.tar.gz");
            let bytes = format!("linux-{arch}").into_bytes();
            let hash = sha256_hex(&bytes);
            fs::write(dir.join(&name), &bytes).unwrap();
            fs::write(
                dir.join(format!("{name}.sha256")),
                format!("{hash}  {name}\n"),
            )
            .unwrap();
        }
        // `payload` is here so a future change that hashes the zip sidecar still sees it used.
        assert_eq!(sha256_hex(payload), *payload_hash);
    }

    /// A PE32 header whose certificate table points at `cert_off`. The bytes are
    /// only as real as the trailer parser needs (MZ, PE, optional-header magic,
    /// security data directory).
    fn pe32_header(cert_off: u32, cert_size: u32) -> Vec<u8> {
        const HEADER_LEN: usize = 0x80 + 4 + 20 + 96 + 16 * 8;
        let mut h = vec![0u8; HEADER_LEN];
        h[0] = b'M';
        h[1] = b'Z';
        h[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        let pe = 0x80;
        h[pe..pe + 4].copy_from_slice(b"PE\0\0");
        let coff = pe + 4;
        h[coff..coff + 2].copy_from_slice(&0x14cu16.to_le_bytes());
        let opt_size: u16 = 96 + 16 * 8;
        h[coff + 16..coff + 18].copy_from_slice(&opt_size.to_le_bytes());
        let opt = coff + 20;
        h[opt..opt + 2].copy_from_slice(&0x10bu16.to_le_bytes());
        h[opt + 92..opt + 96].copy_from_slice(&16u32.to_le_bytes());
        let sec = opt + 96 + 4 * 8;
        h[sec..sec + 4].copy_from_slice(&cert_off.to_le_bytes());
        h[sec + 4..sec + 8].copy_from_slice(&cert_size.to_le_bytes());
        h
    }
}
