//! Crypto protocol for ShadowSocks UDP
//!
//! Payload with stream cipher
//! ```plain
//! +-------+----------+
//! |  IV   | Payload  |
//! +-------+----------+
//! | Fixed | Variable |
//! +-------+----------+
//! ```
//!
//! Payload with AEAD cipher
//!
//! ```plain
//! UDP (after encryption, *ciphertext*)
//! +--------+-----------+-----------+
//! | NONCE  |  *Data*   |  Data_TAG |
//! +--------+-----------+-----------+
//! | Fixed  | Variable  |   Fixed   |
//! +--------+-----------+-----------+
//! ```

use std::{io, slice};

use byte_string::ByteStr;
use bytes::{BufMut, BytesMut};
use log::trace;

use crate::crypto::{self, CipherCategory, CipherType, CryptoMode};

/// Encrypt payload into ShadowSocks UDP encrypted packet
pub fn encrypt_payload(t: CipherType, key: &[u8], payload: &[u8], dst: &mut BytesMut) -> io::Result<()> {
    match t.category() {
        CipherCategory::Stream => encrypt_payload_stream(t, key, payload, dst),
        CipherCategory::Aead => encrypt_payload_aead(t, key, payload, dst),
    }
}

fn encrypt_payload_stream(t: CipherType, key: &[u8], payload: &[u8], dst: &mut BytesMut) -> io::Result<()> {
    let iv = t.gen_init_vec();
    let mut cipher = crypto::new_stream(t, key, &iv, CryptoMode::Encrypt);

    trace!("UDP packet generated stream iv {:?}", ByteStr::new(&iv));

    dst.reserve(iv.len() + payload.len());

    // First of all, IV
    dst.put_slice(&iv);

    // Encrypted data
    cipher.update(&payload[..], dst)?;
    cipher.finalize(dst)?;

    Ok(())
}

fn encrypt_payload_aead(t: CipherType, key: &[u8], payload: &[u8], dst: &mut BytesMut) -> io::Result<()> {
    let salt = t.gen_salt();
    let tag_size = t.tag_size();
    let mut cipher = crypto::new_aead_encryptor(t, key, &salt);

    trace!("UDP packet generated AEAD salt {:?}", ByteStr::new(&salt));

    dst.reserve(salt.len() + payload.len() + tag_size);

    // First of all, salt
    dst.put_slice(&salt);

    // Encrypted data
    unsafe {
        let remaining = dst.bytes_mut();
        let b = slice::from_raw_parts_mut(remaining.as_mut_ptr() as *mut u8, remaining.len());

        cipher.encrypt(payload, b);
        dst.advance_mut(payload.len() + tag_size);
    }

    Ok(())
}

/// Decrypt payload from ShadowSocks UDP encrypted packet
pub fn decrypt_payload(t: CipherType, key: &[u8], payload: &[u8]) -> io::Result<Option<Vec<u8>>> {
    match t.category() {
        CipherCategory::Stream => decrypt_payload_stream(t, key, payload),
        CipherCategory::Aead => decrypt_payload_aead(t, key, payload),
    }
}

fn decrypt_payload_stream(t: CipherType, key: &[u8], payload: &[u8]) -> io::Result<Option<Vec<u8>>> {
    let iv_size = t.iv_size();
    if payload.len() < iv_size {
        return Ok(None);
    }

    let iv = &payload[..iv_size];
    let data = &payload[iv_size..];

    trace!("UDP packet got stream IV {:?}", ByteStr::new(iv));

    let mut cipher = crypto::new_stream(t, key, iv, CryptoMode::Decrypt);

    let mut recv_payload = Vec::with_capacity(data.len());
    cipher.update(data, &mut recv_payload)?;
    cipher.finalize(&mut recv_payload)?;

    Ok(Some(recv_payload))
}

fn decrypt_payload_aead(t: CipherType, key: &[u8], payload: &[u8]) -> io::Result<Option<Vec<u8>>> {
    let tag_size = t.tag_size();
    let salt_size = t.salt_size();

    if payload.len() < tag_size + salt_size {
        return Ok(None);
    }

    let salt = &payload[..salt_size];
    let data = &payload[salt_size..];

    trace!("UDP packet got AEAD salt {:?}", ByteStr::new(salt));

    let data_length = payload.len() - tag_size - salt_size;

    let mut cipher = crypto::new_aead_decryptor(t, key, salt);

    let mut recv_payload = vec![0u8; data_length];
    cipher.decrypt(data, &mut recv_payload)?;

    Ok(Some(recv_payload))
}
