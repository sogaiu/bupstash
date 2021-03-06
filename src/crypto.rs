use super::address::*;
use super::sodium;
use serde::{Deserialize, Serialize};
use std::convert::TryInto;

pub const HASH_BYTES: usize = sodium::crypto_generichash_BYTES as usize;

pub const BOX_NONCEBYTES: usize =
    sodium::crypto_box_curve25519xchacha20poly1305_NONCEBYTES as usize;
pub const BOX_PUBLICKEYBYTES: usize =
    sodium::crypto_box_curve25519xchacha20poly1305_PUBLICKEYBYTES as usize;
pub const BOX_SECRETKEYBYTES: usize =
    sodium::crypto_box_curve25519xchacha20poly1305_SECRETKEYBYTES as usize;
pub const BOX_BEFORENMBYTES: usize =
    sodium::crypto_box_curve25519xchacha20poly1305_BEFORENMBYTES as usize;
pub const BOX_MACBYTES: usize = sodium::crypto_box_curve25519xchacha20poly1305_MACBYTES as usize;

pub const BOX_PRE_SHARED_KEY_BYTES: usize = sodium::crypto_generichash_KEYBYTES as usize;

pub const CHUNK_FOOTER_NO_COMPRESSION: u8 = 0;
pub const CHUNK_FOOTER_ZSTD_COMPRESSED: u8 = 1;

pub fn init() {
    unsafe {
        sodium::sodium_init();
    };
}

#[inline(always)]
pub fn randombytes(buf: &mut [u8]) {
    unsafe {
        sodium::randombytes_buf(buf.as_mut_ptr() as *mut std::ffi::c_void, buf.len());
    }
}

#[inline(always)]
pub fn memzero(buf: &mut [u8]) {
    unsafe {
        sodium::sodium_memzero(buf.as_mut_ptr() as *mut std::ffi::c_void, buf.len());
    }
}

#[derive(Clone)]
pub struct BoxNonce {
    pub bytes: [u8; BOX_NONCEBYTES as usize],
}

impl BoxNonce {
    pub fn new() -> Self {
        let mut bytes: [u8; BOX_NONCEBYTES as usize] = [0; BOX_NONCEBYTES];
        randombytes(&mut bytes[..]);
        BoxNonce { bytes }
    }

    pub fn inc(&mut self) {
        unsafe { sodium::sodium_increment(self.bytes.as_mut_ptr(), self.bytes.len()) }
    }
}

impl Default for BoxNonce {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Serialize, Deserialize, PartialEq, Clone, Debug)]
pub struct BoxSecretKey {
    pub bytes: [u8; BOX_SECRETKEYBYTES],
}

impl BoxSecretKey {
    /* nothing */
}

impl Drop for BoxSecretKey {
    fn drop(&mut self) {
        memzero(&mut self.bytes[..]);
    }
}

#[derive(Serialize, Deserialize, PartialEq, Clone, Debug)]
pub struct BoxPublicKey {
    pub bytes: [u8; BOX_PUBLICKEYBYTES as usize],
}

impl BoxPublicKey {
    /* nothing */
}

pub fn box_keypair() -> (BoxPublicKey, BoxSecretKey) {
    let mut pk = BoxPublicKey {
        bytes: [0; BOX_PUBLICKEYBYTES],
    };
    let mut sk = BoxSecretKey {
        bytes: [0; BOX_SECRETKEYBYTES],
    };
    unsafe {
        sodium::crypto_box_curve25519xchacha20poly1305_keypair(
            pk.bytes.as_mut_ptr(),
            sk.bytes.as_mut_ptr(),
        );
    }
    (pk, sk)
}

#[derive(Clone)]
pub struct BoxKey {
    pub bytes: [u8; BOX_BEFORENMBYTES],
}

impl BoxSecretKey {
    /* nothing */
}

impl Drop for BoxKey {
    fn drop(&mut self) {
        memzero(&mut self.bytes[..]);
    }
}

#[inline(always)]
pub fn box_compute_key(pk: &BoxPublicKey, sk: &BoxSecretKey, psk: &BoxPreSharedKey) -> BoxKey {
    let mut unmixed_key_bytes: [u8; BOX_BEFORENMBYTES] =
        unsafe { std::mem::MaybeUninit::uninit().assume_init() };
    if unsafe {
        sodium::crypto_box_curve25519xchacha20poly1305_beforenm(
            unmixed_key_bytes.as_mut_ptr(),
            pk.bytes.as_ptr(),
            sk.bytes.as_ptr(),
        )
    } != 0
    {
        BoxKey {
            bytes: [0; BOX_BEFORENMBYTES],
        }
    } else {
        /*
          XXX TODO FIXME REVIEWME:
          Integrate the preshared key bytes with the computed secret so the
          decrypting party must have had access to one of our keys. Post
          quantum is a threat to our asymmetric key security, the PSK is
          intended to help us gracefully degrade to symmetric key security,
          even if the asymmetric key is broken.

          This key mixing relies on the implementation of the crypto box, the
          result of crypto_box_curve25519xchacha20poly1305_beforenm is the precomputed
          crypto_secretbox_xsalsa20poly1305 key, which are simply random keys. Using
          generic hash to mix the psk with this key should result is another random key.

          We need advice from experts on how to do this appropriately, and if
          what even we are doing is right at all.
        */

        let mut mixed_key_bytes: [u8; BOX_BEFORENMBYTES] =
            unsafe { std::mem::MaybeUninit::uninit().assume_init() };

        debug_assert!(BOX_PRE_SHARED_KEY_BYTES == sodium::crypto_generichash_KEYBYTES as usize);

        unsafe {
            if sodium::crypto_generichash(
                mixed_key_bytes.as_mut_ptr(),
                mixed_key_bytes.len(),
                unmixed_key_bytes.as_ptr(),
                unmixed_key_bytes.len().try_into().unwrap(),
                psk.bytes.as_ptr(),
                psk.bytes.len(),
            ) != 0
            {
                panic!();
            }
        };

        BoxKey {
            bytes: mixed_key_bytes,
        }
    }
}

#[inline(always)]
pub fn box_encrypt(bt: &mut [u8], pt: &[u8], nonce: &mut BoxNonce, bk: &BoxKey) {
    if bt.len() != pt.len() + BOX_NONCEBYTES + BOX_MACBYTES {
        panic!("box_encrypt output slice wrong size")
    }
    bt[..BOX_NONCEBYTES].clone_from_slice(&nonce.bytes[..]);
    if unsafe {
        sodium::crypto_box_curve25519xchacha20poly1305_easy_afternm(
            bt[nonce.bytes.len()..].as_mut_ptr(),
            pt.as_ptr(),
            pt.len().try_into().unwrap(),
            nonce.bytes.as_ptr(),
            bk.bytes.as_ptr(),
        )
    } != 0
    {
        panic!();
    }
    nonce.inc();
}

#[inline(always)]
pub fn box_decrypt(pt: &mut [u8], bt: &[u8], bk: &BoxKey) -> bool {
    if bt.len() < BOX_NONCEBYTES + BOX_MACBYTES {
        return false;
    }
    if pt.len() != bt.len() - BOX_NONCEBYTES - BOX_MACBYTES {
        return false;
    }
    let nonce = &bt[..BOX_NONCEBYTES];
    let ct = &bt[BOX_NONCEBYTES..];
    if unsafe {
        sodium::crypto_box_curve25519xchacha20poly1305_open_easy_afternm(
            pt.as_mut_ptr(),
            ct.as_ptr(),
            ct.len().try_into().unwrap(),
            nonce.as_ptr(),
            bk.bytes.as_ptr(),
        )
    } != 0
    {
        return false;
    }
    true
}

fn zstd_compress_chunk(mut data: Vec<u8>) -> Vec<u8> {
    // Our max chunk size means this should never happen.
    assert!(data.len() <= 0xffffffff);
    let mut compressed_data = zstd::block::compress(&data, 0).unwrap();
    if (compressed_data.len() + 4) >= data.len() {
        data.push(CHUNK_FOOTER_NO_COMPRESSION);
        data
    } else {
        compressed_data.reserve(5);
        let sz = data.len() as u32;
        compressed_data.push((sz & 0x000000ff) as u8);
        compressed_data.push(((sz & 0x0000ff00) >> 8) as u8);
        compressed_data.push(((sz & 0x00ff0000) >> 16) as u8);
        compressed_data.push(((sz & 0xff000000) >> 24) as u8);
        compressed_data.push(CHUNK_FOOTER_ZSTD_COMPRESSED);
        compressed_data
    }
}

fn decompress_chunk(mut data: Vec<u8>) -> Result<Vec<u8>, failure::Error> {
    if data.is_empty() {
        failure::bail!("data chunk was too small, missing footer");
    }

    let data = match data[data.len() - 1] {
        footer if footer == CHUNK_FOOTER_NO_COMPRESSION => {
            data.pop();
            data
        }
        footer if footer == CHUNK_FOOTER_ZSTD_COMPRESSED => {
            data.pop();
            if data.len() < 4 {
                failure::bail!("data footer missing decompressed size");
            }
            let data_len = data.len();
            let decompressed_sz = ((data[data_len - 1] as u32) << 24)
                | ((data[data_len - 2] as u32) << 16)
                | ((data[data_len - 3] as u32) << 8)
                | (data[data_len - 4] as u32);
            data.truncate(data.len() - 4);
            zstd::block::decompress(&data, decompressed_sz as usize)?
        }
        _ => failure::bail!("unknown footer type type"),
    };
    Ok(data)
}

#[derive(Clone, Copy)]
pub enum DataCompression {
    None,
    Zstd,
}

#[derive(Clone)]
pub struct EncryptionContext {
    nonce: BoxNonce,
    ephemeral_pk: BoxPublicKey,
    ephemeral_bk: BoxKey,
}

impl EncryptionContext {
    pub fn new(recipient: &BoxPublicKey, psk: &BoxPreSharedKey) -> EncryptionContext {
        let nonce = BoxNonce::new();
        let (ephemeral_pk, ephemeral_sk) = box_keypair();
        let ephemeral_bk = box_compute_key(recipient, &ephemeral_sk, &psk);
        EncryptionContext {
            nonce,
            ephemeral_pk,
            ephemeral_bk,
        }
    }

    pub fn encrypt_data(&mut self, mut pt: Vec<u8>, compression: DataCompression) -> Vec<u8> {
        let pt = match compression {
            DataCompression::None => {
                pt.push(CHUNK_FOOTER_NO_COMPRESSION);
                pt
            }
            DataCompression::Zstd => zstd_compress_chunk(pt),
        };
        let ct_len = pt.len() + BOX_NONCEBYTES + BOX_MACBYTES + self.ephemeral_pk.bytes.len();
        let mut ct = Vec::with_capacity(ct_len);
        unsafe { ct.set_len(ct_len) };
        box_encrypt(
            &mut ct[..ct_len - self.ephemeral_pk.bytes.len()],
            &pt,
            &mut self.nonce,
            &self.ephemeral_bk,
        );
        ct[ct_len - self.ephemeral_pk.bytes.len()..].clone_from_slice(&self.ephemeral_pk.bytes[..]);
        ct
    }
}

#[derive(Clone)]
pub struct DecryptionContext {
    sk: BoxSecretKey,
    psk: BoxPreSharedKey,
    ephemeral_pk: BoxPublicKey,
    ephemeral_bk: BoxKey,
}

impl DecryptionContext {
    pub fn new(sk: BoxSecretKey, psk: BoxPreSharedKey) -> DecryptionContext {
        DecryptionContext {
            sk,
            psk,
            ephemeral_pk: BoxPublicKey {
                bytes: [0; BOX_PUBLICKEYBYTES],
            },
            ephemeral_bk: BoxKey {
                bytes: [0; BOX_BEFORENMBYTES],
            },
        }
    }

    pub fn decrypt_data(&mut self, ct: Vec<u8>) -> Result<Vec<u8>, failure::Error> {
        if ct.len() < BOX_PUBLICKEYBYTES + BOX_NONCEBYTES + BOX_MACBYTES {
            failure::bail!("data corrupt (too small)");
        }

        {
            let pk_slice = &ct[ct.len() - BOX_PUBLICKEYBYTES..];
            for i in 0..BOX_PUBLICKEYBYTES {
                if pk_slice[i] != self.ephemeral_pk.bytes[i] {
                    self.ephemeral_pk.bytes[..].clone_from_slice(pk_slice);
                    self.ephemeral_bk = box_compute_key(&self.ephemeral_pk, &self.sk, &self.psk);
                    break;
                }
            }
        }

        let pt_len = ct.len() - BOX_NONCEBYTES - BOX_MACBYTES - BOX_PUBLICKEYBYTES;
        let mut pt = Vec::with_capacity(pt_len);
        unsafe { pt.set_len(pt_len) };

        if !box_decrypt(
            &mut pt,
            &ct[..ct.len() - BOX_PUBLICKEYBYTES],
            &self.ephemeral_bk,
        ) {
            failure::bail!("data corrupt");
        }

        decompress_chunk(pt)
    }
}

#[derive(Serialize, Deserialize, PartialEq, Clone, Debug)]
pub struct PartialHashKey {
    pub bytes: [u8; sodium::crypto_generichash_KEYBYTES as usize],
}

impl PartialHashKey {
    pub fn new() -> Self {
        let mut bytes: [u8; sodium::crypto_generichash_KEYBYTES as usize] =
            unsafe { std::mem::MaybeUninit::uninit().assume_init() };
        randombytes(&mut bytes[..]);
        PartialHashKey { bytes }
    }
}

impl Default for PartialHashKey {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PartialHashKey {
    fn drop(&mut self) {
        memzero(&mut self.bytes[..]);
    }
}

#[derive(Serialize, Deserialize, PartialEq, Clone, Debug)]
pub struct BoxPreSharedKey {
    pub bytes: [u8; 32],
}

impl BoxPreSharedKey {
    pub fn new() -> Self {
        let mut bytes: [u8; 32] = [0; 32];
        randombytes(&mut bytes[..]);
        BoxPreSharedKey { bytes }
    }
}

impl Default for BoxPreSharedKey {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for BoxPreSharedKey {
    fn drop(&mut self) {
        memzero(&mut self.bytes[..]);
    }
}

pub fn derive_hash_key(part1: &PartialHashKey, part2: &PartialHashKey) -> HashKey {
    let mut hs = HashState::new(None);
    hs.update(&part1.bytes[..]);
    hs.update(&part2.bytes[..]);
    let bytes = hs.finish();
    HashKey {
        part1: part1.clone(),
        part2: part2.clone(),
        bytes,
    }
}

#[derive(Serialize, Deserialize, PartialEq, Clone)]
pub struct HashKey {
    pub part1: PartialHashKey,
    pub part2: PartialHashKey,
    pub bytes: [u8; sodium::crypto_generichash_KEYBYTES as usize],
}

impl Drop for HashKey {
    fn drop(&mut self) {
        memzero(&mut self.bytes[..]);
    }
}

pub struct HashState {
    st: sodium::crypto_generichash_state,
}

impl HashState {
    pub fn new(key: Option<&HashKey>) -> HashState {
        let mut h = HashState {
            st: unsafe { std::mem::MaybeUninit::uninit().assume_init() },
        };

        if unsafe {
            sodium::crypto_generichash_init(
                &mut h.st as *mut sodium::crypto_generichash_state,
                if let Some(k) = key {
                    k.bytes.as_ptr() as *const u8
                } else {
                    std::ptr::null()
                },
                if let Some(k) = key { k.bytes.len() } else { 0 },
                HASH_BYTES,
            )
        } != 0
        {
            panic!()
        }
        h
    }

    pub fn update(&mut self, data: &[u8]) {
        if unsafe {
            sodium::crypto_generichash_update(
                &mut self.st as *mut sodium::crypto_generichash_state,
                data.as_ptr() as *const u8,
                data.len().try_into().unwrap(),
            )
        } != 0
        {
            panic!();
        };
    }

    pub fn finish(mut self) -> [u8; HASH_BYTES] {
        let mut out: [u8; HASH_BYTES] = unsafe { std::mem::MaybeUninit::uninit().assume_init() };
        if unsafe {
            sodium::crypto_generichash_final(
                &mut self.st as *mut sodium::crypto_generichash_state,
                out.as_mut_ptr(),
                out.len(),
            )
        } != 0
        {
            panic!();
        }

        out
    }
}

pub fn keyed_content_address(data: &[u8], key: &HashKey) -> Address {
    let mut hs = HashState::new(Some(key));
    hs.update(data);
    let bytes = hs.finish();
    Address { bytes }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_round_trip() {
        init();
        let mut nonce = BoxNonce::new();
        let (pk, sk) = box_keypair();
        let psk = BoxPreSharedKey::new();
        let bk = box_compute_key(&pk, &sk, &psk);
        let pt1 = vec![1, 2, 3];
        let mut bt = Vec::new();
        bt.resize_with(pt1.len() + BOX_NONCEBYTES + BOX_MACBYTES, Default::default);
        box_encrypt(&mut bt, &pt1, &mut nonce, &bk);
        let mut pt2 = Vec::<u8>::new();
        pt2.resize_with(pt1.len(), Default::default);
        assert!(box_decrypt(&mut pt2, &bt, &bk));
        assert_eq!(pt1, pt2);
    }

    #[test]
    fn data_round_trip() {
        init();
        let (pk, sk) = box_keypair();
        let psk = BoxPreSharedKey::new();
        let pt1 = vec![1, 2, 3];
        let mut ectx1 = EncryptionContext::new(&pk, &psk);
        let mut ectx2 = EncryptionContext::new(&pk, &psk);
        let ct1 = ectx1.encrypt_data(pt1.clone(), DataCompression::None);
        let ct2 = ectx2.encrypt_data(pt1.clone(), DataCompression::Zstd);
        let mut dctx = DecryptionContext::new(sk, psk);
        let pt2 = dctx.decrypt_data(ct1).unwrap();
        let pt3 = dctx.decrypt_data(ct2).unwrap();
        assert_eq!(pt1, pt2);
        assert_eq!(pt1, pt3);
    }

    #[test]
    fn box_nonce_inc() {
        init();
        let mut nonce = BoxNonce::new();
        nonce.bytes = [0; sodium::crypto_secretbox_NONCEBYTES as usize];
        for _i in 0..255 {
            nonce.inc()
        }
        assert_eq!(nonce.bytes[0], 255);
        assert_eq!(nonce.bytes[1], 0);
        nonce.inc();
        assert_eq!(nonce.bytes[0], 0);
        assert_eq!(nonce.bytes[1], 1);
        nonce.bytes = [255; sodium::crypto_secretbox_NONCEBYTES as usize];
        nonce.inc();
        for b in nonce.bytes.iter() {
            assert_eq!(*b, 0);
        }
    }
}
