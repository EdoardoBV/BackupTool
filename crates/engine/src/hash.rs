use std::fmt;
use std::str::FromStr;

use md5::Md5;
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};
use xxhash_rust::xxh3::Xxh3;
use xxhash_rust::xxh64::Xxh64;

/// Supported checksum types. All but SHA-256 are ASC MHL formats, and their
/// names match the MHL element names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HashAlgo {
    Xxh64,
    Xxh3,
    Xxh128,
    Md5,
    Sha1,
    Sha256,
    C4,
}

impl HashAlgo {
    pub const ALL: [HashAlgo; 7] = [
        HashAlgo::Xxh64,
        HashAlgo::Xxh3,
        HashAlgo::Xxh128,
        HashAlgo::Md5,
        HashAlgo::Sha1,
        HashAlgo::Sha256,
        HashAlgo::C4,
    ];

    /// Whether ASC MHL can carry this kind of checksum.
    pub fn in_mhl(self) -> bool {
        self != HashAlgo::Sha256
    }

    pub fn id(self) -> &'static str {
        match self {
            HashAlgo::Xxh64 => "xxh64",
            HashAlgo::Xxh3 => "xxh3",
            HashAlgo::Xxh128 => "xxh128",
            HashAlgo::Md5 => "md5",
            HashAlgo::Sha1 => "sha1",
            HashAlgo::Sha256 => "sha256",
            HashAlgo::C4 => "c4",
        }
    }
}

impl fmt::Display for HashAlgo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

impl FromStr for HashAlgo {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let wanted = s.to_ascii_lowercase();
        HashAlgo::ALL
            .into_iter()
            .find(|a| a.id() == wanted)
            .ok_or_else(|| format!("unknown hash algorithm: {s}"))
    }
}

/// Streaming hasher for any supported algorithm.
pub struct Hasher(Inner);

enum Inner {
    Xxh64(Xxh64),
    // Xxh3 carries a large internal buffer; keep the enum small.
    Xxh3(Box<Xxh3>),
    Xxh128(Box<Xxh3>),
    Md5(Md5),
    Sha1(Sha1),
    Sha256(Sha256),
    C4(Sha512),
}

impl Hasher {
    pub fn new(algo: HashAlgo) -> Self {
        Hasher(match algo {
            HashAlgo::Xxh64 => Inner::Xxh64(Xxh64::new(0)),
            HashAlgo::Xxh3 => Inner::Xxh3(Box::new(Xxh3::new())),
            HashAlgo::Xxh128 => Inner::Xxh128(Box::new(Xxh3::new())),
            HashAlgo::Md5 => Inner::Md5(Md5::new()),
            HashAlgo::Sha1 => Inner::Sha1(Sha1::new()),
            HashAlgo::Sha256 => Inner::Sha256(Sha256::new()),
            HashAlgo::C4 => Inner::C4(Sha512::new()),
        })
    }

    pub fn update(&mut self, data: &[u8]) {
        match &mut self.0 {
            Inner::Xxh64(h) => h.update(data),
            Inner::Xxh3(h) | Inner::Xxh128(h) => h.update(data),
            Inner::Md5(h) => h.update(data),
            Inner::Sha1(h) => h.update(data),
            Inner::Sha256(h) => h.update(data),
            Inner::C4(h) => h.update(data),
        }
    }

    /// Digest in the textual form MHL and other DIT tools use: lowercase
    /// big-endian hex, or the 90-character C4 ID.
    pub fn finalize(self) -> String {
        match self.0 {
            Inner::Xxh64(h) => format!("{:016x}", h.digest()),
            Inner::Xxh3(h) => format!("{:016x}", h.digest()),
            Inner::Xxh128(h) => format!("{:032x}", h.digest128()),
            Inner::Md5(h) => to_hex(&h.finalize()),
            Inner::Sha1(h) => to_hex(&h.finalize()),
            Inner::Sha256(h) => to_hex(&h.finalize()),
            Inner::C4(h) => c4_id(&h.finalize()),
        }
    }
}

fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0x0f) as usize] as char);
    }
    out
}

/// C4 ID (SMPTE ST 2114): the SHA-512 digest as a base-58 number, left-padded
/// with '1' (the zero digit) to 88 characters and prefixed with "c4".
fn c4_id(digest: &[u8]) -> String {
    const ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let mut number = digest.to_vec();
    let mut out = [b'1'; 90];
    out[0] = b'c';
    out[1] = b'4';
    let mut pos = out.len();
    while number.iter().any(|&b| b != 0) {
        let mut remainder = 0u32;
        for byte in number.iter_mut() {
            let acc = (remainder << 8) | u32::from(*byte);
            *byte = (acc / 58) as u8;
            remainder = acc % 58;
        }
        pos -= 1;
        out[pos] = ALPHABET[remainder as usize];
    }
    String::from_utf8(out.to_vec()).expect("base58 alphabet is ASCII")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(algo: HashAlgo, data: &[u8]) -> String {
        let mut h = Hasher::new(algo);
        h.update(data);
        h.finalize()
    }

    #[test]
    fn empty_input_matches_published_vectors() {
        assert_eq!(digest(HashAlgo::Xxh64, b""), "ef46db3751d8e999");
        assert_eq!(digest(HashAlgo::Xxh3, b""), "2d06800538d394c2");
        assert_eq!(digest(HashAlgo::Xxh128, b""), "99aa06d3014798d86001c324468d497f");
        assert_eq!(digest(HashAlgo::Md5, b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(digest(HashAlgo::Sha1, b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            digest(HashAlgo::Sha256, b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            digest(HashAlgo::C4, b""),
            "c459dsjfscH38cYeXXYogktxf4Cd9ibshE3BHUo6a58hBXmRQdZrAkZzsWcbWtDg5oQstpDuni4Hirj75GEmTc1sFT"
        );
    }

    #[test]
    fn known_input_matches_published_vectors() {
        assert_eq!(digest(HashAlgo::Md5, b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(digest(HashAlgo::Sha1, b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(digest(HashAlgo::Xxh64, b"abc"), "44bc2cf5ad770999");
    }

    #[test]
    fn chunking_does_not_change_the_digest() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i * 31 % 251) as u8).collect();
        for algo in HashAlgo::ALL {
            let mut split = Hasher::new(algo);
            for part in data.chunks(777) {
                split.update(part);
            }
            assert_eq!(split.finalize(), digest(algo, &data), "{algo}");
        }
    }

    #[test]
    fn algorithm_names_round_trip() {
        for algo in HashAlgo::ALL {
            assert_eq!(algo.id().parse::<HashAlgo>().unwrap(), algo);
        }
        assert!("crc32".parse::<HashAlgo>().is_err());
    }
}
