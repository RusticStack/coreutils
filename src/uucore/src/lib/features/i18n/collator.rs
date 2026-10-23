// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

use std::{cmp::Ordering, sync::OnceLock};

#[cfg(not(windows))]
use icu_collator::{self, CollatorBorrowed};

use crate::i18n::{DEFAULT_LOCALE, get_collating_locale};

#[cfg(windows)]
use windows_collator::Collator as CollatorBorrowed;

pub use icu_collator::options::{
    AlternateHandling, CaseLevel, CollatorOptions, MaxVariable, Strength,
};

static COLLATOR: OnceLock<CollatorBorrowed> = OnceLock::new();

/// Will initialize the collator if not already initialized.
/// returns `true` if initialization happened
#[cfg(not(windows))]
pub fn try_init_collator(opts: CollatorOptions) -> bool {
    COLLATOR
        .set(CollatorBorrowed::try_new(get_collating_locale().0.clone().into(), opts).unwrap())
        .is_ok()
}

/// Skua: on Windows the collator is Windows' own (see `windows_collator`); `opts` are ignored.
/// Returns `false` (byte order stays) when Windows does not know the locale.
#[cfg(windows)]
pub fn try_init_collator(_opts: CollatorOptions) -> bool {
    windows_collator::Collator::new().is_some_and(|c| COLLATOR.set(c).is_ok())
}

/// Will initialize the collator and panic if already initialized.
#[cfg(not(windows))]
pub fn init_collator(opts: CollatorOptions) {
    COLLATOR
        .set(CollatorBorrowed::try_new(get_collating_locale().0.clone().into(), opts).unwrap())
        .expect("Collator already initialized");
}

/// Will initialize the collator and panic if already initialized.
#[cfg(windows)]
pub fn init_collator(opts: CollatorOptions) {
    assert!(try_init_collator(opts), "Collator already initialized");
}

/// Check if locale collation should be used.
pub fn should_use_locale_collation() -> bool {
    // Skua: a locale Windows does not know collates as C, as in Git Bash.
    #[cfg(windows)]
    if !windows_collator::locale_is_known() {
        return false;
    }
    get_collating_locale().0 != DEFAULT_LOCALE
}

/// Skua: whether [`locale_cmp`] compares by a locale (a collator is initialized), not by bytes.
pub fn collator_is_active() -> bool {
    get_collating_locale().0 != DEFAULT_LOCALE && COLLATOR.get().is_some()
}

/// Initialize the collator for locale-aware string comparison if needed.
///
/// This function checks if the current locale requires locale-aware collation
/// (UTF-8 encoding) and initializes the ICU collator with appropriate settings
/// if necessary. For C/POSIX locales, no initialization is needed as byte
/// comparison is sufficient.
///
/// # Returns
///
/// `true` if the collator was initialized for a UTF-8 locale, `false` if
/// using C/POSIX locale (no initialization needed).
///
/// # Example
///
/// ```
/// use uucore::i18n::collator::init_locale_collation;
///
/// if init_locale_collation() {
///     // Using locale-aware collation
/// } else {
///     // Using byte comparison (C/POSIX locale)
/// }
/// ```
pub fn init_locale_collation() -> bool {
    use crate::i18n::{UEncoding, get_locale_encoding};

    // Use ICU collation only for UTF-8 locales that are NOT C/POSIX
    // (e.g. en_US.UTF-8, but not C.UTF-8 — C still uses byte comparison)
    if get_locale_encoding() != UEncoding::Utf8 || !should_use_locale_collation() {
        return false;
    }

    // UTF-8 locale - initialize collator with Shifted mode to match GNU behavior
    let mut opts = CollatorOptions::default();
    opts.alternate_handling = Some(AlternateHandling::Shifted);
    opts.strength = Some(Strength::Quaternary);

    try_init_collator(opts)
}

/// Compute the ICU collation sort key for the given input bytes and append it to `buf`.
/// This allows pre-computing sort keys once per line, then comparing them with simple
/// byte comparison during sorting (much faster than calling `compare_utf8` per comparison).
pub fn compute_sort_key_utf8(input: &[u8], buf: &mut Vec<u8>) {
    let c = COLLATOR
        .get()
        .expect("compute_sort_key_utf8 called before collator initialization");
    c.write_sort_key_utf8_to(input, buf)
        .expect("ICU write_sort_key_utf8_to failed");
}

/// Compare both strings with regard to the current locale.
pub fn locale_cmp(left: &[u8], right: &[u8]) -> Ordering {
    // If the detected locale is 'C', just do byte-wise comparison
    if get_collating_locale().0 == DEFAULT_LOCALE {
        left.cmp(right)
    } else {
        // Fall back to byte comparison if collator is not available
        COLLATOR
            .get()
            .map_or_else(|| left.cmp(right), |c| c.compare_utf8(left, right))
    }
}

/// Skua: collation on Windows as Git Bash (Cygwin) does it. Cygwin's `strcoll`/`wcscoll` call
/// `CompareStringW` with the locale's LCID and no flags, and `strxfrm` calls `LCMapStringW` with
/// `LCMAP_SORTKEY`; Git Bash's GNU `sort` and `ls` compare with them. So in a UTF-8 locale
/// (`LANG=en_US.UTF-8`) the order is Windows' "word sort": lower case before upper case of the
/// same letter, accents after the base letter, hyphen ignored at the first level
/// (`ab` < `a-b` < `aB`, `x-` < `-x`), other punctuation before digits before letters, and `ß`
/// equal to `ss`. ICU (uutils' default) orders punctuation and case differently.
#[cfg(windows)]
mod windows_collator {
    use std::cmp::Ordering;
    use std::sync::OnceLock;

    use windows_sys::Win32::Globalization::{
        COMPARESTRING_RESULT, CSTR_EQUAL, CSTR_LESS_THAN, CompareStringEx, IsValidLocaleName,
        LCMAP_SORTKEY, LCMapStringEx,
    };

    use crate::i18n::get_collating_locale;

    /// The collating locale as a Windows locale name (`en-US`), NUL-terminated; `None` when
    /// Windows does not know it.
    fn locale_name() -> Option<&'static [u16]> {
        static NAME: OnceLock<Option<Vec<u16>>> = OnceLock::new();
        NAME.get_or_init(|| {
            let name: Vec<u16> = get_collating_locale()
                .0
                .to_string()
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            // SAFETY: `name` is NUL-terminated.
            (unsafe { IsValidLocaleName(name.as_ptr()) } != 0).then_some(name)
        })
        .as_deref()
    }

    pub fn locale_is_known() -> bool {
        locale_name().is_some()
    }

    fn wide(input: &[u8]) -> Vec<u16> {
        String::from_utf8_lossy(input).encode_utf16().collect()
    }

    /// A length as the `i32` the Windows APIs take.
    fn len_i32(len: usize) -> i32 {
        i32::try_from(len).unwrap_or(i32::MAX)
    }

    #[derive(Debug)]
    pub struct Collator {
        name: &'static [u16],
    }

    impl Collator {
        pub fn new() -> Option<Self> {
            locale_name().map(|name| Self { name })
        }

        /// `CompareStringEx(locale, 0, ..)`, as Cygwin's `wcscoll`.
        pub fn compare_utf8(&self, left: &[u8], right: &[u8]) -> Ordering {
            let (l, r) = (wide(left), wide(right));
            // SAFETY: both buffers are valid for the lengths passed; `name` is NUL-terminated.
            let res: COMPARESTRING_RESULT = unsafe {
                CompareStringEx(
                    self.name.as_ptr(),
                    0,
                    l.as_ptr(),
                    len_i32(l.len()),
                    r.as_ptr(),
                    len_i32(r.len()),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                )
            };
            match res {
                CSTR_LESS_THAN => Ordering::Less,
                CSTR_EQUAL => Ordering::Equal,
                0 => left.cmp(right),
                _ => Ordering::Greater,
            }
        }

        /// `LCMapStringEx(locale, LCMAP_SORTKEY, ..)`: byte keys that compare as
        /// [`Self::compare_utf8`] does.
        pub fn write_sort_key_utf8_to(
            &self,
            input: &[u8],
            buf: &mut Vec<u8>,
        ) -> Result<(), std::convert::Infallible> {
            let src = wide(input);
            if src.is_empty() {
                return Ok(());
            }
            let map = |dest: *mut u16, dest_len: i32| {
                // SAFETY: `src` is valid for its length; `dest` is null with length 0 (a size
                // query) or has `dest_len` bytes (LCMAP_SORTKEY counts bytes).
                unsafe {
                    LCMapStringEx(
                        self.name.as_ptr(),
                        LCMAP_SORTKEY,
                        src.as_ptr(),
                        len_i32(src.len()),
                        dest,
                        dest_len,
                        std::ptr::null(),
                        std::ptr::null(),
                        0,
                    )
                }
            };
            let size = usize::try_from(map(std::ptr::null_mut(), 0)).unwrap_or(0);
            if size == 0 {
                // Not mappable: the bytes, after a marker that sorts them last.
                buf.push(0xff);
                buf.extend_from_slice(input);
                return Ok(());
            }
            let start = buf.len();
            buf.resize(start + size, 0);
            let written = map(buf[start..].as_mut_ptr().cast(), len_i32(size));
            buf.truncate(start + usize::try_from(written).unwrap_or(0));
            // The key ends with a NUL byte; without it a prefix still sorts first.
            if buf.len() > start && buf.last() == Some(&0) {
                buf.pop();
            }
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::Collator;
        use std::cmp::Ordering;

        #[test]
        fn word_sort_order_as_git_bash() {
            let name: Vec<u16> = "en-US\0".encode_utf16().collect();
            let c = Collator {
                name: Box::leak(name.into_boxed_slice()),
            };
            let key = |s: &str| {
                let mut k = Vec::new();
                c.write_sort_key_utf8_to(s.as_bytes(), &mut k).unwrap();
                k
            };
            // Pairs measured with Git Bash `LANG=en_US.UTF-8 sort`.
            for (a, b) in [
                ("a", "A"),
                ("ab", "a-b"),
                ("a-b", "aB"),
                ("x-", "-x"),
                ("_c", "~y"),
                ("~y", "01"),
                ("9", "a"),
                ("A", "ä"),
                ("ae", "Æ"),
                ("Æ", "b"),
                ("e", "é"),
                ("é", "É"),
                ("coop", "co-op"),
                ("z", "Z"),
                ("Z", "ω"),
            ] {
                assert_eq!(
                    c.compare_utf8(a.as_bytes(), b.as_bytes()),
                    Ordering::Less,
                    "{a} < {b}"
                );
                assert!(key(a) < key(b), "key({a}) < key({b})");
            }
            assert_eq!(c.compare_utf8("ß".as_bytes(), b"ss"), Ordering::Equal);
            assert_eq!(key("ß"), key("ss"));
        }
    }
}
