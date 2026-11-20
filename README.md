# RawDog

Raw files in, images out. No Lightroom, no fuss.

A no-nonsense RAW converter supporting JPEG, TIFF, and PNG output. Built for Sony ARW; it also accepts most other camera RAW formats (DNG, CR2, CR3, NEF, RAF, ORF, RW2, PEF, and more).

Two decoder libraries are used. [rawler](https://github.com/dnglab/dnglab) is tried first: it knows cameras up to now (for example the Sony a7 IV, a7R V, and a1), Canon CR3, and the exact sensor crop of most bodies. [rawloader](https://github.com/pedrocr/rawloader) decodes a few files that rawler does not. For CR3 files the main metadata fields are copied (camera, lens, exposure, date, GPS).

Not supported: Nikon High Efficiency NEF (Z 8, Z f), and some phone DNG files (Samsung Galaxy S23 Ultra). rawdog reports an error for these instead of writing a broken image.

## Installation

```sh
cargo install --path .
```

## Usage

Run `rawdog --help` for every option.

### Basics

Convert a file, or every RAW file in a directory, to JPEG next to the originals:

```sh
rawdog photo.ARW
rawdog ./photos/
```

Write to another directory. With `-R`, subdirectories are converted too, and their layout is mirrored under the output directory:

```sh
rawdog ./photos/ -o ./jpegs/
rawdog -R ./photos/ -o ./jpegs/   # photos/2024/a.ARW -> jpegs/2024/a.jpg
```

Files given directly on the command line go straight into `-o`. If two inputs would write the same output file, rawdog reports the conflict and converts nothing.

### Size, format, and quality

```sh
rawdog photo.ARW -r 2048          # long edge 2048 pixels
rawdog photo.ARW -q 85            # JPEG quality 85 (default 92)
rawdog photo.ARW -f tiff          # 16-bit TIFF
rawdog ./photos/ -f png -r 2048   # 16-bit PNG, resized
```

Resized images look soft; `--sharpen` adds output sharpening, with an optional amount from 0 to 3 (default 0.5):

```sh
rawdog ./trip -r 2048 --sharpen
```

For a contest or website with a file size limit, `--max-size` lowers the JPEG quality of each file until it fits. `--quality` stays the highest quality used. Sizes are decimal (1 MB is 1,000,000 bytes), so the limit is met in either convention. A file that is too large even at the lowest quality fails with its size; use `-r` to make it smaller:

```sh
rawdog ./contest -r 3000 --max-size 2MB -o ./entries
```

Every output carries an sRGB colour profile, so colours are the same in every viewer.

### Developing

Brighten a dark batch by two thirds of a stop and set the white balance to 3400 K:

```sh
rawdog ./reception --exposure 0.67 --wb 3400
```

`--exposure` rolls off highlights instead of clipping them. `--wb` replaces the as-shot white balance of every file; add `--tint` to correct a green or magenta cast (above 1 is more magenta).

### Culling and proofs

`--preview` writes the JPEG preview that the camera embedded in each RAW file, with the camera's own colours. It is the fastest way to look through a card. The size depends on the camera: 1616x1080 for Sony bodies, full resolution for many Canon and Nikon bodies.

```sh
rawdog -R /Volumes/CARD --preview -o ./proofs
```

`--fast` develops the RAW data but scales during decoding: several times faster than the default, and slightly softer. It needs `-r`:

```sh
rawdog ./card -r 2048 --fast -o ./proofs
```

`--gallery` writes `gallery.html` into the output directory: a page of the output images with file name, capture time, camera, and rating. It works offline and on a phone. Large images are shown as small copies in `gallery-thumbs/`, and each links to the full image, so the page stays fast with full-size output:

```sh
rawdog ./wedding -r 1600 --fast -o ./proofs --gallery
```

`--watermark` blends a logo into the bottom-right corner of every image, for proofs and web exports. Use a PNG with a transparent background. The long side of the logo is a fifth of the image's short edge, so it has the same size on landscape and portrait images:

```sh
rawdog ./wedding -r 2048 --watermark ~/logo.png -o ./web
```

### Selecting files

Convert only your picks. Ratings come from sidecar XMP files written by Lightroom, Capture One, or darktable, or from the rating set in the camera:

```sh
rawdog -R /Volumes/CARD --min-rating 3 -o ./picks
```

A rotation set while culling in Lightroom or Capture One is stored in the same sidecar file, and rawdog applies it in place of the camera's orientation. darktable keeps rotation in its edit history, which rawdog does not read.

Or convert by colour label, from the same sidecar files. Lightroom and Capture One store the label name (`Red`, or the text set in Lightroom's label set); darktable's five colours are read as red, yellow, green, blue, and purple. A file with any of the listed labels is converted:

```sh
rawdog -R /Volumes/CARD --label red,green -o ./selects
```

Convert one day of a multi-day card, from one of two bodies:

```sh
rawdog -R /Volumes/CARD --since 2026-11-14 --until 2026-11-14 --camera 7M4 -o ./day2
```

Convert the frames a client chose from the gallery. Each image in `gallery.html` has a tick box, and the page lists the names of the ticked images to copy and send back; ticks are kept when the page is opened again in the same browser. Paste the names into a file; names may be separated by lines, commas, or spaces, and may keep their extension:

```sh
rawdog -R /Volumes/CARD --picks picks.txt -o ./finals
```

Names are compared with the RAW file names, without letter case, so proofs made with `--name` need their `{stem}` in the name. rawdog warns about each listed name that matches no RAW file, so a typing error is not missed.

Dates are the camera clock's local date. A file without the metadata that a filter needs is excluded; `--dry-run` lists each excluded file with the reason.

### Naming and importing

`--name` sets the output path from a template. Import a card into folders by capture date, with the camera model in each file name:

```sh
rawdog -R /Volumes/CARD -o ~/Pictures --name "{date}/{model}_{stem}"
```

Template fields: `{stem}` (input file name without extension), `{date}` (2024-03-09), `{year}`, `{month}`, `{day}`, `{time}` (070542), `{model}`, `{lens}`, `{serial}` (the body's serial number, to keep two bodies of one model apart), `{rating}` (0 for unrated), and `{seq}`. A file without that metadata gets `unknown`; some cameras, such as Sony bodies, do not record the serial number in standard metadata. Characters that are not allowed in file names on exFAT drives or Windows (`/ \ : | * ? " < >`) become `_`.

`{seq}` numbers the files of the run in capture order (0001, 0002, ...). Two bodies at a wedding both write `DSC00001`, and a burst puts several frames in one second; with `{seq}` the files of both cameras get one sequence and never share a name:

```sh
rawdog -R /Volumes/CARD_A /Volumes/CARD_B -o ./wedding --name "{date}/{seq}_{stem}"
```

If a camera clock was set wrong, `--time-shift` corrects the capture time: for every camera (`+1h`), or per camera model to line up two bodies before numbering them:

```sh
rawdog -R /Volumes/CARD_A /Volumes/CARD_B -o ./wedding --name "{date}/{seq}_{stem}" --time-shift "7M4=+3m12s,Z 6=-1h"
```

Offsets use `d`, `h`, `m`, and `s`, such as `-1d`, `+30m`, or `+1h2m5s`. A camera model matches like `--camera`; a camera that no rule names is not shifted, unless one offset is given without a model. The corrected time is used by the template, `--since`, `--until`, and the gallery, and replaces the capture dates in the output metadata. With `-f original` the RAW files are copied unchanged; only their names use the corrected time.

The numbers depend on which files the run converts, so adding files to a later run renumbers the files taken after them. For that reason a `{seq}` run refuses to start when any of its output files exists; use `--overwrite` to replace them, or write to an empty directory.

The template path is below the `-o` directory. Without `-o` it is below the directory of each input file. With a template, `-R` does not mirror the input tree, so include `{date}` or another field that keeps equal file names apart.

`-f original` copies the RAW files themselves instead of converting them, for example into an archive by date:

```sh
rawdog -R /Volumes/CARD -f original -o ~/Archive --name "{year}/{date}/{stem}"
```

Each copy is compared byte for byte with the original and keeps its modification time; the card is never changed. XMP sidecars (`NAME.xmp` from Lightroom or Capture One, `NAME.ARW.xmp` from darktable) are copied with their RAW file, and a sidecar that already exists in the archive is kept. With `--with-jpeg`, the JPEG or HEIF that a camera set to RAW+JPEG wrote next to each RAW file (`DSC1.JPG` or `DSC1.HIF` for `DSC1.ARW`) is copied and checked in the same way. Image options such as `-r` cannot be used with `-f original`.

### Metadata

Camera, lens, exposure, and date metadata are copied to JPEG, PNG, and TIFF output. For the web, remove the location and add your name:

```sh
rawdog ./trip -r 2048 --strip-gps --artist "J. Doe" --copyright "(c) {year} J. Doe"
```

`{year}` in `--copyright` becomes the year the photo was taken (after `--time-shift`), so a defaults file stays right in a new year and for old shoots. A file without a capture date gets the current year.

`--strip-metadata` copies no metadata at all.

### Large runs and scripts

```sh
rawdog -R ./card -o ./jpegs --dry-run        # show what would happen; write nothing
rawdog ./photos/ --update                    # convert again only RAW files newer than their output
rawdog ./photos/ -j 2                        # convert two files at a time
rawdog -R /Volumes/CARD -o ./jpegs --report run.csv
rawdog ./photos/ --quiet                     # print errors only
```

By default rawdog chooses how many files to convert at the same time from the installed RAM and the size of the largest input, because too many large files at once make the system swap. The choice does not know how much memory other apps use. If a browser or editor already holds most of it, full-size runs can stall while the system swaps; use `-j 1`, or `--fast` for proofs, which needs about a third of the memory.

`--report` lists each file with its output, its status (converted, skipped, failed, or excluded), the error message, and the time taken. It is JSON if the name ends in `.json`, otherwise CSV.

### Default options

Options you use every time can go in `~/.config/rawdog/defaults`, one per line. The value follows the option after a space and needs no quotes:

```
# ~/.config/rawdog/defaults
--copyright (c) {year} J. Doe
--artist J. Doe
--strip-gps
--sharpen 0.5
```

`--sharpen` needs its amount in the file. An option on the command line overrides the same option in the file. `--no-defaults` ignores the file for one run, and `RAWDOG_DEFAULTS` names a different file.

## Existing Files

- An existing output file is skipped. `--overwrite` replaces it, and `--update` replaces it only when the RAW file is newer.
- rawdog never writes over its own input.
- rawdog never replaces a file whose name differs from the output name only in letter case. A camera that records RAW+JPEG writes `DSC1.JPG` next to `DSC1.ARW`, and on macOS that file would be replaced by `DSC1.jpg`. Use `-o` or `--name` to write the output somewhere else.
- Dot-files in a scanned directory are ignored. macOS writes `._NAME` files on memory cards.

## Exit Status

rawdog exits with `0` when every input converted or was skipped. It exits
non-zero when any conversion failed, when an input path does not exist, when
no RAW files were found, or when outputs conflict. Valid inputs are still
converted when some other inputs fail or are missing.
A refused or failed file also gives a non-zero exit status, with `--dry-run` too.

## macOS Finder Integration

You can add a right-click Quick Action so you can select ARW files in Finder and convert them without opening a terminal.

1. Open **Automator** and create a new **Quick Action**
2. Set "Workflow receives current" to **files or folders** in **Finder**
3. Add a **Run Shell Script** action with "Pass input" set to **as arguments**
4. Paste this script:

```bash
export PATH="/usr/local/bin:$HOME/.cargo/bin:$PATH"
rawdog "$@"
```

5. Save as "Convert with RawDog"

Now right-click any ARW files in Finder → **Quick Actions** → **Convert with RawDog**. It handles multiple files at once.

To prompt for an output folder instead, use this script:

```bash
export PATH="/usr/local/bin:$HOME/.cargo/bin:$PATH"

OUTPUT_DIR=$(osascript -e 'set theFolder to choose folder with prompt "Choose output folder"' -e 'POSIX path of theFolder' 2>/dev/null)

if [ -z "$OUTPUT_DIR" ]; then
    exit 0
fi

rawdog "$@" -o "$OUTPUT_DIR"
```

More recipes, each saved as its own Quick Action in the same way. They select folders as well as files, show a notification with the result, and use your [defaults file](#default-options).

Make proofs of the selected files or folders into a `proofs` folder next to them, and open the gallery page:

```bash
export PATH="/usr/local/bin:$HOME/.cargo/bin:$PATH"

OUT="$(dirname "$1")/proofs"
if RESULT=$(rawdog -R "$@" -r 2048 --fast --sharpen 0.5 -o "$OUT" --gallery 2>&1); then
    open "$OUT/gallery.html"
fi
osascript -e "display notification \"$(echo "$RESULT" | grep -E 'succeeded|No RAW' | tail -1)\" with title \"RawDog proofs\""
```

Import a card into an archive by date, with sidecars, verified byte for byte:

```bash
export PATH="/usr/local/bin:$HOME/.cargo/bin:$PATH"

RESULT=$(rawdog -R "$@" -f original -o "$HOME/Pictures/Archive" --name "{year}/{date}/{stem}" 2>&1)
osascript -e "display notification \"$(echo "$RESULT" | grep -E 'succeeded|No RAW' | tail -1)\" with title \"RawDog import\""
```

If the action doesn't appear, check **System Settings → Privacy & Security → Extensions → Finder**.

## License

MIT
