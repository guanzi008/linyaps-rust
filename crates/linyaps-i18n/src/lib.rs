use std::borrow::Cow;
use std::collections::BTreeMap;
use std::env;
use std::sync::OnceLock;

pub const DOMAIN: &str = "linyaps";

pub const LANGUAGES: &[&str] = &[
    "ady", "af", "af_ZA", "ak", "am", "am_ET", "ar", "ar_EG", "ast", "az", "bg", "bn", "bo", "bqi",
    "br", "ca", "cgg", "cs", "da", "de", "el", "el_GR", "en_AU", "en_GB", "en_NO", "en_US", "eo",
    "es", "et", "eu", "fa", "fi", "fil", "fr", "gl", "gl_ES", "he", "hi_IN", "hr", "hu", "hy",
    "id", "id_ID", "it", "ja", "ka", "kab", "kk", "km_KH", "kn_IN", "ko", "ku", "ku_IQ", "ky",
    "ky@Arab", "la", "lo", "lt", "lv", "ml", "mn", "mr", "ms", "nb", "ne", "nl", "pa", "pam", "pl",
    "ps", "pt", "pt_BR", "ro", "ru", "ru_UA", "sc", "si", "sk", "sl", "sq", "sr", "sv", "sv_SE",
    "sw", "ta", "te", "th", "tr", "tzm", "ug", "uk", "ur", "uz", "vi", "zh_CN", "zh_HK", "zh_TW",
];

pub const TRANSLATED_LANGUAGES: &[&str] = &[
    "ca", "en_GB", "en_US", "es", "fi", "fr", "it", "pl", "pt_BR", "ru", "sq", "uk", "zh_CN",
    "zh_HK", "zh_TW",
];

#[derive(Debug, Default)]
struct Catalog {
    messages: BTreeMap<String, String>,
}

static ACTIVE_CATALOG: OnceLock<Option<Catalog>> = OnceLock::new();
static ACTIVE_LANGUAGE: OnceLock<Option<&'static str>> = OnceLock::new();

pub fn language() -> Option<&'static str> {
    *ACTIVE_LANGUAGE.get_or_init(detect_language)
}

pub fn gettext(message: &str) -> Cow<'_, str> {
    let Some(catalog) = ACTIVE_CATALOG
        .get_or_init(|| language().and_then(catalog_for_language))
        .as_ref()
    else {
        return Cow::Borrowed(message);
    };
    catalog
        .messages
        .get(message)
        .filter(|translation| !translation.is_empty())
        .map_or_else(
            || Cow::Borrowed(message),
            |translation| Cow::Owned(translation.clone()),
        )
}

pub fn translate_document(document: &str) -> String {
    let Some(catalog) = ACTIVE_CATALOG
        .get_or_init(|| language().and_then(catalog_for_language))
        .as_ref()
    else {
        return document.to_string();
    };
    let mut replacements = catalog
        .messages
        .iter()
        .filter(|(message, translation)| !message.is_empty() && !translation.is_empty())
        .collect::<Vec<_>>();
    replacements.sort_by_key(|(message, _)| std::cmp::Reverse(message.len()));
    replacements
        .into_iter()
        .fold(document.to_string(), |document, (message, translation)| {
            document.replace(message, translation)
        })
}

pub fn format(message: &str, arguments: &[&dyn std::fmt::Display]) -> String {
    let translated = gettext(message);
    let mut output = String::with_capacity(translated.len() + arguments.len() * 8);
    let mut argument = arguments.iter();
    let mut characters = translated.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '{' && characters.peek() == Some(&'}') {
            characters.next();
            if let Some(value) = argument.next() {
                use std::fmt::Write;
                let _ = write!(output, "{value}");
            } else {
                output.push_str("{}");
            }
        } else {
            output.push(character);
        }
    }
    output
}

pub fn mo_bytes(language: &str) -> Vec<u8> {
    let mut messages = catalog_for_language(language)
        .map(|catalog| catalog.messages)
        .unwrap_or_default();
    messages.entry(String::new()).or_insert_with(|| {
        format!(
            "Content-Type: text/plain; charset=UTF-8\nContent-Transfer-Encoding: 8bit\nLanguage: {language}\n"
        )
    });
    encode_mo(&messages)
}

fn detect_language() -> Option<&'static str> {
    detect_language_with(|name| env::var(name).ok())
}

fn detect_language_with(mut variable: impl FnMut(&str) -> Option<String>) -> Option<&'static str> {
    let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .find_map(|name| variable(name).filter(|value| !value.is_empty()));
    if locale
        .as_deref()
        .is_none_or(|value| matches!(locale_language(value), "C" | "POSIX"))
    {
        return None;
    }
    let candidates = variable("LANGUAGE")
        .filter(|value| !value.is_empty())
        .map(|value| value.split(':').map(str::to_string).collect::<Vec<_>>())
        .unwrap_or_else(|| locale.into_iter().collect());
    for candidate in candidates {
        let candidate = locale_language(&candidate).replace('-', "_");
        if matches!(candidate.as_str(), "C" | "POSIX") {
            return None;
        }
        if let Some(language) = resolve_language(&candidate) {
            return Some(language);
        }
        let candidate = candidate.split('@').next().unwrap_or(&candidate);
        if let Some(language) = resolve_language(candidate) {
            return Some(language);
        }
        if let Some((base, _)) = candidate.split_once('_')
            && let Some(language) = resolve_language(base)
        {
            return Some(language);
        }
    }
    None
}

fn locale_language(locale: &str) -> &str {
    locale.split('.').next().unwrap_or(locale)
}

fn resolve_language(candidate: &str) -> Option<&'static str> {
    TRANSLATED_LANGUAGES
        .iter()
        .copied()
        .find(|language| *language == candidate)
}

fn catalog_for_language(language: &str) -> Option<Catalog> {
    Some(parse_po(po_source(language)?))
}

fn po_source(language: &str) -> Option<&'static str> {
    Some(match language {
        "ady" => include_str!("../catalogs/ady.po"),
        "af" => include_str!("../catalogs/af.po"),
        "af_ZA" => include_str!("../catalogs/af_ZA.po"),
        "ak" => include_str!("../catalogs/ak.po"),
        "am" => include_str!("../catalogs/am.po"),
        "am_ET" => include_str!("../catalogs/am_ET.po"),
        "ar" => include_str!("../catalogs/ar.po"),
        "ar_EG" => include_str!("../catalogs/ar_EG.po"),
        "ast" => include_str!("../catalogs/ast.po"),
        "az" => include_str!("../catalogs/az.po"),
        "bg" => include_str!("../catalogs/bg.po"),
        "bn" => include_str!("../catalogs/bn.po"),
        "bo" => include_str!("../catalogs/bo.po"),
        "bqi" => include_str!("../catalogs/bqi.po"),
        "br" => include_str!("../catalogs/br.po"),
        "ca" => include_str!("../catalogs/ca.po"),
        "cgg" => include_str!("../catalogs/cgg.po"),
        "cs" => include_str!("../catalogs/cs.po"),
        "da" => include_str!("../catalogs/da.po"),
        "de" => include_str!("../catalogs/de.po"),
        "el" => include_str!("../catalogs/el.po"),
        "el_GR" => include_str!("../catalogs/el_GR.po"),
        "en_AU" => include_str!("../catalogs/en_AU.po"),
        "en_GB" => include_str!("../catalogs/en_GB.po"),
        "en_NO" => include_str!("../catalogs/en_NO.po"),
        "en_US" => include_str!("../catalogs/en_US.po"),
        "eo" => include_str!("../catalogs/eo.po"),
        "es" => include_str!("../catalogs/es.po"),
        "et" => include_str!("../catalogs/et.po"),
        "eu" => include_str!("../catalogs/eu.po"),
        "fa" => include_str!("../catalogs/fa.po"),
        "fi" => include_str!("../catalogs/fi.po"),
        "fil" => include_str!("../catalogs/fil.po"),
        "fr" => include_str!("../catalogs/fr.po"),
        "gl" => include_str!("../catalogs/gl.po"),
        "gl_ES" => include_str!("../catalogs/gl_ES.po"),
        "he" => include_str!("../catalogs/he.po"),
        "hi_IN" => include_str!("../catalogs/hi_IN.po"),
        "hr" => include_str!("../catalogs/hr.po"),
        "hu" => include_str!("../catalogs/hu.po"),
        "hy" => include_str!("../catalogs/hy.po"),
        "id" => include_str!("../catalogs/id.po"),
        "id_ID" => include_str!("../catalogs/id_ID.po"),
        "it" => include_str!("../catalogs/it.po"),
        "ja" => include_str!("../catalogs/ja.po"),
        "ka" => include_str!("../catalogs/ka.po"),
        "kab" => include_str!("../catalogs/kab.po"),
        "kk" => include_str!("../catalogs/kk.po"),
        "km_KH" => include_str!("../catalogs/km_KH.po"),
        "kn_IN" => include_str!("../catalogs/kn_IN.po"),
        "ko" => include_str!("../catalogs/ko.po"),
        "ku" => include_str!("../catalogs/ku.po"),
        "ku_IQ" => include_str!("../catalogs/ku_IQ.po"),
        "ky" => include_str!("../catalogs/ky.po"),
        "ky@Arab" => include_str!("../catalogs/ky@Arab.po"),
        "la" => include_str!("../catalogs/la.po"),
        "lo" => include_str!("../catalogs/lo.po"),
        "lt" => include_str!("../catalogs/lt.po"),
        "lv" => include_str!("../catalogs/lv.po"),
        "ml" => include_str!("../catalogs/ml.po"),
        "mn" => include_str!("../catalogs/mn.po"),
        "mr" => include_str!("../catalogs/mr.po"),
        "ms" => include_str!("../catalogs/ms.po"),
        "nb" => include_str!("../catalogs/nb.po"),
        "ne" => include_str!("../catalogs/ne.po"),
        "nl" => include_str!("../catalogs/nl.po"),
        "pa" => include_str!("../catalogs/pa.po"),
        "pam" => include_str!("../catalogs/pam.po"),
        "pl" => include_str!("../catalogs/pl.po"),
        "ps" => include_str!("../catalogs/ps.po"),
        "pt" => include_str!("../catalogs/pt.po"),
        "pt_BR" => include_str!("../catalogs/pt_BR.po"),
        "ro" => include_str!("../catalogs/ro.po"),
        "ru" => include_str!("../catalogs/ru.po"),
        "ru_UA" => include_str!("../catalogs/ru_UA.po"),
        "sc" => include_str!("../catalogs/sc.po"),
        "si" => include_str!("../catalogs/si.po"),
        "sk" => include_str!("../catalogs/sk.po"),
        "sl" => include_str!("../catalogs/sl.po"),
        "sq" => include_str!("../catalogs/sq.po"),
        "sr" => include_str!("../catalogs/sr.po"),
        "sv" => include_str!("../catalogs/sv.po"),
        "sv_SE" => include_str!("../catalogs/sv_SE.po"),
        "sw" => include_str!("../catalogs/sw.po"),
        "ta" => include_str!("../catalogs/ta.po"),
        "te" => include_str!("../catalogs/te.po"),
        "th" => include_str!("../catalogs/th.po"),
        "tr" => include_str!("../catalogs/tr.po"),
        "tzm" => include_str!("../catalogs/tzm.po"),
        "ug" => include_str!("../catalogs/ug.po"),
        "uk" => include_str!("../catalogs/uk.po"),
        "ur" => include_str!("../catalogs/ur.po"),
        "uz" => include_str!("../catalogs/uz.po"),
        "vi" => include_str!("../catalogs/vi.po"),
        "zh_CN" => include_str!("../catalogs/zh_CN.po"),
        "zh_HK" => include_str!("../catalogs/zh_HK.po"),
        "zh_TW" => include_str!("../catalogs/zh_TW.po"),
        _ => return None,
    })
}

fn parse_po(source: &str) -> Catalog {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Field {
        Id,
        Translation,
    }

    fn commit(
        messages: &mut BTreeMap<String, String>,
        message: &mut String,
        translation: &mut String,
        fuzzy: bool,
    ) {
        if fuzzy && message.is_empty() {
            *translation = translation
                .split_inclusive('\n')
                .filter(|line| !line.starts_with("POT-Creation-Date:"))
                .collect();
        }
        if (!fuzzy || message.is_empty()) && !translation.is_empty() {
            messages.insert(std::mem::take(message), std::mem::take(translation));
        } else {
            message.clear();
            translation.clear();
        }
    }

    let mut messages = BTreeMap::new();
    let mut message = String::new();
    let mut translation = String::new();
    let mut field = None;
    let mut fuzzy = false;
    let mut has_entry = false;
    for line in source.lines().chain(std::iter::once("")) {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            if has_entry {
                commit(&mut messages, &mut message, &mut translation, fuzzy);
            }
            field = None;
            fuzzy = false;
            has_entry = false;
        } else if let Some(flags) = trimmed.strip_prefix("#,") {
            fuzzy |= flags.split(',').any(|flag| flag.trim() == "fuzzy");
        } else if let Some(value) = trimmed.strip_prefix("msgid ") {
            if has_entry && field == Some(Field::Translation) {
                commit(&mut messages, &mut message, &mut translation, fuzzy);
                fuzzy = false;
            }
            message = decode_po_string(value);
            field = Some(Field::Id);
            has_entry = true;
        } else if let Some(value) = trimmed.strip_prefix("msgstr ") {
            translation = decode_po_string(value);
            field = Some(Field::Translation);
            has_entry = true;
        } else if trimmed.starts_with('"') {
            let value = decode_po_string(trimmed);
            match field {
                Some(Field::Id) => message.push_str(&value),
                Some(Field::Translation) => translation.push_str(&value),
                None => {}
            }
        }
    }
    Catalog { messages }
}

fn decode_po_string(value: &str) -> String {
    let value = value.trim();
    let value = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value);
    let mut output = String::with_capacity(value.len());
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }
        match characters.next() {
            Some('n') => output.push('\n'),
            Some('r') => output.push('\r'),
            Some('t') => output.push('\t'),
            Some('"') => output.push('"'),
            Some('\\') => output.push('\\'),
            Some(character) => output.push(character),
            None => output.push('\\'),
        }
    }
    output
}

fn encode_mo(messages: &BTreeMap<String, String>) -> Vec<u8> {
    let entries = messages.iter().collect::<Vec<_>>();
    let count = entries.len() as u32;
    let originals_offset = 28_u32;
    let translations_offset = originals_offset + count * 8;
    let strings_offset = translations_offset + count * 8;
    let mut originals = Vec::new();
    let mut translations = Vec::new();
    let mut original_table = Vec::new();
    let mut translation_table = Vec::new();
    for (message, _) in &entries {
        original_table.push((
            message.len() as u32,
            strings_offset + originals.len() as u32,
        ));
        originals.extend_from_slice(message.as_bytes());
        originals.push(0);
    }
    let translations_start = strings_offset + originals.len() as u32;
    for (_, translation) in &entries {
        translation_table.push((
            translation.len() as u32,
            translations_start + translations.len() as u32,
        ));
        translations.extend_from_slice(translation.as_bytes());
        translations.push(0);
    }
    let mut output = Vec::with_capacity(translations_start as usize + translations.len());
    for value in [
        0x9504_12de,
        0,
        count,
        originals_offset,
        translations_offset,
        0,
        0,
    ] {
        output.extend_from_slice(&value.to_le_bytes());
    }
    for (length, offset) in original_table.into_iter().chain(translation_table) {
        output.extend_from_slice(&length.to_le_bytes());
        output.extend_from_slice(&offset.to_le_bytes());
    }
    output.extend_from_slice(&originals);
    output.extend_from_slice(&translations);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiline_and_escaped_po_entries() {
        let catalog = parse_po("msgid \"hello\\n\"\n\"world\"\nmsgstr \"你好\\n\"\n\"世界\"\n");
        assert_eq!(catalog.messages.get("hello\nworld").unwrap(), "你好\n世界");
    }

    #[test]
    fn translated_catalogs_have_real_messages() {
        let catalog = catalog_for_language("zh_CN").unwrap();
        assert_eq!(
            catalog.messages.get("Run an application").unwrap(),
            "运行应用程序"
        );
        assert!(
            !catalog_for_language("de")
                .unwrap()
                .messages
                .contains_key("Run an application")
        );
        assert!(
            LANGUAGES
                .iter()
                .all(|language| po_source(language).is_some())
        );
    }

    #[test]
    fn writes_valid_little_endian_mo_layout() {
        let bytes = mo_bytes("zh_CN");
        assert_eq!(&bytes[..4], &0x9504_12de_u32.to_le_bytes());
        let count = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        assert!(count > 100);
    }

    #[test]
    fn follows_gettext_locale_precedence_and_c_locale_rules() {
        let language = |values: &[(&str, &str)]| {
            detect_language_with(|name| {
                values
                    .iter()
                    .find_map(|(key, value)| (*key == name).then(|| (*value).to_string()))
            })
        };
        assert_eq!(
            language(&[("LANG", "C.UTF-8"), ("LANGUAGE", "zh_CN")]),
            None
        );
        assert_eq!(
            language(&[("LANG", "en_US.UTF-8"), ("LANGUAGE", "de:zh_CN")]),
            Some("zh_CN")
        );
        assert_eq!(language(&[("LANG", "fr_FR.UTF-8@euro")]), Some("fr"));
        assert_eq!(language(&[("LC_ALL", "C"), ("LANG", "zh_CN.UTF-8")]), None);
    }
}
