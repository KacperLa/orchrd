# Terminal question extractor experiment

Read-only prototype for extracting human-input dialogs from Herdr terminal text
without agent-specific parsers. It is not connected to Orchrd's running services
and never sends keystrokes, answers, or permissions to an agent.

`extractor.py` uses the existing `[intent]` endpoint and model, requests a JSON
schema, then validates the returned structure and literal screen quotations.
Question purpose, answer mode, UI interaction method, and completeness are
separate fields. Literal grounding does not prove the model's interpretation is
correct or that a screen is still current.

## Reproduce

Python 3.11 or later, with no third-party Python dependencies:

```bash
python3 -m unittest discover -s experiments/question_extractor -v

python3 experiments/question_extractor/evaluate.py \
  --config ~/.config/orchrd/config.toml \
  --environment-file ~/.config/orchrd/secrets.env \
  --split holdout --repeat 2 \
  --output /tmp/extractor-evaluation.jsonl

python3 experiments/question_extractor/analyze.py /tmp/extractor-evaluation.jsonl
```

The live evaluation command makes billable calls if the configured endpoint
charges for inference. Credentials are read without executing shell code, and
are not stored in result files. `--response-format json_object` and
`--response-format prompt` are explicit alternatives for endpoints that do not
support JSON Schema. There is no silent fallback or model substitution.

To evaluate captured screens, provide `--cases /path/to/cases.json`. The case
format is demonstrated in `cases.json`. Only `screen` and the lifecycle hint go
to the model; IDs, provenance, split, and expected answers are withheld.
Capture with `herdr agent read <target> --source visible --format text` to avoid
moving the terminal viewport. Keep private captures and model outputs outside
the repository.

## Method

The corpus contains 36 manually authored **synthetic** screens: eight development
cases and 28 held-out cases. It covers arrow menus, numeric and letter shortcuts,
checkboxes, free text, permissions with different scopes, authentication,
incomplete screens, pagination, missing selection styling, answered questions,
ordinary terminal output, and instruction-like text inside logs and options.
These are not recordings of 36 real dialogs or evidence of compatibility with
36 agent UIs.

The first prompt was tested on the development cases, then refined once. V2 was
frozen before the held-out requests; expected answers were not adjusted after
seeing held-out results. V1 prompt, schema, and corpus snapshots are retained in
`results/` because V2 adds an explicit `complete` field. V2 responses include
that field in their scores. The source for V1 is not the current extractor.

The benchmark requests the configured model `bean`, using JSON Schema mode and
the configured 60-second network timeout. Up to three synthetic requests run at
once. The first live-screen check overlapped that run with two additional
requests. Latency therefore measures this endpoint under benchmark load, not
isolated production latency. Socket timeouts are not hard end-to-end deadlines.
Failed requests are not automatically retried; repeats are independently scored.

The exact-pass score includes every annotated field **and** literal evidence
validation. `analyze.py` separately reports question classification, field-level
accuracy, false active requests, unsupported control inferences, failures, and
repeat stability. Conservative abstentions and incorrect actions are counted
separately in the report. A correct question type does not imply a safe input
plan. No keyboard controller is implemented by this experiment.

The generic structured-output request shape follows the
[OpenAI Structured Outputs documentation](https://developers.openai.com/api/docs/guides/structured-outputs).
Schema conformance is checked locally; schema conformance alone does not
guarantee semantically correct extraction.

## Results with `bean`

**The current extractor is not reliable enough to replace the connectors or
drive terminal input.** This is a small feasibility experiment, not a production
accuracy estimate.

| Frozen V2 held-out synthetic evaluation | Result |
| --- | --- |
| Attempts (28 screens, twice each) | 56 |
| Active status, purpose, and answer mode all correct | 49/56 (87.5%) |
| Every annotated field plus literal grounding correct | 37/56 (66.1%) |
| Cases passing both repetitions | 14/28 (50%) |
| Returned outputs passing structure/grounding validation | 49/56 |
| Network timeouts | 4/56 |
| Median response time, excluding failed requests | 21.298 seconds |
| False active questions on synthetic screens | 0 |

All 52 returned synthetic outputs matched the annotated option labels; that
includes empty option lists on negative cases. The remaining problems included
eight unsupported assertions that custom input was unavailable, two hotkey menus
classified as keyboard navigation, three grounding/consistency rejections, and
conservative abstention on a working screen. Permissions with once/persistent/
deny choices were distinguished correctly in the small number of completed
permission cases, but this is insufficient evidence to automate approvals.

V1 and V2 each scored 4/8 on their development runs. V2's extra completeness field
and one round of prompt refinements did not establish a reliable general parser.
The archived V1 corpus is provided to preserve its original scoring contract.

### Real Herdr captures

Two visible screens from the installed Herdr 0.9.3 session were manually
annotated: one live Codex command-approval dialog and one idle Codex prompt.
They were read without sending input or moving the viewport. Raw captures,
annotations, and full model responses remain in `/tmp`; only aggregate scores
are stored here, avoiding publication of another project's terminal contents.

The 102-line captures were each tested twice while the synthetic evaluation was
running: three requests timed out; the fourth misclassified the idle placeholder
`Ask Codex to do anything` as an active free-text question. The quote validator
rejected that response because it also substituted a selection glyph.

After the main run, the same two captures were cropped to their bottom 30 lines
and tested one at a time, without changing the prompt:

- The approval dialog was recognized correctly, with all three option meanings,
  selected item, and displayed `y`, `p`, and `esc` shortcuts. It took 50.289 seconds
  and passed structural and quote validation. The exact grader still marked it
  wrong because it separated the parenthesized shortcut suffixes from the option
  labels. Manual review considers this a harmless labeling difference; the
  original exact expectation has not been changed to inflate the score.
- The idle placeholder was again misclassified as a question, in 24.98 seconds.
  This time it **passed quote validation**, showing that grounded text can still
  be semantically misinterpreted as an unanswered request.

These live tests cover one agent UI and two screens, not cross-agent reliability.
Cropping and reducing local concurrency helped the calls complete but did not
resolve the false positive. No answers or approvals were submitted.

### Implication for the Herdr architecture

The results support continuing the experiment, rather than wiring this version
into the controller. Basic question and option extraction is promising; deciding
whether a request is live, interpreting controls, consistency, and latency still
need work. System One's answer selection remains downstream of this problem:
it cannot repair an incorrectly extracted active question.

The next iteration should separate active-dialog detection and question
extraction from keyboard planning, include generic empty-input placeholders as
negative development examples, and use line references for evidence rather than
asking the model to regenerate decorative text. It needs a larger collection of
real screens from different agents and repeated tests on unseen captures before
it can support removing structured connectors. Current examples must become
development data, not be presented as an untouched hold-out set for that iteration.

The [System One detector follow-up](../dialog_detector/README.md) evaluates that
separation, including lifecycle gates, request-source checks, and real-capture
replays. It remains a read-only experiment.
