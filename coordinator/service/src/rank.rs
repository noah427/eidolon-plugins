//! Rank blocks: the per-rank text a session is told beside its persona, and the
//! content binding that keeps a retry from injecting different text.
//!
//! A **rank block** is one file per rank under the plugin's `ranks/` directory —
//! `imperator.md`, `legatus.md`, `tribunus.md`, `miles.md`, and optionally
//! `centurio.md`. Its bytes are appended to the child's resolved system prompt at
//! spawn (`--system-append <file>`), before the session is created, so the
//! harness's `SessionStart` record captures the prompt the child actually had.
//!
//! Three rules, and nothing else — the fourth bullet is how the third is kept:
//!
//! - **The rank names are the operator's, not this plugin's.** [`RANKS`] is the
//!   whole vocabulary. An unknown name is refused by name; no rank is added,
//!   renamed or inferred, and this module writes no doctrine of its own into a
//!   block. The shipped blocks say only that they are unwritten.
//! - **A rank with no block means no addition, not a failure.** No file, or a file
//!   holding nothing but the placeholder comment, is `None`: the launch carries no
//!   `--system-append` at all. A block that exists but cannot be read is a
//!   refusal — an unreadable block is not an absent one, and guessing which would
//!   silently change what a child is told.
//! - **A block's bytes are bound into the launch.** [`Blocks::resolve`] reads the
//!   bytes and hashes them; the digest goes into the launch's immutable request.
//!   [`Blocks::verify`] reads them again immediately before the spawn and refuses
//!   if they no longer hash to what the operation was recorded with. A retry after
//!   somebody edited the file is refused, never quietly re-told.
//! - **The digest travels with the text.** The bytes that were hashed are the bytes
//!   the host writes to a launch-private file, flushes, makes read-only, re-reads and
//!   re-hashes, and hands the child, so the CLI's own read composes what the record
//!   names. The block's path is never handed onward: a path is a promise that the
//!   file will still say the same thing when somebody else reads it, and that promise
//!   is exactly what the digest replaces.
//!
//! The digest is SHA-256 of the exact bytes. It is implemented here, in this file,
//! so that binding a launch needs no crate the plugin did not already have. It is
//! not a security primitive and is not claimed as one: it binds a launch to a text,
//! and the text it binds is the text that is handed over.

use std::path::{Path, PathBuf};

/// The rank vocabulary, exactly as the operator's convention spells it. `centurio`
/// is carried because it was named; whether any launch uses it is not this
/// module's decision.
pub const RANKS: [&str; 5] = ["imperator", "legatus", "tribunus", "miles", "centurio"];

/// One resolved, written rank block: which rank, where it was read from, the digest
/// of those bytes, and the bytes themselves.
///
/// The bytes are carried, not just the path, because the digest has to name what
/// the child is actually told. `--system-append` takes a *path*, so handing it the
/// block would let a third read (the CLI's own) compose text this module never
/// hashed. [`Blocks::verify`] therefore returns the bytes it hashed, and the host
/// writes exactly those bytes to a launch-private file and passes that.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Addition {
    pub rank: String,
    /// The block the bytes were read from, for messages that name the operator's
    /// file. Not what the child is handed.
    pub path: PathBuf,
    pub digest: String,
    /// The exact bytes hashed above.
    pub bytes: Vec<u8>,
}

/// The directory the blocks live in. `dir` is the plugin's own `ranks/`, resolved
/// by the monitor (`EIDOLON_COORDINATOR_RANKS`, else the installed plugin folder).
#[derive(Clone, Debug)]
pub struct Blocks {
    dir: PathBuf,
}

impl Blocks {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Read the named rank's block and hash it. `Ok(None)` is "this rank has no
    /// addition" — no file, or a file that holds only the placeholder comment.
    pub fn resolve(&self, rank: Option<&str>) -> Result<Option<Addition>, String> {
        let Some(rank) = rank else { return Ok(None) };
        if !RANKS.contains(&rank) {
            return Err(unknown_rank(rank));
        }
        if self.dir.as_os_str().is_empty() {
            return Err(format!(
                "the monitor has no rank-block directory configured, so the `{rank}` block cannot be read. \
                 A rank is text an operator writes and the monitor appends; with nowhere to read it from, \
                 refusing is the only honest answer"
            ));
        }
        let path = self.dir.join(format!("{rank}.md"));
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(format!(
                    "the `{rank}` rank block at {} exists but cannot be read: {e}. An unreadable block is \
                     not an absent one — launching without it would tell the child something nobody wrote",
                    path.display()
                ))
            }
        };
        if unwritten(&bytes) {
            return Ok(None);
        }
        // The digest and the path handed to `--system-append` must name the same
        // bytes. Canonicalising once, here, is what makes that true even if the
        // plugin directory is reached through a symlink.
        let path = path.canonicalize().map_err(|e| {
            format!("the `{rank}` rank block at {} could not be resolved: {e}", path.display())
        })?;
        let digest = digest_hex(&bytes);
        Ok(Some(Addition { rank: rank.to_string(), path, digest, bytes }))
    }

    /// Read the block again, immediately before a spawn, and compare it with what
    /// the operation was recorded with. `request.rank_digest` is the binding; this
    /// is the check that a change between the request and the spawn refuses.
    pub fn verify(&self, rank: Option<&str>, digest: Option<&str>) -> Result<Option<Addition>, String> {
        let Some(rank) = rank else {
            return match digest {
                Some(_) => Err(
                    "the launch request binds a rank digest but names no rank; refusing to append text \
                     nobody can name"
                        .to_string(),
                ),
                None => Ok(None),
            };
        };
        let now = self.resolve(Some(rank))?;
        let path = self.dir.join(format!("{rank}.md"));
        match (now, digest) {
            (Some(addition), Some(bound)) if addition.digest == bound => Ok(Some(addition)),
            (Some(addition), Some(bound)) => Err(format!(
                "the `{rank}` rank block at {} changed after this launch was prepared: its bytes now hash to \
                 {}, and the operation is bound to {bound}. Refusing to spawn — the child would be told text \
                 nobody reviewed for this operation",
                addition.path.display(),
                addition.digest
            )),
            (Some(addition), None) => Err(format!(
                "the `{rank}` rank block at {} changed after this launch was prepared: the operation is bound \
                 to no block, and the file now holds text hashing to {}. Refusing to spawn — the child would \
                 be told text nobody reviewed for this operation",
                addition.path.display(),
                addition.digest
            )),
            (None, Some(bound)) => Err(format!(
                "the `{rank}` rank block at {} changed after this launch was prepared: it is gone (or holds \
                 only the unwritten placeholder), and the operation is bound to {bound}. Refusing to spawn — \
                 the child would be told something other than what the operation recorded",
                path.display()
            )),
            (None, None) => Ok(None),
        }
    }
}

/// The refusal an unknown rank gets, naming the whole vocabulary so a caller does
/// not have to guess which spelling this plugin accepts.
pub fn unknown_rank(rank: &str) -> String {
    format!(
        "`{rank}` is not a rank this plugin knows: the ranks are {}. They are the operator's convention; \
         this plugin does not add, rename or infer one, and it will not treat an unknown word as a rank",
        RANKS.join(", ")
    )
}

/// A block is *unwritten* when it holds nothing but whitespace and HTML comments.
/// The shipped blocks are exactly that: a comment saying the rank's text is the
/// operator's and has not been written yet. Nothing is appended for such a block.
///
/// The rule is deliberately this small. It is not markup support: an unterminated
/// `<!--` is text, not a placeholder, and the operator's prose is appended
/// verbatim — comments included — because the digest in the launch record is the
/// digest of the file's exact bytes, and stripping would break that bond.
fn unwritten(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    let mut rest = text.as_ref();
    let mut written = String::new();
    while let Some(start) = rest.find("<!--") {
        written.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => return false,
        }
    }
    written.push_str(rest);
    written.trim().is_empty()
}

/// SHA-256, lowercase hex. FIPS 180-4; the test vectors below are the check.
pub fn digest_hex(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let mut message = bytes.to_vec();
    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for base in (0..message.len()).step_by(64) {
        let chunk = &message[base..base + 64];
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes([chunk[4 * i], chunk[4 * i + 1], chunk[4 * i + 2], chunk[4 * i + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(value);
        }
    }
    h.iter().map(|word| format!("{word:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("coord-rank-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sha256_matches_the_published_vectors() {
        // FIPS 180-4 / the usual NIST examples, including one that spans a block.
        assert_eq!(digest_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(digest_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(
            digest_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            digest_hex(&vec![b'a'; 1_000_000]),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn no_rank_and_no_block_are_both_no_addition() {
        let dir = tmpdir("absent");
        let blocks = Blocks::new(dir.clone());
        assert_eq!(blocks.resolve(None).unwrap(), None);
        assert_eq!(blocks.resolve(Some("miles")).unwrap(), None, "a missing file is no addition, not an error");
        assert_eq!(blocks.verify(None, None).unwrap(), None);
        assert_eq!(blocks.verify(Some("miles"), None).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_shipped_placeholder_shape_is_unwritten_but_prose_is_written() {
        let dir = tmpdir("placeholder");
        let blocks = Blocks::new(dir.clone());
        std::fs::write(dir.join("miles.md"), "<!--\nmiles — unwritten. The operator's words go here.\n-->\n").unwrap();
        assert_eq!(blocks.resolve(Some("miles")).unwrap(), None, "a comment-only block appends nothing");

        std::fs::write(dir.join("miles.md"), "<!-- unwritten -->\nHold the flank; report the count.\n").unwrap();
        let addition = blocks.resolve(Some("miles")).unwrap().expect("prose is a written block");
        assert_eq!(addition.rank, "miles");
        assert_eq!(addition.path, dir.join("miles.md").canonicalize().unwrap());
        assert_eq!(addition.digest, digest_hex(std::fs::read(dir.join("miles.md")).unwrap().as_slice()));

        // An unterminated comment is text, not a placeholder.
        std::fs::write(dir.join("miles.md"), "<!-- unwritten\n").unwrap();
        assert!(blocks.resolve(Some("miles")).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_blocks_that_ship_with_the_plugin_append_nothing() {
        // The shipped files are placeholders. If one of them were text, every
        // launch of that rank would carry a paragraph about being unwritten.
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ranks");
        let blocks = Blocks::new(dir.clone());
        for rank in RANKS {
            assert_eq!(
                blocks.resolve(Some(rank)).unwrap(),
                None,
                "{rank}.md ships as a placeholder and must resolve to no addition"
            );
        }
    }

    #[test]
    fn a_rank_outside_the_vocabulary_is_refused_by_name() {
        let dir = tmpdir("unknown");
        let blocks = Blocks::new(dir.clone());
        let e = blocks.resolve(Some("duke")).unwrap_err();
        assert!(e.contains("`duke` is not a rank"), "{e}");
        for rank in RANKS {
            assert!(e.contains(rank), "the refusal names every rank, missing {rank}: {e}");
        }
        std::fs::write(dir.join("duke.md"), "text").unwrap();
        assert!(blocks.resolve(Some("duke")).is_err(), "a file does not create a rank");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_block_is_a_refusal_not_an_absence() {
        let dir = tmpdir("unreadable");
        let blocks = Blocks::new(dir.clone());
        std::fs::create_dir_all(dir.join("miles.md")).unwrap();
        let e = blocks.resolve(Some("miles")).unwrap_err();
        assert!(e.contains("exists but cannot be read"), "{e}");
        assert!(e.contains("not an absent one"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_refuses_bytes_that_changed_after_the_request_was_bound() {
        let dir = tmpdir("changed");
        let blocks = Blocks::new(dir.clone());
        std::fs::write(dir.join("tribunus.md"), "the first text\n").unwrap();
        let bound = blocks.resolve(Some("tribunus")).unwrap().unwrap();

        assert_eq!(
            blocks.verify(Some("tribunus"), Some(&bound.digest)).unwrap(),
            Some(bound.clone()),
            "unchanged bytes are the same addition"
        );

        std::fs::write(dir.join("tribunus.md"), "different text\n").unwrap();
        let e = blocks.verify(Some("tribunus"), Some(&bound.digest)).unwrap_err();
        assert!(e.contains("changed after this launch was prepared"), "{e}");
        assert!(e.contains("tribunus.md"), "and it names the file: {e}");
        assert!(e.contains(&bound.digest), "and the digest the operation is bound to: {e}");

        // Appearing, and disappearing, are the same kind of change.
        assert!(blocks.verify(Some("tribunus"), None).unwrap_err().contains("changed"));
        std::fs::remove_file(dir.join("tribunus.md")).unwrap();
        assert!(blocks.verify(Some("tribunus"), Some(&bound.digest)).unwrap_err().contains("changed"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
