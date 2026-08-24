//! Turning an agent's reply into something worth hearing.
//!
//! Agent replies are Markdown, and the message pane renders them as Markdown.
//! The spoken path had been handed the same string untouched, so a voice read
//! `**ready**` as "star star ready star star", a fenced block character by
//! character including its backticks, and a link as its whole URL. None of
//! that is information to a listener; all of it is punctuation the writer
//! meant for an eye.
//!
//! So exactly one side is flattened. [`flatten_markdown_for_speech`] is called
//! at [`super::tts_backend::AmbientTts::speak`] — the single door every reply
//! goes through on its way to a voice, local or server — and nowhere near the
//! text that is published, stored or shown. The message stays Markdown.
//!
//! ## What it does, and what it deliberately does not
//!
//! This is not a Markdown parser and must not become one. It removes the
//! marks a listener cannot hear and keeps every word:
//!
//! | Written | Spoken |
//! |---|---|
//! | `**bold**`, `_italic_`, `~~gone~~` | bold, italic, gone |
//! | `# Heading` | Heading. |
//! | `` `value` `` | value |
//! | ```` ```rust … ``` ```` | "code block" — the code itself is not read |
//! | `[the docs](https://…)` | the docs |
//! | `- one` / `1. two` | one. two. |
//! | `> quoted` | quoted |
//!
//! Text that is not Markdown comes back unchanged, which is the property the
//! whole thing rests on: most replies are ordinary sentences, and a flattener
//! that edited those would be worse than the punctuation it removes.
//!
//! Sentence-ending punctuation is added to headings and list items because a
//! voice reads punctuation as pacing. Without it "Done. Next steps Install the
//! thing Run it" arrives as one breathless run.

/// What a fenced code block is spoken as, in place of its contents.
///
/// Reading code aloud is unusable — sigils, indentation and all — and dropping
/// it silently would leave "here is the fix:" answered by nothing. Naming it is
/// the only honest option a listener can act on.
const CODE_BLOCK_SPOKEN_AS: &str = "code block.";

/// The longest run of emphasis characters CommonMark can give meaning to.
const MAX_EMPHASIS_RUN: usize = 3;

/// Flatten Markdown to the plain text a voice should read.
///
/// Never fails and never drops words: anything it does not recognise is passed
/// through, so an unusual reply is spoken with its punctuation rather than
/// spoken not at all.
pub(crate) fn flatten_markdown_for_speech(markdown: &str) -> String {
    let mut spoken: Vec<String> = Vec::new();
    let mut fence: Option<String> = None;

    for raw_line in markdown.lines() {
        let line = raw_line.trim_end();
        let trimmed = line.trim_start();

        // ── Fenced code ──────────────────────────────────────────────────
        if let Some(open) = fence.as_deref() {
            if closes_fence(trimmed, open) {
                fence = None;
            }
            continue;
        }
        if let Some(open) = opening_fence(trimmed) {
            fence = Some(open);
            push_line(&mut spoken, CODE_BLOCK_SPOKEN_AS.to_string());
            continue;
        }

        // ── Block marks ──────────────────────────────────────────────────
        let body = strip_block_quotes(trimmed);
        let body = body.trim_start();
        if body.is_empty() || is_rule(body) {
            continue;
        }
        let (body, ends_a_sentence) = match strip_heading(body) {
            Some(heading) => (heading, true),
            None => match strip_list_marker(body) {
                Some(item) => (item, true),
                None => (body, false),
            },
        };

        let mut said = flatten_inline(body);
        if said.is_empty() {
            continue;
        }
        if ends_a_sentence && !ends_with_sentence_punctuation(&said) {
            said.push('.');
        }
        push_line(&mut spoken, said);
    }

    spoken.join("\n")
}

/// Append `line`, unless it repeats the "code block" line already there.
///
/// A reply that alternates prose and fences is common; two adjacent fences
/// with nothing between them are not two pieces of news.
fn push_line(spoken: &mut Vec<String>, line: String) {
    if line == CODE_BLOCK_SPOKEN_AS
        && spoken.last().map(String::as_str) == Some(CODE_BLOCK_SPOKEN_AS)
    {
        return;
    }
    spoken.push(line);
}

/// The fence a line opens, if it opens one: three or more backticks or tildes.
fn opening_fence(trimmed: &str) -> Option<String> {
    for marker in ['`', '~'] {
        let run = trimmed.chars().take_while(|c| *c == marker).count();
        if run >= 3 {
            // An info string may follow the opening fence, never the closing
            // one, and a backtick fence may not carry a backtick in it.
            if marker == '`' && trimmed[run..].contains('`') {
                continue;
            }
            return Some(marker.to_string().repeat(run));
        }
    }
    None
}

/// Whether `trimmed` closes a fence opened with `open`.
fn closes_fence(trimmed: &str, open: &str) -> bool {
    let Some(marker) = open.chars().next() else {
        return false;
    };
    let run = trimmed.chars().take_while(|c| *c == marker).count();
    run >= open.chars().count() && trimmed[run..].trim().is_empty()
}

/// Drop any number of `>` quote marks from the front of a line.
fn strip_block_quotes(trimmed: &str) -> &str {
    let mut rest = trimmed;
    while let Some(stripped) = rest.strip_prefix('>') {
        rest = stripped.trim_start();
    }
    rest
}

/// A thematic break, a setext underline, or a table's separator row — every
/// line that is drawing rather than saying.
fn is_rule(body: &str) -> bool {
    let mut seen = false;
    for c in body.chars() {
        match c {
            '-' | '=' | '_' | '*' | '|' | ':' => seen = true,
            ' ' | '\t' => {}
            _ => return false,
        }
    }
    seen
}

/// The text of an ATX heading, or `None` when the line is not one.
fn strip_heading(body: &str) -> Option<&str> {
    let hashes = body.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &body[hashes..];
    if !rest.starts_with([' ', '\t']) && !rest.is_empty() {
        return None;
    }
    Some(rest.trim().trim_end_matches('#').trim_end())
}

/// The text of a bullet or numbered list item, or `None` when it is neither.
fn strip_list_marker(body: &str) -> Option<&str> {
    if let Some(rest) = body.strip_prefix(['-', '*', '+']) {
        // "- item", not "-1 degree" and not "---".
        if rest.starts_with([' ', '\t']) {
            return Some(rest.trim_start());
        }
        return None;
    }
    let digits = body.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > 9 {
        return None;
    }
    let rest = &body[digits..];
    let rest = rest.strip_prefix(['.', ')'])?;
    if !rest.starts_with([' ', '\t']) {
        return None;
    }
    Some(rest.trim_start())
}

fn ends_with_sentence_punctuation(said: &str) -> bool {
    said.chars()
        .next_back()
        .is_some_and(|c| matches!(c, '.' | '!' | '?' | ':' | ';' | ','))
}

/// Remove the inline marks from one line, keeping every word.
///
/// Iterative, and deliberately so. Link labels nest, and walking into one by
/// calling this function again meant a reply's bracket depth became the
/// process's stack depth: about ten thousand nested `[` — twenty kilobytes of
/// text, well inside one message — overflowed the 2 MiB stack a Tauri command
/// runs on and aborted the whole app, with quadratic time and memory on the way
/// there. A reply is remote text, so that was reachable from anything the bound
/// agent said. The labels are resolved once by [`link_spans`] and walked
/// through with an explicit stack, which is linear in both.
fn flatten_inline(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let links = link_spans(&chars);
    let emphasis = emphasis_pairs(&chars);
    let mut out = String::with_capacity(line.len());
    // Labels being walked through, innermost last: where each one ends, and
    // where to carry on once it has.
    let mut open_labels: Vec<LabelSpan> = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        // Only the innermost label can end here. Anything a code span or an
        // autolink jumped clean over is stale and is dropped without a jump.
        while let Some(&label) = open_labels.last() {
            if label.close > i {
                break;
            }
            open_labels.pop();
            if label.close == i {
                i = label.resume;
            }
        }
        if i >= chars.len() {
            break;
        }

        match chars[i] {
            // A backslash escape is the author saying "this one is literal".
            '\\' if i + 1 < chars.len() && chars[i + 1].is_ascii_punctuation() => {
                out.push(chars[i + 1]);
                i += 2;
            }
            '`' => match code_span(&chars, i) {
                Some((text, next)) => {
                    out.push_str(&text);
                    i = next;
                }
                None => {
                    out.push('`');
                    i += 1;
                }
            },
            '!' => {
                // An image speaks its alt text — written for someone who
                // cannot see it, which is exactly this listener. The `[` that
                // follows is handled by the arm below on the next pass.
                if links.at(i + 1).is_none() {
                    out.push('!');
                }
                i += 1;
            }
            '[' => match links.at(i) {
                // Step into the label and keep going: its text is inline
                // content like any other, and the address is skipped when the
                // closing bracket is reached.
                Some(label) => {
                    open_labels.push(label);
                    i += 1;
                }
                None => {
                    out.push('[');
                    i += 1;
                }
            },
            // An autolink is a bare URL in angle brackets. The brackets are
            // markup; the address is what a plain-text reply would have said
            // here anyway, so it is left to be read.
            '<' => match autolink(&chars, i) {
                Some((url, next)) => {
                    out.push_str(&url);
                    i = next;
                }
                None => {
                    out.push('<');
                    i += 1;
                }
            },
            '*' | '~' | '_' => match emphasis.at(i) {
                // Both ends of a pair go together, or neither does.
                Some(run) => i += run,
                None => {
                    out.push(chars[i]);
                    i += 1;
                }
            },
            c => {
                out.push(c);
                i += 1;
            }
        }
    }

    collapse_spaces(&out)
}

/// The contents of a backtick code span starting at `start`, and where it ends.
///
/// The inside of a code span is literal by definition, so it is copied out
/// without another inline pass — `` `a*b` `` keeps its star.
fn code_span(chars: &[char], start: usize) -> Option<(String, usize)> {
    let ticks = chars[start..].iter().take_while(|c| **c == '`').count();
    let mut i = start + ticks;
    while i < chars.len() {
        if chars[i] != '`' {
            i += 1;
            continue;
        }
        let run = chars[i..].iter().take_while(|c| **c == '`').count();
        if run == ticks {
            let text: String = chars[start + ticks..i].iter().collect();
            return Some((text.trim().to_string(), i + run));
        }
        i += run;
    }
    None
}

/// One link or image label: where it ends, and where its whole link does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LabelSpan {
    /// Index of the `]` that closes the label.
    close: usize,
    /// Index to carry on from — past the address, when there is one.
    resume: usize,
}

/// Every link label on the line, indexed by the `[` that opens it.
///
/// Empty when the line has no `[` at all, which is almost every line: the
/// lookup answers `None` for any index, and nothing is allocated.
struct LinkSpans(Vec<Option<LabelSpan>>);

impl LinkSpans {
    fn at(&self, index: usize) -> Option<LabelSpan> {
        self.0.get(index).copied().flatten()
    }
}

/// The longest address that is still read as one.
///
/// A link's address is a URL, and one longer than this is not a link anyone
/// wrote — it is an unclosed bracket with the rest of the reply behind it.
/// Bounding the scan keeps [`link_spans`] linear over the line; past the bound
/// the `[` is simply spoken, which is the safe direction to fail in.
const MAX_LINK_ADDRESS_CHARS: usize = 2048;

/// Resolve every label on the line in one left-to-right pass.
///
/// Covers the three shapes a reply actually uses — `[text](url)`,
/// `[text][ref]` and the shortcut `[text]` — because all three speak the same:
/// the label, never the address. A `[` with no `]` after it gets no span and is
/// therefore spoken as itself; so is one whose address never closes.
fn link_spans(chars: &[char]) -> LinkSpans {
    if !chars.contains(&'[') {
        return LinkSpans(Vec::new());
    }
    let mut spans: Vec<Option<LabelSpan>> = vec![None; chars.len()];
    let mut open: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            // A backslash escape is the author saying "this one is literal",
            // so the bracket behind it opens and closes nothing.
            '\\' => i += 1,
            '[' => open.push(i),
            ']' => {
                if let Some(start) = open.pop() {
                    if let Some(resume) = address_end(chars, i + 1) {
                        spans[start] = Some(LabelSpan { close: i, resume });
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    LinkSpans(spans)
}

/// Where the address following a label ends, or `None` when it never closes.
fn address_end(chars: &[char], after: usize) -> Option<usize> {
    match chars.get(after) {
        Some('(') => skip_balanced(chars, after, '(', ')'),
        Some('[') => skip_balanced(chars, after, '[', ']'),
        // A shortcut label — `[text]` with nothing after it — is its own link.
        _ => Some(after),
    }
}

/// Index just past the balanced `open`/`close` pair beginning at `start`.
///
/// Gives up after [`MAX_LINK_ADDRESS_CHARS`] rather than running to the end of
/// the line: an address that long is a bracket someone forgot to close, and
/// scanning for its end once per label is how a line of them becomes quadratic.
fn skip_balanced(chars: &[char], start: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = start;
    let limit = start.saturating_add(MAX_LINK_ADDRESS_CHARS);
    while i <= limit {
        let c = *chars.get(i)?;
        if c == '\\' {
            i += 2;
            continue;
        }
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return Some(i + 1);
            }
        }
        i += 1;
    }
    None
}

/// A `<https://…>` autolink: the address without its brackets, and where it
/// ends.
fn autolink(chars: &[char], start: usize) -> Option<(String, usize)> {
    let close = chars[start + 1..]
        .iter()
        .position(|c| *c == '>' || c.is_whitespace())?;
    let inner: String = chars[start + 1..start + 1 + close].iter().collect();
    if chars.get(start + 1 + close) != Some(&'>') {
        return None;
    }
    let is_url = inner.starts_with("http://")
        || inner.starts_with("https://")
        || (inner.contains('@') && !inner.contains(' '));
    is_url.then(|| (inner, start + close + 2))
}

/// The emphasis runs on a line that actually pair up, by the index each starts
/// at and how many characters long it is.
struct EmphasisPairs(Vec<Option<usize>>);

impl EmphasisPairs {
    fn at(&self, index: usize) -> Option<usize> {
        self.0.get(index).copied().flatten()
    }
}

/// Match every emphasis run on the line against a closing one.
///
/// A mark is only a mark if something closes it. `*` before a word is emphasis
/// when a later `*` after a word ends it, and otherwise it is a glob or a
/// multiplication sign the author typed — `rm *.rs` and `5*5` were being spoken
/// as "rm .rs" and "55", which is the flattener's own "keeps every word" rule
/// broken by the flattener.
///
/// The flanking test decides which end a run *could* be, and is deliberately
/// not a full CommonMark pass: a run with whitespace on both sides is
/// arithmetic or a drawing ("2 * 3"), and an underscore between two word
/// characters belongs to the word (`snake_case`). Pairing is then the ordinary
/// stack: a run that can close takes the nearest unclosed opener of its own
/// marker, and anything still unclosed at the end of the line is text.
fn emphasis_pairs(chars: &[char]) -> EmphasisPairs {
    if !chars.iter().any(|c| matches!(c, '*' | '~' | '_')) {
        return EmphasisPairs(Vec::new());
    }
    let mut pairs: Vec<Option<usize>> = vec![None; chars.len()];
    // One stack per marker: `*` never closes a `~`.
    let mut open: Vec<(char, usize, usize)> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let marker = chars[i];
        // An escaped mark is a character, not a mark, and closes nothing.
        if marker == '\\' {
            i += 2;
            continue;
        }
        if !matches!(marker, '*' | '~' | '_') {
            i += 1;
            continue;
        }
        let run = chars[i..].iter().take_while(|c| **c == marker).count();
        // Longer than any emphasis CommonMark gives meaning to: it is drawing.
        if run > MAX_EMPHASIS_RUN {
            i += run;
            continue;
        }
        let before = i.checked_sub(1).and_then(|at| chars.get(at)).copied();
        let after = chars.get(i + run).copied();
        let opens = after.is_some_and(|c| !c.is_whitespace());
        let closes = before.is_some_and(|c| !c.is_whitespace());
        let intraword_underscore = marker == '_'
            && before.is_some_and(char::is_alphanumeric)
            && after.is_some_and(char::is_alphanumeric);
        if !intraword_underscore {
            if let Some(position) = closes
                .then(|| {
                    open.iter()
                        .rposition(|(open_marker, _, _)| *open_marker == marker)
                })
                .flatten()
            {
                let (_, start, open_run) = open.remove(position);
                pairs[start] = Some(open_run);
                pairs[i] = Some(run);
                // Anything left open inside the pair was never closed.
                open.truncate(position);
            } else if opens {
                open.push((marker, i, run));
            }
        }
        i += run;
    }
    EmphasisPairs(pairs)
}

/// One space between words, none at the ends. Removing a mark leaves a gap.
fn collapse_spaces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
#[path = "speech_text_tests.rs"]
mod speech_text_tests;
