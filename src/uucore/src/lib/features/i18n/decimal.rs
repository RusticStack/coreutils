// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

use std::sync::{Mutex, OnceLock, PoisonError};

use icu_decimal::provider::DecimalSymbolsV1;
use icu_locale::{Locale, locale};
use icu_provider::prelude::*;

use crate::i18n::get_numeric_locale;

/// Return the decimal separator for the given locale
fn get_decimal_separator(loc: Locale) -> String {
    let data_locale = DataLocale::from(loc);

    let request = DataRequest {
        id: DataIdentifierBorrowed::for_locale(&data_locale),
        metadata: DataRequestMetadata::default(),
    };

    let response: DataResponse<DecimalSymbolsV1> =
        icu_decimal::provider::Baked.load(request).unwrap();

    response.payload.get().decimal_separator().to_string()
}

fn intern_locale_str(
    cache: &OnceLock<Mutex<Vec<(Locale, &'static str)>>>,
    loc: Locale,
    build: impl FnOnce(Locale) -> String,
) -> &'static str {
    let cache = cache.get_or_init(|| Mutex::new(Vec::new()));
    let mut guard = cache.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((_, existing)) = guard.iter().find(|(k, _)| *k == loc) {
        return existing;
    }
    let leaked: &'static str = Box::leak(build(loc.clone()).into_boxed_str());
    guard.push((loc, leaked));
    leaked
}

/// Return the decimal separator from the language we're working with.
/// Example:
///  Say we need to format 1000.5
///     `en_US`: 1,000.5 -> decimal separator is '.'
///     `fr_FR`: 1 000,5 -> decimal separator is ','
pub fn locale_decimal_separator() -> &'static str {
    static DECIMAL_SEP: OnceLock<String> = OnceLock::new();
    static INPROC_CACHE: OnceLock<Mutex<Vec<(Locale, &'static str)>>> = OnceLock::new();
    if crate::inproc::active() {
        return intern_locale_str(
            &INPROC_CACHE,
            get_numeric_locale().0.clone(),
            get_decimal_separator,
        );
    }

    DECIMAL_SEP.get_or_init(|| get_decimal_separator(get_numeric_locale().0.clone()))
}

/// Return the grouping separator for the given locale
fn get_grouping_separator(loc: Locale) -> String {
    let data_locale = DataLocale::from(loc);

    let request = DataRequest {
        id: DataIdentifierBorrowed::for_locale(&data_locale),
        metadata: DataRequestMetadata::default(),
    };

    let response: DataResponse<DecimalSymbolsV1> =
        icu_decimal::provider::Baked.load(request).unwrap();

    response.payload.get().grouping_separator().to_string()
}

/// Return the grouping separator from the language we're working with.
/// Example:
///  Say we need to format 1,000
///     `en_US`: 1,000 -> grouping separator is ','
///     `fr_FR`: 1 000 -> grouping separator is '\u{202f}'
pub fn locale_grouping_separator() -> &'static str {
    static GROUPING_SEP: OnceLock<String> = OnceLock::new();
    static INPROC_CACHE: OnceLock<Mutex<Vec<(Locale, &'static str)>>> = OnceLock::new();
    let build = |loc: Locale| {
        // C/POSIX locale (represented as "und") has no grouping separator.
        if loc == locale!("und") {
            String::new()
        } else {
            get_grouping_separator(loc)
        }
    };
    if crate::inproc::active() {
        return intern_locale_str(&INPROC_CACHE, get_numeric_locale().0.clone(), build);
    }

    GROUPING_SEP.get_or_init(|| build(get_numeric_locale().0.clone()))
}

#[cfg(test)]
mod tests {
    use icu_locale::locale;

    use super::{get_decimal_separator, get_grouping_separator};

    #[test]
    fn test_simple_decimal_separator() {
        assert_eq!(get_decimal_separator(locale!("en")), ".");
        assert_eq!(get_decimal_separator(locale!("fr")), ",");
    }

    #[test]
    fn test_simple_grouping_separator() {
        assert_eq!(get_grouping_separator(locale!("en")), ",");
        assert_eq!(get_grouping_separator(locale!("fr")), "\u{202f}");
    }
}
