//! BIP-32 hierarchical deterministic keys (watch-only server side).
//!
//! Implements the `CKDpriv`/`CKDpub` derivation rules and base58check
//! serialization from [BIP-0032]. Only what a watch-only subscription server
//! needs: derive an account xpub from a seed (cold storage), then derive user
//! addresses from that xpub without ever touching the private key. Addresses
//! are rendered with the [EIP-55] mixed-case checksum.
//!
//! [BIP-0032]: https://github.com/bitcoin/bips/blob/master/bip-0032.mediawiki
//! [EIP-55]: https://eips.ethereum.org/EIPS/eip-55
//!
//! Note: per BIP-32, non-hardened `CKDpriv` uses the *compressed parent public
//! key* (`serP(K_par) ‖ ser32(i)`) as HMAC input, while hardened uses
//! `0x00 ‖ ser256(k_par) ‖ ser32(i)` — a common implementation pitfall that
//! is covered by the official test vectors below.

use anyhow::{Context, Result, bail};
use hmac::digest::{FixedOutput, Update};
use hmac::{KeyInit, SimpleHmac};
use k256::elliptic_curve::ff::PrimeField;
use k256::elliptic_curve::sec1::{FromSec1Point, Sec1Point, ToSec1Point};
use k256::{AffinePoint, ProjectivePoint, Scalar};
use ripemd::Ripemd160;
use sha2::{Digest, Sha256, Sha512};
use sha3::Keccak256;

use std::fmt;
use std::str::FromStr;

const VERSION_PUB: u32 = 0x0488_B21E;
const VERSION_PRIV: u32 = 0x0488_ADE4;

/// Offset of hardened child indexes (`i'` = `i + HARDENED`).
pub const HARDENED: u32 = 0x8000_0000;

/// Default account path for Ethereum (BIP-44 coin type 60, external chain).
pub const DEFAULT_ACCOUNT_PATH: &str = "m/44'/60'/0'/0";

/// Parse a derivation path such as `m/44'/60'/0'/0` (`h` also marks hardened).
pub fn parse_path(path: &str) -> Result<Vec<u32>> {
    let rest = path
        .strip_prefix('m')
        .with_context(|| format!("derivation path must start with 'm': {path}"))?;
    rest.split('/')
        .skip(1)
        .map(|seg| {
            let (num, hardened) = match seg.strip_suffix(['\'', 'h']) {
                Some(n) => (n, true),
                None => (seg, false),
            };
            let n: u32 = num
                .parse()
                .with_context(|| format!("bad path segment {seg:?} in {path}"))?;
            if n >= HARDENED {
                bail!("path segment {seg:?} out of range");
            }
            Ok(if hardened { n | HARDENED } else { n })
        })
        .collect()
}

/// Extended private key. Only used offline by `genxpub`; the server never
/// loads one.
#[derive(Clone, PartialEq, Eq)]
pub struct Xprv {
    depth: u8,
    parent_fp: [u8; 4],
    child_num: u32,
    chain_code: [u8; 32],
    key: [u8; 32],
}

/// Extended public key: the server's watch-only view of the account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Xpub {
    depth: u8,
    parent_fp: [u8; 4],
    child_num: u32,
    chain_code: [u8; 32],
    key: [u8; 33],
}

impl fmt::Debug for Xprv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Xprv")
            .field("depth", &self.depth)
            .field("child_num", &self.child_num)
            .finish_non_exhaustive()
    }
}

impl Xprv {
    /// Master key from a BIP-32 seed (16–64 bytes).
    pub fn from_seed(seed: &[u8]) -> Result<Self> {
        if !(16..=64).contains(&seed.len()) {
            bail!("seed must be 16-64 bytes, got {}", seed.len());
        }
        let i = hmac_sha512(b"Bitcoin seed", seed);
        let key: [u8; 32] = i[..32].try_into().unwrap();
        let chain_code: [u8; 32] = i[32..].try_into().unwrap();
        scalar_be(&key).context("master key out of range")?;
        Ok(Self {
            depth: 0,
            parent_fp: [0; 4],
            child_num: 0,
            chain_code,
            key,
        })
    }

    /// Derive along a full path, e.g. [`DEFAULT_ACCOUNT_PATH`].
    pub fn derive_path(&self, path: &str) -> Result<Self> {
        parse_path(path)?
            .into_iter()
            .try_fold(self.clone(), |key, index| key.derive(index))
    }

    /// `CKDpriv`: derive a child (hardened when `index >= HARDENED`).
    pub fn derive(&self, index: u32) -> Result<Self> {
        let mut data = Vec::with_capacity(37);
        if index >= HARDENED {
            data.push(0);
            data.extend_from_slice(&self.key);
        } else {
            data.extend_from_slice(&self.to_xpub().key);
        }
        data.extend_from_slice(&index.to_be_bytes());
        let i = hmac_sha512(&self.chain_code, &data);
        let il = scalar_be(&i[..32]).context("IL out of range")?;
        let k = scalar_be(&self.key).context("parent key out of range")?;
        let child_key = scalar_bytes(il.add(&k));
        let chain_code: [u8; 32] = i[32..].try_into().unwrap();
        Ok(Self {
            depth: self.depth.checked_add(1).context("max depth exceeded")?,
            parent_fp: self.fingerprint(),
            child_num: index,
            chain_code,
            key: child_key,
        })
    }

    pub fn fingerprint(&self) -> [u8; 4] {
        let mut fp = [0u8; 4];
        fp.copy_from_slice(&hash160(&self.to_xpub().key)[..4]);
        fp
    }

    pub fn to_xpub(&self) -> Xpub {
        let k = scalar_be(&self.key).expect("valid key");
        let point = (ProjectivePoint::GENERATOR * k).to_affine();
        let key: [u8; 33] = point.to_sec1_point(true).as_bytes().try_into().unwrap();
        Xpub {
            depth: self.depth,
            parent_fp: self.parent_fp,
            child_num: self.child_num,
            chain_code: self.chain_code,
            key,
        }
    }

    pub fn to_base58(&self) -> String {
        let mut payload = Vec::with_capacity(74);
        payload.push(self.depth);
        payload.extend_from_slice(&self.parent_fp);
        payload.extend_from_slice(&self.child_num.to_be_bytes());
        payload.extend_from_slice(&self.chain_code);
        payload.push(0);
        payload.extend_from_slice(&self.key);
        base58check(VERSION_PRIV, &payload)
    }
}

impl fmt::Display for Xprv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_base58())
    }
}

impl FromStr for Xpub {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let body = from_base58check(s)?;
        if u32::from_be_bytes(body[0..4].try_into()?) != VERSION_PUB {
            bail!("not an xpub (wrong version bytes)");
        }
        let key: [u8; 33] = body[45..78].try_into()?;
        decode_affine(&key).context("xpub key is not a valid secp256k1 point")?;
        Ok(Self {
            depth: body[4],
            parent_fp: body[5..9].try_into()?,
            child_num: u32::from_be_bytes(body[9..13].try_into()?),
            chain_code: body[13..45].try_into()?,
            key,
        })
    }
}

impl Xpub {
    /// `CKDpub`: derive a non-hardened child.
    pub fn derive(&self, index: u32) -> Result<Self> {
        if index >= HARDENED {
            bail!("cannot derive hardened index from an xpub");
        }
        let mut data = Vec::with_capacity(37);
        data.extend_from_slice(&self.key);
        data.extend_from_slice(&index.to_be_bytes());
        let i = hmac_sha512(&self.chain_code, &data);
        let il = scalar_be(&i[..32]).context("IL out of range")?;
        let parent = decode_affine(&self.key).context("invalid parent key")?;
        let child = ProjectivePoint::from(parent) + ProjectivePoint::GENERATOR * il;
        let key: [u8; 33] = child
            .to_affine()
            .to_sec1_point(true)
            .as_bytes()
            .try_into()
            .unwrap();
        let chain_code: [u8; 32] = i[32..].try_into().unwrap();
        Ok(Self {
            depth: self.depth.checked_add(1).context("max depth exceeded")?,
            parent_fp: self.fingerprint(),
            child_num: index,
            chain_code,
            key,
        })
    }

    /// EIP-55 checksummed Ethereum address of this key.
    pub fn address(&self) -> String {
        // Every constructor validates the point, so decoding cannot fail.
        let affine = decode_affine(&self.key).expect("xpub key validated at construction");
        let enc = affine.to_sec1_point(false);
        let hash = Keccak256::digest(&enc.as_bytes()[1..]);
        checksum_address(&hash[12..])
    }

    pub fn fingerprint(&self) -> [u8; 4] {
        let mut fp = [0u8; 4];
        fp.copy_from_slice(&hash160(&self.key)[..4]);
        fp
    }

    pub fn to_base58(&self) -> String {
        let mut payload = Vec::with_capacity(74);
        payload.push(self.depth);
        payload.extend_from_slice(&self.parent_fp);
        payload.extend_from_slice(&self.child_num.to_be_bytes());
        payload.extend_from_slice(&self.chain_code);
        payload.extend_from_slice(&self.key);
        base58check(VERSION_PUB, &payload)
    }
}

impl fmt::Display for Xpub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_base58())
    }
}

/// EIP-55: uppercase each hex letter whose keccak nibble is >= 8.
pub fn checksum_address(addr: &[u8]) -> String {
    let lower = hex::encode(addr);
    let hash = Keccak256::digest(lower.as_bytes());
    let mut out = String::with_capacity(42);
    out.push_str("0x");
    for (i, c) in lower.chars().enumerate() {
        let nibble = (hash[i / 2] >> (if i % 2 == 0 { 4 } else { 0 })) & 0x0f;
        out.push(if nibble >= 8 {
            c.to_ascii_uppercase()
        } else {
            c
        });
    }
    out
}

fn decode_affine(sec1_bytes: &[u8]) -> Result<AffinePoint> {
    let point = Sec1Point::<k256::Secp256k1>::from_bytes(sec1_bytes)?;
    let affine: Option<AffinePoint> = AffinePoint::from_sec1_point(&point).into();
    affine.context("invalid sec1 point")
}

fn scalar_be(bytes: &[u8]) -> Result<Scalar> {
    let arr: k256::elliptic_curve::FieldBytes<k256::Secp256k1> =
        bytes.try_into().context("scalar must be 32 bytes")?;
    let s: Option<Scalar> = Scalar::from_repr(arr).into();
    s.context("scalar out of range")
}

fn scalar_bytes(s: Scalar) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&s.to_bytes());
    out
}

fn hmac_sha512(key: &[u8], data: &[u8]) -> [u8; 64] {
    let mut mac = SimpleHmac::<Sha512>::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(data);
    let out = <SimpleHmac<Sha512> as FixedOutput>::finalize_fixed(mac);
    let mut arr = [0u8; 64];
    arr.copy_from_slice(&out);
    arr
}

fn sha256d(data: &[u8]) -> [u8; 32] {
    Sha256::digest(Sha256::digest(data)).into()
}

fn hash160(data: &[u8]) -> [u8; 20] {
    Ripemd160::digest(Sha256::digest(data)).into()
}

fn base58check(version: u32, payload: &[u8]) -> String {
    let mut data = Vec::with_capacity(4 + payload.len());
    data.extend_from_slice(&version.to_be_bytes());
    data.extend_from_slice(payload);
    let checksum = sha256d(&data);
    data.extend_from_slice(&checksum[..4]);
    bs58::encode(data).into_string()
}

fn from_base58check(s: &str) -> Result<Vec<u8>> {
    let raw = bs58::decode(s).into_vec().context("invalid base58")?;
    if raw.len() != 82 {
        bail!("invalid base58check length");
    }
    let (body, checksum) = raw.split_at(78);
    if sha256d(body)[..4] != *checksum {
        bail!("bad base58check checksum");
    }
    Ok(body.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bip32_test_vector_1() {
        let seed = hex::decode("000102030405060708090a0b0c0d0e0f").unwrap();
        let m = Xprv::from_seed(&seed).unwrap();
        assert_eq!(
            m.to_string(),
            "xprv9s21ZrQH143K3QTDL4LXw2F7HEK3wJUD2nW2nRk4stbPy6cq3jPPqjiChkVvvNKmPGJxWUtg6LnF5kejMRNNU3TGtRBeJgk33yuGBxrMPHi"
        );
        assert_eq!(
            m.to_xpub().to_string(),
            "xpub661MyMwAqRbcFtXgS5sYJABqqG9YLmC4Q1Rdap9gSE8NqtwybGhePY2gZ29ESFjqJoCu1Rupje8YtGqsefD265TMg7usUDFdp6W1EGMcet8"
        );

        let m0h = m.derive(HARDENED).unwrap();
        assert_eq!(
            m0h.to_string(),
            "xprv9uHRZZhk6KAJC1avXpDAp4MDc3sQKNxDiPvvkX8Br5ngLNv1TxvUxt4cV1rGL5hj6KCesnDYUhd7oWgT11eZG7XnxHrnYeSvkzY7d2bhkJ7"
        );
        let m0h_xpub = m0h.to_xpub();
        assert_eq!(
            m0h_xpub.to_string(),
            "xpub68Gmy5EdvgibQVfPdqkBBCHxA5htiqg55crXYuXoQRKfDBFA1WEjWgP6LHhwBZeNK1VTsfTFUHCdrfp1bgwQ9xv5ski8PX9rL2dZXvgGDnw"
        );

        let m0h1 = m0h.derive(1).unwrap();
        assert_eq!(
            m0h1.to_string(),
            "xprv9wTYmMFdV23N2TdNG573QoEsfRrWKQgWeibmLntzniatZvR9BmLnvSxqu53Kw1UmYPxLgboyZQaXwTCg8MSY3H2EU4pWcQDnRnrVA1xe8fs"
        );

        let parsed: Xpub = "xpub68Gmy5EdvgibQVfPdqkBBCHxA5htiqg55crXYuXoQRKfDBFA1WEjWgP6LHhwBZeNK1VTsfTFUHCdrfp1bgwQ9xv5ski8PX9rL2dZXvgGDnw"
            .parse()
            .unwrap();
        assert_eq!(
            parsed.derive(1).unwrap().to_string(),
            "xpub6ASuArnXKPbfEwhqN6e3mwBcDTgzisQN1wXN9BJcM47sSikHjJf3UFHKkNAWbWMiGj7Wf5uMash7SyYq527Hqck2AxYysAA7xmALppuCkwQ"
        );
        assert_eq!(
            m0h1.to_xpub().address(),
            parsed.derive(1).unwrap().address()
        );
    }

    #[test]
    fn address_of_private_key_one() {
        let key: [u8; 33] =
            hex::decode("0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798")
                .unwrap()
                .try_into()
                .unwrap();
        let xpub = Xpub {
            depth: 1,
            parent_fp: [0; 4],
            child_num: 0,
            chain_code: [0; 32],
            key,
        };
        assert_eq!(xpub.address(), "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf");
    }

    #[test]
    fn eip55_reference_vectors() {
        for a in [
            "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed",
            "0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d359",
            "0xdbF03B407c01E7cD3CBea99509d93f8DDDC8C6FB",
            "0xD1220A0cf47c7B9Be7A2E6BA89F429762e7b9aDb",
        ] {
            let raw = hex::decode(&a[2..]).unwrap();
            assert_eq!(checksum_address(&raw), a);
        }
    }

    #[test]
    fn path_parsing_and_derivation() {
        assert_eq!(
            parse_path("m/44'/60'/0'/0").unwrap(),
            vec![HARDENED + 44, HARDENED + 60, HARDENED, 0]
        );
        assert_eq!(parse_path("m/0h/1").unwrap(), vec![HARDENED, 1]);
        assert_eq!(parse_path("m").unwrap(), Vec::<u32>::new());
        assert!(parse_path("44'/60'").is_err());
        assert!(parse_path("m/x").is_err());
        assert!(parse_path("m/2147483648").is_err());

        let seed = hex::decode("000102030405060708090a0b0c0d0e0f").unwrap();
        let m = Xprv::from_seed(&seed).unwrap();
        assert_eq!(
            m.derive_path("m/0'/1").unwrap(),
            m.derive(HARDENED).unwrap().derive(1).unwrap()
        );
        assert!(Xprv::from_seed(&[0; 8]).is_err());
    }

    #[test]
    fn hardened_derivation_from_xpub_is_rejected() {
        let seed = hex::decode("000102030405060708090a0b0c0d0e0f").unwrap();
        let xpub = Xprv::from_seed(&seed).unwrap().to_xpub();
        assert!(xpub.derive(HARDENED).is_err());
    }

    #[test]
    fn roundtrip_parse() {
        let seed = hex::decode("000102030405060708090a0b0c0d0e0f").unwrap();
        let xprv = Xprv::from_seed(&seed).unwrap();
        let xpub = xprv.to_xpub();
        let s = xpub.to_string();
        assert_eq!(Xpub::from_str(&s).unwrap(), xpub);
        assert!(Xpub::from_str("xprv9s21ZrQH143K3QTDL4LXw2F7HEK3wJUD2nW2nRk4stbPy6cq3jPPqjiChkVvvNKmPGJxWUtg6LnF5kejMRNNU3TGtRBeJgk33yuGBxrMPHi").is_err());
        assert!(Xpub::from_str("xpub661MyMwAqRbcFtXgS5sYJABqqG9YLmC4Q1Rdap9gSE8NqtwybGhePY2gZ29ESFjqJoCu1Rupje8YtGqsefD265TMg7usUDFdp6W1EGMcet9").is_err());
    }
}
