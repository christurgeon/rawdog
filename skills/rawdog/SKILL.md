---
name: rawdog
description: Convert camera RAW files (ARW, CR2, CR3, NEF, RAF, DNG, and more) to JPEG, TIFF, or PNG with the rawdog CLI. Use when the user wants to convert, resize, cull, rename, import a memory card, make proofs or a client gallery, or deliver photos from RAW files.
---

# rawdog

rawdog is a command-line RAW converter. Run `rawdog --help` for every
option; the help is grouped by task and ends with examples.

## Before you run it

- Check that it is installed: `rawdog --version`. To install from a clone of
  the repo: `cargo install --path .`.
- rawdog reads `~/.config/rawdog/defaults` (Windows:
  `%APPDATA%\rawdog\defaults`) and adds those options to each run. A run
  prints which options came from the file. Use `--no-defaults` when the
  user's defaults (for example a watermark) must not apply.
- For a large or unfamiliar run, run it first with `--dry-run` (`-n`). It
  prints what each file would become and writes nothing.
- Use `--info` to see the metadata rawdog reads (capture time, camera,
  serial, lens, exposure, rating, labels), one tab-separated line per file.
  With selection options, the last column says `selected` or why a file is
  excluded. It converts nothing.

## Safe by default

- An existing output is skipped. `--overwrite` replaces it. `--update`
  replaces it only when the RAW file is newer.
- rawdog never writes over its own input.
- If two inputs would write the same output, rawdog converts nothing and
  reports the conflict.
- The exit status is non-zero if any file failed or an input does not exist.
  The other files still convert. A selection that matches nothing exits 0.

## Common tasks

```sh
rawdog photo.ARW                                  # photo.jpg next to the RAW
rawdog -R /Volumes/CARD -o ./jpegs                # a whole card, folders mirrored
rawdog ./shoot -o ./web -r 2048 --max-size 1MB    # long edge 2048 px, files of 1 MB or less
rawdog ./shoot -o ./out -r 1080 -r 2048 -r full   # several sizes in one run
rawdog ./shoot -o ./prints -f tiff --dpi 300      # 16-bit TIFF for print
rawdog ./shoot -o ./proofs -r 1600 --fast --gallery   # quick proofs and gallery.html
rawdog ./shoot -o ./proofs --preview              # camera's embedded JPEG; fastest
```

Select files (filters combine; ratings and labels come from XMP sidecars,
for example from Lightroom or Capture One):

```sh
rawdog ./shoot -o ./keepers --min-rating 3
rawdog ./shoot -o ./out --label red,green --since 2026-05-01 --until 2026-05-03
rawdog ./shoot -o ./final --picks client-picks.txt   # names from a client list
rawdog -R ./card -o ./out --camera "ILCE-7M4" --lens "24-70"
```

Name and import (`--name` paths are relative to `-o`):

```sh
rawdog -R /Volumes/CARD -o ~/Pictures --name "{date}/{model}_{stem}"
rawdog -R /Volumes/CARD -o ~/Archive -f original --name "{year}/{date}/{stem}" --manifest
rawdog -R /Volumes/A /Volumes/B -o ./wedding --name "{date}/{seq}_{stem}"
rawdog -R /Volumes/CARD -o ./cull -r 2048 --fast --name "{burst}/{stem}"
```

Template fields: `{stem}` `{date}` `{year}` `{month}` `{day}` `{time}`
`{model}` `{lens}` `{serial}` `{shutter}` `{aperture}` `{iso}` `{focal}`
`{rating}` `{label}` `{seq}` `{burst}`. A missing value becomes `unknown`.
With `--name`, `-R` does not mirror folders, so include a field that keeps
equal file names apart, such as `{date}` or `{seq}`.

Develop:

- `-e 0.7` exposure in EV. `--wb 5500` sets white balance in kelvin, and
  `--tint 1.1` (0.5 to 2; above 1 is more magenta) adjusts it.
  `--wb-from grey-card.ARW` uses the white balance of a reference frame.
- `--look camera` (default) or `--look flat`. `--sharpen 0.5`.
- `--aspect 4:5` crops to a ratio. `--watermark logo.png`, limited to some
  sizes with `--watermark-sizes`.

Metadata:

- `--artist`, `--copyright "(c) {year} J. Doe"`, `--keywords`.
- `--strip-gps` or `--strip-metadata` for public delivery.
- `--time-shift +1h` (or per body: `"7M4=+3m12s"`) fixes a wrong camera clock.
- `--gpx track.gpx` adds GPS from a track. `--gpx-tz +02:00` gives the camera
  clock's offset when the file does not record it.
- `--keep-rating` copies the rating and label into the output.

Bursts and brackets: `--burst-pick ev0` keeps the normal exposure of each
bracket; `--burst-pick sharpest` keeps the sharpest frame. `--burst-gap`
(seconds, default 2) sets where a burst ends.

Runs and scripts:

- `--report run.csv` (or `.json`) lists each file's status and time.
- `--files-from list.txt` (or `-` for stdin) takes paths from another
  program, as one run.
- `--watch` keeps converting new files as they arrive (tethered shooting).
  Stop it with Ctrl-C.
- `-j 1` converts one file at a time when memory is short. `--quiet` prints
  errors only.

## Limits

- Nikon High Efficiency NEF (Z 8, Z f) and some phone DNG files do not
  decode. rawdog reports an error for each such file.
- `--gallery` needs `-o` and JPEG or PNG output.
- `--info` cannot be used with `--watch`, `--report`, or `--dry-run`.
