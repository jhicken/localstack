//! `.env` parser that matches `op run --env-file` (1Password CLI 2.x), so one file works both for
//! `op run --env-file=.env -- <cmd>` in a terminal and for services imported into LStack.
//!
//! Rules (checked against `op` 2.34; see the golden tests below):
//! - `KEY=value`, `KEY = value`, `export KEY=value`; blank lines and `#` lines are skipped.
//! - Unquoted values end at `#` or end of line and are trimmed. Backslashes are literal.
//! - `"…"` may span lines; `\n`, `\"`, `\\` and `\$` are escapes, any other `\x` stays as is.
//! - `'…'` is literal (may span lines, no expansion).
//! - After a closing quote only spaces and an optional `# comment` may follow.
//! - `$NAME` / `${NAME}` (NAME = letters, digits, `_`) expand in unquoted and double-quoted values,
//!   from keys earlier in the file, then the given environment; unknown names become empty.
//! - A value that is entirely `op://…` (after expansion) is a 1Password secret reference.
//! - Duplicate keys: the last one wins.
//! - `lstack://NAME` (LStack's own syntax, ignored by `op run`) points at a secret stored in LStack.

/// One `KEY=value` from the file (after expansion), in file order, duplicates removed (last wins).
#[derive(Debug, Clone, PartialEq)]
pub struct Var {
    pub key: String,
    pub value: String,
    /// The value as written, before `$VAR` expansion (differs from `value` only when it had variables).
    pub template: String,
    pub line: usize,
}

impl Var {
    /// The 1Password secret reference, when the whole value is one.
    pub fn op_ref(&self) -> Option<&str> {
        is_op_ref(&self.value).then_some(self.value.as_str())
    }

    /// The LStack secret name, when the whole value is `lstack://NAME`.
    pub fn lstack_ref(&self) -> Option<&str> {
        lstack_ref(&self.value)
    }

    /// Neither kind of reference: passed to the service as written.
    pub fn is_plain(&self) -> bool {
        self.op_ref().is_none() && self.lstack_ref().is_none()
    }
}

pub const LSTACK_SCHEME: &str = "lstack://";

/// `lstack://NAME` → `NAME` (letters, digits and `_`, not starting with a digit).
pub fn lstack_ref(v: &str) -> Option<&str> {
    let n = v.strip_prefix(LSTACK_SCHEME)?;
    let mut cs = n.chars();
    let first = cs.next()?;
    ((first.is_ascii_alphabetic() || first == '_') && cs.all(|c| c.is_ascii_alphanumeric() || c == '_') && n.len() <= 128).then_some(n)
}

/// Vault, item and field names may contain spaces (`op://Home Automation/Mongo/password`), as `op run` allows.
/// Control and invisible/bidi characters aren't: they could make a prompt show a different vault or item.
pub fn is_op_ref(v: &str) -> bool {
    v.starts_with("op://") && v.len() > 5 && v.trim_end() == v && !has_hidden(v)
}

/// Control characters, and Unicode format characters that change how text looks without showing up
/// (zero-width, bidi overrides/isolates, soft hyphen, BOM).
pub fn has_hidden(s: &str) -> bool {
    s.chars().any(|c| {
        c.is_control()
            || matches!(c as u32,
                0x00AD | 0x061C | 0x115F | 0x1160 | 0x17B4 | 0x17B5 | 0x180B..=0x180F | 0x200B..=0x200F | 0x202A..=0x202E
                | 0x2060..=0x206F | 0x3164 | 0xFE00..=0xFE0F | 0xFEFF | 0xFFA0 | 0xFFF9..=0xFFFB | 0xE0000..=0xE007F)
    })
}

/// The parts of `op://vault/item[/section]/field[?query]`, for showing what a reference points at.
pub fn ref_parts(r: &str) -> Option<(String, String, String)> {
    let rest = r.strip_prefix("op://")?;
    let path = rest.split('?').next().unwrap_or(rest);
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() < 3 || parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    Some((parts[0].to_string(), parts[1].to_string(), parts[2..].join("/")))
}

/// `op://…?attribute=otp` changes every 30 s, so a cached copy would be useless.
pub fn is_totp_ref(r: &str) -> bool {
    r.split_once('?').is_some_and(|(_, q)| q.split('&').any(|kv| kv.eq_ignore_ascii_case("attribute=otp") || kv.eq_ignore_ascii_case("attribute=totp")))
}

/// `{{ op://… }}` is `op inject` template syntax; `op run` doesn't resolve it in env files.
pub fn has_inject_template(v: &str) -> bool {
    v.contains("{{") && v.contains("op://") && v.contains("}}")
}

struct P<'a> {
    c: Vec<char>,
    i: usize,
    line: usize,
    col: usize,
    lookup: &'a dyn Fn(&str) -> Option<String>,
}

fn name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

impl<'a> P<'a> {
    fn peek(&self) -> Option<char> {
        self.c.get(self.i).copied()
    }
    fn bump(&mut self) -> Option<char> {
        let ch = self.c.get(self.i).copied()?;
        self.i += 1;
        if ch == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(ch)
    }
    fn err(&self, msg: &str) -> String {
        format!("line {} column {}: {msg}", self.line, self.col)
    }
    fn skip_inline_space(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.bump();
        }
    }
    fn skip_to_eol(&mut self) {
        while let Some(c) = self.peek() {
            if c == '\n' {
                break;
            }
            self.bump();
        }
    }
}

/// Expand `$NAME` / `${NAME}` in `s` (used for unquoted values; double-quoted values expand inline).
fn expand(s: &str, vars: &[(String, String)], lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let c: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < c.len() {
        if c[i] == '$' {
            if let Some((v, used)) = var_at(&c[i + 1..], vars, lookup) {
                out.push_str(&v);
                i += 1 + used;
                continue;
            }
        }
        out.push(c[i]);
        i += 1;
    }
    out
}

/// After a `$`: `{NAME}` or `NAME`. Returns the value and how many chars were used.
fn var_at(c: &[char], vars: &[(String, String)], lookup: &dyn Fn(&str) -> Option<String>) -> Option<(String, usize)> {
    let get = |n: &str| vars.iter().rev().find(|(k, _)| k == n).map(|(_, v)| v.clone()).or_else(|| lookup(n)).unwrap_or_default();
    if c.first() == Some(&'{') {
        let end = c.iter().position(|&ch| ch == '}')?;
        let name: String = c[1..end].iter().collect();
        if name.is_empty() || !name.chars().all(name_char) {
            return None;
        }
        return Some((get(&name), end + 1));
    }
    let n = c.iter().take_while(|&&ch| name_char(ch)).count();
    if n == 0 {
        return None;
    }
    let name: String = c[..n].iter().collect();
    Some((get(&name), n))
}

/// Parse an env file. `lookup` supplies variables not defined earlier in the file.
pub fn parse(src: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<Vec<Var>, String> {
    parse_spans(src, lookup).map(|(v, _)| v)
}

/// Where one assignment sits in the (`\r\n`-normalized) text, in chars: from the start of its line
/// through its trailing newline. Duplicates each get one.
#[derive(Debug, Clone, PartialEq)]
struct Span {
    key: String,
    start: usize,
    end: usize,
    export: bool,
}

fn parse_spans(src: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<(Vec<Var>, Vec<Span>), String> {
    let src = src.replace("\r\n", "\n");
    let mut spans: Vec<Span> = Vec::new();
    let mut p = P { c: src.chars().collect(), i: 0, line: 1, col: 1, lookup };
    let mut vars: Vec<(String, String)> = Vec::new();
    let mut out: Vec<Var> = Vec::new();
    loop {
        // Skip whitespace and comment lines.
        while let Some(c) = p.peek() {
            if c.is_whitespace() {
                p.bump();
            } else if c == '#' {
                p.skip_to_eol();
            } else {
                break;
            }
        }
        if p.peek().is_none() {
            break;
        }
        let line = p.line;
        let start = p.c[..p.i].iter().rposition(|&c| c == '\n').map_or(0, |i| i + 1);
        let mut export = false;
        // Key, with an optional `export ` prefix.
        let mut key = String::new();
        while let Some(c) = p.peek() {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' {
                key.push(c);
                p.bump();
            } else {
                break;
            }
        }
        if key == "export" && matches!(p.peek(), Some(' ' | '\t')) {
            p.skip_inline_space();
            export = true;
            key.clear();
            while let Some(c) = p.peek() {
                if c.is_ascii_alphanumeric() || c == '_' || c == '.' {
                    key.push(c);
                    p.bump();
                } else {
                    break;
                }
            }
        }
        if key.is_empty() {
            return Err(p.err(&format!("unexpected character {:?}", p.peek().unwrap_or(' '))));
        }
        p.skip_inline_space();
        if p.peek() != Some('=') {
            return Err(p.err("missing '='"));
        }
        p.bump();
        p.skip_inline_space();
        let (value, template) = match p.peek() {
            Some('\'') => {
                p.bump();
                let mut v = String::new();
                loop {
                    match p.bump() {
                        None => return Err(p.err("unterminated single-quoted value")),
                        Some('\'') => break,
                        Some(c) => v.push(c),
                    }
                }
                after_quote(&mut p)?;
                (v.clone(), v)
            }
            Some('"') => {
                p.bump();
                let (mut v, mut t) = (String::new(), String::new());
                loop {
                    match p.bump() {
                        None => return Err(p.err("unterminated double-quoted value")),
                        Some('"') => break,
                        Some('\\') => {
                            let n = p.bump().ok_or_else(|| p.err("unterminated double-quoted value"))?;
                            t.push('\\');
                            t.push(n);
                            match n {
                                'n' => v.push('\n'),
                                '"' | '\\' | '$' => v.push(n),
                                _ => {
                                    v.push('\\');
                                    v.push(n);
                                }
                            }
                        }
                        Some('$') => {
                            t.push('$');
                            let rest: Vec<char> = p.c[p.i..].iter().take_while(|&&c| c != '"' && c != '\n').copied().collect();
                            match var_at(&rest, &vars, p.lookup) {
                                Some((val, used)) => {
                                    v.push_str(&val);
                                    for _ in 0..used {
                                        if let Some(c) = p.bump() {
                                            t.push(c);
                                        }
                                    }
                                }
                                None => v.push('$'),
                            }
                        }
                        Some(c) => {
                            v.push(c);
                            t.push(c);
                        }
                    }
                }
                after_quote(&mut p)?;
                (v, t)
            }
            _ => {
                let mut raw = String::new();
                while let Some(c) = p.peek() {
                    if c == '\n' || c == '#' {
                        break;
                    }
                    raw.push(c);
                    p.bump();
                }
                p.skip_to_eol();
                let t = raw.trim().to_string();
                (expand(&t, &vars, p.lookup), t)
            }
        };
        vars.push((key.clone(), value.clone()));
        let end = if p.peek() == Some('\n') { p.i + 1 } else { p.i };
        spans.push(Span { key: key.clone(), start, end, export });
        out.retain(|v: &Var| v.key != key);
        out.push(Var { key, value, template, line });
    }
    Ok((out, spans))
}

/// How `value` is written so `parse` (and `op run`) read it back unchanged: bare when it's simple,
/// else single-quoted (literal), else double-quoted with escapes.
pub fn format_value(value: &str) -> String {
    let bare = |c: char| c.is_ascii_alphanumeric() || "_-./:@,+%=~^!?*&<>|[]{}()".contains(c);
    if !value.is_empty() && value.chars().all(bare) {
        return value.to_string();
    }
    if !value.contains('\'') && !value.contains('\n') {
        return format!("'{value}'");
    }
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '$' => out.push_str("\\$"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `src` with `key` set to `value` (the last assignment rewritten in place, earlier duplicates
/// dropped; appended when absent), or with every assignment of `key` removed (`value` = None).
/// Comments, order and other lines stay as they are.
pub fn edit(src: &str, key: &str, value: Option<&str>) -> Result<String, String> {
    let crlf = src.contains("\r\n");
    let norm = src.replace("\r\n", "\n");
    let (_, spans) = parse_spans(&norm, &|_| None)?;
    let chars: Vec<char> = norm.chars().collect();
    let mine: Vec<&Span> = spans.iter().filter(|s| s.key == key).collect();
    let line = |export: bool| value.map(|v| format!("{}{key}={}\n", if export { "export " } else { "" }, format_value(v)));
    let mut out = String::new();
    let mut i = 0;
    for (n, s) in mine.iter().enumerate() {
        out.extend(&chars[i..s.start]);
        if n + 1 == mine.len() {
            if let Some(l) = line(s.export) {
                out.push_str(&l);
            }
        }
        i = s.end;
    }
    out.extend(&chars[i..]);
    if mine.is_empty() {
        if let Some(l) = line(false) {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&l);
        }
    }
    Ok(if crlf { out.replace('\n', "\r\n") } else { out })
}

fn after_quote(p: &mut P) -> Result<(), String> {
    p.skip_inline_space();
    match p.peek() {
        None | Some('\n') => Ok(()),
        Some('#') => {
            p.skip_to_eol();
            Ok(())
        }
        Some(c) => Err(p.err(&format!("unexpected character {c:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(n: &str) -> Option<String> {
        match n {
            "APP" => Some("zz".into()),
            "HOME" => Some("/Users/x".into()),
            _ => None,
        }
    }

    fn get(vars: &[Var], k: &str) -> String {
        vars.iter().find(|v| v.key == k).unwrap_or_else(|| panic!("missing {k}")).value.clone()
    }

    // Golden results captured from `APP=zz op run --env-file=<file> -- env` (op 2.34.1).
    #[test]
    fn matches_op_run_basics() {
        let src = "# comment\nA=plain\nB = spaced\nexport C=exported\nD=\"double quoted # not comment\"\nE='single $HOME'\nF=unquoted # trailing comment\nG=\"multi\nline\"\nH=${APP}x\nI=\"esc\\nnl\"\nA=dup\nJ=\n";
        let v = parse(src, &env).unwrap();
        assert_eq!(get(&v, "A"), "dup");
        assert_eq!(get(&v, "B"), "spaced");
        assert_eq!(get(&v, "C"), "exported");
        assert_eq!(get(&v, "D"), "double quoted # not comment");
        assert_eq!(get(&v, "E"), "single $HOME");
        assert_eq!(get(&v, "F"), "unquoted");
        assert_eq!(get(&v, "G"), "multi\nline");
        assert_eq!(get(&v, "H"), "zzx");
        assert_eq!(get(&v, "I"), "esc\nnl");
        assert_eq!(get(&v, "J"), "");
        assert_eq!(v.iter().filter(|x| x.key == "A").count(), 1);
    }

    #[test]
    fn matches_op_run_expansion_and_escapes() {
        let src = "L1=$APP\nL2=\"d$APP\"\nL3=\"d${APP}\"\nL4=${A0}\nL5=$MISSING.y\nL6=a#b\nL7=  x y  \nL9=a\\nb\nL10=\"tab\\tq\\\"x\\\\y\"\nA0=fromfile\nL11=${A0}\nL12=`bt`\n";
        let v = parse(src, &env).unwrap();
        assert_eq!(get(&v, "L1"), "zz");
        assert_eq!(get(&v, "L2"), "dzz");
        assert_eq!(get(&v, "L3"), "dzz");
        assert_eq!(get(&v, "L4"), ""); // defined later in the file: not visible yet
        assert_eq!(get(&v, "L5"), ".y");
        assert_eq!(get(&v, "L6"), "a");
        assert_eq!(get(&v, "L7"), "x y");
        assert_eq!(get(&v, "L9"), "a\\nb");
        assert_eq!(get(&v, "L10"), "tab\\tq\"x\\y");
        assert_eq!(get(&v, "L11"), "fromfile");
        assert_eq!(get(&v, "L12"), "`bt`");
    }

    #[test]
    fn matches_op_run_edge_cases() {
        let src = "M1='multi\nline'\nM2=$$x\nM3=a$1b\nM4=\"\\$APP\"\nM5=\"\"\n  M6=lead\nM7=crlf\r\nM10=a=b=c\nM11=\"x\" # c\n";
        let v = parse(src, &env).unwrap();
        assert_eq!(get(&v, "M1"), "multi\nline");
        assert_eq!(get(&v, "M2"), "$");
        assert_eq!(get(&v, "M3"), "a");
        assert_eq!(get(&v, "M4"), "$APP");
        assert_eq!(get(&v, "M5"), "");
        assert_eq!(get(&v, "M6"), "lead");
        assert_eq!(get(&v, "M7"), "crlf");
        assert_eq!(get(&v, "M10"), "a=b=c");
        assert_eq!(get(&v, "M11"), "x");
    }

    #[test]
    fn lstack_references() {
        let v = parse("A=lstack://GH_TOKEN\nB=lstack://bad-name\nC=\"lstack://X1\"\nD=op://a/b/c\n", &env).unwrap();
        let g = |k: &str| v.iter().find(|x| x.key == k).unwrap().clone();
        assert_eq!(g("A").lstack_ref(), Some("GH_TOKEN"));
        assert!(g("B").lstack_ref().is_none() && g("B").is_plain());
        assert_eq!(g("C").lstack_ref(), Some("X1"));
        assert!(!g("D").is_plain() && g("D").lstack_ref().is_none());
        assert!(lstack_ref("lstack://9X").is_none() && lstack_ref("lstack://").is_none());
    }

    #[test]
    fn edits_round_trip() {
        let src = "# top\nA=1\nexport B=\"two\nlines\" # c\n\n# keep\nA=dup\nC='x'\n";
        let e = edit(src, "A", Some("new value")).unwrap();
        assert_eq!(e, "# top\nexport B=\"two\nlines\" # c\n\n# keep\nA='new value'\nC='x'\n");
        let e = edit(&e, "B", Some("it's $HOME\nx")).unwrap();
        assert!(e.contains("export B=\"it's \\$HOME\\nx\"\n"));
        assert_eq!(get(&parse(&e, &env).unwrap(), "B"), "it's $HOME\nx");
        let e = edit(&e, "D", Some("lstack://D")).unwrap();
        assert!(e.ends_with("C='x'\nD=lstack://D\n"));
        let e = edit(&e, "B", None).unwrap();
        assert_eq!(e, "# top\n\n# keep\nA='new value'\nC='x'\nD=lstack://D\n");
        assert_eq!(edit("X=1", "Y", Some("")).unwrap(), "X=1\nY=''\n");
        assert_eq!(edit("X=1\r\nY=2\r\n", "X", Some("3")).unwrap(), "X=3\r\nY=2\r\n");
        assert_eq!(edit("", "K", Some("v")).unwrap(), "K=v\n");
        for v in ["plain", "with space", "q'uote", "back\\slash $X \"d\"", "", "a#b", "op://Home Vault/x/y"] {
            let e = edit("", "K", Some(v)).unwrap();
            assert_eq!(get(&parse(&e, &env).unwrap(), "K"), v, "{e}");
        }
    }

    #[test]
    fn errors_like_op_run() {
        assert!(parse("K\n", &env).unwrap_err().contains("missing '='"));
        assert!(parse("L8=\"q\" trailing\n", &env).unwrap_err().contains("unexpected character"));
        assert!(parse("X=\"open\n", &env).is_err());
    }

    #[test]
    fn secret_references() {
        let src = "DB=op://$APP/db/password\nTOK=\"op://Dev/GitHub/token\"\nPORT=3000\nTPL=\"{{ op://Dev/x/y }}\"\nOTP=op://Dev/x/one-time password?attribute=otp\n";
        let v = parse(src, &env).unwrap();
        let db = v.iter().find(|x| x.key == "DB").unwrap();
        assert_eq!(db.op_ref(), Some("op://zz/db/password"));
        assert_eq!(db.template, "op://$APP/db/password");
        assert_eq!(v.iter().find(|x| x.key == "TOK").unwrap().op_ref(), Some("op://Dev/GitHub/token"));
        assert_eq!(v.iter().find(|x| x.key == "PORT").unwrap().op_ref(), None);
        let tpl = v.iter().find(|x| x.key == "TPL").unwrap();
        assert!(tpl.op_ref().is_none() && has_inject_template(&tpl.value));
        assert_eq!(ref_parts("op://Dev/GitHub/sec/token?x=1"), Some(("Dev".into(), "GitHub".into(), "sec/token".into())));
        assert!(ref_parts("op://Dev/x").is_none());
        let v = parse("A=op://Home Automation/Mongo String/password\nB=\"op://Home Automation/x/y\"\n", &env).unwrap();
        assert_eq!(v.iter().find(|x| x.key == "A").unwrap().op_ref(), Some("op://Home Automation/Mongo String/password"));
        assert_eq!(v.iter().find(|x| x.key == "B").unwrap().op_ref(), Some("op://Home Automation/x/y"));
        assert_eq!(ref_parts("op://Home Automation/Mongo String/password"), Some(("Home Automation".into(), "Mongo String".into(), "password".into())));
        assert!(!is_op_ref("op://a/b/c\td"));
        assert!(!is_op_ref("op://Dev\u{202E}etavirP/b/c"));
        assert!(!is_op_ref("op://Dev/b\u{200B}/c"));
        assert!(is_totp_ref("op://Dev/x/otp?attribute=otp"));
        assert!(!is_totp_ref("op://Dev/x/password"));
    }
}
