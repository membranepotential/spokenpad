# Development tools

These are not usage examples. Cargo builds anything here against the crate,
which is why the project's development tools live in this directory:

- `eval.rs`: word error rate on the five committed clips (`eval-samples/`).
- `corpus.rs`: the author's private corpus through either decode path, with
  the counts WER hides; needs `eval-samples/local/`, which is not in the
  repository.
- `pane.rs`: opens the dictation pane with no daemon, and can write a
  screenshot.
- `screenshot.rs`: composes `docs/screenshot.png` on a private X server.

Each file's header says how to run it.
