//! Canonical, configuration-free formatting for Oreslang.
//!
//! The formatter deliberately has no style options. `format_source` is the SDK
//! entry point used by the CLI, editors, build tools, and language servers.

use std::fmt;

const INDENT: &str = "  ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatError {
    line: usize,
    message: String,
}

impl FormatError {
    fn new(line: usize, message: impl Into<String>) -> Self {
        Self {
            line,
            message: message.into(),
        }
    }

    pub fn line(&self) -> usize {
        self.line
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for FormatError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DefineKind {
    Module,
    Class,
    Interface,
    Trait,
    Struct,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BraceKind {
    Callable,
    Module,
    Class,
    Interface,
    Trait,
    Struct,
    Actor,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BraceFrame {
    kind: BraceKind,
    indent: usize,
    order: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DefineFrame {
    kind: DefineKind,
    order: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrevEvent {
    Other,
    CallableEnd(usize),
}

#[derive(Debug, Default, Clone, Copy)]
struct LexState {
    block_comment: bool,
    quote: Option<char>,
    escape: bool,
}

/// Format Oreslang source using the one canonical style.
///
/// The current formatter is intentionally configuration-free:
///
/// - two-space indentation;
/// - LF line endings and exactly one final newline;
/// - trailing whitespace removed;
/// - at most one ordinary blank line;
/// - exactly two blank lines between sibling executable callable bodies;
/// - executable callable return separators are canonicalized to `->`;
/// - interface/trait callable signatures are canonicalized to `=>`;
/// - `end`, `fi`, `done`, braces, class/interface/trait/struct/module nesting,
///   actor bodies, and strings/comments are indentation-aware.
///
/// It does not reorder declarations or rewrite tokens whose meaning depends on
/// the compiler's evolving grammar.
pub fn format_source(source: &str) -> Result<String, FormatError> {
    let normalized = source.replace("\r\n", "\n").replace('\r', "\n");
    let mut out: Vec<String> = Vec::new();
    let mut indent = 0usize;
    let mut pending_blank = false;
    let mut prev_event = PrevEvent::Other;
    let mut define_stack: Vec<DefineFrame> = Vec::new();
    let mut brace_stack: Vec<BraceFrame> = Vec::new();
    let mut lex = LexState::default();
    let mut next_order = 0usize;

    for (line_index, raw_line) in normalized.lines().enumerate() {
        let line_no = line_index + 1;
        let trimmed_end = raw_line.trim_end();
        if trimmed_end.trim().is_empty() {
            pending_blank = true;
            continue;
        }

        let mut content = trimmed_end.trim_start().to_string();
        let starts_end = starts_word(&content, "end");
        let starts_fi = starts_word(&content, "fi");
        let starts_done = starts_word(&content, "done");
        let branch_line = starts_word(&content, "else") || starts_word(&content, "elseif");

        let leading_closing_braces = leading_closing_braces(&content);
        let keyword_dedent = usize::from(starts_end || starts_fi || starts_done || branch_line);
        let pre_dedent = leading_closing_braces + keyword_dedent;
        indent = indent.saturating_sub(pre_dedent);

        if starts_end {
            define_stack.pop().ok_or_else(|| {
                FormatError::new(line_no, "encountered `end` without a matching define block")
            })?;
        }

        let signature_context = nearest_signature_context(&define_stack, &brace_stack);
        let callable_body = is_callable_body_header(&content, signature_context);
        let callable_signature = is_callable_signature(&content, signature_context);
        if callable_body {
            content = canonicalize_callable_arrow(&content, "->");
        } else if callable_signature {
            content = canonicalize_callable_arrow(&content, "=>");
        }

        let current_indent = indent;
        let annotation_line = content.trim_start().starts_with('@');
        let desired_blanks = if out.is_empty() || is_closer_line(&content) {
            0
        } else if (callable_body || annotation_line)
            && matches!(prev_event, PrevEvent::CallableEnd(i) if i == current_indent)
        {
            2
        } else if pending_blank {
            1
        } else {
            0
        };
        push_blank_lines(&mut out, desired_blanks);
        pending_blank = false;

        out.push(format!("{}{}", INDENT.repeat(current_indent), content));

        let mut line_lex = lex;
        let brace_events = scan_braces(&content, &mut line_lex);
        lex = line_lex;

        let opens = brace_events.iter().filter(|&&c| c == '{').count();
        let closes = brace_events.iter().filter(|&&c| c == '}').count();
        let mut callable_open_available = callable_body;
        let mut container_open_available = brace_container_kind(&content);
        let mut ended_callable = None;
        for event in brace_events {
            match event {
                '{' => {
                    let kind = if callable_open_available {
                        callable_open_available = false;
                        BraceKind::Callable
                    } else if let Some(kind) = container_open_available.take() {
                        kind
                    } else {
                        BraceKind::Other
                    };
                    next_order += 1;
                    brace_stack.push(BraceFrame {
                        kind,
                        indent: current_indent,
                        order: next_order,
                    });
                }
                '}' => {
                    let frame = brace_stack.pop().ok_or_else(|| {
                        FormatError::new(line_no, "encountered `}` without a matching `{`")
                    })?;
                    if frame.kind == BraceKind::Callable {
                        ended_callable = Some(frame.indent);
                    }
                }
                _ => unreachable!(),
            }
        }

        if let Some(ended_indent) = ended_callable {
            prev_event = PrevEvent::CallableEnd(ended_indent);
        } else {
            prev_event = PrevEvent::Other;
        }

        let nonleading_closes = closes.saturating_sub(leading_closing_braces);
        if opens >= nonleading_closes {
            indent += opens - nonleading_closes;
        } else {
            indent = indent.saturating_sub(nonleading_closes - opens);
        }

        if let Some(kind) = define_kind(&content)
            && !contains_unquoted_char(&content, '{')
            && !ends_statement(&content)
        {
            next_order += 1;
            define_stack.push(DefineFrame {
                kind,
                order: next_order,
            });
            indent += 1;
        }

        if opens_do_block(&content) && !branch_line {
            indent += 1;
        }

        if branch_line {
            indent += 1;
        }
    }

    if lex.block_comment || lex.quote.is_some() {
        return Err(FormatError::new(
            normalized.lines().count().max(1),
            "unterminated string or block comment",
        ));
    }
    if !brace_stack.is_empty() {
        return Err(FormatError::new(
            normalized.lines().count().max(1),
            "unterminated `{` block",
        ));
    }
    if !define_stack.is_empty() {
        return Err(FormatError::new(
            normalized.lines().count().max(1),
            "unterminated define block",
        ));
    }

    while out.last().is_some_and(|line| line.is_empty()) {
        out.pop();
    }

    let mut result = out.join("\n");
    result.push('\n');
    Ok(result)
}

/// Returns `true` when `source` is already in canonical form.
pub fn is_formatted(source: &str) -> Result<bool, FormatError> {
    Ok(format_source(source)? == source)
}

fn push_blank_lines(out: &mut Vec<String>, count: usize) {
    while out.last().is_some_and(|line| line.is_empty()) {
        out.pop();
    }
    for _ in 0..count {
        out.push(String::new());
    }
}

fn starts_word(line: &str, word: &str) -> bool {
    if !line.starts_with(word) {
        return false;
    }
    line[word.len()..]
        .chars()
        .next()
        .is_none_or(|c| !is_ident_char(c))
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn leading_closing_braces(line: &str) -> usize {
    line.chars()
        .take_while(|c| c.is_whitespace() || *c == '}')
        .filter(|c| *c == '}')
        .count()
}

fn is_closer_line(line: &str) -> bool {
    line.starts_with('}')
        || starts_word(line, "end")
        || starts_word(line, "fi")
        || starts_word(line, "done")
}

fn ends_statement(line: &str) -> bool {
    line.trim_end().ends_with(';')
}

fn nearest_signature_context(defines: &[DefineFrame], braces: &[BraceFrame]) -> bool {
    let mut newest: Option<(usize, bool)> = None;

    for frame in defines {
        let is_signature = matches!(frame.kind, DefineKind::Interface | DefineKind::Trait);
        if newest.is_none_or(|(order, _)| frame.order > order) {
            newest = Some((frame.order, is_signature));
        }
    }

    for frame in braces {
        let is_signature = match frame.kind {
            BraceKind::Interface | BraceKind::Trait => Some(true),
            BraceKind::Callable
            | BraceKind::Module
            | BraceKind::Class
            | BraceKind::Struct
            | BraceKind::Actor => Some(false),
            BraceKind::Other => None,
        };
        if let Some(is_signature) = is_signature
            && newest.is_none_or(|(order, _)| frame.order > order)
        {
            newest = Some((frame.order, is_signature));
        }
    }

    newest
        .map(|(_, is_signature)| is_signature)
        .unwrap_or(false)
}

fn define_kind(line: &str) -> Option<DefineKind> {
    let mut words = line.split_whitespace();
    if words.next()? != "define" {
        return None;
    }
    Some(match words.next()? {
        "module" => DefineKind::Module,
        "class" => DefineKind::Class,
        "interface" => DefineKind::Interface,
        "trait" => DefineKind::Trait,
        "struct" => DefineKind::Struct,
        _ => DefineKind::Other,
    })
}

fn brace_container_kind(line: &str) -> Option<BraceKind> {
    if !contains_unquoted_char(line, '{') {
        return None;
    }
    let prefix = line.split('{').next().unwrap_or(line);
    let words: Vec<&str> = prefix
        .split(|c: char| !is_ident_char(c))
        .filter(|word| !word.is_empty())
        .collect();
    if words.contains(&"interface") {
        Some(BraceKind::Interface)
    } else if words.contains(&"trait") {
        Some(BraceKind::Trait)
    } else if words.contains(&"class") {
        Some(BraceKind::Class)
    } else if words.contains(&"module") {
        Some(BraceKind::Module)
    } else if words.contains(&"struct") {
        Some(BraceKind::Struct)
    } else if words.contains(&"actor") {
        Some(BraceKind::Actor)
    } else {
        None
    }
}

fn is_callable_body_header(line: &str, signature_context: bool) -> bool {
    if !contains_unquoted_char(line, '{') || !line.contains('(') {
        return false;
    }
    let lower = line.trim_start();
    for kw in [
        "if", "for", "while", "switch", "match", "catch", "recover", "defer",
    ] {
        if starts_word(lower, kw) {
            return false;
        }
    }
    if lower.starts_with("obj{") || lower.starts_with("arr[") {
        return false;
    }
    if contains_word(lower, "fnc") || contains_word(lower, "routine") {
        return true;
    }
    // Methods omit `fnc`. A braced callable inside interface/trait is a default
    // implementation and therefore still uses executable `->` syntax.
    if signature_context
        || line.contains("self")
        || line.contains(") ->")
        || line.contains("): ")
        || line.contains(") =>")
    {
        return true;
    }
    // Class/actor method shape, including `[Symbol.iterator]()`.
    let prefix = lower.split('(').next().unwrap_or("").trim();
    !prefix.contains('=') && !prefix.contains('.') && !prefix.ends_with("new")
}

fn is_callable_signature(line: &str, signature_context: bool) -> bool {
    signature_context
        && line.trim_end().ends_with(';')
        && line.contains('(')
        && !contains_unquoted_char(line, '{')
}

fn contains_word(line: &str, word: &str) -> bool {
    line.split(|c: char| !is_ident_char(c))
        .any(|part| part == word)
}

fn canonicalize_callable_arrow(line: &str, desired: &str) -> String {
    let body_at = line
        .find('{')
        .or_else(|| line.rfind(';'))
        .unwrap_or(line.len());
    let Some(close_paren) = line[..body_at].rfind(')') else {
        return line.to_string();
    };
    let tail = &line[close_paren + 1..body_at];
    let separator = if let Some(pos) = tail.find("=>") {
        Some((pos, 2))
    } else if let Some(pos) = tail.find("->") {
        Some((pos, 2))
    } else {
        tail.find(':').map(|pos| (pos, 1))
    };
    let Some((sep_pos, sep_len)) = separator else {
        return line.to_string();
    };

    let absolute = close_paren + 1 + sep_pos;
    let before = line[..absolute].trim_end();
    let after = line[absolute + sep_len..].trim_start();
    format!("{before} {desired} {after}")
}

fn opens_do_block(line: &str) -> bool {
    let code = strip_line_comment(line);
    let t = code.trim_end();
    (t.ends_with(" do") || t.ends_with("; do")) && !starts_word(t.trim_start(), "done")
}

fn strip_line_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut quote: Option<u8> = None;
    let mut escape = false;
    let mut i = 0;
    while i + 1 < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if matches!(b, b'"' | b'\'' | b'`') {
            quote = Some(b);
            i += 1;
            continue;
        }
        if b == b'/' && bytes[i + 1] == b'/' {
            return &line[..i];
        }
        i += 1;
    }
    line
}

fn contains_unquoted_char(line: &str, needle: char) -> bool {
    let mut state = LexState::default();
    scan_visible(line, &mut state)
        .into_iter()
        .any(|c| c == needle)
}

fn scan_braces(line: &str, state: &mut LexState) -> Vec<char> {
    scan_visible(line, state)
        .into_iter()
        .filter(|c| matches!(c, '{' | '}'))
        .collect()
}

fn scan_visible(line: &str, state: &mut LexState) -> Vec<char> {
    let chars: Vec<char> = line.chars().collect();
    let mut visible = Vec::new();
    let mut i = 0usize;

    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();

        if state.block_comment {
            if c == '*' && next == Some('/') {
                state.block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }

        if let Some(q) = state.quote {
            if state.escape {
                state.escape = false;
            } else if c == '\\' {
                state.escape = true;
            } else if c == q {
                state.quote = None;
            }
            i += 1;
            continue;
        }

        if c == '/' && next == Some('/') {
            break;
        }
        if c == '/' && next == Some('*') {
            state.block_comment = true;
            i += 2;
            continue;
        }
        if matches!(c, '"' | '\'' | '`') {
            state.quote = Some(c);
            i += 1;
            continue;
        }

        visible.push(c);
        i += 1;
    }

    visible
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_executable_and_signature_arrows() {
        let src = r#"define interface Named
  fnc name(): String;
end

define trait Retryable
  fnc retry() -> bool;
end

define class User implements Named, Retryable as
  pub name() => String {
    return "u";
  }
end
"#;
        let got = format_source(src).unwrap();
        assert!(got.contains("fnc name() => String;"));
        assert!(got.contains("fnc retry() => bool;"));
        assert!(got.contains("pub name() -> String {"));
    }
}
