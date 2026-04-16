use bitcoin::hashes::{Hash, sha256};
use bls12_381::{G1Affine, G1Projective, G2Affine, G2Projective, Scalar};
use borsh::{BorshDeserialize, BorshSerialize};
use group::Group;
use group::ff::Field;
use rand::rngs::OsRng;
use secp256k1::rand::rngs::OsRng as SecpOsRng;
use secp256k1::{Keypair, Message, SECP256K1};

fn roundtrip<T: BorshSerialize + BorshDeserialize + PartialEq + std::fmt::Debug>(v: &T) {
    let bytes = borsh::to_vec(v).expect("serialize");
    let back: T = borsh::from_slice(&bytes).expect("deserialize");
    assert_eq!(v, &back);
}

#[derive(Debug, PartialEq, BorshSerialize, BorshDeserialize)]
struct BlsBundle {
    #[borsh(
        serialize_with = "super::bls_scalar::serialize",
        deserialize_with = "super::bls_scalar::deserialize"
    )]
    scalar: Scalar,
    #[borsh(
        serialize_with = "super::bls_g1_affine::serialize",
        deserialize_with = "super::bls_g1_affine::deserialize"
    )]
    g1: G1Affine,
    #[borsh(
        serialize_with = "super::bls_g2_affine::serialize",
        deserialize_with = "super::bls_g2_affine::deserialize"
    )]
    g2: G2Affine,
    #[borsh(
        serialize_with = "super::bls_g1_projective::serialize",
        deserialize_with = "super::bls_g1_projective::deserialize"
    )]
    g1p: G1Projective,
    #[borsh(
        serialize_with = "super::bls_g2_projective::serialize",
        deserialize_with = "super::bls_g2_projective::deserialize"
    )]
    g2p: G2Projective,
}

#[test]
fn bls_roundtrip() {
    let mut rng = OsRng;
    let bundle = BlsBundle {
        scalar: Scalar::random(&mut rng),
        g1: G1Affine::generator(),
        g2: G2Affine::generator(),
        g1p: G1Projective::random(&mut rng),
        g2p: G2Projective::random(&mut rng),
    };
    roundtrip(&bundle);
}

#[derive(Debug, PartialEq, BorshSerialize, BorshDeserialize)]
struct SecpBundle {
    #[borsh(
        serialize_with = "super::secp_public_key::serialize",
        deserialize_with = "super::secp_public_key::deserialize"
    )]
    pk: secp256k1::PublicKey,
    #[borsh(
        serialize_with = "super::secp_secret_key::serialize",
        deserialize_with = "super::secp_secret_key::deserialize"
    )]
    sk: secp256k1::SecretKey,
    #[borsh(
        serialize_with = "super::secp_xonly_public_key::serialize",
        deserialize_with = "super::secp_xonly_public_key::deserialize"
    )]
    xonly: secp256k1::XOnlyPublicKey,
    #[borsh(
        serialize_with = "super::secp_ecdsa_signature::serialize",
        deserialize_with = "super::secp_ecdsa_signature::deserialize"
    )]
    ecdsa: secp256k1::ecdsa::Signature,
    #[borsh(
        serialize_with = "super::secp_schnorr_signature::serialize",
        deserialize_with = "super::secp_schnorr_signature::deserialize"
    )]
    schnorr: secp256k1::schnorr::Signature,
}

#[test]
fn secp_roundtrip() {
    let mut rng = SecpOsRng;
    let kp = Keypair::new(SECP256K1, &mut rng);
    let sk = secp256k1::SecretKey::from_keypair(&kp);
    let pk = secp256k1::PublicKey::from_keypair(&kp);
    let (xonly, _) = kp.x_only_public_key();
    let msg = Message::from_digest(*sha256::Hash::hash(b"hello").as_byte_array());
    let ecdsa = SECP256K1.sign_ecdsa(&msg, &sk);
    let schnorr = SECP256K1.sign_schnorr(&msg, &kp);

    let bundle = SecpBundle {
        pk,
        sk,
        xonly,
        ecdsa,
        schnorr,
    };
    roundtrip(&bundle);
}

#[derive(Debug, PartialEq, BorshSerialize, BorshDeserialize)]
struct HashBundle {
    #[borsh(
        serialize_with = "super::sha256_hash::serialize",
        deserialize_with = "super::sha256_hash::deserialize"
    )]
    h: sha256::Hash,
}

#[test]
fn sha256_roundtrip() {
    let bundle = HashBundle {
        h: sha256::Hash::hash(b"hello world"),
    };
    roundtrip(&bundle);
}
