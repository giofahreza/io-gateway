# Interactive 3D terminal UI

The optional 3D Café keeps the Rust management client and uses a small Python
Panda3D helper to render the scene on the GPU. It displays live gateway accounts
as selectable four-legged cats walking around a 20 × 20 café with walls on all four
sides, and custom models as a robot barista at the bar. Colors and lighting are
kept soft and warm on purpose. Account details, authentication,
refresh, enable/disable, filtering, ordering, and confirmations use the existing
Rust controls.

## Setup and launch

Use a terminal with true color and Unicode half-block support, a working hardware
OpenGL driver, and Python with Panda3D 1.10.16. This version has been tested on
Linux with Python 3.12 and an Intel Mesa GPU.

For an isolated Python installation using [uv](https://docs.astral.sh/uv/):

```sh
uv venv --python 3.12 "${XDG_DATA_HOME:-$HOME/.local/share}/iogw/panda3d"
uv pip install --python "${XDG_DATA_HOME:-$HOME/.local/share}/iogw/panda3d/bin/python" panda3d==1.10.16

cargo build --release --locked --bin iogw
./target/release/iogw tui --3d
```

The helper automatically uses the private runtime installed above, then falls
back to `python3` (`python` on Windows). On Windows, the private runtime location
is `%LOCALAPPDATA%/iogw/panda3d/Scripts/python.exe`.
`IOGW_PANDA3D_PYTHON` overrides runtime discovery with a Python executable; it is
not a shell command.
Keep your usual gateway URL and authentication configuration.

You can also start `iogw` normally and press **F5** or **Ctrl+G**.
The built-in mesh needs no pixel-art pack. **F4 / Ctrl+V** opens the separate
pixel-art mode, which still requires its optional packs.

## Controls

| Input | Action |
| --- | --- |
| F5 / Ctrl+G | Enter 3D, or return to the normal UI and stop the helper |
| Left mouse click | Select a cat, robot, or visible label |
| Left mouse drag | Orbit the camera (left/right) and tilt it (up/down); a drag never selects. A wall between the camera and the room drops to a low wainscot so the café stays visible |
| Enter | Open the selected account or custom-model details |
| Left/Right or h/l | Select the previous/next actor |
| Up/Down or j/k | Navigate the focused account/model section |
| PageUp / PageDown | Previous/next page (up to seven actors per page) |
| Home / End | First/last actor |
| [ / ] | Orbit the camera all the way around |
| + / - or mouse wheel | Zoom from the whole floor down to a close-up |
| 0 | Reset the camera |
| a | Switch the quota view; labels show one or all limits plus credits, as in the animated UI |
| Space | Pause/resume motion |
| b | Show/hide labels |
| Esc | Close a modal; otherwise leave 3D |
| ? | Show all shared management commands |

Clicking selects an actor. Use the existing keys to open details or perform an
action. Clicks and drags behind a modal are ignored. Labels carry the same rows as
the animated UI's bubbles, with sanitized gateway data: provider, status,
identity, quota windows (with reset countdowns, highlighted at 90%), credits in the
`a` view, and the custom-model list with its selection marker. Provider colors, status collars, and a gold selection base help
identify actors. Each cat patrols its own territory, walking with a four-paw
step and pausing to look around, blink, and swish its tail. Territories never
overlap, so cats cannot end up on top of one another. Labels follow them, and clicking a cat or its label selects it. Walking is
decorative and does not imply account activity. Space pauses everyone in place, and
`IOGW_REDUCED_MOTION=1` starts animation paused.

## Rendering and failures

Rust owns input, account identities, text, network requests, and picking.
The helper receives only embedded geometry and visual parameters, including each
actor's position, heading, and gait phase. It returns RGB frames through pipes,
which Rust displays inside the terminal using half-block characters. It does not
open another window or play a recorded animation.

Frames are rendered at 3× the terminal's half-block pixel grid and averaged down
when drawn, so edges and thin details are sharper. A terminal cell still shows
two pixels; finer glyphs such as braille would need extra terminal support.

Rendering runs off the input thread, with one frame in flight, at most 20 render
requests per second, and at most 1920 × 1200 rendered pixels (640 × 400 shown). Paused scenes request another
GPU frame only when their visible state changes. Camera movement and account
selection still work while paused.

If Python, Panda3D, or a hardware context is unavailable, the client returns to
the normal UI with a setup hint. Software rasterizers such as llvmpipe are
rejected. Startup and frame timeouts cover stalled helpers, and leaving 3D or
quitting kills and reaps the helper.

This mode adds Python, engine, driver, and framebuffer memory. The normal and
pixel-art views remain available for lower resource use. Earlier standalone
prototype measurements are not measurements of this integrated implementation.

Linux/Intel GPU rendering and interaction have been exercised locally. macOS,
Windows, other GPU drivers, and remote GPU setups still need validation. Over SSH,
the GPU and Python runtime must be available on the machine running `iogw`.

## Pixel-accurate display (sixel, opt-in)

By default the scene is drawn with half-block characters, so it can never be
sharper than the terminal's cell grid (two pixels per cell). A terminal that
supports [sixel](https://en.wikipedia.org/wiki/Sixel) graphics can show the
rendered frame itself, pixel for pixel:

```sh
IOGW_3D_SIXEL=1 iogw tui --3d
```

You need a sixel terminal that reports its pixel size (for example foot or
WezTerm). Inside tmux, also set `set -g allow-passthrough on`: tmux's own sixel
storage cannot hold an image that keeps changing under text, so the images are
passed through, and with `on` tmux forwards them only while the window is on
screen. The header shows `· sixel` when the mode is active. If a requirement is
missing, it says why and the half-block display is used instead.

The scene is rendered at exactly the pane's pixel size (up to 1920 × 1200;
larger panes use half-blocks) and only the regions that changed are sent.
Labels and modals stay ordinary text: the image is never painted over them and
returns when they go away. A paused scene sends nothing; inside tmux the whole
image is repainted every couple of seconds in case tmux redrew over it.

Measured with foot 1.28 inside tmux 3.8 in a 159 × 69 pane: the screen matched
the rendered frame exactly (no pixel off by more than one colour step), at about
0.35 MB/s and about 2% of a tmux CPU core while animating. Only a model of a sixel
terminal (text over a cell removes the image there; a newer image replaces an
older one) and tmux itself were exercised; confirm on your own terminal.

## Validation

```sh
cargo test --locked --bin iogw
cargo build --release --locked --bin iogw
python3 scripts/test-iogw-performance.py --binary target/release/iogw
python3 scripts/test-iogw-3d.py --binary target/release/iogw --python "${XDG_DATA_HOME:-$HOME/.local/share}/iogw/panda3d/bin/python"
```

The 3D integration suite uses a demo HTTP gateway and a real POSIX PTY/GPU. It
checks account picking (by clicking the labels of walking cats) and actions,
custom-model details, paging, camera controls and click-drag orbit, the `a` quota
view, walking, resize, pause, authentication, explicit startup, missing runtimes, software GPU
rejection, helper failure/cleanup, and sixel mode with its fallbacks. Its process checks require Linux `/proc`.
