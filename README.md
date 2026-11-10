# slopshot 📸🤖

> A tiny photo culler for Sony (`.ARW`) and Canon (`.CR3`) RAW files, and the camera clips shot alongside them. Open folder, star, pick, reject, trim, export. That's it.

---

## ⚠️⚠️⚠️ HUGE DISCLAIMER ⚠️⚠️⚠️

```
 ██████████████████████████████████████████████████████████████
 █                                                            █
 █   THIS ENTIRE APP IS VIBECODED.                            █
 █                                                            █
 █   Every line was written by an AI. I typed English.        █
 █   The AI typed Rust. Nobody read the borrow checker        █
 █   errors except the AI, and it fixed them by vibes.        █
 █                                                            █
 █   I made this because I couldn't find a simple ARW         █
 █   culler that didn't want a subscription, a catalog,       █
 █   or my firstborn. So I prompted one into existence.       █
 █                                                            █
 █   NO SUPPORT. NO WARRANTY. NO ROADMAP. NO SLA.             █
 █   NO PROMISES IT WON'T EAT YOUR PHOTOS (it shouldn't,      █
 █   it only copies, but hey — back up your stuff).           █
 █                                                            █
 ██████████████████████████████████████████████████████████████
```

```
        ,-~~-.___.
       / |  '     \          "I have no idea what I'm doing"
      (  )         0              — the dev, about this codebase
       \_/-, ,----'
          ====           //
         /  \-'~;    /~~~(O)
        /  __/~|   /       |
      =(  _____| (_________|
```

**FAQ**

| Question | Answer |
|---|---|
| Does it work? | 🔥🐶☕ *"This is fine."* It works on my machine™ (Sony a6400 and a6700, Linux) and on someone else's machine™ (Canon EOS RP, Windows 11). |
| Will you fix my bug? | lol. lmao, even. |
| Can I open an issue? | You can. I may read it. An AI may fix it. Neither is guaranteed. |
| PRs welcome? | Sure, but you're reviewing AI slop with more AI slop. Godspeed. One has been merged so far: an AI wrote it, an AI reviewed it, and it got *better* in review. Nobody involved has a pulse. 🤖🤝🤖 |
| Is the code good? | It has comments. It compiles. It passed the vibe check. It now also passes the overflow check (ask the cursed 120-byte CR3). |
| Why is it called slopshot? | Because it's AI slop that shoots (photos). Read the room. |
| Should I use this for my wedding clients' photos? | Back them up first. Then maybe. Then back them up again. |

> **tl;dr:** 100% AI-generated, 0% support. If it helps you, nice. If it breaks, you get to keep both pieces.

---

## Features

Despite all of the above, it actually does stuff:

### Culling
- **Open a folder** of `.ARW` / `.CR3` files (`File → Open folder…`, or pass the folder as an argument)
- **Open a whole tree** with `File → Open folder recursively…`: every subfolder, however deep. The name filter matches folder names too, and sorting by name keeps each folder's photos together.
- **Gallery grid** with thumbnails, filename, capture date and an EXIF summary (`50mm  f/2.8  1/125s  ISO 1000`)
- **Star ratings** (1–5), **pick** ✔ and **reject** ✖ flags. Rejected shots get dimmed so they stop judging you. `P`/`X` toggle, so hitting one twice is a no-op, not a commitment.
- **Cull with a filter on**: filter to *Unflagged*, open the large view and start pressing `P`/`X`. A photo that drops out of the filter stays on screen (greyed out in the film strip, in its old slot), and the arrows carry on to its old neighbours instead of teleporting you back to photo #1. Changed your mind? Press the key again and it's back.
- **Progress bar** in the status bar: where the current photo sits in the list, and how many come after it (`3 / 8 · 5 left`). It follows your filter and sort.
- **Ratings persist** in a small `.slopshot.json` file in the photo folder. Your RAWs are never modified.
- **A recursive open keeps one file**, in the folder you opened, with paths pointing into the subfolders (`{"recursive": true, "marks": {"100MSDCF/DSC00001.ARW": {...}}}`). The first recursive open also pulls in any `.slopshot.json` already sitting in the subfolders. Open that folder normally later and slopshot asks whether you want the subfolders too. Answer "Just this folder" and the subfolders' marks are still kept in the file.
- **In-camera ratings** are imported if you starred shots on the camera
- **Export** copies every picked file to a folder of your choice, with a progress bar. Existing files are skipped, never overwritten. After a recursive open the subfolders are recreated in the destination, because your camera has absolutely taken more than one `DSC00001.ARW`.

### Video clips
Camera clips (`.MP4`, `.MOV`, `.MTS`) show up in the same grid as the photos, with a ▶ badge and their length, and get the same stars, picks, rejects, filters and sidecar. Everything video goes through [ffmpeg](https://ffmpeg.org), which has to be installed (see [Building](#building)).

- **Player** in the large view: play/pause with sound, frame stepping, ±5 s jumps, a timeline to scrub. While a seek catches up, scrubbing shows the clip's keyframes (decoded in the background at thumbnail size), so dragging along the timeline doesn't wait on the decoder.
- **Trim**: set the start and end with `[` and `]`, the buttons, or by dragging the handles on the timeline. Only the kept part is exported.
- **Color**: exposure, contrast, saturation, white balance (temperature/tint), and **levels** you drag right on the histogram (black point, midtones, white point), plus **Auto levels**. A second histogram shows the result. Copy a clip's look and paste it onto the next one; `\` flips back to the original. The preview and the export use the same color math: the grade is baked into a 3D LUT for ffmpeg, so the file looks like what you saw.
- **Export** of picked clips: trim, color and compression in one pass. Encoders are picked from what your machine can actually run (Intel Quick Sync, NVIDIA NVENC, or x265/x264 on the CPU), defaulting to HEVC at 1080p on the GPU; 4K drone footage comes out at a fraction of its size. Clips are never upscaled. The capture date (moved to the trim start), GPS location and file date are kept, so photo libraries like Immich file the clip in the right place. Or skip re-encoding and just cut the clip (instant, but the cut lands on a keyframe and color isn't applied).
- Edits live in the same `.slopshot.json`, next to the clip's rating. The original files are never touched.

### Filter & sort
- Filter by **type** (photos / clips), **name**, **minimum rating**, **flag** (picked / rejected / unflagged / not rejected) and **date range**. Partial dates work (`2026-10` = all of October).
- Sort by **name**, **date** or **rating**, ascending or descending

### Viewing
- **Large view** with **zoom** (scroll, `+`/`-`, pinch) and **pan** (drag)
- **On-screen arrows** at the edges of the large view, for going back and forth with the mouse
- **Full resolution when zoomed**, from the best available source:
  1. the camera JPG next to the RAW (RAW+JPEG shooters)
  2. the full-size JPEG embedded in the RAW (a6700-generation Sony bodies, every CR3)
  3. a real RAW develop via [`rawler`](https://crates.io/crates/rawler) if neither exists
- **Focus point overlay**: shows where the camera focused (eye/face box on supported Sony bodies; the face box or in-focus AF points on Canon). `Z` zooms to 100% *right on the eye*, and stepping through a burst while zoomed recentres on each shot's focus point. Great for "which one is actually sharp".
- **EXIF info panel**: camera, lens, focal length (and full-frame equivalent), aperture, shutter, ISO, exposure comp, resolution, file size
- **Film strip** 🎞️ under the large view: the current photo with 3–6 neighbours each side (as many as fit), in the grid's filter/sort order, with flags and stars. Click a frame to jump to it, `S` to hide it. It's drawn from the thumbnails, so it doesn't care what brand your camera is. It does have sprocket holes, because a model that has never seen film decided film needs sprocket holes.
- **Display brightness**: an exposure slider plus **Auto** brightness, for those night shots where the preview is basically a black rectangle. Display only; files are never touched.

### Speed
- Never decodes the RAW just to browse. It reads the JPEG previews the camera already embedded.
- Thumbnails decode in the background on half your cores at low priority, so your machine stays usable
- Neighbouring photos are pre-decoded, so arrow-keying through the large view is instant
- On a6700-class bodies and CR3s the full-res JPEG is fetched once you pause on a photo, so zooming feels instant
- Parsing is a hand-rolled TIFF/EXIF/MakerNote reader that needs ~2 reads per ARW (3–4 per CR3, which hides its metadata a box or two deeper). Nice on slow USB drives. An earlier version of this README said "~2 reads per file" for CR3 too. A reviewer counted. The AI has been informed.

---

## Keyboard shortcuts

| Key | Action |
|---|---|
| `←` `→` `↑` `↓` | Move selection / previous–next photo (also from a photo that just left the filter). The on-screen arrows in the large view do the same. |
| `1`–`5` | Rate · `0` clears |
| `P` / `X` / `U` | Pick / reject / unflag · `P`/`X` again clears it |
| `Enter` / `Space` / double-click | Open large view |
| `Esc` | Back to grid |
| `Z` | 100% at the focus point / back to fit |
| Double-click (large view) | 100% where you clicked / back to fit |
| Scroll, `+`, `-` | Zoom |
| Drag | Pan |
| `A` | Toggle auto brightness |
| `F` | Toggle focus point overlay |
| `I` | Toggle info panel |
| `S` | Toggle film strip (large view) |

With a clip in the large view:

| Key | Action |
|---|---|
| `Space` / `K` / click the picture | Play / pause |
| `,` `.` | Previous / next frame |
| `J` `L` | Back / forward 5 seconds |
| `[` `]` | Trim: keep from here / up to here |
| `M` | Mute |
| `\` | Show the original (no color edits) |
| Double-click a slider or levels handle | Reset it |

---

## Building

You need Rust **1.89 or newer** (`rawler` refuses anything older; last built with 1.99.1).

```sh
git clone https://github.com/the-rectifier/slopshot.git
cd slopshot
cargo build --release
./target/release/slopshot                     # opens with a folder picker
./target/release/slopshot ~/Pictures/shoot    # or open a folder directly (asks first if it has a recursive sidecar)
```

To install it on Linux, with a launcher entry:

```sh
cargo install-desktop
```

That puts the binary in `~/.local/bin` and adds `applications/slopshot.desktop` and the icon (`icons/hicolor/{scalable,256x256}/apps/slopshot.*`) to `~/.local/share` (or `$XDG_DATA_HOME`). To remove it, run `cargo uninstall --root ~/.local slopshot` and delete those files.

On Linux the folder picker goes through the XDG desktop portal (`xdg-desktop-portal` plus a backend for your desktop). Windows works: a contributor ran it on Windows 11, so for once the README was *underselling* itself. macOS *should* work since every dependency is cross-platform. That has never been tested. See disclaimer. 🙃

The first build takes a while because `rawler` is chunky. Go make coffee. ☕

Clips need `ffmpeg` and `ffprobe` on your `PATH` at runtime (any recent build; `apt install ffmpeg`, `scoop install ffmpeg`, `brew install ffmpeg`), or set `SLOPSHOT_FFMPEG` to the ffmpeg executable. Without it, photos work as before and clips show "no preview". On Linux the player's sound goes through ALSA, so building needs the ALSA headers (`libasound2-dev` / `alsa-lib-devel`).

There are tests now, which is a sentence nobody expected to read in this README:

```sh
cargo test
```

They cover CR3 box walking, a whole synthetic CR3 end to end, XMP ratings, Canon AF maths, film strip layout, culling with a filter on, recursive opening and its sidecar, export name clashes, clip probing, the color math (preview and export LUT agreeing), clip export through the real ffmpeg with every encoder found, and one cursed file that used to crash a worker thread. They were also written by an AI. Obviously.

---

## Tested on

- Sony **a6400** (RAW+JPEG, landscape night shots)
- Sony **a6700** (RAW only, portrait cosplay shots)
- Linux (Wayland, Hyprland)

Contributor-tested (the maintainer has never touched one):

- Canon **EOS RP** (CR3, EF 24-105, beach shots in Chania, Windows 11). CR3 support was contributed by a *different* AI, prompted by a *different* human, and reviewed by yet another AI. The slop is now multi-vendor. 🤖🤝🤖

Other Sony bodies will probably work, since they share the ARW format. Other CR3 bodies (EOS R/M/90D and newer) share one layout too, so they *should* work. "Should" is doing a lot of heavy lifting there. NEF/RAF/CR2 aren't opened at all. If you want those, you know what to do (prompt it yourself 🤖).

Before you blame the AI, check what slopshot actually reads from your files (preview size, EXIF, rating, focus point):

```sh
SLOPSHOT_SAMPLES=~/Pictures/shoot cargo test --release samples -- --ignored --nocapture
```

---

## Known limitations

- Only Sony `.ARW` and Canon `.CR3` photos (plus video clips)
- The RAW-develop fallback has no camera tone curve, so it looks flatter than the camera JPEG. It's brightness-matched to the preview, but it isn't a raw editor.
- Mirrored orientations (EXIF 2/4/5/7) aren't handled. Your camera almost certainly never writes them.
- One full-res image is kept in memory at a time (~100–200 MB for 24–26 MP)
- Marks made in a recursive open stay in the root's file. Opening a subfolder on its own doesn't see them, and once the root's file exists, marks made in a subfolder on its own aren't picked up by it.
- "Every subfolder" means every subfolder, including hidden ones like `.Trash-1000`.
- Clips: color editing is for regular (SDR, Rec.709) footage. HDR clips (HLG/PQ) can be trimmed and compressed and stay HDR, but the color panel is off for them, and there's no LUT loading for log footage (S-Log, C-Log) yet.
- Jumping to a new spot in a clip takes about half a second for 4K (a fresh decode from the previous keyframe); the keyframe cache covers scrubbing in the meantime.
- Clip export uses Intel/NVIDIA GPU encoders or the CPU. AMD and Linux VAAPI encoders aren't wired up, so on those machines it falls back to x265/x264.

---

## License

[MIT](LICENSE). Do whatever you want with it. If it breaks, you were warned in a box made of block characters (and in the license, in all caps).

```
  ┌──────────────────────────────────────────────┐
  │  slopshot: proudly mass-produced by several  │
  │  models that have never held a camera        │
  └──────────────────────────────────────────────┘
```
