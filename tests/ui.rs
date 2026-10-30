//! The page works offline: it loads nothing from another host, and its only outside links are the footer's.

const PAGE: &str = include_str!("../src/ui.html");

#[test]
fn page_loads_nothing_from_another_host() {
    let footer = [
        "https://nowmyagentcan.com",
        "https://github.com/nowmyagentcan/image-convert",
        "https://x.com/chayp05",
    ];
    for pat in ["src=", "href=", "url(", "@import", "fetch(", "action="] {
        for (i, _) in PAGE.match_indices(pat) {
            let rest = PAGE[i + pat.len()..].trim_start_matches(['"', '\'', ' ']);
            let target = &rest[..rest.find(['"', '\'', ' ', ')', '>']).unwrap_or(rest.len())];
            assert!(
                target.starts_with("data:") || !target.contains("://") || footer.contains(&target),
                "{pat} loads {target}"
            );
        }
    }
}
