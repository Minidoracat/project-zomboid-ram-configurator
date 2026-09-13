// SPDX-License-Identifier: GPL-3.0-only
// Project Zomboid RAM Configurator - GUI translation catalogs.
// Copyright (C) 2023-2026 Minidoracat

//! Table-driven translation catalogs.
//!
//! Every locale is a JSON file under `gui/locales/` that is embedded at compile
//! time, so the executable stays a single self-contained file. Adding a
//! language means adding one file and one row to [`BUNDLED`] - no code branches
//! anywhere else.

use std::collections::BTreeMap;

use serde::Deserialize;

/// Locale used when the requested one is unknown and for missing keys.
pub const FALLBACK_CODE: &str = "zh-TW";

/// The registry. One row per bundled locale: (file name, embedded contents).
const BUNDLED: &[(&str, &str)] = &[
    ("zh-TW.json", include_str!("../locales/zh-TW.json")),
    ("en.json", include_str!("../locales/en.json")),
    ("zh-CN.json", include_str!("../locales/zh-CN.json")),
];

#[derive(Deserialize)]
struct CatalogFile {
    code: String,
    label: String,
    lang: String,
    messages: BTreeMap<String, String>,
}

/// One loaded locale.
pub struct Catalog {
    /// Locale code as accepted on the command line, e.g. `zh-TW`.
    pub code: String,
    /// Name of the language in that language itself, for the picker.
    pub label: String,
    messages: BTreeMap<String, String>,
}

/// The loaded catalogs plus the currently active selection.
pub struct I18n {
    catalogs: Vec<Catalog>,
    active: usize,
    fallback: usize,
}

impl I18n {
    /// Loads every bundled locale and selects `preferred` when it is known.
    ///
    /// Returns `Err` when a catalog fails to parse or when the locales do not
    /// declare exactly the same keys; that is a packaging bug, so it is
    /// reported at startup instead of showing holes in the window.
    pub fn new(preferred: Option<&str>) -> Result<Self, String> {
        let mut catalogs = Vec::with_capacity(BUNDLED.len());
        for (name, raw) in BUNDLED {
            let parsed: CatalogFile =
                serde_json::from_str(raw).map_err(|err| format!("{name}: {err}"))?;
            if parsed.code.is_empty() || parsed.label.is_empty() || parsed.lang.is_empty() {
                return Err(format!("{name}: code, label and lang must not be empty"));
            }
            catalogs.push(Catalog {
                code: parsed.code,
                label: parsed.label,
                messages: parsed.messages,
            });
        }

        let fallback = catalogs
            .iter()
            .position(|c| c.code == FALLBACK_CODE)
            .ok_or_else(|| format!("missing fallback locale {FALLBACK_CODE}"))?;

        let mut problems = Vec::new();
        for (index, catalog) in catalogs.iter().enumerate() {
            if index == fallback {
                continue;
            }
            let reference = &catalogs[fallback].messages;
            let missing: Vec<&str> = reference
                .keys()
                .filter(|key| !catalog.messages.contains_key(key.as_str()))
                .map(|key| key.as_str())
                .collect();
            let extra: Vec<&str> = catalog
                .messages
                .keys()
                .filter(|key| !reference.contains_key(key.as_str()))
                .map(|key| key.as_str())
                .collect();
            if !missing.is_empty() {
                problems.push(format!("{} missing {}", catalog.code, missing.join(", ")));
            }
            if !extra.is_empty() {
                problems.push(format!("{} unexpected {}", catalog.code, extra.join(", ")));
            }
        }
        if !problems.is_empty() {
            return Err(problems.join("; "));
        }

        let active = preferred
            .and_then(|code| {
                catalogs
                    .iter()
                    .position(|c| c.code.eq_ignore_ascii_case(code))
            })
            .unwrap_or(fallback);

        Ok(Self {
            catalogs,
            active,
            fallback,
        })
    }

    /// True when `code` names a bundled locale.
    pub fn knows(&self, code: &str) -> bool {
        self.catalogs
            .iter()
            .any(|c| c.code.eq_ignore_ascii_case(code))
    }

    /// Locale codes joined for command-line messages.
    pub fn code_list(&self) -> String {
        self.catalogs
            .iter()
            .map(|c| c.code.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Language names for the picker, in registry order.
    pub fn labels(&self) -> Vec<String> {
        self.catalogs.iter().map(|c| c.label.clone()).collect()
    }

    /// Index of the active locale inside [`I18n::labels`].
    pub fn active_index(&self) -> usize {
        self.active
    }

    /// Switches the active locale by picker index. Out-of-range is ignored.
    pub fn set_active_index(&mut self, index: usize) {
        if index < self.catalogs.len() {
            self.active = index;
        }
    }

    /// True when the key exists in the active or the fallback locale.
    pub fn has(&self, key: &str) -> bool {
        self.catalogs[self.active].messages.contains_key(key)
            || self.catalogs[self.fallback].messages.contains_key(key)
    }

    /// Active translation, falling back to [`FALLBACK_CODE`], then to the key.
    pub fn t<'a>(&'a self, key: &'a str) -> &'a str {
        if let Some(text) = self.catalogs[self.active].messages.get(key) {
            return text;
        }
        if let Some(text) = self.catalogs[self.fallback].messages.get(key) {
            return text;
        }
        debug_assert!(false, "missing translation key: {key}");
        key
    }

    /// Active translation with `{name}` placeholders replaced.
    pub fn tp(&self, key: &str, params: &[(&str, &str)]) -> String {
        expand(self.t(key), params)
    }

    /// The same message in every bundled locale, for output that is produced
    /// before a language has been chosen (command-line and startup failures).
    pub fn every_locale(&self, key: &str, params: &[(&str, &str)]) -> Vec<String> {
        self.catalogs
            .iter()
            .filter_map(|catalog| catalog.messages.get(key))
            .map(|template| expand(template, params))
            .collect()
    }
}

/// Replaces `{name}` placeholders. Unknown names are left untouched so a
/// mistake shows up in the text instead of silently dropping information.
fn expand(template: &str, params: &[(&str, &str)]) -> String {
    if params.is_empty() || !template.contains('{') {
        return template.to_string();
    }
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                match params.iter().find(|(key, _)| *key == name) {
                    Some((_, value)) => out.push_str(value),
                    None => {
                        out.push('{');
                        out.push_str(name);
                        out.push('}');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simplified_chinese_is_selectable_case_insensitively() {
        let fallback = I18n::new(None).expect("bundled locales must load and match");
        let selected = I18n::new(Some("ZH-cn")).expect("zh-CN must load");
        assert!(selected.knows("zh-CN"));
        assert_ne!(selected.active_index(), fallback.active_index());
    }

    #[test]
    fn placeholders_are_substituted() {
        assert_eq!(expand("{value} GB", &[("value", "24")]), "24 GB");
        assert_eq!(expand("{a}/{b}", &[("b", "2"), ("a", "1")]), "1/2");
        assert_eq!(expand("{unset}", &[("value", "24")]), "{unset}");
        assert_eq!(expand("no params", &[("value", "24")]), "no params");
        assert_eq!(expand("open {", &[("value", "24")]), "open {");
    }
}
