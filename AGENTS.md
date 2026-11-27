# AGENTS.md

Notes for coding agents that work on rawdog. For how to use the CLI, see
`README.md` and `skills/rawdog/SKILL.md`.

## What this is

rawdog is a Rust command-line RAW converter for photographers. It writes
JPEG, TIFF, or PNG from camera RAW files. It also does proofs and galleries,
culling by rating or label, card import and naming, and delivery sizes. Sony
ARW is the main target. Other formats go through the same decoders.

## Layout

| Path | Contents |
|---|---|
| `src/main.rs` | CLI options (clap derive), input discovery, selection, the run loop, reports, `--watch`, `--info` |
| `src/converter.rs` | Decode (rawler first, then rawloader), develop (imagepipe), resize, sharpen, watermark, encode |
| `src/exif.rs` | Read metadata and write it into outputs (EXIF, XMP, ICC), GPS, ratings and labels from XMP sidecars |
| `src/naming.rs` | `--name` templates and the `NameInfo` metadata each file gets |
| `src/gallery.rs` | `gallery.html` proof pages |
| `src/report.rs` | `--report` CSV and JSON |
| `src/timeshift.rs` | `--time-shift` offsets |
| `src/gpx.rs` | `--gpx` track matching |
| `tests/integration_test.rs` | End-to-end tests that run the built binary |
| `.github/workflows/ci.yml` | Clippy and tests on Ubuntu, macOS, and Windows |
| `.github/workflows/release.yml` | Release builds for 5 targets; a `v*` tag publishes them |

## Build and check

```sh
cargo build --release
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

CI runs the last two commands on all three operating systems. Run both
before you open a pull request.

Do not run `cargo fmt` on the whole tree. The code is not fully rustfmt
formatted, and a tree-wide format makes an unrelated diff. Match the style of
the code around your change.

## Tests

- Unit tests live in a `mod tests` at the end of each source file.
- Integration tests build small JPEG or PNG inputs in a `TempDir` with helpers
  such as `jpeg_with_exif`, then run the binary with `run_os`. Ratings and
  labels come from an XMP sidecar next to the input (`a.xmp` for `a.jpg`).
- No RAW sample files are in the repo. Changes to decoding or colour need a
  manual check on real RAW files, and a note in the pull request on what you
  checked.
- Paths must work on Windows. Do not hard-code `/` in expected paths.

## Conventions

- One change per pull request, on a feature branch. Do not commit to `main`.
- A change to the look of every output (colour, tone, sharpening defaults)
  needs a pull request that says so in its title, with before and after
  measurements. The maintainer decides on these.
- Every new option gets a `help_heading` that matches its group in
  `--help`, a doc comment written as the help text, and a section or line in
  `README.md`.
- Options that cannot work together use clap `conflicts_with` or
  `requires`, so the error comes before any work.
- rawdog never writes over its own input and never replaces an existing
  output unless `--overwrite` or `--update` is given. Keep it that way.
- A file that fails or is refused gives a non-zero exit status. Other files
  in the run still convert.
- Messages for the user go to stderr, except the `Found N RAW file(s)` line,
  `--dry-run` output, and `--info` output, which go to stdout.
- Help text, README text, and comments use short, plain sentences.

## Known limits

- Nikon High Efficiency NEF (Z 8, Z f) and some phone DNG files (Samsung
  Galaxy S23 Ultra) do not decode. rawdog reports an error for these.
- CR3 metadata is copied field by field, not as a whole EXIF block.

## Writing style

Write all responses in ASD-STE100 Simplified Technical English (STE). This applies to chat responses, code comments, commit messages, and documentation.

- Keep instructions to 20 words or fewer. Keep descriptive sentences to 25 words or fewer.
- Use the active voice, simple verb tenses, and one meaning for each word.
- Use the same term for the same concept.
- Do not change code, commands, file paths, error messages, or quoted text to follow these rules.
