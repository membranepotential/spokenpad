# Does a starting editor carry spokenpad's marker before it has a UI?

_2026-09-23, at 92fc17d, Neovim 0.12.5 (Manjaro)._

## Question

In pane mode, `NvimSession::attach_existing` stops an editor of spokenpad's
that has no UI (`stop_invisible`, `qall!`). The review of 2026-09-23 asked
whether a press can stop an editor the user has just started with
`spokenpad editor`, in the moment before its terminal UI attaches. An
editor counts as spokenpad's by `g:spokenpad_owner`, which the `--cmd` it
was started with sets, or by a buffer an earlier session pinned, which a
fresh editor does not have.

## Method

Two probes against a Neovim listening on a socket in the scratch directory,
queried with `nvim --server SOCKET --remote-expr`. Every process was reaped
afterwards (`pgrep` found none).

1. A terminal UI, as `spokenpad editor` starts one: `script -qfc "nvim
   --clean --cmd 'let g:x = 1' --listen SOCKET" /dev/null`, sampled every
   20 ms from the moment the socket appeared, 60 samples per run, three
   runs, each sample `exists("g:x") . "/" . len(nvim_list_uis())`.
2. A server that waits for a UI and never gets one, as `:restart` leaves
   one behind: `nvim --clean --embed --cmd "let g:x = 1" --listen SOCKET`
   with a pipe on stdin that nothing writes to, queried after one second
   for `exists("g:x")` and `len(nvim_list_uis())`.

## Data

No recordings; two throwaway Neovim processes per run.

## Results

| Probe | marker set, no UI | marker not set, one UI | marker set, one UI | marker not set, no UI |
|---|---|---|---|---|
| 1, run 1 (60 samples) | 0 | 4 | 55 | 0 |
| 1, run 2 (60 samples) | 0 | 4 | 55 | 0 |
| 1, run 3 (60 samples) | 0 | 5 | 54 | 0 |
| 2 | – | – | – | 1 of 1 |

(Probe 1 has 59 or 60 answers per run: the first sample can precede the
listener.)

## Conclusion

The terminal UI attaches before Neovim runs `--cmd`: no sample showed the
marker without a UI. An editor started with `spokenpad editor` therefore
becomes spokenpad's only once it has a UI, and `stop_invisible` cannot stop
it while it starts. No change was made; see
[decisions.md](../decisions.md#a-starting-spokenpad-editor-is-never-stopped-as-invisible-2026-09-23).
A `nvim.editor` that runs Neovim headless has no UI at all, and in pane mode
is stopped as before.

Probe 2 raises an open question. An embedded server that waits for its UI
has not run `--cmd` either, so it carries no marker. If the server
`:restart` leaves behind waits the same way, `attach_existing` refuses it
as an unrelated editor ("refusing unrelated nvim socket") before it asks
whether it has a UI, and `stop_invisible` never runs for it. The entry
"An editor no pane shows is stopped, not dictated into" says that server
carries the marker, and its test uses a headless editor, which runs `--cmd`.
A real `:restart` in a pane has not been probed.
