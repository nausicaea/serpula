use crate::config::{DOMAIN, SUBDOMAIN, TLD};
use std::path::Path;

use crate::config::Config;

const MAX_LABEL_LEN: usize = 63;
const MAX_NAME_LEN: usize = 255;
const FALLBACK_LABEL: &str = "label";

pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// This attempts to follow IETF RFC 1035
///
/// Strategy:
/// 1. Any character that isn't an ASCII letter, digit, or hyphen is mapped
///    to a hyphen.
/// 2. Runs of consecutive hyphens are collapsed to a single hyphen (this is
///    what makes the function idempotent and guarantees no long hyphen
///    runs, at the cost of not preserving multi-hyphen sequences that were
///    already present, e.g. "a--b" -> "a-b").
/// 3. Leading and trailing hyphens are stripped, since a label must start
///    and end with an alphanumeric character.
/// 4. The result is truncated to 63 characters (the RFC 1035 label limit),
///    with any hyphen left dangling by truncation removed.
/// 5. If nothing valid survives, a fallback label is returned so the
///    result is never empty.
pub fn sanitize_as_domain_label(s: &str) -> String {
    // Step 1: replace invalid characters with '-'.
    let mapped: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();

    // Step 2: collapse consecutive hyphens.
    let mut collapsed = String::with_capacity(mapped.len());
    let mut prev_was_hyphen = false;
    for c in mapped.chars() {
        if c == '-' {
            if !prev_was_hyphen {
                collapsed.push('-');
            }
            prev_was_hyphen = true;
        } else {
            collapsed.push(c);
            prev_was_hyphen = false;
        }
    }

    // Step 3: trim leading/trailing hyphens.
    let trimmed = collapsed.trim_matches('-');

    // Step 4: truncate to the max label length, then drop any hyphen left
    // dangling at the new end by truncation.
    let mut truncated: String = trimmed.chars().take(MAX_LABEL_LEN).collect();
    while truncated.ends_with('-') {
        truncated.pop();
    }

    // Step 5: never return an empty label.
    if truncated.is_empty() {
        return FALLBACK_LABEL.to_string();
    }

    truncated
}

pub fn plist_label(cfg: &Config, name: &str) -> String {
    let mut rdn = format!(
        "{TLD}.{DOMAIN}.{SUBDOMAIN}.{}.{name}",
        sanitize_as_domain_label(&cfg.username)
    );
    rdn.truncate(MAX_NAME_LEN);
    rdn
}

pub fn schedule_interval(seconds: u64) -> String {
    format!("    <key>StartInterval</key>\n    <integer>{seconds}</integer>")
}

pub fn schedule_calendar(weekday: Option<u8>, hour: u8, minute: u8) -> String {
    let mut s = String::from("    <key>StartCalendarInterval</key>\n    <dict>\n");
    if let Some(wd) = weekday {
        s.push_str(&format!(
            "        <key>Weekday</key>\n        <integer>{wd}</integer>\n"
        ));
    }
    s.push_str(&format!(
        "        <key>Hour</key>\n        <integer>{hour}</integer>\n        <key>Minute</key>\n        <integer>{minute}</integer>\n    </dict>"
    ));
    s
}

pub fn plist_document(config: &Config, exe: &Path, name: &str, schedule_xml: &str) -> String {
    let label = plist_label(config, name);
    let out_log = config.log_dir.join(format!("{name}.stdout.log"));
    let err_log = config.log_dir.join(format!("{name}.stderr.log"));

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
<key>Label</key>
<string>{}</string>
<key>ProgramArguments</key>
<array>
    <string>{}</string>
    <string>{}</string>
</array>
<key>EnvironmentVariables</key>
<dict>
    <key>PATH</key>
    <string>/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin</string>
</dict>
{}
<key>RunAtLoad</key>
<false/>
<key>StandardOutPath</key>
<string>{}</string>
<key>StandardErrorPath</key>
<string>{}</string>
<key>ProcessType</key>
<string>Background</string>
<key>LowPriorityIO</key>
<true/>
</dict>
</plist>
"#,
        xml_escape(&label),
        xml_escape(&exe.to_string_lossy()),
        xml_escape(name),
        schedule_xml,
        xml_escape(&out_log.to_string_lossy()),
        xml_escape(&err_log.to_string_lossy()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::{prop_assert, prop_assert_eq, proptest};
    use std::path::PathBuf;

    fn dummy_config() -> Config {
        Config {
            home_dir: PathBuf::from("/home/test"),
            username: "alice".to_string(),
            secrets_path: PathBuf::from("/home/test/.secrets/env"),
            runtime_dir: PathBuf::from("/home/test/.run"),
            log_dir: PathBuf::from("/home/test/.logs"),
            cache_dir: PathBuf::from("/home/test/.cache"),
            backup_source: PathBuf::from("/home/test"),
            hostname: "myhost".to_string(),
            ntfy_server: "https://ntfy.sh".to_string(),
            ntfy_prefix: "prefix".into(),
            keep_hourly: "24".to_string(),
            keep_daily: "14".to_string(),
            keep_weekly: "4".to_string(),
            keep_monthly: "12".to_string(),
            keep_yearly: "10".to_string(),
            check_percent: "10".to_string(),
        }
    }

    #[test]
    fn escapes_xml_chars() {
        assert_eq!(xml_escape(r#"<>&"'"#), "&lt;&gt;&amp;&quot;&apos;");
    }

    #[test]
    fn xml_escape_plain_string_unchanged() {
        assert_eq!(xml_escape("hello world"), "hello world");
    }

    #[test]
    fn sanitize_non_alnum() {
        assert_eq!(sanitize_as_domain_label("john.doe+mac"), "john-doe-mac");
    }

    #[test]
    fn sanitize_alnum_unchanged() {
        assert_eq!(sanitize_as_domain_label("alice123"), "alice123");
    }

    #[test]
    fn plist_label_format() {
        let cfg = dummy_config();
        let label = plist_label(&cfg, "backup");
        assert_eq!(label, "net.nausicaea.serpula.alice.backup");
    }

    #[test]
    fn plist_label_sanitizes_username() {
        let mut cfg = dummy_config();
        cfg.username = "john.doe".to_string();
        let label = plist_label(&cfg, "check");
        assert_eq!(label, "net.nausicaea.serpula.john-doe.check");
    }

    #[test]
    fn schedule_interval_format() {
        let xml = schedule_interval(3600);
        assert!(xml.contains("<key>StartInterval</key>"));
        assert!(xml.contains("<integer>3600</integer>"));
    }

    #[test]
    fn calendar_contains_weekday_when_present() {
        let xml = schedule_calendar(Some(0), 3, 30);
        assert!(xml.contains("<key>Weekday</key>"));
        assert!(xml.contains("<integer>0</integer>"));
    }

    #[test]
    fn calendar_omits_weekday_when_none() {
        let xml = schedule_calendar(None, 2, 0);
        assert!(!xml.contains("Weekday"));
        assert!(xml.contains("<key>Hour</key>"));
        assert!(xml.contains("<integer>2</integer>"));
    }

    #[test]
    fn plist_document_contains_label_and_exe() {
        let cfg = dummy_config();
        let exe = PathBuf::from("/usr/local/bin/restic-backup");
        let xml = plist_document(&cfg, &exe, "backup", &schedule_interval(3600));
        assert!(xml.contains("net.nausicaea.serpula.alice.backup"));
        assert!(xml.contains("/usr/local/bin/restic-backup"));
        assert!(xml.contains("<string>backup</string>"));
        assert!(xml.contains("backup.stdout.log"));
        assert!(xml.contains("backup.stderr.log"));
    }

    #[test]
    fn plist_document_escapes_special_chars_in_exe() {
        let cfg = dummy_config();
        let exe = PathBuf::from("/path/with/<special>&chars/bin");
        let xml = plist_document(&cfg, &exe, "backup", &schedule_interval(60));
        assert!(xml.contains("&lt;special&gt;&amp;chars"));
    }

    /// Independently written reference implementation, used purely as a
    /// test oracle for the differential test below. Kept deliberately
    /// separate from `src/lib.rs` so a bug in one is unlikely to be
    /// mirrored in the other.
    fn reference_escape(s: &str) -> String {
        let mut out = String::new();
        for c in s.chars() {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                '\'' => out.push_str("&apos;"),
                other => out.push(other),
            }
        }
        out
    }

    /// Minimal unescaper, understanding only the five entities that
    /// `xml_escape` is e[118;1:3uxpected to produce. Used to check that escaping
    /// is reversible.
    fn reference_unescape(s: &str) -> Option<String> {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '&' {
                out.push(c);
                continue;
            }
            let rest: String = chars.clone().collect();
            let (entity, replacement) = [
                ("amp;", '&'),
                ("lt;", '<'),
                ("gt;", '>'),
                ("quot;", '"'),
                ("apos;", '\''),
            ]
            .into_iter()
            .find(|(entity, _)| rest.starts_with(entity))?;
            out.push(replacement);
            for _ in 0..entity.len() {
                chars.next();
            }
        }
        Some(out)
    }

    proptest! {
        /// Escaping must never panic, including on arbitrary non-ASCII input.
        #[test]
        fn does_not_panic(s in ".*") {
            let _ = xml_escape(&s);
        }

        /// Output length in chars/bytes never shrinks: every substitution
        /// is either a no-op or an expansion.
        #[test]
        fn length_never_decreases(s in ".*") {
            let escaped = xml_escape(&s);
            prop_assert!(escaped.len() >= s.len());
        }

        /// A string containing none of the five special characters must
        /// pass through completely unchanged.
        #[test]
        fn identity_on_plain_text(s in "[^&<>'\"]*") {
            prop_assert_eq!(xml_escape(&s), s);
        }

        /// The output never contains a bare '<' or '>'; every occurrence
        /// in the input must have been converted to an entity.
        #[test]
        fn no_raw_angle_brackets(s in ".*") {
            let escaped = xml_escape(&s);
            prop_assert!(!escaped.contains('<'));
            prop_assert!(!escaped.contains('>'));
        }

        /// Every '&' in the output must begin one of the five known
        /// entities; there must be no "naked" ampersand left over.
        #[test]
        fn every_ampersand_starts_known_entity(s in ".*") {
            let escaped = xml_escape(&s);
            let mut rest = escaped.as_str();
            while let Some(pos) = rest.find('&') {
                let tail = &rest[pos + 1..];
                let starts_known = ["amp;", "lt;", "gt;", "quot;", "apos;"]
                    .iter()
                    .any(|e| tail.starts_with(e));
                prop_assert!(starts_known, "unescaped '&' found in {:?}", escaped);
                rest = &tail[1..];
            }
        }

        /// Differential test: xml_escape must agree with an independently
        /// written reference implementation on every input.
        #[test]
        fn matches_reference_implementation(s in ".*") {
            prop_assert_eq!(xml_escape(&s), reference_escape(&s));
        }

        /// Escaping then unescaping (with a matching reference unescaper)
        /// must recover the original string exactly.
        #[test]
        fn roundtrips_through_reference_unescape(s in ".*") {
            let escaped = xml_escape(&s);
            let recovered = reference_unescape(&escaped);
            prop_assert_eq!(recovered, Some(s));
        }

        /// Escaping distributes over concatenation: escaping two strings
        /// separately and joining the results equals escaping the
        /// concatenation directly. Catches bugs where escaping depends on
        /// look-ahead/look-behind context across a boundary.
        #[test]
        fn distributes_over_concatenation(a in ".*", b in ".*") {
            let combined = format!("{a}{b}");
            prop_assert_eq!(
                xml_escape(&combined),
                format!("{}{}", xml_escape(&a), xml_escape(&b))
            );
        }

        /// The number of times each entity appears in the output equals
        /// the number of times the corresponding character appeared in
        /// the input. Because every '&' in the output originates from an
        /// escaped input '&', entity substrings can't be produced by
        /// coincidental adjacency of unrelated plain text, so this count
        /// is a reliable per-character check.
        #[test]
        fn entity_counts_match_source_counts(s in ".*") {
            let escaped = xml_escape(&s);
            for (ch, entity) in [
                ('&', "&amp;"),
                ('<', "&lt;"),
                ('>', "&gt;"),
                ('"', "&quot;"),
                ('\'', "&apos;"),
            ] {
                let source_count = s.chars().filter(|c| *c == ch).count();
                let entity_count = escaped.matches(entity).count();
                prop_assert_eq!(source_count, entity_count);
            }
        }
    }

    #[test]
    fn unit_edge_cases() {
        assert_eq!(xml_escape(""), "");
        assert_eq!(xml_escape("hello"), "hello");
        assert_eq!(xml_escape("&"), "&amp;");
        assert_eq!(xml_escape("<>&\"'"), "&lt;&gt;&amp;&quot;&apos;");
        assert_eq!(xml_escape("a & b"), "a &amp; b");
        assert_eq!(xml_escape("<<>>&&"), "&lt;&lt;&gt;&gt;&amp;&amp;");
        assert_eq!(xml_escape("日本語 & 中文"), "日本語 &amp; 中文");
    }

    fn is_valid_label_char(c: char) -> bool {
        c.is_ascii_alphanumeric() || c == '-'
    }

    proptest! {
        #[test]
        fn output_respects_max_length(s in ".*") {
            let out = sanitize_as_domain_label(&s);
            prop_assert!(out.len() <= MAX_LABEL_LEN);
        }

        #[test]
        fn output_uses_only_valid_characters(s in ".*") {
            let out = sanitize_as_domain_label(&s);
            prop_assert!(out.chars().all(is_valid_label_char));
        }

        #[test]
        fn output_does_not_start_with_hyphen(s in ".*") {
            let out = sanitize_as_domain_label(&s);
            prop_assert!(!out.starts_with('-'));
        }

        #[test]
        fn output_does_not_end_with_hyphen(s in ".*") {
            let out = sanitize_as_domain_label(&s);
            prop_assert!(!out.ends_with('-'));
        }

        #[test]
        fn output_is_deterministic(s in ".*") {
            prop_assert_eq!(sanitize_as_domain_label(&s), sanitize_as_domain_label(&s));
        }

        #[test]
        fn sanitizing_is_idempotent(s in ".*") {
            let once = sanitize_as_domain_label(&s);
            let twice = sanitize_as_domain_label(&once);
            prop_assert_eq!(once, twice);
        }

        #[test]
        fn output_is_ascii(s in ".*") {
            let out = sanitize_as_domain_label(&s);
            prop_assert!(out.is_ascii());
        }

        #[test]
        fn output_is_never_empty(s in ".*") {
            let out = sanitize_as_domain_label(&s);
            prop_assert!(!out.is_empty());
        }

        #[test]
        fn output_has_no_absurd_hyphen_runs(s in ".*") {
            let out = sanitize_as_domain_label(&s);
            prop_assert!(!out.contains("---"));
        }

        // FIXED generator: no adjacent hyphens (since the implementation
        // collapses them), and length capped at 63 to match MAX_LABEL_LEN.
        // Each repetition contributes at most 2 chars (an optional hyphen
        // plus an alphanumeric), so {0,31} keeps the total at 1 + 2*31 = 63.
        #[test]
        fn valid_ascii_labels_are_left_essentially_unchanged(
            s in "[a-zA-Z](-?[a-zA-Z0-9]){0,31}"
        ) {
            let out = sanitize_as_domain_label(&s);
            prop_assert!(out.eq_ignore_ascii_case(&s));
        }
    }

    #[test]
    fn empty_input_produces_valid_label() {
        let out = sanitize_as_domain_label("");
        assert!(!out.is_empty());
        assert!(out.len() <= MAX_LABEL_LEN);
        assert!(out.chars().all(is_valid_label_char));
        assert!(!out.starts_with('-'));
        assert!(!out.ends_with('-'));
    }

    #[test]
    fn overlong_input_is_truncated_to_limit() {
        let long_input = "a".repeat(500);
        let out = sanitize_as_domain_label(&long_input);
        assert!(out.len() <= MAX_LABEL_LEN);
        assert!(!out.ends_with('-'));
    }

    #[test]
    fn input_of_only_hyphens_is_sanitized() {
        let out = sanitize_as_domain_label("------");
        assert!(!out.starts_with('-'));
        assert!(!out.ends_with('-'));
        assert_eq!(out, FALLBACK_LABEL);
    }

    #[test]
    fn input_with_unicode_is_sanitized_to_ascii() {
        let out = sanitize_as_domain_label("héllo wörld🚀");
        assert!(out.is_ascii());
        assert!(out.chars().all(is_valid_label_char));
    }

    #[test]
    fn input_with_underscores_and_spaces_is_sanitized() {
        let out = sanitize_as_domain_label("my_domain label");
        assert_eq!(out, "my-domain-label");
    }

    #[test]
    fn exactly_max_length_input_is_preserved_in_length() {
        let input: String = "a".repeat(MAX_LABEL_LEN);
        let out = sanitize_as_domain_label(&input);
        assert_eq!(out.len(), MAX_LABEL_LEN);
    }
}
