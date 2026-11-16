# RawDog

Raw files in, images out. No Lightroom, no fuss.

A no-nonsense RAW converter supporting JPEG, TIFF, and PNG output. Built for Sony ARW; it also accepts most other camera RAW formats (DNG, CR2, CR3, NEF, RAF, ORF, RW2, PEF, and more).

Two decoder libraries are used. [rawloader](https://github.com/pedrocr/rawloader) decodes cameras up to about 2018. For a file that it does not know, rawdog uses [rawler](https://github.com/dnglab/dnglab), which knows newer cameras such as the Sony a7 IV and a7R V, and Canon CR3 files. Metadata is not copied from CR3 files.

## Installation

```sh
cargo install --path .
```

## Usage

Convert a single file:

```sh
rawdog photo.ARW
```

Convert all RAW files in a directory:

```sh
rawdog ./photos/
```

Specify an output directory:

```sh
rawdog ./photos/ -o ./jpegs/
```

Convert a directory tree recursively. With `-o`, the subdirectory layout under
the input directory is mirrored under the output directory:

```sh
rawdog -R ./photos/ -o ./jpegs/   # photos/2024/a.ARW -> jpegs/2024/a.jpg
```

Files given directly on the command line go straight into `-o`. If two inputs
would write the same output file, rawdog reports the conflict and converts
nothing.

Resize the long edge to 2048px:

```sh
rawdog photo.ARW -r 2048
```

Set JPEG quality to 85:

```sh
rawdog photo.ARW -q 85
```

Output as 16-bit TIFF:

```sh
rawdog photo.ARW -f tiff
```

Output as 16-bit PNG with resize:

```sh
rawdog ./photos/ -f png -r 2048
```

Combine options:

```sh
rawdog ./photos/ -o ./jpegs/ -q 95 -r 3000 --overwrite
```

Make small proofs quickly. `--fast` scales during RAW decoding, which is several times faster and slightly softer:

```sh
rawdog ./card -r 2048 --fast -o ./proofs
```

Import a card into folders by capture date, with the camera model in each file name:

```sh
rawdog -R /Volumes/CARD -o ~/Pictures --name "{date}/{model}_{stem}"
```

Template fields: `{stem}` (input file name without extension), `{date}` (2024-03-09), `{year}`, `{month}`, `{day}`, `{time}` (070542), and `{model}`. A file without that metadata gets `unknown`.

The template path is below the `-o` directory. Without `-o` it is below the directory of each input file. With a template, `-R` does not mirror the input tree, so include `{date}` or another field that keeps equal file names apart.

Brighten a dark batch by two thirds of a stop and set the white balance to 3400 K:

```sh
rawdog ./reception --exposure 0.67 --wb 3400
```

`--exposure` rolls off highlights instead of clipping them. `--wb` replaces the as-shot white balance of every file; add `--tint` to correct a green or magenta cast.

Resized images look soft. Add output sharpening:

```sh
rawdog ./trip -r 2048 --sharpen
```

`--sharpen` takes an optional amount from 0 to 3; the default is 0.5.

Export for the web without location data, with your name and copyright:

```sh
rawdog ./trip -r 2048 --strip-gps --artist "J. Doe" --copyright "(c) 2026 J. Doe"
```

Camera, lens, exposure, and date metadata are copied to JPEG, PNG, and TIFF output. Use `--strip-metadata` to copy none of it.

Cull a card quickly. `--preview` writes the JPEG preview that the camera embedded in each RAW file, with the camera's own colours, and does not develop the RAW data:

```sh
rawdog -R /Volumes/CARD --preview -o ./proofs
```

The preview size depends on the camera: 1616x1080 for Sony bodies, full resolution for many Canon and Nikon bodies.

Preview what a run will do, without writing files:

```sh
rawdog -R ./card -o ./jpegs --dry-run
```

Convert again only the files whose RAW is newer than the existing output:

```sh
rawdog ./photos/ --update
```

Set how many files are converted at the same time. By default rawdog chooses this from the installed RAM and the size of the largest input, because too many large files at once make the system swap:

```sh
rawdog ./photos/ -j 2
```

Print errors only, for use in scripts:

```sh
rawdog ./photos/ --quiet
```

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

If the action doesn't appear, check **System Settings → Privacy & Security → Extensions → Finder**.

## License

MIT
