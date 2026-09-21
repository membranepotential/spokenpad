# Experiments

One file per experiment, named `YYYY-MM-DD-slug.md`. An experiment is any
measurement that answers a question about spokenpad: a benchmark, a replay of
recorded captures, a model or parameter comparison, a spike of a new
technique. Write the file in the same change as the experiment, also when the
result is negative or inconclusive: a result nobody wrote down gets measured
twice.

Experiments from before 2026-09-21 are recorded in
[../decisions.md](../decisions.md), [../evaluation.md](../evaluation.md) and
[../asr.md](../asr.md).

## Format

```markdown
# <Title: the question in a few words>

_Date, commit the code ran at, machine if timing matters._

## Question
What we wanted to know, and why.

## Method
What ran: command lines, configuration, code (path in the repo, or the
scratch code inline when it was not kept).

## Data
Which recordings or inputs, how many, where they live. Private recordings are
named by count and kind only; their content never appears here.

## Results
The numbers, as tables. State the measurement noise when it is known.

## Conclusion
What the numbers show, what they do not show, and what was decided or is
still open. Link the `decisions.md` entry when one followed.
```

## Rules

- Never quote the content of the user's private recordings. The five
  committed `eval-samples` references are the only transcripts in the repo.
- State the sherpa-onnx version, the model and the decoding method for every
  ASR number.
- Timings taken while other work ran on the machine are labelled as such.
