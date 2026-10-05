//! A line-at-a-time TOML highlighter for the config tab. Not a parser: it
//! only needs to color `config.toml`-shaped text (tables, `key = value`,
//! comments) and leaves anything else plain.

use super::theme::Role;

/// `line` split into pieces, each with the role to draw it in (`None`:
/// default text). The pieces concatenate back to `line`.
pub fn line(line: &str) -> Vec<(Option<Role>, &str)> {
    let body = line.trim_start();
    let indent = &line[..line.len() - body.len()];
    let mut out = Vec::new();
    if !indent.is_empty() {
        out.push((None, indent));
    }
    if body.is_empty() {
        return out;
    }
    if body.starts_with('#') {
        out.push((Some(Role::Muted), body));
        return out;
    }
    if body.starts_with('[') {
        let end = comment_start(body).unwrap_or(body.len());
        out.push((Some(Role::Accent), &body[..end]));
        if end < body.len() {
            out.push((Some(Role::Muted), &body[end..]));
        }
        return out;
    }
    let Some(eq) = body.find('=') else {
        out.push((None, body));
        return out;
    };
    out.push((Some(Role::Info), &body[..eq]));
    out.push((None, "="));
    let rest = &body[eq + 1..];
    let value_start = rest.len() - rest.trim_start().len();
    if value_start > 0 {
        out.push((None, &rest[..value_start]));
    }
    let value = &rest[value_start..];
    let end = comment_start(value).unwrap_or(value.len());
    let (v, comment) = value.split_at(end);
    let v_trim = v.trim_end();
    if !v_trim.is_empty() {
        let role = if v_trim.starts_with(['"', '\'']) {
            Role::Str
        } else if v_trim.starts_with(['[', '{']) {
            // Arrays and inline tables: leave their insides plain.
            return push_rest(out, v, comment, None);
        } else {
            Role::Num
        };
        return push_rest(out, v, comment, Some(role));
    }
    push_rest(out, v, comment, None)
}

fn push_rest<'a>(
    mut out: Vec<(Option<Role>, &'a str)>,
    v: &'a str,
    comment: &'a str,
    role: Option<Role>,
) -> Vec<(Option<Role>, &'a str)> {
    if !v.is_empty() {
        out.push((role, v));
    }
    if !comment.is_empty() {
        out.push((Some(Role::Muted), comment));
    }
    out
}

/// Byte index of a `#` that starts a comment (outside a string) in `s`.
fn comment_start(s: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        match quote {
            Some(q) => {
                if escaped {
                    escaped = false;
                } else if c == '\\' && q == '"' {
                    escaped = true;
                } else if c == q {
                    quote = None;
                }
            }
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '#' => return Some(i),
            None => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(l: &str) -> String {
        line(l).iter().map(|(_, s)| *s).collect()
    }

    #[test]
    fn pieces_concatenate_back() {
        for l in [
            "",
            "   ",
            "# comment",
            "[status.idle]",
            "[status.idle] # t",
            "key = \"v # not a comment\" # real",
            "  n=5",
            "list = [1, 2] # x",
            "garbage",
            "k = 'a\\' # c",
            "key = ",
        ] {
            assert_eq!(joined(l), l);
        }
    }

    #[test]
    fn roles() {
        assert_eq!(line("# c"), [(Some(Role::Muted), "# c")]);
        assert_eq!(line("[a.b]"), [(Some(Role::Accent), "[a.b]")]);
        assert_eq!(
            line("k = \"x#y\" # c"),
            [
                (Some(Role::Info), "k "),
                (None, "="),
                (None, " "),
                (Some(Role::Str), "\"x#y\" "),
                (Some(Role::Muted), "# c")
            ]
        );
        assert_eq!(line("n = 30")[3], (Some(Role::Num), "30"));
        assert_eq!(line("b = true")[3], (Some(Role::Num), "true"));
        assert_eq!(line("a = [1]")[3], (None, "[1]"));
        assert_eq!(line("plain"), [(None, "plain")]);
    }
}
