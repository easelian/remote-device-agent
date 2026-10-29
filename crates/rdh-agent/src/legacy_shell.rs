//! Decode the literal shell wrapper emitted by legacy POSIX installers.
//! Parsing never invokes a shell or performs expansions.
use std::path::Path;

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

// Decode exactly the single-quoted words emitted by the old installer.
// Reconstructing the original spelling keeps shell expansions, operators,
// comments, and arbitrary unquoted fragments out of the accepted format.
fn literal_word(input: &str) -> Option<(String, &str)> {
    let mut rest = input.strip_prefix('\'')?;
    let mut word = String::new();
    loop {
        let end = rest.find('\'')?;
        word.push_str(&rest[..end]);
        rest = &rest[end + 1..];
        if let Some(next) = rest.strip_prefix("\\''") {
            word.push('\'');
            rest = next;
        } else {
            break;
        }
    }
    if word.chars().any(char::is_control) || shell_quote(&word) != input[..input.len() - rest.len()]
    {
        return None;
    }
    Some((word, rest))
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct WrapperWords {
    pub words: Vec<String>,
    pub environment: Vec<(String, String)>,
    pub working_directory: String,
}

pub(crate) fn wrapper_words(bytes: &[u8], installer: bool, config: &Path) -> Option<WrapperWords> {
    let script = std::str::from_utf8(bytes).ok()?.strip_suffix('\n')?;
    let mut lines = script.split('\n');
    if lines.next()? != "#!/bin/sh" {
        return None;
    }
    let mut line = lines.next()?;
    let mut seen = [false; 2];
    let mut environment = Vec::new();
    while let Some(export) = line.strip_prefix("export ") {
        if !installer {
            return None;
        }
        let (name, value) = export.split_once('=')?;
        let index = match name {
            "XDG_CONFIG_HOME" => 0,
            "XDG_STATE_HOME" => 1,
            _ => return None,
        };
        let (value, rest) = literal_word(value)?;
        if seen[index] || !rest.is_empty() {
            return None;
        }
        seen[index] = true;
        environment.push((name.to_owned(), value));
        line = lines.next()?;
    }
    let (directory, trailing) = literal_word(line.strip_prefix("cd ")?)?;
    if trailing != " || exit 1"
        || !Path::new(&directory).is_absolute()
        || (!installer && Some(Path::new(&directory)) != config.parent())
    {
        return None;
    }
    let mut rest = lines.next()?.strip_prefix("exec ")?;
    if lines.next().is_some() {
        return None;
    }
    let mut words = Vec::new();
    let mut installer_tail = false;
    loop {
        if installer && rest == "--recover-lock" {
            words.push(rest.to_owned());
            installer_tail = true;
            break;
        }
        let (word, remaining) = literal_word(rest)?;
        words.push(word);
        if words.len() > 256 {
            return None;
        }
        if remaining.is_empty() {
            break;
        }
        rest = remaining.strip_prefix(' ')?;
    }
    if words.is_empty() || (installer && !installer_tail) {
        return None;
    }
    Some(WrapperWords {
        words,
        environment,
        working_directory: directory,
    })
}
