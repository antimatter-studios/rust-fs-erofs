//! Shared helpers for the EROFS test suite: where scratch files live,
//! where fixtures come from, and the two outside opinions this suite
//! holds itself to — erofs-utils (see [`oracle`]) and the Linux kernel
//! (see [`kernel`]).

mod kernel;
mod oracle;

pub use kernel::{guest_kernel_refusal, guest_kernel_report, sha256_hex};
pub use oracle::{guest_base64, guest_quote, mkfs_from_guest_tree, oracle, Oracle};

use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

static TEST_TEMP_DIR: OnceLock<PathBuf> = OnceLock::new();

/// The scratch root: `<repo>/tmp`, or `FS_EROFS_TEST_TMPDIR` when a
/// caller supplies an exact directory of its own.
///
/// ONE RULE, AND IT IS THE ORACLE'S. Scratch files are what the oracle
/// tools read, and those tools run in the harness VM, which sees this
/// repository mounted at the path the host knows it by — and nothing
/// else of the host. A scratch directory under `/tmp` or `$RUNNER_TEMP`
/// would not exist there. So it lives in the repository (gitignored),
/// on every machine and on CI alike, and a caller-supplied directory
/// outside the repository is refused rather than quietly breaking every
/// oracle test.
#[track_caller]
pub fn select_temp_dir(explicit: Option<&OsStr>, worktree: &Path) -> PathBuf {
    let Some(path) = explicit.filter(|path| !path.is_empty()) else {
        return worktree.join("tmp");
    };
    let path = PathBuf::from(path);
    assert!(
        path.starts_with(worktree),
        "FS_EROFS_TEST_TMPDIR is {}, which is outside {}. The oracle tools run in the \
         harness VM, which sees this repository and nothing else of the host, so scratch \
         files have to live inside it.",
        path.display(),
        worktree.display()
    );
    path
}

/// Create a collision-resistant per-process scratch directory below `base`.
#[doc(hidden)]
pub fn create_unique_temp_dir(base: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(base)?;
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for attempt in 0..1_024_u16 {
        let candidate = base.join(format!(
            "fs-erofs-tests.{}.{}.{}",
            std::process::id(),
            started,
            attempt
        ));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "cannot allocate unique scratch directory below {}",
            base.display()
        ),
    ))
}

/// Preserve an explicit directory or isolate a process beneath a selected root.
#[doc(hidden)]
pub fn materialize_temp_dir(explicit: Option<&OsStr>, root: &Path) -> io::Result<PathBuf> {
    if explicit.filter(|path| !path.is_empty()).is_some() {
        fs::create_dir_all(root)?;
        Ok(root.to_path_buf())
    } else {
        create_unique_temp_dir(root)
    }
}

/// Return the shared scratch directory for this integration-test process.
pub fn temp_dir() -> &'static Path {
    TEST_TEMP_DIR
        .get_or_init(|| {
            let explicit = std::env::var_os("FS_EROFS_TEST_TMPDIR");
            let selected_root = select_temp_dir(explicit.as_deref(), oracle::repo());
            materialize_temp_dir(explicit.as_deref(), &selected_root).unwrap_or_else(|error| {
                panic!(
                    "cannot create EROFS test scratch directory below {}: {error}",
                    selected_root.display()
                )
            })
        })
        .as_path()
}

/// Format a test filename beneath the selected scratch directory.
#[doc(hidden)]
pub fn formatted_temp_path(arguments: fmt::Arguments<'_>) -> String {
    temp_dir()
        .join(arguments.to_string())
        .to_string_lossy()
        .into_owned()
}

#[macro_export]
macro_rules! temp_path {
    ($($argument:tt)*) => {
        $crate::formatted_temp_path(format_args!($($argument)*))
    };
}

/// A scratch directory that deletes itself, inside the repository.
///
/// [`tempfile::TempDir`] puts its directory under the system temporary
/// directory, which the guest does not have — so this is the shape the
/// oracle tests use instead. Same guarantee, one rule kept: everything a
/// tool touches is inside this repository.
pub struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    /// A fresh directory below [`temp_dir`], named after `tag`.
    #[track_caller]
    pub fn new(tag: &str) -> Self {
        let path = create_unique_temp_dir(&temp_dir().join(tag))
            .unwrap_or_else(|error| panic!("cannot create a scratch directory for {tag}: {error}"));
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A path inside it. The file need not exist yet.
    pub fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// The path of a generated fixture under `test-disks/`, or a panic that
/// says how to build it.
///
/// THE ONLY WAY A TEST REACHES A FIXTURE: a test that found its image
/// absent used to print "skip" and return, and a skipped test reads
/// exactly like a passing one, so a checkout without fixtures ran most
/// of the suite against nothing and reported green. `chore test:unit`
/// also relies on this: a test binary that never calls it needs no
/// fixture.
#[track_caller]
pub fn fixture(manifest_dir: &str, name: &str) -> String {
    let path = format!("{manifest_dir}/test-disks/{name}");
    assert!(
        Path::new(&path).is_file(),
        "test-disks/{name} is missing: build the generated fixtures with `chore fixtures` \
         (it boots the fs-linux-test-harness VM; `chore siblings` checks the harness out) \
         and run the tests again. Tests never skip on a missing fixture."
    );
    path
}

/// `fsck.erofs` on `image` must exit 0, or the test fails with its report.
///
/// The oracle suites checked their images with this crate's own reader,
/// which shares this crate's reading of the format and so cannot catch a
/// misreading. `fsck.erofs` walks every superblock, inode and layout
/// assertion upstream makes, CRC32C included. It runs in the harness VM,
/// like every oracle tool (see [`oracle`]).
#[track_caller]
pub fn assert_fsck_clean(image: &str, tag: &str) {
    let out = oracle("fsck.erofs").arg(image).output();
    assert_eq!(
        out.status.code(),
        Some(0),
        "[{tag}] fsck.erofs {image}:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
