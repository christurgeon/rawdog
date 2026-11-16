# RawDog

Raw files in, images out. No Lightroom, no fuss.

A no-nonsense RAW converter supporting JPEG, TIFF, and PNG output. Built for Sony ARW; it also accepts most other camera RAW formats (DNG, CR2, NEF, RAF, ORF, RW2, PEF, and more).

Camera support comes from the [rawloader](https://github.com/pedrocr/rawloader) library. Its camera list stops at about 2018, so files from newer bodies can fail to decode.

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

## Exit Status

rawdog exits with `0` when every input converted or was skipped. It exits
non-zero when any conversion failed, when an input path does not exist, when
no RAW files were found, or when outputs conflict. Valid inputs are still
converted when some other inputs fail or are missing.

Make small proofs quickly. `--fast` scales during RAW decoding, which is several times faster and slightly softer:

```sh
rawdog ./card -r 2048 --fast -o ./proofs
```

Export for the web without location data, with your name and copyright:

```sh
rawdog ./trip -r 2048 --strip-gps --artist "J. Doe" --copyright "(c) 2026 J. Doe"
```

Camera, lens, exposure, and date metadata are copied to JPEG, PNG, and TIFF output. Use `--strip-metadata` to copy none of it.

Preview what a run will do, without writing files:

```sh
rawdog -R ./card -o ./jpegs --dry-run
```

Convert again only the files whose RAW is newer than the existing output:

```sh
rawdog ./photos/ --update
```

Limit memory use by converting two files at a time:

```sh
rawdog ./photos/ -j 2
```

Print errors only, for use in scripts:

```sh
rawdog ./photos/ --quiet
```

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
