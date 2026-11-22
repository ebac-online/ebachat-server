//! The platform, OS and browser a login session records, as Go's `getPlatformName` / `getOSName` /
//! `getBrowserName` / `getBrowserVersion` read them with uasurfer, for the agents seen in practice.

// Based on Mattermost server/channels/app/user_agent.go, Copyright (c) 2015-present Mattermost, Inc.
// Licensed under AGPL-3.0.

pub struct Agent {
    pub platform: &'static str,
    pub os: &'static str,
    pub browser: String,
}

/// `utils.IsMobileRequest`.
pub fn is_mobile(ua: &str) -> bool {
    !ua.is_empty() && ["Mobile", "Android", "iOS", "iPhone", "iPad"].iter().any(|k| ua.contains(k))
}

fn version_after<'a>(ua: &'a str, prefix: &str) -> Option<&'a str> {
    let (_, after) = ua.split_once(prefix)?;
    after.split(|c: char| c.is_whitespace() || c == ';' || c == ')').next().filter(|v| !v.is_empty())
}

/// `major.minor` or `major.minor.patch` as uasurfer prints it, from a dotted version.
fn ua_version(v: &str) -> String {
    let n: Vec<u64> = v
        .split('.')
        .take(3)
        .map(|p| p.chars().take_while(char::is_ascii_digit).collect::<String>().parse().unwrap_or(0))
        .collect();
    let (ma, mi, pa) =
        (n.first().copied().unwrap_or(0), n.get(1).copied().unwrap_or(0), n.get(2).copied().unwrap_or(0));
    if pa == 0 { format!("{ma}.{mi}") } else { format!("{ma}.{mi}.{pa}") }
}

pub fn parse(ua: &str) -> Agent {
    let mobile_app = ua.contains("Mattermost Mobile/");
    let (platform, os) = if ua.contains("Windows Phone") {
        ("Windows Phone", "Windows Phone")
    } else if ua.contains("Windows") {
        ("Windows", windows_name(ua))
    } else if ua.contains("iPhone") {
        ("iPhone", "iOS")
    } else if ua.contains("iPad") {
        ("iPad", "iOS")
    } else if ua.contains("Android") {
        ("Android", "Android")
    } else if ua.contains("Macintosh") || ua.contains("Mac OS X") {
        ("Macintosh", "Mac OS")
    } else if ua.contains("CrOS") {
        ("Linux", "Chrome OS")
    } else if ua.contains("Linux") || ua.contains("X11") {
        ("Linux", "Linux")
    } else if mobile_app {
        ("Android", "Android")
    } else {
        ("Unknown", "")
    };
    let name = if ua.contains("Electron") || ua.contains("Mattermost") && !mobile_app {
        "Desktop App"
    } else if mobile_app {
        "Mobile App"
    } else if ua.contains("mmctl") {
        "mmctl"
    } else if ua.contains("Edg/") || ua.contains("Edge/") {
        "Edge"
    } else if ua.contains("OPR/") || ua.contains("Opera") {
        "Opera"
    } else if ua.contains("Firefox/") {
        "Firefox"
    } else if ua.contains("Chrome/") || ua.contains("CriOS/") {
        "Chrome"
    } else if ua.contains("Safari/") {
        "Safari"
    } else if ua.contains("MSIE") || ua.contains("Trident/") {
        "Internet Explorer"
    } else {
        "Unknown"
    };
    let version = ["Mattermost Mobile/", "Mattermost/", "mmctl/", "Franz/"]
        .iter()
        .find_map(|p| version_after(ua, p).map(|v| v.chars().take(128).collect::<String>()))
        .unwrap_or_else(|| {
            let token = match name {
                "Firefox" => "Firefox/",
                "Chrome" if ua.contains("CriOS/") => "CriOS/",
                "Chrome" => "Chrome/",
                "Edge" if ua.contains("Edg/") => "Edg/",
                "Edge" => "Edge/",
                "Opera" => "OPR/",
                "Safari" => "Version/",
                _ => "",
            };
            if token.is_empty() {
                "0.0".into()
            } else {
                version_after(ua, token).map(ua_version).unwrap_or_else(|| "0.0".into())
            }
        });
    Agent { platform, os, browser: format!("{name}/{version}") }
}

fn windows_name(ua: &str) -> &'static str {
    match version_after(ua, "Windows NT ") {
        Some("5.0") => "Windows 2000",
        Some("5.1") => "Windows XP",
        Some("5.2") => "Windows XP x64 Edition",
        Some("6.0") => "Windows Vista",
        Some("6.1") => "Windows 7",
        Some("6.2") => "Windows 8",
        Some("6.3") => "Windows 8.1",
        Some(v) if v.starts_with("10") => "Windows 10",
        _ => "Windows",
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn agents() {
        let a = super::parse("curl/8.7.1");
        assert_eq!((a.platform, a.os, a.browser.as_str()), ("Unknown", "", "Unknown/0.0"));
        let c = super::parse(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36",
        );
        assert_eq!((c.platform, c.os, c.browser.as_str()), ("Macintosh", "Mac OS", "Chrome/126.0"));
        let d = super::parse(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Mattermost/5.9.0 Chrome/124.0.6367.243 Electron/30.1.2 Safari/537.36",
        );
        assert_eq!((d.platform, d.os, d.browser.as_str()), ("Windows", "Windows 10", "Desktop App/5.9.0"));
    }
}
