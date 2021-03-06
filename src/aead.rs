use crate::{
    kdf::{Kdf, LabeledExpand},
    kex::{Marshallable, Unmarshallable},
    setup::ExporterSecret,
    HpkeError,
};

use core::u8;

use aead::{Aead as BaseAead, NewAead as BaseNewAead};
use digest::generic_array::GenericArray;
use hkdf::Hkdf;

/// Represents authenticated encryption functionality
pub trait Aead {
    /// The underlying AEAD implementation
    type AeadImpl: BaseAead + BaseNewAead + Clone;

    /// The algorithm identifier for an AEAD implementation
    const AEAD_ID: u16;
}

/// The implementation of AES-GCM-128
pub struct AesGcm128 {}

impl Aead for AesGcm128 {
    type AeadImpl = aes_gcm::Aes128Gcm;

    // draft02 §8.3: AES-GCM-128
    const AEAD_ID: u16 = 0x0001;
}

/// The implementation of AES-GCM-128
pub struct AesGcm256 {}

impl Aead for AesGcm256 {
    type AeadImpl = aes_gcm::Aes256Gcm;

    // draft02 §8.3: AES-GCM-256
    const AEAD_ID: u16 = 0x0002;
}

/// The implementation of ChaCha20-Poly1305
pub struct ChaCha20Poly1305 {}

impl Aead for ChaCha20Poly1305 {
    type AeadImpl = chacha20poly1305::ChaCha20Poly1305;

    // draft02 §8.3: ChaCha20Poly1305
    const AEAD_ID: u16 = 0x0003;
}

/// Treats the given seq (which is a bytestring) as a big-endian integer, and increments it
///
/// Return Value
/// ============
/// Returns Ok(()) if successful. Returns Err(()) if an overflow occured.
fn increment_seq<A: Aead>(arr: &mut Seq<A>) -> Result<(), ()> {
    let arr = arr.0.as_mut_slice();
    for byte in arr.iter_mut().rev() {
        if *byte < u8::MAX {
            // If the byte is below the max, increment it
            *byte += 1;
            return Ok(());
        } else {
            // Otherwise, it's at the max, and we'll have to increment a more significant byte. In
            // that case, clear this byte.
            *byte = 0;
        }
    }

    // If we got to the end and never incremented a byte, this array was maxed out
    Err(())
}

// From draft02 §6.6
//     def Context.Nonce(seq):
//       encSeq = encode_big_endian(seq, len(self.nonce))
//       return xor(self.nonce, encSeq)
/// Derives a nonce from the given nonce and a "sequence number". The sequence number is treated as
/// a big-endian integer with length equal to the nonce length.
fn mix_nonce<A: Aead>(base_nonce: &AeadNonce<A>, seq: &Seq<A>) -> AeadNonce<A> {
    // `seq` is already a byte string in big-endian order, so no conversion is necessary.

    // XOR the base nonce bytes with the sequence bytes
    let new_nonce_iter = base_nonce
        .iter()
        .zip(seq.0.iter())
        .map(|(nonce_byte, seq_byte)| nonce_byte ^ seq_byte);

    // This cannot fail, as the length of Nonce<A> is precisely the length of Seq<A>
    GenericArray::from_exact_iter(new_nonce_iter).unwrap()
}

// A nonce is the same thing as a sequence counter. But you never increment a nonce.
pub(crate) type AeadNonce<A> = GenericArray<u8, <<A as Aead>::AeadImpl as BaseAead>::NonceSize>;
pub(crate) type AeadKey<A> = GenericArray<u8, <<A as Aead>::AeadImpl as aead::NewAead>::KeySize>;

/// A sequence counter
struct Seq<A: Aead>(AeadNonce<A>);

/// The default sequence counter is all zeros
impl<A: Aead> Default for Seq<A> {
    fn default() -> Seq<A> {
        Seq(<AeadNonce<A> as Default>::default())
    }
}

// Necessary for test_overflow
#[cfg(test)]
impl<A: Aead> Clone for Seq<A> {
    fn clone(&self) -> Seq<A> {
        Seq(self.0.clone())
    }
}

/// An authenticated encryption tag
pub struct AeadTag<A: Aead>(GenericArray<u8, <A::AeadImpl as BaseAead>::TagSize>);

impl<A: Aead> Marshallable for AeadTag<A> {
    type OutputSize = <A::AeadImpl as BaseAead>::TagSize;

    fn marshal(&self) -> GenericArray<u8, Self::OutputSize> {
        self.0.clone()
    }
}

impl<A: Aead> Unmarshallable for AeadTag<A> {
    fn unmarshal(encoded: &[u8]) -> Result<Self, HpkeError> {
        if encoded.len() != Self::size() {
            Err(HpkeError::InvalidEncoding)
        } else {
            // Copy to a fixed-size array
            let mut arr = <GenericArray<u8, Self::OutputSize> as Default>::default();
            arr.copy_from_slice(encoded);
            Ok(AeadTag(arr))
        }
    }
}

/// The HPKE encryption context. This is what you use to `seal` plaintexts and `open` ciphertexts.
pub(crate) struct AeadCtx<A: Aead, K: Kdf> {
    /// Records whether the nonce sequence counter has overflowed
    overflowed: bool,
    /// The underlying AEAD instance. This also does decryption.
    encryptor: A::AeadImpl,
    /// The base nonce which we XOR with sequence numbers
    nonce: AeadNonce<A>,
    /// The exporter secret, used in the `export()` method
    exporter_secret: ExporterSecret<K>,
    /// The running sequence number
    seq: Seq<A>,
}

// Necessary for test_setup_soundness
#[cfg(test)]
impl<A: Aead, K: Kdf> Clone for AeadCtx<A, K> {
    fn clone(&self) -> AeadCtx<A, K> {
        AeadCtx {
            overflowed: self.overflowed,
            encryptor: self.encryptor.clone(),
            nonce: self.nonce.clone(),
            exporter_secret: self.exporter_secret.clone(),
            seq: self.seq.clone(),
        }
    }
}

impl<A: Aead, K: Kdf> AeadCtx<A, K> {
    /// Makes an AeadCtx from a raw key and nonce
    pub(crate) fn new(
        key: AeadKey<A>,
        nonce: AeadNonce<A>,
        exporter_secret: ExporterSecret<K>,
    ) -> AeadCtx<A, K> {
        AeadCtx {
            overflowed: false,
            encryptor: <A::AeadImpl as aead::NewAead>::new(key),
            nonce,
            exporter_secret,
            seq: <Seq<A> as Default>::default(),
        }
    }

    // def Context.Export(exporter_context, L):
    //     return Expand(self.exporter_secret, exporter_context, L)
    /// Fills a given buffer with secret bytes derived from this encryption context. This value
    /// does not depend on sequence number, so it is constant for the lifetime of this context.
    pub fn export(&self, info: &[u8], out_buf: &mut [u8]) -> Result<(), HpkeError> {
        // Use our exporter secret as the PRK for an HKDF-Expand op. The only time this fails is
        // when the length of the PRK is not the the underlying hash function's digest size. But
        // that's guaranteed by the type system, so we can unwrap().
        let hkdf_ctx = Hkdf::<K::HashImpl>::from_prk(self.exporter_secret.as_slice()).unwrap();

        // This call either succeeds or returns hkdf::InvalidLength (iff the buffer length is more
        // than 255x the digest size of the underlying hash function)
        hkdf_ctx
            .labeled_expand(b"sec", info, out_buf)
            .map_err(|_| HpkeError::InvalidKdfLength)
    }
}

/// The HPKE receiver's context. This is what you use to `open` ciphertexts.
pub struct AeadCtxR<A: Aead, K: Kdf>(AeadCtx<A, K>);

// AeadCtx -> AeadCtxR via wrapping
impl<A: Aead, K: Kdf> From<AeadCtx<A, K>> for AeadCtxR<A, K> {
    fn from(ctx: AeadCtx<A, K>) -> AeadCtxR<A, K> {
        AeadCtxR(ctx)
    }
}

// Necessary for test_setup_soundness
#[cfg(test)]
impl<A: Aead, K: Kdf> Clone for AeadCtxR<A, K> {
    fn clone(&self) -> AeadCtxR<A, K> {
        self.0.clone().into()
    }
}

impl<A: Aead, K: Kdf> AeadCtxR<A, K> {
    // def Context.Open(aad, ct):
    //   pt = Open(self.key, self.Nonce(self.seq), aad, ct)
    //   if pt == OpenError:
    //     return OpenError
    //   self.IncrementSeq()
    //   return pt
    /// Does a "detached open in place", meaning it overwrites `ciphertext` with the resulting
    /// plaintext, and takes the tag as a separate input.
    ///
    /// Return Value
    /// ============
    /// Returns `Ok(())` on success.  If this context has been used for so many encryptions that
    /// the sequence number overflowed, returns `Err(HpkeError::SeqOverflow)`. If this happens,
    /// `plaintext` will be unmodified. If the tag fails to validate, returns
    /// `Err(HpkeError::InvalidTag)`. If this happens, `plaintext` is in an undefined state.
    pub fn open(
        &mut self,
        ciphertext: &mut [u8],
        aad: &[u8],
        tag: &AeadTag<A>,
    ) -> Result<(), HpkeError> {
        if self.0.overflowed {
            // If the sequence counter overflowed, we've been used for far too long. Shut down.
            Err(HpkeError::SeqOverflow)
        } else {
            // Compute the nonce and do the encryption in place
            let nonce = mix_nonce(&self.0.nonce, &self.0.seq);
            let decrypt_res = self
                .0
                .encryptor
                .decrypt_in_place_detached(&nonce, &aad, ciphertext, &tag.0);

            if decrypt_res.is_err() {
                // Opening failed due to a bad tag
                return Err(HpkeError::InvalidTag);
            }

            // Opening was a success
            // Try to increment the sequence counter. If it fails, this was our last
            // decryption.
            if increment_seq(&mut self.0.seq).is_err() {
                self.0.overflowed = true;
            }

            Ok(())
        }
    }

    /// Fills a given buffer with secret bytes derived from this encryption context. This value
    /// does not depend on sequence number, so it is constant for the lifetime of this context.
    ///
    /// Return Value
    /// ============
    /// Returns `Ok(())` on success. If the buffer length is more than 255x the digest size of the
    /// underlying hash function, returns an `Err(HpkeError::InvalidKdfLength)`.
    pub fn export(&self, info: &[u8], out_buf: &mut [u8]) -> Result<(), HpkeError> {
        // Pass to AeadCtx
        self.0.export(info, out_buf)
    }
}

/// The HPKE senders's context. This is what you use to `seal` plaintexts.
pub struct AeadCtxS<A: Aead, K: Kdf>(AeadCtx<A, K>);

// AeadCtx -> AeadCtxS via wrapping
impl<A: Aead, K: Kdf> From<AeadCtx<A, K>> for AeadCtxS<A, K> {
    fn from(ctx: AeadCtx<A, K>) -> AeadCtxS<A, K> {
        AeadCtxS(ctx)
    }
}

// Necessary for test_setup_soundness
#[cfg(test)]
impl<A: Aead, K: Kdf> Clone for AeadCtxS<A, K> {
    fn clone(&self) -> AeadCtxS<A, K> {
        self.0.clone().into()
    }
}

impl<A: Aead, K: Kdf> AeadCtxS<A, K> {
    // def Context.Seal(aad, pt):
    //   ct = Seal(self.key, self.Nonce(self.seq), aad, pt)
    //   self.IncrementSeq()
    //   return ct
    /// Does a "detached seal in place", meaning it overwrites `plaintext` with the resulting
    /// ciphertext, and returns the resulting authentication tag
    ///
    /// Return Value
    /// ============
    /// Returns `Ok(tag)` on success.  If this context has been used for so many encryptions that
    /// the sequence number overflowed, returns `Err(HpkeError::SeqOverflow)`. If this happens,
    /// `plaintext` will be unmodified. If an unspecified error happened during encryption, returns
    /// `Err(HpkeError::Encryption)`. If this happens, the contents of `plaintext` is undefined.
    pub fn seal(&mut self, plaintext: &mut [u8], aad: &[u8]) -> Result<AeadTag<A>, HpkeError> {
        if self.0.overflowed {
            // If the sequence counter overflowed, we've been used for far too long. Shut down.
            Err(HpkeError::SeqOverflow)
        } else {
            // Compute the nonce and do the encryption in place
            let nonce = mix_nonce(&self.0.nonce, &self.0.seq);
            let tag_res = self
                .0
                .encryptor
                .encrypt_in_place_detached(&nonce, &aad, plaintext);

            // Check if an error occurred when encrypting
            let tag = match tag_res {
                Err(_) => return Err(HpkeError::Encryption),
                Ok(t) => t,
            };

            // Try to increment the sequence counter. If it fails, this was our last encryption.
            if increment_seq(&mut self.0.seq).is_err() {
                self.0.overflowed = true;
            }

            // Return the tag
            Ok(AeadTag(tag))
        }
    }

    /// Fills a given buffer with secret bytes derived from this encryption context. This value
    /// does not depend on sequence number, so it is constant for the lifetime of this context.
    ///
    /// Return Value
    /// ============
    /// Returns `Ok(())` on success. If the buffer length is more than 255x the digest size of the
    /// underlying hash function, returns an `Err(HpkeError::InvalidKdfLength)`.
    pub fn export(&self, info: &[u8], out_buf: &mut [u8]) -> Result<(), HpkeError> {
        // Pass to AeadCtx
        self.0.export(info, out_buf)
    }
}

#[cfg(test)]
mod test {
    use super::{AeadTag, AesGcm128, AesGcm256, ChaCha20Poly1305, Seq};
    use crate::{kdf::HkdfSha256, kex::Unmarshallable, test_util::gen_ctx_simple_pair, HpkeError};

    use core::u8;

    /// Tests that encryption context secret export does not change behavior based on the
    /// underlying sequence number
    #[test]
    fn test_export_idempotence() {
        // Set up a context. Logic is algorithm-independent, so we don't care about the types here
        let (mut sender_ctx, _) = gen_ctx_simple_pair::<ChaCha20Poly1305, HkdfSha256>();

        // Get an initial export secret
        let mut secret1 = [0u8; 16];
        sender_ctx
            .export(b"test_export_idempotence", &mut secret1)
            .unwrap();

        // Modify the context by encrypting something
        let mut plaintext = *b"back hand";
        sender_ctx
            .seal(&mut plaintext[..], b"")
            .expect("seal() failed");

        // Get a second export secret
        let mut secret2 = [0u8; 16];
        sender_ctx
            .export(b"test_export_idempotence", &mut secret2)
            .unwrap();

        assert_eq!(secret1, secret2);
    }

    /// Tests that sequence overflowing causes an error. This logic is cipher-agnostic, so we don't
    /// bother making this a macro
    #[test]
    fn test_overflow() {
        // Make a sequence number that's at the max
        let big_seq = {
            let mut buf = <Seq<ChaCha20Poly1305> as Default>::default();
            // Set all the values to the max
            for byte in buf.0.iter_mut() {
                *byte = u8::MAX;
            }
            buf
        };

        let (mut sender_ctx, mut receiver_ctx) =
            gen_ctx_simple_pair::<ChaCha20Poly1305, HkdfSha256>();
        sender_ctx.0.seq = big_seq.clone();
        receiver_ctx.0.seq = big_seq.clone();

        // These should support precisely one more encryption before it registers an overflow

        let msg = b"draxx them sklounst";
        let aad = b"with my prayers";

        // Do one round trip and ensure it works
        {
            let mut plaintext = *msg;
            // Encrypt the plaintext
            let tag = sender_ctx
                .seal(&mut plaintext[..], aad)
                .expect("seal() failed");
            // Rename for clarity
            let mut ciphertext = plaintext;

            // Now to decrypt on the other side
            receiver_ctx
                .open(&mut ciphertext[..], aad, &tag)
                .expect("open() failed");
            // Rename for clarity
            let roundtrip_plaintext = ciphertext;

            // Make sure the output message was the same as the input message
            assert_eq!(msg, &roundtrip_plaintext);
        }

        // Try another round trip and ensure that we've overflowed
        {
            let mut plaintext = *msg;
            // Try to encrypt the plaintext
            match sender_ctx.seal(&mut plaintext[..], aad) {
                Err(HpkeError::SeqOverflow) => {} // Good, this should have overflowed
                Err(e) => panic!("seal() should have overflowed. Instead got {}", e),
                _ => panic!("seal() should have overflowed. Instead it succeeded"),
            }

            // Now try to decrypt something. This isn't a valid ciphertext or tag, but the overflow
            // should fail before the tag check fails.
            let mut dummy_ciphertext = [0u8; 32];
            let dummy_tag = AeadTag::unmarshal(&[0; 16]).unwrap();

            match receiver_ctx.open(&mut dummy_ciphertext[..], aad, &dummy_tag) {
                Err(HpkeError::SeqOverflow) => {} // Good, this should have overflowed
                Err(e) => panic!("open() should have overflowed. Instead got {}", e),
                _ => panic!("open() should have overflowed. Instead it succeeded"),
            }
        }
    }

    /// Tests that `open()` can decrypt things properly encrypted with `seal()`
    macro_rules! test_ctx_correctness {
        ($test_name:ident, $aead_ty:ty) => {
            #[test]
            fn $test_name() {
                type A = $aead_ty;
                type K = HkdfSha256;

                let (mut sender_ctx, mut receiver_ctx) = gen_ctx_simple_pair::<A, K>();

                let msg = b"Love it or leave it, you better gain way";
                let aad = b"You better hit bull's eye, the kid don't play";

                // Encrypt with the sender context
                let mut ciphertext = msg.clone();
                let tag = sender_ctx
                    .seal(&mut ciphertext[..], aad)
                    .expect("seal() failed");

                // Make sure seal() isn't a no-op
                assert!(&ciphertext[..] != &msg[..]);

                // Decrypt with the receiver context
                receiver_ctx
                    .open(&mut ciphertext[..], aad, &tag)
                    .expect("open() failed");
                // Change name for clarity
                let decrypted = ciphertext;
                assert_eq!(&decrypted[..], &msg[..]);
            }
        };
    }

    // The hash function and DH impl shouldn't really matter
    test_ctx_correctness!(test_ctx_correctness_aes128, AesGcm128);
    test_ctx_correctness!(test_ctx_correctness_aes256, AesGcm256);
    test_ctx_correctness!(test_ctx_correctness_chacha, ChaCha20Poly1305);
}
