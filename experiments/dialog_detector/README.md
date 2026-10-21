# System One dialog detector experiment

Read-only follow-up to the [question extractor experiment](../question_extractor/README.md).
It uses Orchrd's existing `[routing]` endpoint and its configured `decision` model.
It never sends keystrokes, answers, prompts, or permissions to a running agent.
The experiment is not installed in the Orchrd service.

The follow-up [source-aware extraction pipeline](PIPELINE.md) has now been
evaluated separately. The guarded detector below is retained for comparison.

## Approach

The System One classifier chooses `modal_dialog`, `conversational_question`,
`no_question`, or `uncertain` from terminal text and Herdr lifecycle metadata.
Previous screen and lifecycle values, when available, are supplied separately
from the current screen. Fixture IDs and expected answers are withheld.

The controller routes modal candidates only when the lifecycle is `blocked`.
Conversational questions have a separate route, available only for `idle` or
`done`, and cannot activate modal controls. Conflicting state yields uncertainty.
These are routes to future **read-only extraction**, not permission to submit an
answer or execute a keyboard plan.

The current, guarded version adds two questions in the same System One request:
where the candidate request came from, and whether its substantive text is
visible. A user-authored, historical, or absent request is ignored when unblocked
and leaves a blocked agent uncertain. Modal extraction additionally requires a
current-dialog origin and visible request text. Conversation extraction requires
a latest-assistant origin; it does not require a separate modal header.

These are correlated model judgments on the same screen, not independent proof.
Herdr's lifecycle detection can itself be screen-based. No agent-name dispatch,
agent-specific regular expressions, or agent-specific parsers are used here.

Each detection is bound to a fingerprint of the agent ID, instance ID, lifecycle,
state transition sequence, and screen text. Tests check invalidation when any
observed field changes. This does not provide an atomic read-and-submit operation,
or detect an indistinguishable replacement dialog without an observed change.

## Evaluation

The first corpus has eight development cases, 36 reused extractor regression
cases, and 24 new synthetic holdout cases. V1's four-way classifier was frozen
after its development run, before testing the new holdout set three times.
V2 used the resulting failures as development data. Those 24 cases therefore
become rechecks for V2, not fresh holdouts. A separate, previously untested
16-case challenge corpus was frozen before V2's challenge calls.

All synthetic screens are manually authored examples, not a representative
random sample of terminal UIs. Repeats in this run were generally consistent;
they do not constitute additional independent screens. The configured alias
`decision` and request specifications are recorded in each summary. The default
network timeout is 30 seconds, with two concurrent requests per runner; the live
replay sometimes overlapped the synthetic runner.

### V1: lifecycle plus four-way classification

| Evaluation | Classification | Routing | Median response |
| --- | --- | --- | --- |
| Eight development cases | 8/8 | 8/8 | 1.857 s |
| 24 new synthetic screens, three repeats | 63/72 | 66/72 | 1.905 s |
| 36 old extractor screens | 34/36 | 34/36 | 1.895 s |
| Captured Codex screens, full and cropped, three repeats | 12/12 | 12/12 | 4.526 s |

The captured idle placeholder `Ask Codex to do anything` was rejected in every
replay. The actual approval dialog was detected in every replay. These are the
same two captured screens from the earlier experiment, not new real-world
holdouts, and they cover only one agent UI.

The new synthetic cases exposed two false routing behaviors: an answered
user-authored question was treated as a question from the assistant, and a
headerless menu was admitted to modal extraction. Each happened in all three
repeats. A loading screen was classified `no_question` rather than `uncertain`,
but the lifecycle gate correctly kept its route uncertain.

### Guarded version

An initial overly broad visibility question rejected clear positives. It was
simplified to assess request text separately from authorship, and limited to
modal readiness. Archived development results retain these failed attempts.
The classifier definitions and policy were then frozen for the challenge run.

On the 16 new challenge screens, tested three times:

- Routing matched the annotations in **44/48** attempts.
- There were **zero false modal or conversation extraction routes**.
- All nine expected conversational extraction routes were admitted correctly.
- The only expected ready modal screen, `Select the destination directory`, was
  rejected in all three repeats. Therefore this challenge alone demonstrates
  **0/3 modal recall**, not successful general modal detection.
- One request timed out. Returned responses had a median latency of **3.314 s**.
- Raw four-way classifications matched in 33/48 attempts; the source and
  visibility gates prevented several incorrect classifications from routing to
  extraction. Routing accuracy must not be presented as classification accuracy.

The guarded live replay returned the correct route in **9/12** attempts; the
other three timed out. All nine completed calls were correct: five approvals
went to modal extraction and four idle screens were ignored. No false extraction
route was observed, but the live sample remains just two recorded Codex screens.
Its median returned-response latency was **3.85 s**. All cropped-screen replays
completed; the three timeouts occurred on full-screen captures.

The broader rechecks expose the cost of that conservative result:

| Guarded recheck | Correct routes | Genuine modal routes missed |
| --- | --- | --- |
| Earlier 24-screen holdout, now development/recheck data | 20/24 | 3/5 |
| Original 36-screen extractor corpus | 22/36 | **14/24** |

There were no false extraction routes in either recheck, but missing 14 of 24
genuine modal requests is unacceptable for the intended voice workflow. The
visibility guard is therefore a failed readiness experiment, not a production
improvement. It must not be promoted just because it removes false positives.

Raw private captures and annotations remain in `/tmp`. Only classifications,
routes, timings, fingerprints, and aggregate metrics are copied into `results/`.
Archived detector source and classifier specifications preserve the tested
versions. No private terminal content or API credentials are published here.

## Interpretation

System One is substantially faster for this narrow classification task than the
full `bean` extractor was for combined extraction and control interpretation.
The source check addresses the observed empty-placeholder/user-question
failures. The visibility check cuts false admissions, but also misses genuine
imperative menus and remains too conservative for the normal voice flow.

The practical next candidate is source-aware detection followed by read-only
extraction, with actual question text and completeness verified there. A partial
dialog can be inspected without being authorized for keyboard interaction.
That candidate is evaluated separately in [PIPELINE.md](PIPELINE.md); its
results do not inherit this experiment's accuracy numbers.

This supports using a detector before extraction in a future Herdr backend, but
does not establish unattended interaction reliability. Uncertain detections need
a read-only inspection path or an ordinary terminal fallback. The extractor's
option/control errors, provider timeouts, fresh-screen binding, and final input
execution remain separate problems. Nothing in this experiment replaces the
current structured connectors or approves a terminal action.

## Reproduce

Python 3.11 or later, standard library only:

```bash
python3 -m unittest discover -s experiments/dialog_detector -v

# Current guarded version on the separate challenge corpus:
python3 experiments/dialog_detector/evaluate_detector.py \
  --cases experiments/dialog_detector/guard-holdout.json \
  --repeat 3 --output /tmp/dialog-detector-challenge.jsonl

# Earlier cases are regressions for the current version:
python3 experiments/dialog_detector/evaluate_detector.py \
  --split regression --output /tmp/dialog-detector-regression.jsonl
```

The evaluation reads the existing `[routing]` model, endpoint, API key, and
timeout from `~/.config/orchrd/config.toml`; override with `--config`. These
commands call the configured service and may incur its normal inference costs.
There is no provider substitution or automatic retry. Private screen replays
use `--cases /path/to/private-cases.json` and should write their results outside
the repository. Cases without history use `previous: null` or omit the field.

Policy tests run without network access. They cover lifecycle gates, wrong
authorship, missing headers, malformed responses, separation of prior/current
screens, stale observation bindings, and metric accounting. Neither the tests
nor evaluation implement terminal input.
