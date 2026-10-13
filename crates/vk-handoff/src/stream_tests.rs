use super::*;

const KEY: [u8; KEY_LEN] = {
    let mut k = [0u8; KEY_LEN];
    let mut i = 0;
    while i < KEY_LEN {
        k[i] = i as u8;
        i += 1;
    }
    k
};

const PREFIX: [u8; PREFIX_LEN] = {
    let mut p = [0u8; PREFIX_LEN];
    let mut i = 0;
    while i < PREFIX_LEN {
        p[i] = 0x40 + i as u8;
        i += 1;
    }
    p
};

fn hex_decode(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn seal(plain: &[u8]) -> (Vec<u8>, Sealed) {
    let mut out = Vec::new();
    let s = encrypt(plain, &mut out, &KEY, &PREFIX).unwrap();
    (out, s)
}

fn open(ct: &[u8]) -> Result<(Vec<u8>, Opened)> {
    let mut out = Vec::new();
    let o = decrypt(ct, &mut out, &KEY, u64::MAX)?;
    Ok((out, o))
}

fn sha_hex(b: &[u8]) -> String {
    crate::hex(&Sha256::digest(b))
}

/// Deterministic multi-chunk plaintext.
fn big() -> Vec<u8> {
    (0..CHUNK * 2 + 12345)
        .map(|i| ((i * 31 + 7) % 251) as u8)
        .collect()
}

// Vectors computed with an independent XChaCha20-Poly1305 implementation (checked against the
// RFC 8439 §2.8.2 and draft-irtf-cfrg-xchacha §A.3.1 tags): key 00..1f, prefix 40..52.

#[test]
fn single_chunk_vector() {
    let (ct, s) = seal(b"vibeke continue test vector");
    assert_eq!(
        ct,
        hex_decode(
            "564b4331404142434445464748494a4b4c4d4e4f50515212cae218bf17d3f5898d16636cfaba4e94b5bb1fb69555a26f678c7d8bee043bc2054c0c01b45dedde8d82"
        )
    );
    assert_eq!(s.size, 27);
    assert_eq!(s.cipher_size, ct.len() as u64);
    assert_eq!(s.cipher_size, cipher_len(27));
    assert_eq!(s.sha256, sha_hex(b"vibeke continue test vector"));
    let (pt, o) = open(&ct).unwrap();
    assert_eq!(pt, b"vibeke continue test vector");
    assert_eq!((o.size, o.sha256), (s.size, s.sha256));
}

#[test]
fn empty_vector() {
    let (ct, s) = seal(b"");
    assert_eq!(
        ct,
        hex_decode(
            "564b4331404142434445464748494a4b4c4d4e4f505152de52c5c665fc7f6ba4751ca0f41813cf"
        )
    );
    assert_eq!(s.size, 0);
    assert_eq!(cipher_len(0), (HEADER_LEN + TAG_LEN) as u64);
    let (pt, o) = open(&ct).unwrap();
    assert!(pt.is_empty());
    assert_eq!(o.size, 0);
}

#[test]
fn multi_chunk_vector() {
    let plain = big();
    let (ct, s) = seal(&plain);
    assert_eq!(ct.len(), 2_109_568);
    assert_eq!(s.cipher_size, cipher_len(plain.len() as u64));
    assert_eq!(
        sha_hex(&ct),
        "78e509799db96b7046a969b5a976684cb6c6ba373d74fa6ccd6fa37b9837f12a"
    );
    assert_eq!(
        s.sha256,
        "6672858328151573611914347f89f5aa8d935b272ce622f82a1deccaae6d45b5"
    );
    let (pt, o) = open(&ct).unwrap();
    assert_eq!(pt, plain);
    assert_eq!(o.sha256, s.sha256);
}

/// The format is `aead::stream::StreamBE32`: the RustCrypto encryptor writes the same chunks.
#[test]
fn matches_the_stream_be32_encryptor() {
    use chacha20poly1305::aead::generic_array::GenericArray;
    use chacha20poly1305::aead::stream::EncryptorBE32;
    let plain = big();
    let (ct, _) = seal(&plain);
    let mut enc = EncryptorBE32::from_aead(cipher(&KEY), GenericArray::from_slice(&PREFIX));
    let mut want = Vec::new();
    want.extend_from_slice(MAGIC);
    want.extend_from_slice(&PREFIX);
    let chunks: Vec<&[u8]> = plain.chunks(CHUNK).collect();
    for c in &chunks[..chunks.len() - 1] {
        want.extend(enc.encrypt_next(Payload { msg: c, aad: AAD }).unwrap());
    }
    want.extend(
        enc.encrypt_last(Payload {
            msg: chunks[chunks.len() - 1],
            aad: AAD,
        })
        .unwrap(),
    );
    assert_eq!(ct, want);
}

#[test]
fn an_exact_multiple_of_the_chunk_ends_with_a_full_last_chunk() {
    let plain = vec![7u8; CHUNK * 2];
    let (ct, s) = seal(&plain);
    assert_eq!(ct.len() as u64, cipher_len(plain.len() as u64));
    assert_eq!(
        ct.len(),
        HEADER_LEN + 2 * (CHUNK + TAG_LEN),
        "no extra empty chunk"
    );
    let (pt, o) = open(&ct).unwrap();
    assert_eq!(pt, plain);
    assert_eq!(o.sha256, s.sha256);
}

#[test]
fn a_missing_last_chunk_fails() {
    let (ct, _) = seal(&big());
    // Cut after the first full chunk: it decrypts, but as a middle chunk.
    let cut = &ct[..HEADER_LEN + CHUNK + TAG_LEN];
    let e = open(cut).unwrap_err();
    assert_eq!(e.kind, "conflict");
    assert!(e.message.contains("without a last chunk"), "{}", e.message);
    // Cut inside a chunk: the tag cannot match.
    let e = open(&ct[..ct.len() - 100]).unwrap_err();
    assert_eq!(e.kind, "conflict");
    // Only the header.
    assert_eq!(open(&ct[..HEADER_LEN]).unwrap_err().kind, "conflict");
    // Not even the header.
    assert_eq!(open(&ct[..10]).unwrap_err().kind, "conflict");
}

#[test]
fn trailing_data_fails() {
    // A full last chunk followed by more bytes.
    let plain = vec![1u8; CHUNK];
    let (mut ct, _) = seal(&plain);
    ct.extend_from_slice(b"extra");
    let e = open(&ct).unwrap_err();
    assert_eq!(e.kind, "conflict");
    assert!(e.message.contains("after the last chunk"), "{}", e.message);
    // A short last chunk followed by more bytes.
    let (mut ct, _) = seal(b"short");
    ct.push(0);
    assert_eq!(open(&ct).unwrap_err().kind, "conflict");
}

#[test]
fn a_bad_tag_or_wrong_key_or_magic_fails() {
    let (ct, _) = seal(b"vibeke continue test vector");
    let mut bad = ct.clone();
    let n = bad.len();
    bad[n - 1] ^= 1;
    assert_eq!(open(&bad).unwrap_err().kind, "conflict");
    let mut bad = ct.clone();
    bad[HEADER_LEN] ^= 1;
    assert_eq!(open(&bad).unwrap_err().kind, "conflict");
    let mut out = Vec::new();
    let mut other = KEY;
    other[0] ^= 1;
    assert_eq!(
        decrypt(&ct[..], &mut out, &other, u64::MAX)
            .unwrap_err()
            .kind,
        "conflict"
    );
    let mut bad = ct.clone();
    bad[0] = b'X';
    let e = open(&bad).unwrap_err();
    assert!(e.message.contains("not a continue bundle"), "{}", e.message);
}

#[test]
fn the_plaintext_limit_holds() {
    let (ct, _) = seal(&big());
    let mut out = Vec::new();
    let e = decrypt(&ct[..], &mut out, &KEY, CHUNK as u64).unwrap_err();
    assert_eq!(e.kind, "too_large");
}

#[test]
fn files_round_trip_privately() {
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("b.tar.zst");
    std::fs::write(&src, big()).unwrap();
    let key = new_key();
    let enc = t.path().join("b.vkc");
    let s = encrypt_file(&src, &enc, &key).unwrap();
    assert_eq!(std::fs::metadata(&enc).unwrap().len(), s.cipher_size);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&enc).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0);
    }
    let dec = t.path().join("out.tar.zst");
    let o = decrypt_file(&enc, &dec, &key, u64::MAX).unwrap();
    assert_eq!(o.sha256, s.sha256);
    assert_eq!(crate::hash_file(&dec).unwrap(), (s.size, s.sha256.clone()));
    // A failure leaves no output behind.
    let dec2 = t.path().join("out2.tar.zst");
    assert!(decrypt_file(&enc, &dec2, &new_key(), u64::MAX).is_err());
    assert!(!dec2.exists());
}
