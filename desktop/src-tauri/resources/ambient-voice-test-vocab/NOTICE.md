# Ambient voice — wake-word tokenizer test vocabulary

These four files are copied verbatim from the keyword-spotting model Buzz
downloads at runtime for the `ambientVoice` preview feature:

    sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01

* `bpe.model` — SentencePiece **Unigram** `ModelProto` (500 pieces).
* `tokens.txt` — the engine-authoritative piece → id table.
* `keywords_raw.txt` / `keywords.txt` — the model's own raw → tokenised
  keyword pair, used as the conformance oracle for
  `src/ambient_voice/wake_word.rs`.

They are **test fixtures only**. Nothing in the shipped app reads them; the
runtime always reads the downloaded model directory. They are committed
because the tokenisation recipe (SentencePiece Unigram Viterbi, not BPE
merges and not greedy longest-match) must be proven against the real model
vocabulary on every CI run — feeding sherpa-onnx a keyword it cannot encode
kills the process outright, so an untested tokenizer is a crash waiting to
happen. Together they are ~250 KB; the model weights (~18 MB) are not
committed, and the tests that need them are `#[ignore]`d.

## Attribution

Model: `sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01`
Author: pkufool (k2-fsa / icefall project)
Source: https://www.modelscope.cn/pkufool/sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01
Mirror: https://github.com/k2-fsa/sherpa-onnx/releases/tag/kws-models
License: Apache License 2.0 — https://www.apache.org/licenses/LICENSE-2.0

The upstream archive ships no LICENSE file; the licence is declared in the
in-archive `README.md` front matter and on the ModelScope repository. Buzz
therefore writes an attribution sidecar next to the downloaded model as well
(see `MODEL_LICENSE.txt` in `src/huddle/models.rs`).

Licensed under the Apache License, Version 2.0 (the "License"); you may not
use these files except in compliance with the License. Unless required by
applicable law or agreed to in writing, software distributed under the
License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS
OF ANY KIND, either express or implied.
