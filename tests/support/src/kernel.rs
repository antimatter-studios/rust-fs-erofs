//! THE KERNEL ORACLE: the real in-kernel EROFS driver, reading back what
//! this crate wrote.
//!
//! `fsck.erofs` is a second opinion on the same file format, written by
//! the same project, from the same specification. Linux is the thing the
//! images are actually for — EROFS exists to be mounted from a source
//! you did not produce, so the mount path is the one that matters. An
//! image `fsck.erofs` calls clean can still be one the kernel mounts
//! differently: a directory entry it will not find, a compressed extent
//! it decodes shorter than we wrote it, an xattr it puts in another
//! place. Nothing on the host can catch that — a loop mount is the
//! kernel's job, it needs root, and macOS has no EROFS at all.
//!
//! So a kernel oracle is a script run INSIDE the harness VM against a
//! loop mount of one of our images. It is the only place in this suite
//! where a filesystem is mounted, and it never happens on the host.
//!
//! THIS REPLACED A `sudo -n mount` FROM INSIDE A TEST. That version
//! could only run on a Linux runner with passwordless sudo, so a
//! developer's machine never ran it at all, and for most of its life it
//! matched the refusal against a list of permission-denied wordings,
//! printed "skipping" and returned ok (#117). In the guest there is
//! nothing to be refused and nothing to skip: a mount that does not
//! happen fails the test.
//!
//! ONE GUEST CALL PER CHECK. The script does the whole comparison —
//! mount, walk, hash, read xattrs, unmount — and prints what it found,
//! rather than paying a round trip per question.
//!
//! THE IMAGE IS COPIED INTO THE GUEST'S OWN DISK for the mount. A loop
//! mount reads through the page cache; pointing that at a file on the 9p
//! share mixes two caches over one file. The copy costs a fraction of a
//! second for these images and removes the question. EROFS is read-only,
//! so there is no copy back: the kernel cannot have changed it.

use std::collections::BTreeMap;
use std::process::Output;

use crate::oracle::{guest_quote, guest_shell, repo, session, Run};

/// Mount `image` read-only in the guest and run `script` against it.
///
/// `$MNT` is the mount point, and `$IMG` the image, inside the guest.
/// The script runs under `bash -euo pipefail`, so an unchecked failure
/// inside it fails the call. Its stdout, its stderr and its exit status
/// come back exactly as they were.
///
/// The mount is `ro` AND the loop device is read-only, which is what
/// EROFS is anyway — the belt is free and it makes the intent explicit.
#[track_caller]
fn guest_kernel_read(image: &str, script: &str) -> Output {
    run(image, script, MountExpectation::Succeeds)
}

/// Whether the mount itself is the thing under test.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MountExpectation {
    /// The mount must succeed; a failure to mount fails the call.
    Succeeds,
    /// The mount must FAIL. The script is not run, and the call reports
    /// what `mount` said.
    Refused,
}

#[track_caller]
fn run(image: &str, script: &str, expectation: MountExpectation) -> Output {
    session();
    assert!(
        std::path::Path::new(image).starts_with(repo()),
        "the kernel oracle was given {image}, which is outside {}. The guest sees this \
         repository and nothing else of the host.",
        repo().display()
    );

    let run = Run::new();
    let guest = guest_script(image, script, expectation, &run);
    let out = guest_shell(&guest)
        .unwrap_or_else(|error| panic!("cannot run the kernel oracle in the guest: {error}"));
    let Some(code) = run.code() else {
        panic!(
            "the kernel oracle could not run in the fs-linux-test-harness VM \
             (the harness exited {:?}). `chore vm:status` shows the VM; `chore vm:up` \
             boots it. The EROFS module and the loop driver are loaded by \
             scripts/vm-setup.sh.\n{}{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    };
    let (stdout, stderr) = run.streams();
    // THE NUMBER IS THE GUEST SCRIPT'S VERDICT, NOT `mount`'s STATUS, and
    // saying so matters most in the `Refused` shape: there, the script
    // exits 0 when the kernel REFUSED the image, so a line reading
    // `mount ... -> 0` next to a test named `..._is_refused_...` reads
    // like a mount that succeeded and a test that passed anyway. Spell
    // the verdict out instead of printing a bare number against a verb.
    let verdict = match (expectation, code) {
        (MountExpectation::Succeeds, 0) => "mounted".to_string(),
        (MountExpectation::Succeeds, n) => format!("FAILED, guest script exit {n}"),
        (MountExpectation::Refused, 0) => "refused by the kernel, as expected".to_string(),
        (MountExpectation::Refused, n) => {
            format!("NOT refused — the kernel accepted it (guest script exit {n})")
        }
    };
    println!("[kernel vm] mount -t erofs -o ro {image} -> {verdict}");
    Output {
        status: std::os::unix::process::ExitStatusExt::from_raw(code << 8),
        stdout,
        stderr,
    }
}

/// The guest side: copy in, mount, run, unmount.
///
/// Every step is checked, and the unmount happens on every path — a loop
/// device left attached to an image the next test rewrites is a failure
/// that lands somewhere else entirely.
///
/// In the `Refused` shape the mount is expected to fail, and the script
/// reports ITS OWN verdict rather than `mount`'s status: it exits 0 when
/// the mount was refused, and 1 — having unmounted first, so a wrongly
/// accepted image does not leave a mount behind — when the kernel took
/// an image it should have rejected. `mount`'s own message is what the
/// caller reads out of stderr.
fn guest_script(image: &str, script: &str, expectation: MountExpectation, run: &Run) -> String {
    let body = match expectation {
        MountExpectation::Succeeds => format!(
            r#"mount -t erofs -o ro "$loop" "$mnt"
status=0
MNT="$mnt" IMG="$IMG" bash -euo pipefail -c {script} > {stdout} 2> {stderr} || status=$?
umount "$mnt""#,
            script = guest_quote(script),
            stdout = guest_quote(&run.stdout.to_string_lossy()),
            stderr = guest_quote(&run.stderr.to_string_lossy()),
        ),
        MountExpectation::Refused => format!(
            r#"status=0
if mount -t erofs -o ro "$loop" "$mnt" > {stdout} 2> {stderr}; then
    umount "$mnt"
    echo "the kernel MOUNTED this image; it was supposed to refuse it" >> {stderr}
    status=1
fi"#,
            stdout = guest_quote(&run.stdout.to_string_lossy()),
            stderr = guest_quote(&run.stderr.to_string_lossy()),
        ),
    };
    format!(
        r#"set -euo pipefail
mkdir -p {dir}
IMG={image}
work="$(mktemp -d /var/tmp/fs-erofs-kernel.XXXXXX)"
mnt="$work/mnt"
mkdir -p "$mnt"
cp --sparse=always "$IMG" "$work/image"
loop="$(losetup --find --show --read-only "$work/image")"
cleanup() {{
    mountpoint -q "$mnt" && umount "$mnt" || true
    losetup -d "$loop" 2>/dev/null || true
}}
trap cleanup EXIT
{body}
losetup -d "$loop"
trap - EXIT
printf %s "$status" > {status}
rm -rf "$work""#,
        dir = guest_quote(&run.dir.to_string_lossy()),
        image = guest_quote(image),
        body = body,
        status = guest_quote(&run.status.to_string_lossy()),
    )
}

/// `guest_kernel_read`, with the script's output as text and a failure
/// that prints everything the guest said.
#[track_caller]
fn guest_kernel_read_ok(image: &str, what: &str, script: &str) -> String {
    let out = guest_kernel_read(image, script);
    assert!(
        out.status.success(),
        "[{what}] the kernel could not read back {image} ({:?}):\n{}{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The kernel must REFUSE to mount `image`, and this returns what it
/// said when it did.
///
/// The other half of a mount oracle: an image the driver knows is
/// damaged has to be one Linux also rejects, or "the kernel mounted it"
/// stops being evidence of anything. A mount that succeeds fails the
/// test.
#[track_caller]
pub fn guest_kernel_refusal(image: &str, what: &str) -> String {
    let out = run(image, "", MountExpectation::Refused);
    assert!(
        out.status.success(),
        "[{what}] the kernel accepted {image}, which this test corrupted on purpose:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// SHA-256 of some bytes, as the hex `sha256sum` prints in the guest.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.hex()
}

/// SHA-256, spelled out so the test support crate needs no dependency
/// for it (FIPS 180-4).
struct Sha256 {
    state: [u32; 8],
    buffer: Vec<u8>,
    length: u64,
}

impl Sha256 {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buffer: Vec::new(),
            length: 0,
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        self.length += bytes.len() as u64;
        self.buffer.extend_from_slice(bytes);
        let mut at = 0;
        while self.buffer.len() - at >= 64 {
            let block: [u8; 64] = self.buffer[at..at + 64].try_into().unwrap();
            self.compress(&block);
            at += 64;
        }
        self.buffer.drain(..at);
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (i, chunk) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes(chunk.try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut v = self.state;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(Self::K[i])
                .wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [
                t1.wrapping_add(t2),
                v[0],
                v[1],
                v[2],
                v[3].wrapping_add(t1),
                v[4],
                v[5],
                v[6],
            ];
        }
        for (i, word) in v.iter().enumerate() {
            self.state[i] = self.state[i].wrapping_add(*word);
        }
    }

    fn hex(mut self) -> String {
        let bits = self.length * 8;
        self.buffer.push(0x80);
        while self.buffer.len() % 64 != 56 {
            self.buffer.push(0);
        }
        let tail = bits.to_be_bytes();
        self.buffer.extend_from_slice(&tail);
        let blocks: Vec<[u8; 64]> = self
            .buffer
            .chunks_exact(64)
            .map(|c| c.try_into().unwrap())
            .collect();
        for block in blocks {
            self.compress(&block);
        }
        self.state.iter().map(|w| format!("{w:08x}")).collect()
    }
}

/// What the kernel reported, as `(kind, path) -> value`.
fn parse(report: &str) -> BTreeMap<(String, String), String> {
    report
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut fields = line.splitn(3, '\t');
            let kind = fields.next().unwrap_or_default().to_string();
            let path = fields.next().unwrap_or_default().to_string();
            let value = fields.next().unwrap_or_default().to_string();
            ((kind, path), value)
        })
        .collect()
}

/// The guest script: walk the mounted tree, hash every file, read the
/// metadata this test cares about, and print `kind<TAB>path<TAB>value`.
///
/// One invocation does all of it. The alternative — a guest call per
/// question — is a hundred round trips for one image.
const REPORT: &str = r#"
cd "$MNT"
find . -mindepth 1 -printf '%P\n' | sort | while read -r path; do
    kind="$(stat -c '%F' "$path" | tr ' ' '-')"
    printf 'type\t%s\t%s\n' "$path" "$kind"
    printf 'mode\t%s\t%s\n' "$path" "$(stat -c '%a' "$path")"
    printf 'uidgid\t%s\t%s\n' "$path" "$(stat -c '%u:%g' "$path")"
    case "$kind" in
        regular-file | regular-empty-file)
            printf 'size\t%s\t%s\n' "$path" "$(stat -c '%s' "$path")"
            printf 'sha256\t%s\t%s\n' "$path" "$(sha256sum "$path" | cut -d' ' -f1)"
            ;;
        symbolic-link)
            printf 'target\t%s\t%s\n' "$path" "$(readlink "$path")"
            ;;
        character-special-file | block-special-file)
            printf 'rdev\t%s\t%s\n' "$path" "$(stat -c '%t:%T' "$path")"
            ;;
    esac
    names="$(getfattr -h -m '.' --absolute-names "$path" 2>/dev/null | sed -n '2,$p' || true)"
    attrs=""
    for name in $names; do
        value="$(getfattr --only-values -n "$name" "$path" 2>/dev/null || true)"
        attrs="$attrs${attrs:+,}$name=$value"
    done
    if [ -n "$attrs" ]; then
        printf 'xattrs\t%s\t%s\n' "$path" "$attrs"
    fi
done
printf 'kernel\t\t%s\n' "$(uname -r)"
"#;

/// Mount `image` read-only in the guest and report everything in it:
/// for every path, its type, mode, owner, size, SHA-256, symlink target,
/// device number and extended attributes, keyed by `(kind, path)`.
///
/// ONE GUEST CALL. A test compares this against what it wrote.
#[track_caller]
pub fn guest_kernel_report(image: &str, what: &str) -> BTreeMap<(String, String), String> {
    parse(&guest_kernel_read_ok(image, what, REPORT))
}
