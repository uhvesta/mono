# concat-videos

A small macOS tool that **losslessly concatenates two or more video clips**
(no re-encode, no quality loss) via a Finder right-click → Quick Action.

The intended use is camera footage split across files in a single shooting
session — e.g. DJI drone clips like `DJI_20260510152334_0047_D.MP4` /
`DJI_20260510152705_0048_D.MP4` — where you want a single contiguous file
without losing any image quality.

## How it works

Uses `ffmpeg`'s **concat demuxer with stream copy** (`-c copy`): the
existing video and audio packets are remuxed into one container without
re-encoding. This is fast (gigabytes per second) and bit-for-bit identical
to the inputs. Only the primary video stream and primary audio stream are
preserved — embedded metadata streams (gyro data, thumbnails) are dropped.

For the concat to succeed, all inputs **must share codec, resolution, frame
rate, and pixel format**. Clips from the same camera as adjacent files in
a sequence virtually always satisfy this.

## Install

```sh
./install.sh
```

This:

- Copies `concat-videos` to `~/.local/bin/concat-videos`.
- Installs the `Concat Videos.workflow` Quick Action to
  `~/Library/Services/` and rewrites it to call the installed binary.

You'll also need `ffmpeg`:

```sh
brew install ffmpeg
```

Restart Finder if the Quick Action doesn't appear right away:

```sh
killall Finder
```

To remove everything later:

```sh
./install.sh uninstall
```

## Use

1. Select two or more video files in Finder.
2. Right-click → **Quick Actions** → **Concat Videos**.
   (On older macOS: Right-click → Services → Concat Videos.)
3. The merged file is written next to the inputs. A macOS notification fires
   on success or failure.

### Output filename

For DJI clips like `DJI_YYYYMMDDHHMMSS_NNNN_X.MP4`, the output keeps the
earliest timestamp and joins the sequence numbers:

| Inputs                                                                  | Output                                |
| ----------------------------------------------------------------------- | ------------------------------------- |
| `DJI_20260510152334_0047_D.MP4`, `DJI_20260510152705_0048_D.MP4`         | `DJI_20260510152334_0047-0048_D.MP4`  |
| `DJI_20260510150025_0045_D.MP4`, `DJI_20260510150408_0046_D.MP4`         | `DJI_20260510150025_0045-0046_D.MP4`  |

For other naming patterns, the basenames are joined with `+`:

| Inputs                  | Output                  |
| ----------------------- | ----------------------- |
| `clip-a.mp4`, `clip-b.mp4` | `clip-a+clip-b.mp4`   |

Files are selected order-independent — inputs are sorted by name before
concatenation so `0048` always comes after `0047`. Existing output files
are not overwritten; a `_2`, `_3`, … suffix is appended.

### CLI usage

The same binary works as a plain command-line tool:

```sh
~/.local/bin/concat-videos clip-a.mp4 clip-b.mp4
```

## Troubleshooting

- **Quick Action doesn't appear in the menu.** Restart Finder
  (`killall Finder`), or check System Settings → Privacy & Security →
  Extensions → Finder / Quick Actions and make sure *Concat Videos* is
  enabled.
- **"ffmpeg not found" notification.** Install Homebrew ffmpeg
  (`brew install ffmpeg`). The Quick Action probes `/opt/homebrew/bin`,
  `/usr/local/bin`, and `/opt/local/bin` directly, so you don't have to
  fix `PATH` for Automator.
- **ffmpeg complains about timebase / non-matching streams.** The inputs
  weren't recorded with identical parameters. Stream copy can't bridge
  that — you'd need a re-encode, which this tool deliberately doesn't do.
- **What got logged?** `~/Library/Logs/concat-videos/concat-videos.log`.
