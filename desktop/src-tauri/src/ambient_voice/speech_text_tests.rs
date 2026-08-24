//! What a voice is given, from what an agent wrote.
//!
//! Every case here is stated as the sentence a listener hears, not as the
//! transformation applied to get there: the harm being fixed is a voice saying
//! "star star", and the property is that no mark survives into speech while
//! every word does.

use super::*;

/// The property the whole flattener rests on: an ordinary reply is untouched.
#[test]
fn a_reply_with_no_markdown_in_it_is_spoken_exactly_as_written() {
    for plain in [
        "Your calendar is clear tomorrow.",
        "I moved the 3pm to Thursday and told Priya.",
        "Two things: the build passed, and the deploy is waiting on you.",
        "It cost 2 * 3 dollars, or maybe 2 _ 3 — I could not read the receipt.",
        "The file is snake_case_name and the flag is --dry-run.",
    ] {
        assert_eq!(flatten_markdown_for_speech(plain), plain);
    }
}

#[test]
fn emphasis_is_heard_as_the_word_and_not_as_its_marks() {
    assert_eq!(
        flatten_markdown_for_speech("The deploy is **ready** and _waiting_."),
        "The deploy is ready and waiting."
    );
    assert_eq!(
        flatten_markdown_for_speech("That was ~~yesterday~~ this morning."),
        "That was yesterday this morning."
    );
    assert_eq!(
        flatten_markdown_for_speech("***All*** of it, __every__ bit."),
        "All of it, every bit."
    );
}

#[test]
fn a_heading_is_spoken_as_a_sentence_so_the_voice_stops_after_it() {
    // Without the full stop a voice runs the heading into the paragraph under
    // it: "Next steps Install the thing".
    assert_eq!(
        flatten_markdown_for_speech("## Next steps\nInstall the thing."),
        "Next steps.\nInstall the thing."
    );
    assert_eq!(
        flatten_markdown_for_speech("# Done?\nNot yet."),
        "Done?\nNot yet."
    );
    // A closed ATX heading loses its trailing hashes too.
    assert_eq!(flatten_markdown_for_speech("### Summary ###"), "Summary.");
    // Not a heading: a hash that is part of what was written.
    assert_eq!(
        flatten_markdown_for_speech("Issue #6397 is the one."),
        "Issue #6397 is the one."
    );
}

#[test]
fn inline_code_is_spoken_as_its_contents_without_the_backticks() {
    assert_eq!(
        flatten_markdown_for_speech("Run `just ci` before you push."),
        "Run just ci before you push."
    );
    // Inside a code span the marks are literal, so they survive: the author
    // meant them as characters, and a listener needs to hear them.
    assert_eq!(
        flatten_markdown_for_speech("The glob is ``*.rs`` here."),
        "The glob is *.rs here."
    );
}

#[test]
fn a_fenced_block_is_named_rather_than_read_out_character_by_character() {
    let reply = "Here is the fix:\n\
                 ```rust\n\
                 let x = **y**;\n\
                 // ``` inside\n\
                 ```\n\
                 Then rebuild.";
    assert_eq!(
        flatten_markdown_for_speech(reply),
        "Here is the fix:\ncode block.\nThen rebuild."
    );
    // Tildes open a fence too, and an unterminated fence still swallows its
    // contents rather than reading them aloud.
    assert_eq!(
        flatten_markdown_for_speech("~~~\nnever closed\n"),
        "code block."
    );
}

#[test]
fn a_link_is_spoken_as_its_text_and_never_as_its_address() {
    assert_eq!(
        flatten_markdown_for_speech("See [the release notes](https://example.test/a/b?c=d)."),
        "See the release notes."
    );
    assert_eq!(
        flatten_markdown_for_speech("See [the **notes**][notes]."),
        "See the notes."
    );
    assert_eq!(
        flatten_markdown_for_speech("![a red build badge](https://example.test/b.png)"),
        "a red build badge"
    );
}

#[test]
fn list_items_are_spoken_as_separate_sentences() {
    let reply = "Three things:\n\
                 - the build passed\n\
                 - the deploy is waiting\n\
                 * Priya replied.\n\
                 1. tell her\n\
                 2) then merge";
    assert_eq!(
        flatten_markdown_for_speech(reply),
        "Three things:\n\
         the build passed.\n\
         the deploy is waiting.\n\
         Priya replied.\n\
         tell her.\n\
         then merge."
    );
}

#[test]
fn quotes_rules_and_table_lines_lose_their_drawing_and_keep_their_words() {
    assert_eq!(
        flatten_markdown_for_speech("> > She said it was fine.\n\n---\n\nSo it is."),
        "She said it was fine.\nSo it is."
    );
    assert_eq!(
        flatten_markdown_for_speech("Heading\n=======\nBody."),
        "Heading\nBody."
    );
}

#[test]
fn an_escaped_mark_is_spoken_as_the_character_the_author_escaped() {
    assert_eq!(
        flatten_markdown_for_speech(r"The literal \*star\* survives."),
        "The literal *star* survives."
    );
}

#[test]
fn nothing_a_reply_can_contain_makes_this_lose_a_word() {
    // The failure mode that would be worse than the punctuation: a reply that
    // is spoken as less than it says. Every alphanumeric run in the input has
    // to appear in the output, whatever marks surround it.
    let reply = "# Report\n\
                 The **build** on [main](https://example.test) is `green`.\n\
                 ```\n\
                 ignored\n\
                 ```\n\
                 - one\n\
                 - two\n\
                 > and three";
    let spoken = flatten_markdown_for_speech(reply);
    for word in [
        "Report", "build", "main", "green", "one", "two", "and", "three",
    ] {
        assert!(spoken.contains(word), "{word:?} was lost from {spoken:?}");
    }
    assert!(!spoken.contains("ignored"), "{spoken}");
    assert!(!spoken.contains("example.test"), "{spoken}");
    for mark in ['*', '`', '#', '[', ']', '>'] {
        assert!(!spoken.contains(mark), "{mark} survived into {spoken:?}");
    }
}

#[test]
fn an_empty_or_marks_only_reply_flattens_to_nothing_rather_than_to_noise() {
    for nothing in ["", "   ", "\n\n", "---", "**", "> "] {
        assert_eq!(flatten_markdown_for_speech(nothing), "", "{nothing:?}");
    }
}
