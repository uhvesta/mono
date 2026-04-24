# parley

`parley` is an experimental Rust library for controlling interactive Claude
Code sessions that run inside `tmux` panes while remaining attachable by a
human user.

The library currently focuses on:

- launching or discovering target panes,
- sending prompt text in reliable chunks,
- submitting turns,
- polling pane snapshots via `tmux capture-pane`,
- detecting idle versus busy pane state without sentinel markers.
