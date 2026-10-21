# Source-aware detection followed by read-only extraction

This experiment removes the detector's visibility question while retaining its
request-source check. System One identifies a candidate; the existing extractor
reads it and checks its text and completeness. No agent-specific parser was
added. Nothing here is installed in Orchrd or sends terminal input.

## Policy

1. `source_detector.py` asks the configured `decision` model for dialog category
   and request origin in one request. It skips historical/user questions and
   empty-input placeholders. Dedicated dialogs require a blocked lifecycle;
   conversational questions require idle/done and the latest assistant origin.
2. `pipeline.py` passes admitted screens to the unchanged `bean` extractor.
   Candidate detection does not imply a complete question exists.
3. A question becomes available for presentation only when extraction passes
   schema and literal-screen validation, claims an active complete request,
   identifies its type and purpose, and supplies choices where required.
   Authentication and inconsistent conversational permissions require inspection.
   Unknown keyboard controls do not prevent reading a question to the human.
4. A snapshot fingerprint binds agent identity, instance, lifecycle, sequence,
   and text. The pipeline accepts a `latest` callback to check freshness before
   and after extraction. Recorded evaluations reuse the same snapshot; they do
   not test concurrent live capture or input execution.

The outputs are `ignore`, `inspect`, or `question_available`. The last means a
question can be presented to a human, never that a choice is executable or an
approval is authorized. Completeness is still a model judgment. Literal quotes
check grounding, not semantic correctness or whether every choice is visible.

## Fresh API regression results

The configured models were used without substitution: `decision` for detection,
`bean` for extraction. Thirteen known synthetic regression screens were tested
twice with fresh responses. This is not a new held-out dataset.

| Measure | Result |
| --- | --- |
| Correct outcome and checked question fields | 24/26 |
| Expected complete questions made available | 12/14 |
| Incorrectly available questions | 0 |
| Negative examples correctly ignored | 8/8 |
| Partial/cropped menus left for inspection | 4/4 |
| Extractor API calls | 16 |
| Provider failures | 0 |

The directory-selection imperative that the visibility guard missed was
recovered in both runs. Both misses were the same bare numbered backend question:
the detector chose `modal_dialog` but `latest_assistant` origin. That combination
remains uncertain, so extraction was skipped. This shows a remaining boundary
between chat questions and dedicated dialogs.

The two incomplete-menu cases reached extraction but were not presented: one
was marked incomplete, and the other failed literal-grounding validation. All
four attempts therefore required inspection. The eight negative attempts
included completed output, an answered question, logged question text, and tool
output containing instructions.

Median time for available questions was **20.112 seconds**; ignored screens took
**1.644 seconds**. The overall median was 13.012 seconds and inspection median
18.367 seconds. Extractor latency remains a material obstacle to a fluid voice
workflow even though detection is fast.

Raw synthetic responses and model/policy/corpus hashes are in
[results/pipeline-fresh-v3.jsonl](results/pipeline-fresh-v3.jsonl) and its
[summary](results/pipeline-fresh-v3.summary.json).

## Separate comparison using recorded extractor outputs

All 36 original extractor screens were rechecked with fresh source detection
and previously recorded extractor outputs matched by exact screen and lifecycle.
Failures were retained; there was no network fallback or cherry-picking.

The pipeline matched 31/36 outcomes, made 16/21 expected questions available,
ignored 9/9 negatives, and made no incorrect question available. There were
25 extractor invocations but **zero new extractor API calls**. Two old extractions
failed grounding, two replayed earlier timeouts, and the numbered backend
question remained uncertain. Three earlier timeout outputs were replayed in
total; one was on a case already expected to require inspection.

The 2.663-second median measures detection plus cached extraction, not live
pipeline performance. These results must remain separate from fresh API results.
See [recorded-output comparison](results/pipeline-replay-v3.summary.json).

## Saved real terminal captures

Two previously recorded Codex screens, cropped to their bottom 30 lines, were
each tested twice with fresh detector and extractor responses. All **4/4**
attempts passed. Both approval attempts returned a complete permission question
with three choices and the correct ordered scopes: allow once, allow persistently,
and deny. Both idle-placeholder attempts were ignored without calling extraction.
Option count, scope, purpose, and input mode were checked; shortcut suffix
formatting was not treated as a semantic label difference.

Approval presentation took **57.645 seconds median**, versus **2.285 seconds**
for ignoring the idle screen. This confirms that latency is especially poor
on a verbose real approval. There were no provider failures in these four calls.
This tiny, repeated sample is a regression check, not evidence of cross-agent
reliability. Captures were recorded earlier, not taken from a live interaction
during this test. Only the [aggregate summary](results/pipeline-private-v3.summary.json)
is stored here; terminal text and extraction responses remain in `/tmp`.

## Reproduce

Python 3.11 or later; standard library only. The scripts read the existing
`[routing]` and `[intent]` configuration. These evaluations call the configured
services and may incur their normal inference costs.

```bash
python3 -m unittest discover -s experiments/dialog_detector -v
python3 -m unittest discover -s experiments/question_extractor -v

python3 experiments/dialog_detector/evaluate_pipeline.py \
  --cases experiments/dialog_detector/pipeline-fresh.json \
  --environment-file ~/.config/orchrd/secrets.env \
  --repeat 2 --output /tmp/pipeline-fresh.jsonl

python3 experiments/dialog_detector/evaluate_pipeline.py \
  --cases experiments/dialog_detector/pipeline-regression.json \
  --replay-results experiments/question_extractor/results/development-v2.jsonl \
  --replay-results experiments/question_extractor/results/holdout-v2.jsonl \
  --output /tmp/pipeline-replay.jsonl
```

Private captures and extraction responses must stay outside the repository.
The tests cover source gating, incomplete extraction, permission scope grading,
invalid quotes, unavailable types, provider failure, and stale snapshots. They
do not establish unattended input reliability. The current result supports
continuing with this pipeline for read-only question presentation; latency,
ambiguous dialog/chat classification, broader fresh evaluation, and answer
submission remain unresolved.
