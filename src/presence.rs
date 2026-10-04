//! Template rendering and the Discord activity payload.

use serde::Serialize;

/// Template variables for one render pass.
#[derive(Default)]
pub struct Vars(Vec<(&'static str, String)>);

impl Vars {
    pub fn set(&mut self, k: &'static str, v: impl Into<String>) {
        self.0.push((k, v.into()));
    }

    pub fn get(&self, k: &str) -> &str {
        self.0.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str()).unwrap_or("")
    }
}

const SEP: char = '·';

/// Substitute `{var}`s. The template is treated as `·`-separated segments:
/// a segment whose variable is empty is dropped entirely (so
/// `"{model} · {today_time} today"` never renders a dangling `"today"`).
///
/// Returns the rendered string and whether every variable it referenced had
/// a meaningful (non-empty, non-zero) value — rotation frames use that to
/// skip themselves.
pub fn render(tpl: &str, vars: &Vars) -> (String, bool) {
    let mut out = String::with_capacity(tpl.len() + 32);
    let mut all_present = true;
    for seg in tpl.split(SEP) {
        let mut text = String::with_capacity(seg.len() + 16);
        let mut drop = false;
        let mut rest = seg;
        while let Some(open) = rest.find('{') {
            text.push_str(&rest[..open]);
            let after = &rest[open + 1..];
            let Some(close) = after.find('}') else {
                text.push_str(&rest[open..]);
                rest = "";
                break;
            };
            let v = vars.get(&after[..close]);
            drop |= v.is_empty();
            all_present &= !v.is_empty() && v != "0";
            text.push_str(v);
            rest = &after[close + 1..];
            if v == "1" {
                singularize(&mut text, &mut rest);
            }
        }
        text.push_str(rest);
        let t = text.trim();
        if drop || t.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str(" · ");
        }
        out.push_str(t);
    }
    (out, all_present)
}

/// After a `1`, turn a following plural word into its singular:
/// `"{prompts} prompts"` renders `"1 prompt"`, not `"1 prompts"`.
fn singularize(text: &mut String, rest: &mut &str) {
    let Some(tail) = rest.strip_prefix(' ') else { return };
    let len = tail.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(tail.len());
    let word = &tail[..len];
    if word.len() > 1 && word.ends_with('s') && !word.ends_with("ss") {
        text.push(' ');
        text.push_str(&word[..len - 1]);
        *rest = &tail[len..];
    }
}

/// Fit Discord's field limits: at most `max` bytes (cut on a char boundary,
/// ending in an ellipsis) and at least 2 characters.
pub fn clamp(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut cut = max.saturating_sub('…'.len_utf8());
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push('…');
    }
    if s.chars().count() == 1 {
        s.push('\u{2800}');
    }
    s
}

#[derive(Serialize, Default)]
pub struct Activity {
    #[serde(rename = "type")]
    pub kind: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_display_type: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamps: Option<Timestamps>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assets: Option<Assets>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub buttons: Vec<Button>,
    pub instance: bool,
}

#[derive(Serialize)]
pub struct Timestamps {
    pub start: i64,
}

#[derive(Serialize, Default)]
pub struct Assets {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub large_image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub large_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub small_image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub small_text: Option<String>,
}

#[derive(Serialize)]
pub struct Button {
    pub label: String,
    pub url: String,
}

/// Non-empty field → `Some(clamped)`.
pub fn field(s: String, max: usize) -> Option<String> {
    if s.is_empty() { None } else { Some(clamp(s, max)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Vars {
        let mut v = Vars::default();
        v.set("project", "demo");
        v.set("tool", "Bash");
        v.set("file", "");
        v.set("tokens", "12.3k");
        v.set("streak", "0");
        v
    }

    #[test]
    fn renders_and_collapses() {
        let (s, ok) = render("{tool} · {file} · {tokens} tokens", &vars());
        assert_eq!(s, "Bash · 12.3k tokens");
        assert!(!ok);
        let (s, ok) = render("Working in {project}", &vars());
        assert_eq!(s, "Working in demo");
        assert!(ok);
        let (_, ok) = render("{streak} day streak", &vars());
        assert!(!ok);
        let (s, _) = render("unterminated {project", &vars());
        assert_eq!(s, "unterminated {project");
        let (s, ok) = render("{nope}x", &vars());
        assert_eq!(s, "");
        assert!(!ok);
        let (s, _) = render("{project} · {file} today · {streak} day streak", &vars());
        assert_eq!(s, "demo · 0 day streak");
    }

    #[test]
    fn singular_after_one() {
        let mut v = Vars::default();
        v.set("n", "1");
        v.set("m", "2");
        assert_eq!(render("{n} prompts · {m} prompts", &v).0, "1 prompt · 2 prompts");
        assert_eq!(render("{n} sessions, {n} day streak", &v).0, "1 session, 1 day streak");
        assert_eq!(render("{n} class · {n}s", &v).0, "1 class · 1s");
    }

    #[test]
    fn singular_edges() {
        let mut v = Vars::default();
        v.set("n", "1");
        v.set("k", "1.0k");
        assert_eq!(render("{n} prompts.", &v).0, "1 prompt.");
        assert_eq!(render("{n} prompts, 2 tools", &v).0, "1 prompt, 2 tools");
        assert_eq!(render("{n}", &v).0, "1");
        assert_eq!(render("{n} ", &v).0, "1");
        assert_eq!(render("{n} s", &v).0, "1 s");
        assert_eq!(render("{n}  prompts", &v).0, "1  prompts");
        assert_eq!(render("{n}prompts", &v).0, "1prompts");
        assert_eq!(render("{k} tokens", &v).0, "1.0k tokens");
        assert_eq!(render("{n} tokens·x", &v).0, "1 token · x");
        // Non-ASCII after the number: no slicing inside a multi-byte char.
        assert_eq!(render("{n} é", &v).0, "1 é");
        assert_eq!(render("{n} tökens", &v).0, "1 tökens");
        assert_eq!(render("{n} {n} prompts", &v).0, "1 1 prompt");
    }

    #[test]
    fn render_edges() {
        let v = vars();
        assert_eq!(render("", &v), (String::new(), true));
        assert_eq!(render("·", &v).0, "");
        assert_eq!(render(" · {project} · ", &v).0, "demo");
        assert_eq!(render("a··b", &v).0, "a · b");
        assert_eq!(render("→ {project} ←", &v).0, "→ demo ←");
        assert_eq!(render("a } b", &v).0, "a } b");
        assert_eq!(render("{}", &v), (String::new(), false));
        assert_eq!(render("{{project}}", &v).0, "");
        assert_eq!(render("é{", &v).0, "é{");
        // Values are inserted verbatim, never re-expanded or re-split.
        let mut v = Vars::default();
        v.set("project", "{tool} · x");
        v.set("tool", "Bash");
        assert_eq!(render("in {project}", &v).0, "in {tool} · x");
        // First definition wins.
        v.set("project", "other");
        assert_eq!(render("{project}", &v).0, "{tool} · x");
    }

    #[test]
    fn field_and_clamp_edges() {
        assert_eq!(field(String::new(), 128), None);
        assert_eq!(clamp(String::new(), 128), "");
        assert_eq!(clamp("ab".into(), 128), "ab");
        assert_eq!(clamp("é".into(), 128), "é\u{2800}");
        let exact = "x".repeat(128);
        assert_eq!(clamp(exact.clone(), 128), exact);
        let c = clamp("\u{1F600}".repeat(40), 128);
        assert!(c.len() <= 128 && c.ends_with('…'), "{c}");
        let c = clamp("x".repeat(129), 128);
        assert_eq!(c.len(), 128);
    }

    #[test]
    fn activity_json_skips_empty_fields() {
        let a = Activity { details: Some("d".into()), ..Activity::default() };
        assert_eq!(sonic_rs::to_string(&a).unwrap(), r#"{"type":0,"details":"d","instance":false}"#);
    }

    #[test]
    fn clamps() {
        assert_eq!(clamp("a".into(), 128), "a\u{2800}");
        let long = "é".repeat(100);
        let c = clamp(long, 128);
        assert!(c.len() <= 128);
        assert!(c.ends_with('…'));
    }
}
