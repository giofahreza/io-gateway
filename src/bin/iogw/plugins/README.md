# Animated scene packs

`pixel-cafe.json`, `pixel-office.json`, and `pixel-station.json` are independent optional artwork downloads.
None is embedded in production `iogw` binaries. Install any combination:

```sh
iogw plugins install src/bin/iogw/plugins/pixel-cafe.json
iogw plugins install src/bin/iogw/plugins/pixel-office.json
iogw plugins install src/bin/iogw/plugins/pixel-station.json
```

Implementation is in `../animated.rs`; the normal TUI owns input, authentication
and network polling. Packs are intentionally data, not native/JS code. The
renderer never interprets sprite rows as terminal text or escape sequences.
Email-only speech bubbles and footer text come from sanitized gateway data, not artwork.
Provider/status precedes email. Quota summaries use two rows normally or at most
four with `a`, sorted by percentage used; `+N` marks omitted summary rows. Provider
overview buckets replace redundant per-model detail unless a detail bucket is
more used than every overview bucket. Known label filler is shortened without
changing original detail labels, model versions, periods or scopes. Long labels
yield to numeric percentages when space is tight. Bubble widths follow content
within bounded lanes; long identities reduce columns rather than overlap, and a
single account is centered. Enter retains the full original usage details.
All themes share quota/identity mapping and controls: F4/Ctrl+V changes UI,
`t` changes theme, `b` toggles rounded bubbles, arrows page, Space pauses.
Quota percentages have compact reset countdowns (max two units); unknown resets
remain unknown. Unchanged relative reset labels retain their original deadline
across unrelated observations, rather than restarting on every Summary update.
`r` refreshes the captured credential, `R` reads all dashboard sections, and
Ctrl+R forces all server quotas. The refresh robot stays visible for at least
900 ms. Requests coalesce, reject stale generations and use existing admin auth.
The full-frame room has no outer padding or below-cat status squares/quota bars.
Local-machine hardware telemetry lives in `../system_metrics.rs`, starts only
in the visible animated Overview, and samples off-thread once per second.
Linux CPU/RAM/root-capacity/physical-disk I/O/default-interface traffic use kernel
counters; AMD sysfs and bounded `nvidia-smi` provide best-effort GPU/VRAM.
Unsupported platforms/counters show `N/A`; no new runtime dependency is required.
The café host draws a single wall-mounted, five-unit server cabinet with shared
rails, handles and patch cables. Readings live inside the units; CPU/GPU fans,
occupied memory banks, drive LEDs and switch traffic reflect available readings.
Unknown/idle devices do not fabricate activity. On wide/tall screens the cabinet
sits above even expanded bubbles and left of the rain glass. Compact terminals
use a three-line five-bay rack strip; very small ones fall back to text readouts.
Office keeps its existing hardware station. The scene-pack schema is unchanged,
so existing installed artwork packs do not need to be replaced.
The normal Overview dispatcher owns
`h/j/k/l`, details/help, filters, ordering, quota mode, prompts and management actions.
Modals render above the scene; the other tabs retain their usual UI. Deletion keeps
the ordinary confirmation and authentication gates. Account order/selection is mapped
from the same filtered, deduplicated Overview rows, not a second navigation model.
Rain is a deterministic time-based, layered renderer, clipped to window glass.
The last chosen theme is stored in `selected-theme.json` beside installed packs.
Missing or invalid alternatives are skipped when another installed theme can load.
If no alternative can load, the current scene stays intact with an installation hint.
Café-only, Office-only and Station-only installations work independently.

Pixel Station uses the café's provider-colored articulated cat rig and single-row
seating on a railway platform. Its host layout is isolated in `../station.rs`:
tiled canopy, left train opening, red central vending/service cabinet, right bench,
lanterns, sunbeam and windblown leaves. On roomy screens the cabinet houses real
machine charts/readouts and the right board houses gateway stats; smaller screens
use compact strips. Train movement and foliage are decorative, driven by the same
pausable clock as the cats; they do not infer live account activity. No people,
image protocols, downloaded bitmap assets or additional dependencies are used.

## Scene API 1

The top-level fields are `schema_version: 1`, `id` (`pixel-cafe`, `pixel-office`, or `pixel-station`), `version`,
`palette`, and `sprites`. Unknown fields are rejected. Keep a version matched
to the host API when distributing a pack independently of the binary.

Palette keys are single printable ASCII characters; values are `#RRGGBB`.
`.` is always transparent. Required semantic roles:

| Keys | Meaning |
| --- | --- |
| `b`, `w`, `f`, `g` | outer background, wall, floor, floorboard |
| `t`, `i`, `m`, `o`, `r` | trim, shadow, wood, highlight, dark wood |
| `h`, `c`, `d`, `a` | healthy, cooldown, disabled, attention |
| `v`, `u`, `n` | night sky, glass, skyline/dark outline |
| `z`, `q`, `j`, `e`, `k` | ambient glow, rain, lamp, cream, warm accent |

Additional palette keys may be used by sprites. `h` pixels in a sprite are
tinted with an account/connection status at rendering time (e.g. cat collars).
Station additionally requires `x` (brick/red cabinet), `l` (green foliage/train),
and `s` (sunlit foliage), which its host scenery uses independently of sprites.

Each sprite has `frame_ms` and `frames`: an array of frames, each an array of
equal-width pixel rows. A single-frame sprite is static. All frames within a
sprite must have the same dimensions. Required names:

- Café: `cat`, `cat_light`, `cat_sleep`, `plant`, `mug`, `terminal`, `pastry`, `cat_sign`, `lock`.
- Office: `desk`, `agent`, `plant`, `terminal`, `lock`.
- Station: `cat`, `cat_light`, `cat_sleep`, `plant`, `lock`.

The room layout is host code; adding arbitrary new theme IDs/layouts requires a
host update. Artwork, palette and frame timing can change independently within
these layouts. These are deliberately constrained scene packs, not general extensions.
Bounds: 256 KiB per pack, 64 colors, 32 sprites, 16 frames per sprite, 40×32
pixels per frame, 80–5000 ms per frame. The renderer clips all drawing to its
bounded canvas and lays out the scene again on resize.

## Verify artwork changes

```sh
cargo test --locked --bin iogw
cargo test --locked --bin iogw animated::tests::write_visual_previews -- --ignored
cargo build --release --locked --bin iogw
python3 scripts/test-iogw-performance.py --binary target/release/iogw
```

The ignored preview test writes HTML terminal frames to `target/iogw-previews/` for local
visual review, including all three scenes, compact, tiny, empty and authentication states.
Release CI copies the JSON into `iogw-pixel-cafe.json`, `iogw-pixel-office.json`, and `iogw-pixel-station.json` and includes all three in
`SHA256SUMS`; normal archives and installers stay artwork-free.

Visual inspiration: [Metrocity](https://github.com/Itz-Agasta/metrocity), an
MIT-licensed terminal screensaver. This pack uses original sprites, no upstream
images or fonts, and renders entirely with half-block pixels rather than
requiring Metrocity's Kitty sprite layer.
Station composition reference: [train station](https://www.reddit.com/r/PixelArt/comments/1fd8cg9/train_station/).
Only the composition is adapted; its original image/video is not shipped.
