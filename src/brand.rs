//! The product name users see. Go's texts name "Mattermost" (translations, emails, pages); this server names the
//! site instead: `TeamSettings.SiteName`, or "EBAChat" when that is empty or still Go's default "Mattermost".
//! Texts about Mattermost's other products (its desktop and mobile apps, Cloud, Enterprise Edition, its license)
//! get neutral wording. Ids, keys, paths, cookies and headers are not touched. See docs/BRANDING.md.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Mutex, OnceLock};

/// The name when the config gives none of its own.
pub const DEFAULT: &str = "EBAChat";

/// What the texts say instead.
const BRAND: &str = "Mattermost";
/// The Korean translations transliterate it.
const BRAND_KO: &str = "매터모스트";

/// Phrases about another product than this site, rewritten before the name itself, in order (the longer first).
const PHRASES: &[(&str, &str)] = &[
    ("the Mattermost Desktop App", "the desktop app"),
    ("Mattermost Desktop App", "the desktop app"),
    ("the Mattermost desktop app", "the desktop app"),
    ("Mattermost desktop app", "the desktop app"),
    ("the Mattermost mobile app", "the mobile app"),
    ("Mattermost Mobile App", "the mobile app"),
    ("Mattermost mobile app", "the mobile app"),
    ("the Mattermost App", "the app"),
    ("the Mattermost app", "the app"),
    ("Mattermost Enterprise Edition", "Enterprise Edition"),
    ("Mattermost Enterprise edition", "Enterprise Edition"),
    ("Mattermost Enterprise", "Enterprise"),
    ("매터모스트 엔터프라이즈", "엔터프라이즈"),
    ("Mattermost Cloud", "Cloud"),
    ("the Mattermost Customer Portal", "the Customer Portal"),
    ("Mattermost customer portal", "customer portal"),
    ("the Mattermost Software Evaluation Agreement", "the Software Evaluation Agreement"),
    ("Mattermost license", "license"),
    ("a Mattermost expert", "an expert"),
    ("the Mattermost changelog", "the changelog"),
    ("Mattermost Security Bulletin", "Security Bulletin"),
    ("Mattermost v1", "v1"),
    // Other locales: the desktop app by its product name.
    ("Mattermost-Desktop-App", "Desktop-App"),
    ("Mattermost Desktop", "Desktop"),
];

/// The name in effect; set from the config at start and on every save or reload.
static NAME: AtomicPtr<String> = AtomicPtr::new(std::ptr::null_mut());

/// The name for a `TeamSettings.SiteName`: itself, or [`DEFAULT`] when empty or Go's default.
pub fn effective(site_name: &str) -> &str {
    let s = site_name.trim();
    if s.is_empty() || s == BRAND { DEFAULT } else { s }
}

/// The site's name, as users see it.
pub fn name() -> &'static str {
    let p = NAME.load(Ordering::Acquire);
    // SAFETY: every pointer stored is a leaked Box, never freed.
    if p.is_null() { DEFAULT } else { unsafe { (*p).as_str() } }
}

/// Follows the config: the name for its `TeamSettings.SiteName`. A replaced name is leaked, as the config's own
/// versions are; it changes only when an admin renames the site.
pub fn set(site_name: &str) {
    let n = effective(site_name);
    if n != name() {
        NAME.store(Box::into_raw(Box::new(n.to_string())), Ordering::Release);
    }
}

fn needs(text: &str) -> bool {
    text.contains(BRAND) || text.contains(BRAND_KO)
}

/// `text` naming the site in effect instead of Mattermost.
pub fn apply(text: &str) -> Cow<'_, str> {
    apply_with(text, name())
}

/// `text` naming `name` instead of Mattermost. Identifiers (`{{.MattermostUsername}}`, `MattermostAppID`) and
/// lowercase URLs and addresses (`docs.mattermost.com`) stay; an inflected form keeps its ending ("Mattermostu" in
/// Czech becomes "EBAChatu").
pub fn apply_with<'a>(text: &'a str, name: &str) -> Cow<'a, str> {
    if !needs(text) {
        return Cow::Borrowed(text);
    }
    let mut s = text.to_string();
    for (from, to) in PHRASES {
        if s.contains(from) {
            s = s.replace(from, to);
        }
    }
    s = s.replace(BRAND_KO, name);
    let mut out = String::with_capacity(s.len() + 16);
    let mut rest = s.as_str();
    while let Some(i) = rest.find(BRAND) {
        let before = rest[..i].chars().next_back().or_else(|| out.chars().next_back());
        let after = rest[i + BRAND.len()..].chars().next();
        let ident = before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
            || after.is_some_and(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        out.push_str(&rest[..i]);
        out.push_str(if ident { BRAND } else { name });
        rest = &rest[i + BRAND.len()..];
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// [`apply`] for a static text: the same text when it does not name Mattermost, else its rewrite, kept for the
/// name in effect (a few dozen texts per name).
pub fn intern(text: &'static str) -> &'static str {
    if !needs(text) {
        return text;
    }
    static DONE: OnceLock<Mutex<HashMap<(usize, usize), &'static str>>> = OnceLock::new();
    let n = name();
    let key = (n.as_ptr() as usize, text.as_ptr() as usize);
    let mut done = DONE.get_or_init(Default::default).lock().unwrap();
    done.entry(key).or_insert_with(|| Box::leak(apply_with(text, n).into_owned().into_boxed_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback() {
        assert_eq!(effective(""), "EBAChat");
        assert_eq!(effective("  "), "EBAChat");
        assert_eq!(effective("Mattermost"), "EBAChat");
        assert_eq!(effective("Acme Chat"), "Acme Chat");
    }

    #[test]
    fn names_the_site() {
        let a = |s| apply_with(s, "Acme").into_owned();
        assert_eq!(a("Reply in Mattermost"), "Reply in Acme");
        assert_eq!(a("Mattermost - Testing Email Settings"), "Acme - Testing Email Settings");
        assert_eq!(a("Mattermost's"), "Acme's");
        // Inflections keep their endings; CJK text around the name stays.
        assert_eq!(a("v Mattermostu"), "v Acmeu");
        assert_eq!(a("Mattermostをインストール"), "Acmeをインストール");
        assert_eq!(a("매터모스트 서버"), "Acme 서버");
        // Not the product's name: identifiers, parameters, URLs, addresses.
        assert_eq!(a("user {{.MattermostUsername}} id"), "user {{.MattermostUsername}} id");
        assert_eq!(a("MattermostAppID is too long"), "MattermostAppID is too long");
        assert_eq!(a("see https://docs.mattermost.com/x"), "see https://docs.mattermost.com/x");
        assert!(matches!(apply_with("no brand here", "Acme"), Cow::Borrowed(_)));
    }

    #[test]
    fn other_products_are_neutral() {
        let a = |s| apply_with(s, "Acme").into_owned();
        assert_eq!(a("use the browser or Mattermost Desktop App."), "use the browser or the desktop app.");
        assert_eq!(
            a("Download the Mattermost app or use a supported browser"),
            "Download the app or use a supported browser"
        );
        assert_eq!(
            a("Service only available in Mattermost Enterprise edition"),
            "Service only available in Enterprise Edition"
        );
        assert_eq!(a("only available in Mattermost Cloud."), "only available in Cloud.");
        assert_eq!(a("Your Mattermost license has expired"), "Your license has expired");
        assert_eq!(a("was removed in Mattermost v12.0."), "was removed in v12.0.");
        assert_eq!(
            a("verwende den Browser oder die Mattermost-Desktop-App."),
            "verwende den Browser oder die Desktop-App."
        );
        assert_eq!(a("매터모스트 엔터프라이즈 에디션"), "엔터프라이즈 에디션");
    }

    #[test]
    fn interned_follows_the_name() {
        static T: &str = "Open Mattermost";
        let before = name();
        assert_eq!(intern(T), apply_with(T, before));
        assert!(std::ptr::eq(intern(T), intern(T)));
        assert_eq!(intern("plain"), "plain");
    }
}
