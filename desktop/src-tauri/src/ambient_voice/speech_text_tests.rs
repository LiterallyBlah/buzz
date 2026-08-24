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
    // contents rather than reading them aloud — it just says which kind of
    // block it was, so the swallowing is accounted for rather than silent.
    assert_eq!(
        flatten_markdown_for_speech("~~~\nnever closed\n"),
        "unfinished code block."
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
fn brackets_that_cross_rather_than_nest_keep_the_words_they_run_through() {
    // `[a [b](c] d)` is a reply whose brackets cross: the inner link's address
    // `(c] d)` contains the `]` that closes the outer label. Stepping into the
    // inner label and carrying on past its address jumped over the outer
    // label's close and everything behind it, so " d)" was never spoken —
    // three words in and the flattener is losing words, which is the one thing
    // it must not do.
    //
    // Spans that cross are not a link anyone wrote, so the inner `[` is spoken
    // as itself: the same answer a bracket that never closes already gets.
    assert_eq!(flatten_markdown_for_speech("[a [b](c] d)"), "a [b](c d)");
    assert_eq!(
        flatten_markdown_for_speech("[why [see](there] and the rest of it)"),
        "why [see](there and the rest of it)"
    );

    // The control: labels that nest are still labels, and the address of a
    // nested one is still never spoken.
    assert_eq!(
        flatten_markdown_for_speech(
            "[the [inner](https://example.test/in) label](https://example.test/out)"
        ),
        "the inner label"
    );
}

#[test]
fn no_arrangement_of_brackets_a_reply_can_contain_loses_a_word() {
    // The case above is one shape of a class — brackets and parentheses a
    // reply can arrange any way at all — so the class is walked rather than
    // sampled: every string up to six characters over both brackets, both
    // parentheses, a space and a word.
    //
    // A word inside `(…)` may be a link's address, and an address is the one
    // thing this flattener drops on purpose, so the expectation is about the
    // words outside them. `][` is left out for the same reason: it is the
    // other way an address begins, and its contents are not parenthesised.
    const ALPHABET: [char; 6] = ['[', ']', '(', ')', ' ', 'w'];
    const LONGEST: u32 = 6;

    /// Words no arrangement of these characters could read as an address.
    fn words_outside_parentheses(input: &str) -> usize {
        let mut depth = 0usize;
        let mut outside = 0usize;
        for c in input.chars() {
            match c {
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                'w' if depth == 0 => outside += 1,
                _ => {}
            }
        }
        outside
    }

    let mut checked = 0usize;
    for length in 1..=LONGEST {
        for combination in 0..ALPHABET.len().pow(length) {
            let mut rest = combination;
            let input: String = (0..length)
                .map(|_| {
                    let c = ALPHABET[rest % ALPHABET.len()];
                    rest /= ALPHABET.len();
                    c
                })
                .collect();
            if input.contains("][") {
                continue;
            }
            let spoken = flatten_markdown_for_speech(&input);
            assert!(
                spoken.matches('w').count() >= words_outside_parentheses(&input),
                "{input:?} was spoken as {spoken:?}"
            );
            checked += 1;
        }
    }
    assert!(checked > 40_000, "only {checked} arrangements were walked");
}

#[test]
fn an_unfinished_code_block_is_said_to_be_unfinished() {
    // A fence that never closes holds back everything after it, which is
    // CommonMark's own reading and stays. What must not happen is it sounding
    // identical to a finished answer: a reply cut off mid-fence is ordinary,
    // and the listener is the only person who cannot see that the text
    // carries on.
    let closed = flatten_markdown_for_speech("Here is the fix:\n```\nlet x = 1;\n```\nDone.");
    let cut_off = flatten_markdown_for_speech("Here is the fix:\n```\nlet x = 1;\nand more");
    assert_ne!(
        closed, cut_off,
        "a reply that stops inside a code block sounds like one that does not"
    );
    assert_eq!(closed, "Here is the fix:\ncode block.\nDone.");
    assert_eq!(cut_off, "Here is the fix:\nunfinished code block.");

    // The code itself still goes unread, both ways round.
    assert!(!cut_off.contains("let x"), "{cut_off}");
    assert!(!cut_off.contains("and more"), "{cut_off}");
}

#[test]
fn a_finished_block_keeps_its_name_when_an_unfinished_fence_follows_it() {
    // Two finished blocks in a row are one piece of news, and still are.
    assert_eq!(
        flatten_markdown_for_speech("```\nx\n```\n```\ny\n```"),
        "code block."
    );
    // A finished block and then a fence that never closes are two, and the
    // second was being folded onto the first and then renaming it: the whole
    // reply said "unfinished code block." as though the block that did finish
    // had never been there.
    assert_eq!(
        flatten_markdown_for_speech("```\nx\n```\n```"),
        "code block.\nunfinished code block."
    );
    assert_eq!(
        flatten_markdown_for_speech("Here:\n```\nlet x = 1;\n```\nAnd then:\n```\nlet y ="),
        "Here:\ncode block.\nAnd then:\nunfinished code block."
    );
}

#[test]
fn a_task_list_speaks_its_items_and_not_its_boxes() {
    // `[x]` is a drawing of a ticked box. Spoken, it was the letter x at the
    // head of every finished item. Neither state is announced: a voice that
    // said "done" or "not done" would be adding a word the writer did not.
    assert_eq!(
        flatten_markdown_for_speech("- [ ] todo one\n- [x] done two\n- [X] done three"),
        "todo one.\ndone two.\ndone three."
    );
    // A bracket that is not a checkbox is left to the ordinary link rules.
    assert_eq!(
        flatten_markdown_for_speech("- [the docs](https://example.test) first"),
        "the docs first."
    );
    assert_eq!(
        flatten_markdown_for_speech("- [xy] not a box"),
        "xy not a box."
    );
}

#[test]
fn a_star_that_closes_nothing_is_a_character_and_not_a_mark() {
    // A mark is only a mark if something closes it. These four are what an
    // agent writes about files and arithmetic, and every one of them was being
    // spoken as less than it said: "rm .rs", "src/.rs", "55", "23".
    for verbatim in [
        "run rm *.rs and ls *.md now",
        "src/*.rs matches",
        "5*5 = 25",
        "2*3 is 6",
    ] {
        assert_eq!(flatten_markdown_for_speech(verbatim), verbatim);
    }

    // Unpaired underscores and tildes go the same way, and so does a run too
    // long to be emphasis at all.
    for verbatim in [
        "the flag is _dry_run and the other is _x",
        "roughly ~50 people",
        "a ** b and c ~~~~ d",
    ] {
        assert_eq!(flatten_markdown_for_speech(verbatim), verbatim);
    }
}

#[test]
fn emphasis_that_does_close_is_still_stripped() {
    // The control for the rule above: the marks a writer actually paired must
    // still come off, or the fix would have traded one wrong reading for
    // another.
    assert_eq!(
        flatten_markdown_for_speech("The file *matters* and so does **this**."),
        "The file matters and so does this."
    );
    assert_eq!(
        flatten_markdown_for_speech("Delete *.rs but keep *this* one."),
        "Delete *.rs but keep this one."
    );
    assert_eq!(
        flatten_markdown_for_speech("~~gone~~, _here_, and 2 * 3."),
        "gone, here, and 2 * 3."
    );
}

/// Run `body` on a thread with a stack far smaller than any the app gives this
/// code, so that recursion over a reply's own structure fails the test rather
/// than passing on whatever headroom the harness happened to have.
fn on_a_small_stack<T: Send + 'static>(body: impl FnOnce() -> T + Send + 'static) -> T {
    const SMALL_STACK_BYTES: usize = 256 * 1024;
    std::thread::Builder::new()
        .stack_size(SMALL_STACK_BYTES)
        .spawn(body)
        .expect("spawn")
        .join()
        .expect("the flattener died on a reply it was given")
}

#[test]
fn a_reply_nested_thousands_deep_is_spoken_rather_than_taken_as_an_attack() {
    // Twenty kilobytes of `[`, which is nothing for a message, used to be an
    // abort: each label was walked into by calling the flattener again, so a
    // reply's bracket depth became the process's stack depth and ten thousand
    // levels overflowed the 2 MiB stack a Tauri command runs on. Replies are
    // remote text — this was anything the bound agent said.
    //
    // The word in the middle is the point. Depth is not a reason to lose it.
    const DEEP: usize = 10_000;
    let reply = format!("{}buried{}", "[".repeat(DEEP), "]".repeat(DEEP));

    let started = std::time::Instant::now();
    let spoken = on_a_small_stack(move || flatten_markdown_for_speech(&reply));
    let took = started.elapsed();

    assert_eq!(spoken, "buried");
    // Generous, because it is diagnosing a shape and not a machine: the linear
    // walk is microseconds, and anything quadratic in the nesting is minutes.
    assert!(
        took < std::time::Duration::from_secs(5),
        "flattening {DEEP} nested labels took {took:?}"
    );
}

#[test]
fn a_line_of_marks_that_pair_with_nothing_is_read_once_and_not_once_per_mark() {
    // Every `~` here can only open — whitespace before it, a letter after —
    // and every `*` can only close, so nothing on the line pairs with anything.
    // While one stack held every marker's openers, each `*` searched all of
    // them and found none: a hundred kilobytes on one line, which is one reply,
    // took about a second inside the flattener before a word reached the voice,
    // and it is the reply that chooses how long that line is.
    const RUNS: usize = 16_666; // ≈ 100 kB on a single line
    let line = " ~a".repeat(RUNS) + &"b* ".repeat(RUNS);

    let started = std::time::Instant::now();
    let spoken = flatten_markdown_for_speech(&line);
    let took = started.elapsed();

    // Nothing paired, so nothing was a mark and every character is still said.
    assert_eq!(spoken.matches("~a").count(), RUNS, "openers were lost");
    assert_eq!(spoken.matches("b*").count(), RUNS, "closers were lost");
    // Two orders of magnitude above the single pass and a fifth of the search
    // it replaced: scheduler noise cannot reach this, and a per-mark search
    // cannot hide under it.
    assert!(
        took < std::time::Duration::from_millis(200),
        "pairing {} kB of marks that close nothing took {took:?}",
        line.len() / 1000
    );
}

#[test]
fn brackets_that_open_nothing_are_spoken_rather_than_swallowed() {
    // The other half: a `[` with no `]` after it is not a label, so it is a
    // character the author typed and the listener hears. Degrading a runaway
    // nesting into silence would trade a crash for a reply that lies.
    const MANY: usize = 10_000;
    let reply = format!("{}stranded", "[".repeat(MANY));
    let spoken = on_a_small_stack(move || flatten_markdown_for_speech(&reply));
    assert_eq!(spoken.matches('[').count(), MANY);
    assert!(spoken.ends_with("stranded"), "the word was lost");

    // And the everyday version of the same rule.
    assert_eq!(
        flatten_markdown_for_speech("The bracket [ is not a link."),
        "The bracket [ is not a link."
    );
    assert_eq!(
        flatten_markdown_for_speech("See [the docs](https://example.test"),
        "See [the docs](https://example.test"
    );
}

#[test]
fn an_address_as_long_as_the_bound_names_is_still_an_address() {
    // The bound is on the address; the brackets around it are not part of it.
    // One character short of what the constant is called was where the line
    // between "a link" and "a bracket someone forgot to close" actually fell.
    let longest = "a".repeat(MAX_LINK_ADDRESS_CHARS);
    assert_eq!(
        flatten_markdown_for_speech(&format!("See [the docs]({longest}).")),
        "See the docs."
    );
    let one_over = "a".repeat(MAX_LINK_ADDRESS_CHARS + 1);
    let past_the_bound = format!("See [the docs]({one_over}).");
    assert_eq!(
        flatten_markdown_for_speech(&past_the_bound),
        past_the_bound,
        "past the bound the bracket is spoken rather than swallowed"
    );
}

#[test]
fn an_empty_or_marks_only_reply_flattens_to_nothing_rather_than_to_noise() {
    for nothing in ["", "   ", "\n\n", "---", "**", "> "] {
        assert_eq!(flatten_markdown_for_speech(nothing), "", "{nothing:?}");
    }
}
