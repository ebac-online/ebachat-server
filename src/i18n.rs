//! Mattermost's English strings (`i18n/en.json`, verbatim from the 12.1.0 server): the messages of API errors,
//! looked up by error id as Go's `AppError.Translate` does with the default locale. Every text comes out naming
//! the site instead of Mattermost (`crate::brand`), before its parameters are filled in.

// i18n/en.json: Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::HashMap;
use std::sync::OnceLock;

static EN: &str = include_str!("../i18n/en.json");

fn table() -> &'static HashMap<&'static str, String> {
    static T: OnceLock<HashMap<&'static str, String>> = OnceLock::new();
    T.get_or_init(|| {
        #[derive(serde::Deserialize)]
        struct Entry<'a> {
            id: &'a str,
            translation: serde_json::Value,
        }
        let entries: Vec<Entry<'static>> = serde_json::from_str(EN).unwrap_or_default();
        entries
            .into_iter()
            .filter_map(|e| {
                // Plural forms are objects ({"one", "other"}); "other" serves.
                let t = match e.translation {
                    serde_json::Value::String(s) => s,
                    serde_json::Value::Object(m) => m.get("other")?.as_str()?.to_string(),
                    _ => return None,
                };
                Some((e.id, t))
            })
            .collect()
    })
}

/// The English text of `id`, if it needs no parameters.
pub fn t(id: &str) -> Option<&'static str> {
    table().get(id).map(String::as_str).filter(|s| !s.contains("{{")).map(crate::brand::intern)
}

fn plurals() -> &'static HashMap<&'static str, String> {
    static T: OnceLock<HashMap<&'static str, String>> = OnceLock::new();
    T.get_or_init(|| {
        #[derive(serde::Deserialize)]
        struct Entry<'a> {
            id: &'a str,
            translation: serde_json::Value,
        }
        let entries: Vec<Entry<'static>> = serde_json::from_str(EN).unwrap_or_default();
        entries
            .into_iter()
            .filter_map(|e| Some((e.id, e.translation.as_object()?.get("one")?.as_str()?.to_string())))
            .collect()
    })
}

/// `T(id, count, params)`: English plural forms, "one" for 1 and "other" otherwise.
pub fn tr_n(id: &str, count: usize, args: &[(&str, &str)]) -> Option<String> {
    let s = if count == 1 { plurals().get(id).or_else(|| table().get(id)) } else { table().get(id) }?;
    let mut s = crate::brand::apply(s).into_owned();
    for (k, v) in args {
        s = s.replace(&format!("{{{{.{k}}}}}"), v);
    }
    Some(s)
}

/// The English text of `id` with `{{.Name}}` parameters filled in.
pub fn tr(id: &str, args: &[(&str, &str)]) -> Option<String> {
    let mut s = crate::brand::apply(table().get(id)?).into_owned();
    for (k, v) in args {
        s = s.replace(&format!("{{{{.{k}}}}}"), v);
    }
    Some(s)
}

/// A locale's strings: plain ones, and plural forms by CLDR category.
#[derive(Default)]
struct Locale {
    plain: HashMap<String, String>,
    plural: HashMap<String, HashMap<String, String>>,
}

fn load_locale(json: &str) -> Locale {
    #[derive(serde::Deserialize)]
    struct Entry {
        id: String,
        translation: serde_json::Value,
    }
    let entries: Vec<Entry> = serde_json::from_str(json).unwrap_or_default();
    let mut l = Locale::default();
    for e in entries {
        match e.translation {
            serde_json::Value::String(s) if !s.is_empty() => {
                l.plain.insert(e.id, s);
            }
            serde_json::Value::Object(m) => {
                let forms: HashMap<String, String> = m
                    .into_iter()
                    .filter_map(|(k, v)| Some((k, v.as_str().filter(|s| !s.is_empty())?.to_string())))
                    .collect();
                if !forms.is_empty() {
                    l.plural.insert(e.id, forms);
                }
            }
            _ => {}
        }
    }
    l
}

/// The directory of the other locales' files (`i18n/*.json` of the server, Apache-2.0): `EBACHAT_I18N_DIR`, or
/// `i18n` in the working directory. English is built in.
fn i18n_dir() -> std::path::PathBuf {
    std::env::var("EBACHAT_I18N_DIR").map(Into::into).unwrap_or_else(|_| "i18n".into())
}

/// The locales Go knows (`supportedLocales`), loaded on first use.
fn locale(name: &str) -> Option<&'static Locale> {
    use std::sync::Mutex;
    static ALL: OnceLock<Mutex<HashMap<String, &'static Locale>>> = OnceLock::new();
    const SUPPORTED: &[&str] = &[
        "de", "en", "en-AU", "es", "fr", "it", "hu", "nl", "pl", "pt-BR", "ro", "sv", "vi", "tr", "bg", "ru", "uk",
        "fa", "ko", "zh-CN", "zh-TW", "ja", "da", "cs", "hr", "lt", "nb-NO", "pt", "fi",
    ];
    if !SUPPORTED.contains(&name) {
        return None;
    }
    let mut all = ALL.get_or_init(Default::default).lock().unwrap();
    if let Some(l) = all.get(name) {
        return Some(l);
    }
    let l: &'static Locale = if name == "en" {
        Box::leak(Box::new(load_locale(EN)))
    } else {
        let json = std::fs::read_to_string(i18n_dir().join(format!("{name}.json"))).ok()?;
        Box::leak(Box::new(load_locale(&json)))
    };
    all.insert(name.to_string(), l);
    Some(l)
}

/// The CLDR plural category of `n` in a language (the forms the server's translations use).
fn plural_category(lang: &str, n: i64) -> &'static str {
    let lang = lang.split('-').next().unwrap_or(lang);
    let (m10, m100) = (n % 10, n % 100);
    match lang {
        "ja" | "ko" | "zh" | "vi" | "tr" | "fa" => "other",
        "ru" | "uk" | "hr" => {
            if m10 == 1 && m100 != 11 {
                "one"
            } else if (2..=4).contains(&m10) && !(12..=14).contains(&m100) {
                "few"
            } else if lang == "hr" {
                "other"
            } else {
                "many"
            }
        }
        "pl" => {
            if n == 1 {
                "one"
            } else if (2..=4).contains(&m10) && !(12..=14).contains(&m100) {
                "few"
            } else {
                "many"
            }
        }
        "cs" => match n {
            1 => "one",
            2..=4 => "few",
            _ => "other",
        },
        "lt" => {
            if m10 == 1 && !(11..=19).contains(&m100) {
                "one"
            } else if (2..=9).contains(&m10) && !(11..=19).contains(&m100) {
                "few"
            } else {
                "other"
            }
        }
        "fr" | "pt" if n == 0 || n == 1 => "one",
        _ if n == 1 => "one",
        _ => "other",
    }
}

/// Go template parameters, `{{.Name}}` or `{{ .Name }}`; unknown ones become `<no value>` as Go prints them.
fn fill(s: String, args: &[(&str, &str)]) -> String {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"\{\{-?\s*\.(\w+)\s*-?\}\}").unwrap());
    re.replace_all(&s, |c: &regex::Captures| {
        args.iter().find(|(k, _)| *k == &c[1]).map(|(_, v)| v.to_string()).unwrap_or_else(|| "<no value>".into())
    })
    .into_owned()
}

/// `i18n.GetUserTranslations(locale)(id, args)`: the user's locale (English when the server does not have it),
/// English when the locale lacks the string, the id when neither has it. A `Count` argument picks the plural form.
pub fn tl(locale_name: &str, id: &str, args: &[(&str, &str)]) -> String {
    let count = args.iter().find(|(k, _)| *k == "Count").and_then(|(_, v)| v.parse::<i64>().ok());
    let lookup = |name: &str| -> Option<String> {
        let l = locale(name)?;
        if let Some(n) = count
            && let Some(forms) = l.plural.get(id)
        {
            let cat = plural_category(name, n);
            return forms.get(cat).or_else(|| forms.get("other")).cloned();
        }
        if let Some(s) = l.plain.get(id) {
            return Some(s.clone());
        }
        l.plural.get(id).and_then(|f| f.get("other").cloned())
    };
    let name = if locale(locale_name).is_some() { locale_name } else { "en" };
    let s = lookup(name).or_else(|| lookup("en")).unwrap_or_else(|| id.to_string());
    fill(crate::brand::apply(&s).into_owned(), args)
}

#[cfg(test)]
mod tests {
    #[test]
    fn looks_up_by_id() {
        assert_eq!(
            super::t("api.context.session_expired.app_error"),
            Some("Invalid or expired session, please login again.")
        );
        assert!(super::tr("api.context.invalid_body_param.app_error", &[("Name", "x")]).unwrap().contains(" x "));
    }

    #[test]
    fn user_locales_and_plurals() {
        assert_eq!(super::tl("xx", "January", &[]), "January");
        assert_eq!(super::tl("en", "no.such.id", &[]), "no.such.id");
        let one =
            super::tl("en", "api.post.get_message_for_notification.images_sent", &[("Count", "1"), ("Filenames", "a")]);
        assert_eq!(one, "1 image sent: a");
        assert_eq!(super::plural_category("ru", 5), "many");
        assert_eq!(super::plural_category("ru", 22), "few");
    }

    /// No locale's text names Mattermost once rendered, but in identifiers and lowercase URLs (docs/BRANDING.md).
    #[test]
    fn texts_name_the_site() {
        let name = crate::brand::name();
        assert_eq!(super::t("api.templates.post_body.button"), Some(format!("Reply in {name}").as_str()));
        assert_eq!(
            super::t("api.bot.teams_channels.add_message_mobile").unwrap(),
            "Please add me to teams and channels you want me to interact in. To do this, use the browser or the desktop app."
        );
        let de = super::tl("de", "api.bot.teams_channels.add_message_mobile", &[]);
        assert!(de.contains("die Desktop-App") && !de.contains("Mattermost"), "{de}");
        let msg = super::tr(
            "api.upgrade_to_enterprise.invalid-user.app_error",
            &[("MattermostUsername", "mm"), ("Path", "/p")],
        )
        .unwrap();
        assert!(msg.starts_with(&format!("{name} was unable")) && msg.contains("system user mm "), "{msg}");
        for f in std::fs::read_dir("i18n").unwrap() {
            let path = f.unwrap().path();
            let entries: Vec<serde_json::Value> =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            for e in entries {
                let id = e["id"].as_str().unwrap();
                let texts: Vec<&str> = match &e["translation"] {
                    serde_json::Value::String(s) => vec![s],
                    serde_json::Value::Object(m) => m.values().filter_map(|v| v.as_str()).collect(),
                    _ => vec![],
                };
                for s in texts {
                    let out = crate::brand::apply_with(s, "Acme");
                    let left = out
                        .match_indices("Mattermost")
                        .any(|(i, _)| !out[i + "Mattermost".len()..].starts_with(|c: char| c.is_ascii_uppercase()));
                    assert!(!left && !out.contains("매터모스트"), "{}: {id}: {out}", path.display());
                }
            }
        }
    }

    /// An API error's message, as the API sends it.
    #[test]
    fn errors_name_the_site() {
        let name = crate::brand::name();
        let e = crate::app::AppError::bad_request("ent.elasticsearch.test_config.license.error", "");
        assert_eq!(e.message, "Your license doesn't support indexed search.");
        let e = crate::app::AppError::bad_request("web.error.unsupported_browser.no_longer_support", "");
        assert_eq!(e.message, format!("This browser is no longer supported by {name}"));
        let e = crate::app::AppError::bad_request("no.such.id", "Mattermost could not do it.");
        assert_eq!(e.message, format!("{name} could not do it."));
    }
}
