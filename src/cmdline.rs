//! Shell-command helpers: the user's login shell, the files sourced before a script runs, and
//! splitting `cd dir && FOO=1 source .env && npm run dev` into cwd / env / source / command.

use crate::paths;
use std::collections::BTreeMap;
use std::path::Path;

/// The user's login shell: `$SHELL`, else the passwd entry, else zsh (the macOS default).
pub fn user_shell() -> String {
    if let Ok(s) = std::env::var("SHELL") {
        if !s.is_empty() {
            return s;
        }
    }
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if !pw.is_null() && !(*pw).pw_shell.is_null() {
            let s = std::ffi::CStr::from_ptr((*pw).pw_shell).to_string_lossy().into_owned();
            if !s.is_empty() {
                return s;
            }
        }
    }
    "/bin/zsh".into()
}

fn shell_name(shell: &str) -> &str {
    Path::new(shell).file_name().and_then(|n| n.to_str()).unwrap_or("zsh")
}

/// Interactive config of a shell (what Terminal loads), used when a service doesn't set `source:`.
pub fn default_source(shell: &str) -> Vec<String> {
    match shell_name(shell) {
        "zsh" => vec!["~/.zshrc".into()],
        "bash" => vec!["~/.bashrc".into()],
        "fish" => vec!["~/.config/fish/config.fish".into()],
        _ => vec![],
    }
}

fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Arguments for `<shell> …` that run `command` the way a Terminal tab would: a login shell
/// (so ~/.zprofile, Homebrew's shellenv, /etc/paths apply) that first sources `source` files
/// (default: the shell's rc file). The command itself is passed via `LSTACK_COMMAND` and `eval`ed
/// so aliases and functions from those files work.
pub fn shell_args(shell: &str, source: Option<&[String]>) -> Vec<String> {
    let explicit = source.is_some();
    let files = source.map(|s| s.to_vec()).unwrap_or_else(|| default_source(shell));
    let fish = shell_name(shell) == "fish";
    let mut script = String::new();
    for f in &files {
        let f = paths::expand_home(f.trim());
        if f.is_empty() {
            continue;
        }
        let p = if f.contains('/') { f } else { format!("./{f}") };
        let q = sq(&p);
        if fish {
            script.push_str(&format!("test -r {q}; and source {q} </dev/null >/dev/null\n"));
        } else if explicit {
            script.push_str(&format!(
                "if [ -r {q} ]; then . {q} </dev/null >/dev/null; else echo \"lstack: cannot read {}\" >&2; fi\n",
                p.replace(['"', '$', '`', '\\'], "")
            ));
        } else {
            script.push_str(&format!("[ -r {q} ] && . {q} </dev/null >/dev/null\n"));
        }
    }
    if fish {
        script.push_str("set -l __lstack_cmd $LSTACK_COMMAND; set -e LSTACK_COMMAND; eval $__lstack_cmd");
    } else {
        if shell_name(shell) == "bash" {
            script.push_str("shopt -s expand_aliases\n");
        }
        script.push_str("__lstack_cmd=$LSTACK_COMMAND; unset LSTACK_COMMAND; eval \"$__lstack_cmd\"");
    }
    vec!["-l".into(), "-c".into(), script]
}

// ------------------------------------------------------------------ splitting

#[derive(Debug, Default, PartialEq, Clone, serde::Serialize)]
pub struct Split {
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    pub source: Vec<String>,
}

impl Split {
    pub fn changed(&self) -> bool {
        self.cwd.is_some() || !self.env.is_empty() || !self.source.is_empty()
    }
}

#[derive(Debug)]
enum Tok {
    /// A word: unquoted value, byte span in the input, and whether it needs runtime expansion.
    Word { val: String, start: usize, dynamic: bool },
    /// `&&`, `;` or a newline: a safe boundary between leading setup steps and the command.
    Sep,
    /// Anything we don't model (`|`, `||`, `&`, redirects, subshells…): stop splitting here.
    Other { start: usize },
}

fn tokenize(s: &str) -> Vec<Tok> {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' => i += 1,
            b'\n' | b';' => {
                out.push(Tok::Sep);
                i += 1;
            }
            b'&' if b.get(i + 1) == Some(&b'&') => {
                out.push(Tok::Sep);
                i += 2;
            }
            b'#' => {
                out.push(Tok::Other { start: i });
                return out;
            }
            b'|' | b'&' | b'<' | b'>' | b'(' | b')' | b'{' | b'}' => {
                out.push(Tok::Other { start: i });
                return out;
            }
            _ => {
                let start = i;
                let mut val = String::new();
                let mut dynamic = false;
                while i < b.len() {
                    let c = b[i];
                    match c {
                        b' ' | b'\t' | b'\n' | b';' | b'&' | b'|' | b'<' | b'>' | b'(' | b')' => break,
                        b'\'' => {
                            let Some(end) = s[i + 1..].find('\'') else {
                                return vec![Tok::Other { start }];
                            };
                            val.push_str(&s[i + 1..i + 1 + end]);
                            i += end + 2;
                        }
                        b'"' => {
                            i += 1;
                            loop {
                                let Some(&c) = b.get(i) else {
                                    return vec![Tok::Other { start }];
                                };
                                match c {
                                    b'"' => {
                                        i += 1;
                                        break;
                                    }
                                    b'\\' if matches!(b.get(i + 1), Some(b'"' | b'\\' | b'$' | b'`')) => {
                                        val.push(b[i + 1] as char);
                                        i += 2;
                                    }
                                    b'$' | b'`' => {
                                        dynamic = true;
                                        val.push(c as char);
                                        i += 1;
                                    }
                                    _ => {
                                        let ch = s[i..].chars().next().unwrap();
                                        val.push(ch);
                                        i += ch.len_utf8();
                                    }
                                }
                            }
                        }
                        b'\\' => {
                            if let Some(ch) = s[i + 1..].chars().next() {
                                val.push(ch);
                                i += 1 + ch.len_utf8();
                            } else {
                                i += 1;
                            }
                        }
                        b'$' | b'`' | b'*' | b'?' | b'[' | b'{' | b'}' => {
                            dynamic = true;
                            val.push(c as char);
                            i += 1;
                        }
                        _ => {
                            let ch = s[i..].chars().next().unwrap();
                            val.push(ch);
                            i += ch.len_utf8();
                        }
                    }
                }
                out.push(Tok::Word { val, start, dynamic });
            }
        }
    }
    out
}

fn assignment(val: &str) -> Option<(String, String)> {
    let (k, v) = val.split_once('=')?;
    let ok = !k.is_empty() && !k.starts_with(|c: char| c.is_ascii_digit()) && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    ok.then(|| (k.to_string(), v.to_string()))
}

fn resolve_dir(dir: &str, base: &str) -> String {
    let d = paths::expand_home(dir);
    let p = if d.starts_with('/') { std::path::PathBuf::from(d) } else { Path::new(base).join(d) };
    // Normalize `.`/`..` lexically (the directory may not exist on this machine yet).
    let mut parts: Vec<std::ffi::OsString> = vec![];
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                parts.pop();
            }
            std::path::Component::Normal(x) => parts.push(x.to_os_string()),
            _ => {}
        }
    }
    let mut out = std::path::PathBuf::from("/");
    out.extend(parts);
    out.to_string_lossy().into_owned()
}

/// Peel leading setup steps off a command: `cd <dir>`, `export A=1`, `A=1 B=2` (alone or as a
/// prefix of the real command), and `source <file>` / `. <file>`. Stops at the first thing it can't
/// represent faithfully (expansions, pipes, subshells…), leaving the rest of the command untouched.
/// `base` is the directory relative `cd` paths resolve against.
pub fn split(command: &str, base: &str) -> Split {
    let command = command.trim();
    let toks = tokenize(command);
    let mut out = Split { command: command.to_string(), ..Default::default() };
    let mut cwd = base.to_string();
    let mut i = 0;
    loop {
        while matches!(toks.get(i), Some(Tok::Sep)) {
            i += 1;
        }
        let seg_start = i;
        let mut words: Vec<(&str, usize, bool)> = vec![];
        while let Some(Tok::Word { val, start, dynamic }) = toks.get(i) {
            words.push((val, *start, *dynamic));
            i += 1;
        }
        let ends_clean = matches!(toks.get(i), Some(Tok::Sep));
        let rest_start = match toks.get(seg_start) {
            Some(Tok::Word { start, .. }) | Some(Tok::Other { start }) => *start,
            _ => command.len(),
        };
        let stop = |out: &mut Split, at: usize| out.command = command[at..].trim().to_string();
        if words.is_empty() {
            stop(&mut out, rest_start);
            break;
        }
        // Leading assignments: `A=1 B=2 npm run dev`, `A=1 source x`, or a segment of only assignments.
        let n = words.iter().take_while(|w| !w.2 && assignment(w.0).is_some()).count();
        if n > 0 && n == words.len() && !ends_clean {
            stop(&mut out, rest_start);
            break;
        }
        out.env.extend(words[..n].iter().filter_map(|w| assignment(w.0)));
        let rest = &words[n..];
        if rest.is_empty() {
            continue;
        }
        let w0 = rest[0].0;
        let all_static = rest.iter().all(|w| !w.2);
        if ends_clean && all_static && rest.len() == 2 && w0 == "cd" && rest[1].0 != "-" {
            cwd = resolve_dir(rest[1].0, &cwd);
            out.cwd = Some(cwd.clone());
            continue;
        }
        if ends_clean && all_static && rest.len() == 2 && (w0 == "source" || w0 == ".") {
            let f = rest[1].0;
            let f = if f.starts_with('/') || f.starts_with('~') { f.to_string() } else { resolve_dir(f, &cwd) };
            out.source.push(f);
            continue;
        }
        if ends_clean && all_static && w0 == "export" && rest.len() > 1 && rest[1..].iter().all(|w| assignment(w.0).is_some()) {
            out.env.extend(rest[1..].iter().filter_map(|w| assignment(w.0)));
            continue;
        }
        stop(&mut out, rest[0].1);
        break;
    }
    if out.command.is_empty() {
        return Split { command: command.to_string(), ..Default::default() };
    }
    out
}

/// Quote argv words back into one shell command line (for `lstack add name -- cmd args…`).
pub fn join_argv(argv: &[String]) -> String {
    if argv.len() == 1 {
        return argv[0].clone();
    }
    argv.iter()
        .map(|a| {
            let plain = !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,~^".contains(c));
            if plain { a.clone() } else { sq(a) }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(p: &[(&str, &str)]) -> BTreeMap<String, String> {
        p.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn splits_cd_env_source() {
        let s = split("cd ~/code/api && export PORT=3000 NODE_ENV=dev && source .venv/bin/activate && DEBUG='a b' npm run dev -- --x", "/tmp");
        let home = paths::user_home().to_string_lossy().into_owned();
        assert_eq!(s.cwd.as_deref(), Some(format!("{home}/code/api").as_str()));
        assert_eq!(s.env, env(&[("PORT", "3000"), ("NODE_ENV", "dev"), ("DEBUG", "a b")]));
        assert_eq!(s.source, vec![format!("{home}/code/api/.venv/bin/activate")]);
        assert_eq!(s.command, "npm run dev -- --x");
    }

    #[test]
    fn relative_cd_and_semicolons() {
        let s = split("cd ../web; cd src ; bun run dev", "/a/b");
        assert_eq!(s.cwd.as_deref(), Some("/a/web/src"));
        assert_eq!(s.command, "bun run dev");
        let s = split("PORT=3000 source .env && bun dev", "/p");
        assert_eq!((s.env.len(), s.source.clone(), s.command.as_str()), (1, vec!["/p/.env".to_string()], "bun dev"));
    }

    #[test]
    fn stops_at_things_it_cannot_represent() {
        assert!(!split("npm run dev", "/").changed());
        let s = split("FOO=$HOME/x npm start", "/");
        assert!(!s.changed());
        assert_eq!(s.command, "FOO=$HOME/x npm start");
        let s = split("cd api && npm i | tee log && npm start", "/");
        assert_eq!(s.cwd.as_deref(), Some("/api"));
        assert_eq!(s.command, "npm i | tee log && npm start");
        assert!(!split("(cd x && make)", "/").changed());
        assert!(!split("cd \"$DIR\" && make", "/").changed());
        assert_eq!(split("cd x &&", "/").command, "cd x &&");
        assert_eq!(split("A=1 B=2", "/").command, "A=1 B=2");
    }

    #[test]
    fn shell_args_default_and_explicit() {
        let a = shell_args("/bin/zsh", None);
        assert_eq!(a[0], "-l");
        assert!(a[2].contains(".zshrc"));
        assert!(a[2].ends_with("eval \"$__lstack_cmd\""));
        let a = shell_args("/bin/zsh", Some(&[]));
        assert!(!a[2].contains(".zshrc"));
        let a = shell_args("/bin/bash", Some(&["env.sh".into()]));
        assert!(a[2].contains("'./env.sh'") && a[2].contains("cannot read") && a[2].contains("expand_aliases"));
    }

    #[test]
    fn joins_argv() {
        assert_eq!(join_argv(&["cd x && npm start".into()]), "cd x && npm start");
        assert_eq!(join_argv(&["sh".into(), "-c".into(), "echo hi".into()]), "sh -c 'echo hi'");
    }
}
