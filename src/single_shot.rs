use crate::{
    aead::{Aead, AeadTag},
    kdf::Kdf as KdfTrait,
    kem::{EncappedKey, Kem as KemTrait},
    kex::KeyExchange,
    op_mode::{OpModeR, OpModeS},
    setup::{setup_receiver, setup_sender},
    HpkeError,
};

use rand::{CryptoRng, RngCore};

// def SealAuthPSK(pkR, info, aad, pt, psk, pskID, skS):
//   enc, ctx = SetupAuthPSKS(pkR, info, psk, pskID, skS)
//   ct = ctx.Seal(aad, pt)
//   return enc, ct
/// Does a `setup_sender` and `AeadCtx::seal` in one shot. That is, it does a key encapsulation to
/// the specified recipient and encrypts the provided plaintext in place. See `setup::setup_sender`
/// and `AeadCtx::seal` for more detail.
///
/// Return Value
/// ============
/// Returns `Ok((encapped_key, tag))` on success. If an error happened during key exchange, returns
/// `Err(HpkeError::InvalidKeyExchange)`. If an unspecified error happened during encryption,
/// returns `Err(HpkeError::Encryption)`. In this case, the contents of `plaintext` is undefined.
pub fn single_shot_seal<A, Kdf, Kem, R>(
    mode: &OpModeS<Kem::Kex, Kdf>,
    pk_recip: &<Kem::Kex as KeyExchange>::PublicKey,
    info: &[u8],
    plaintext: &mut [u8],
    aad: &[u8],
    csprng: &mut R,
) -> Result<(EncappedKey<Kem::Kex>, AeadTag<A>), HpkeError>
where
    A: Aead,
    Kdf: KdfTrait,
    Kem: KemTrait,
    R: CryptoRng + RngCore,
{
    // Encap a key
    let (encapped_key, mut aead_ctx) =
        setup_sender::<A, Kdf, Kem, R>(mode, pk_recip, info, csprng)?;
    // Encrypt
    let tag = aead_ctx.seal(plaintext, aad)?;

    Ok((encapped_key, tag))
}

// def OpenAuthPSK(enc, skR, info, aad, ct, psk, pskID, pkS):
//   ctx = SetupAuthPSKR(enc, skR, info, psk, pskID, pkS)
//   return ctx.Open(aad, ct)
/// Does a `setup_receiver` and `AeadCtx::open` in one shot. That is, it does a key decapsulation
/// for the specified recipient and decrypts the provided plaintext in place. See
/// `setup::setup_reciever` and `AeadCtx::open` for more detail.
///
/// Return Value
/// ============
/// Returns `Ok()` on success. If an error happened during key exchange, returns
/// `Err(HpkeError::InvalidKeyExchange)`. If an unspecified error happened during decryption,
/// returns `Err(HpkeError::Encryption)`. In this case, the contents of `ciphertext` is undefined.
pub fn single_shot_open<A, Kdf, Kem>(
    mode: &OpModeR<Kem::Kex, Kdf>,
    sk_recip: &<Kem::Kex as KeyExchange>::PrivateKey,
    encapped_key: &EncappedKey<Kem::Kex>,
    info: &[u8],
    ciphertext: &mut [u8],
    aad: &[u8],
    tag: &AeadTag<A>,
) -> Result<(), HpkeError>
where
    A: Aead,
    Kdf: KdfTrait,
    Kem: KemTrait,
{
    // Decap the key
    let mut aead_ctx = setup_receiver::<A, Kdf, Kem>(mode, sk_recip, encapped_key, info)?;
    // Decrypt
    aead_ctx.open(ciphertext, aad, tag)
}

#[cfg(test)]
mod test {
    use super::{single_shot_open, single_shot_seal};
    use crate::{
        aead::ChaCha20Poly1305,
        kdf::HkdfSha256,
        kem::Kem as KemTrait,
        kex::KeyExchange,
        op_mode::{OpModeR, OpModeS},
        test_util::gen_psk_bundle,
    };

    use rand::{rngs::StdRng, SeedableRng};

    macro_rules! test_single_shot_correctness {
        ($test_name:ident, $aead:ty, $kdf:ty, $kem:ty) => {
            /// Tests that `single_shot_open` can open a `single_shot_seal` ciphertext. This
            /// doens't need to be tested for all ciphersuite combinations, since its correctness
            /// follows from the correctness of `seal/open` and `setup_sender/setup_receiver`.
            #[test]
            fn $test_name() {
                type A = $aead;
                type Kdf = $kdf;
                type Kem = $kem;
                type Kex = <Kem as KemTrait>::Kex;

                let msg = b"Good night, a-ding ding ding ding ding";
                let aad = b"Five four three two one";

                let mut csprng = StdRng::from_entropy();

                // Set up an arbitrary info string, a random PSK, and an arbitrary PSK ID
                let info = b"why would you think in a million years that that would actually work";
                let psk_bundle = gen_psk_bundle::<Kdf>();

                // Generate the sender's and receiver's long-term keypairs
                let (sk_sender_id, pk_sender_id) = Kex::gen_keypair(&mut csprng);
                let (sk_recip, pk_recip) = Kex::gen_keypair(&mut csprng);

                // Construct the sender's encryption context, and get an encapped key
                let sender_mode = OpModeS::<Kex, _>::AuthPsk(
                    (sk_sender_id, pk_sender_id.clone()),
                    psk_bundle.clone(),
                );

                // Use the encapped key to derive the reciever's encryption context
                let receiver_mode = OpModeR::<Kex, _>::AuthPsk(pk_sender_id, psk_bundle);

                // Encrypt with the first context
                let mut ciphertext = msg.clone();
                let (encapped_key, tag) = single_shot_seal::<A, _, Kem, _>(
                    &sender_mode,
                    &pk_recip,
                    &info[..],
                    &mut ciphertext[..],
                    aad,
                    &mut csprng,
                )
                .expect("single_shot_seal() failed");

                // Make sure seal() isn't a no-op
                assert!(&ciphertext[..] != &msg[..]);

                // Decrypt with the second context
                single_shot_open::<A, _, Kem>(
                    &receiver_mode,
                    &sk_recip,
                    &encapped_key,
                    info,
                    &mut ciphertext[..],
                    aad,
                    &tag,
                )
                .expect("single_shot_open() failed");
                // Change name for clarity
                let decrypted = ciphertext;
                assert_eq!(&decrypted[..], &msg[..]);
            }
        };
    }

    #[cfg(feature = "x25519-dalek")]
    test_single_shot_correctness!(
        test_single_shot_correctness_x25519,
        ChaCha20Poly1305,
        HkdfSha256,
        crate::kem::X25519HkdfSha256
    );

    #[cfg(feature = "p256")]
    test_single_shot_correctness!(
        test_single_shot_correctness_p256,
        ChaCha20Poly1305,
        HkdfSha256,
        crate::kem::DhP256HkdfSha256
    );
}
