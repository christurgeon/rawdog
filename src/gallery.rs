//! The `--gallery` page: a static HTML proof sheet of the output images.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// One image on the page.
pub struct Entry {
    /// The output file.
    pub image: PathBuf,
    /// A smaller copy shown on the page; the link opens `image`.
    pub thumbnail: Option<PathBuf>,
    pub caption: Caption,
}

pub struct Caption {
    pub name: String,
    pub taken: Option<String>,
    pub model: Option<String>,
    /// Exposure settings, such as "1/250 s · f/2.8 · ISO 800".
    pub exposure: Option<String>,
    pub rating: Option<i8>,
}

/// Directory for thumbnails, below the page's directory.
pub const THUMB_DIR: &str = "gallery-thumbs";

/// Long edge of a thumbnail. A card is about 260 CSS pixels wide, which is
/// about 800 pixels on a phone screen.
const THUMB_EDGE: u32 = 800;

/// The thumbnail of `image`: a JPEG in `dir/gallery-thumbs/` at the image's
/// path relative to `dir`. It is made when it is missing or older than the
/// image. `None` when the image is small enough to show as it is, or
/// cannot be read.
pub fn thumbnail(dir: &Path, image: &Path) -> Option<PathBuf> {
    let (width, height) = image::image_dimensions(image).ok()?;
    if width.max(height) <= THUMB_EDGE * 3 / 2 {
        return None;
    }
    let mut thumb = dir.join(THUMB_DIR).join(image.strip_prefix(dir).ok()?).into_os_string();
    let is_jpeg = image
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg"));
    if !is_jpeg {
        thumb.push(".jpg");
    }
    let thumb = PathBuf::from(thumb);
    let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let fresh = matches!((modified(&thumb), modified(image)), (Some(t), Some(i)) if t >= i);
    if !fresh {
        let small = image::open(image).ok()?.thumbnail(THUMB_EDGE, THUMB_EDGE).into_rgb8();
        std::fs::create_dir_all(thumb.parent()?).ok()?;
        small.save_with_format(&thumb, image::ImageFormat::Jpeg).ok()?;
    }
    Some(thumb)
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// A relative URL from the page's directory to `image`, with each path
/// segment percent-encoded.
fn relative_url(page_dir: &Path, image: &Path) -> Option<String> {
    let relative = image.strip_prefix(page_dir).ok()?;
    let segments: Vec<String> = relative
        .components()
        .map(|c| {
            let text = c.as_os_str().to_string_lossy();
            let mut out = String::new();
            for b in text.bytes() {
                if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                    out.push(b as char);
                } else {
                    let _ = write!(out, "%{b:02X}");
                }
            }
            out
        })
        .collect();
    Some(segments.join("/"))
}

/// Render the page. Images that are not below `page_dir` are left out.
/// With `refresh`, the page reloads itself every that many seconds.
pub fn render(title: &str, page_dir: &Path, entries: &[Entry], refresh: Option<u64>) -> String {
    let mut cards = String::new();
    for entry in entries {
        let Some(url) = relative_url(page_dir, &entry.image) else {
            continue;
        };
        let src = entry
            .thumbnail
            .as_deref()
            .and_then(|t| relative_url(page_dir, t))
            .unwrap_or_else(|| url.clone());
        let c = &entry.caption;
        let mut details = Vec::new();
        if let Some(taken) = &c.taken {
            details.push(escape(taken));
        }
        if let Some(model) = &c.model {
            details.push(escape(model));
        }
        if let Some(exposure) = &c.exposure {
            details.push(escape(exposure));
        }
        if let Some(r) = c.rating.filter(|r| *r > 0) {
            details.push("★".repeat(r as usize));
        }
        let _ = writeln!(
            cards,
            "<figure><a href=\"{url}\"><img src=\"{src}\" loading=\"lazy\" alt=\"{name}\"></a>\
             <figcaption><label><input type=\"checkbox\" value=\"{name}\"> <b>{name}</b></label>\
             <span>{details}</span></figcaption></figure>",
            name = escape(&c.name),
            details = details.join(" · "),
        );
    }
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
{refresh}
<title>{title}</title>
<style>
  :root {{ color-scheme: light dark; --bg: #f6f6f4; --fg: #1d1d1b; --muted: #6b6b66; --card: #ffffff; }}
  @media (prefers-color-scheme: dark) {{ :root {{ --bg: #161615; --fg: #ececea; --muted: #9a9a94; --card: #22221f; }} }}
  body {{ margin: 0; padding: 16px; background: var(--bg); color: var(--fg); font: 14px/1.4 -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }}
  h1 {{ font-size: 18px; font-weight: 600; margin: 0 0 4px; }}
  p.count {{ margin: 0 0 16px; color: var(--muted); }}
  main {{ display: grid; gap: 12px; grid-template-columns: repeat(auto-fill, minmax(min(260px, 100%), 1fr)); }}
  figure {{ margin: 0; background: var(--card); border-radius: 6px; overflow: hidden; }}
  img {{ display: block; width: 100%; aspect-ratio: 3 / 2; object-fit: contain; background: #000; }}
  figcaption {{ padding: 8px 10px; display: flex; flex-direction: column; gap: 2px; overflow-wrap: anywhere; }}
  figcaption span {{ color: var(--muted); font-size: 12px; }}
  figcaption label {{ display: flex; gap: 6px; align-items: center; cursor: pointer; }}
  figcaption input {{ width: 18px; height: 18px; margin: 0; flex: none; }}
  figure:has(input:checked) {{ outline: 3px solid #d9822b; }}
  #picks {{ position: sticky; bottom: 0; margin: 16px -16px -16px; padding: 10px 16px; background: var(--card); border-top: 1px solid var(--muted); display: flex; gap: 10px; align-items: center; flex-wrap: wrap; }}
  #picks textarea {{ flex: 1 1 240px; min-height: 2.6em; max-height: 8em; font: 12px ui-monospace, monospace; background: var(--bg); color: var(--fg); }}
  #picks button {{ font: inherit; padding: 6px 12px; }}
</style>
</head>
<body>
<h1>{title}</h1>
<p class="count">{count} images</p>
<main>
{cards}</main>
<div id="picks"><span id="count">0 picked</span>
<textarea readonly aria-label="Picked file names" placeholder="Tick the images you want. Their names appear here to copy and send back."></textarea>
<button type="button">Copy list</button></div>
<script>
(() => {{
  const boxes = [...document.querySelectorAll("figcaption input")];
  const list = document.querySelector("#picks textarea");
  const key = "rawdog-picks:" + location.pathname;
  const update = () => {{
    const names = boxes.filter(b => b.checked).map(b => b.value);
    list.value = names.join("\n");
    document.getElementById("count").textContent = names.length + " picked";
    try {{ localStorage.setItem(key, JSON.stringify(names)); }} catch (e) {{}}
  }};
  try {{
    const saved = new Set(JSON.parse(localStorage.getItem(key) || "[]"));
    boxes.forEach(b => {{ b.checked = saved.has(b.value); }});
  }} catch (e) {{}}
  boxes.forEach(b => b.addEventListener("change", update));
  document.querySelector("#picks button").addEventListener("click", () => {{
    list.select();
    if (navigator.clipboard) navigator.clipboard.writeText(list.value).catch(() => document.execCommand("copy"));
    else document.execCommand("copy");
  }});
  update();
}})();
</script>
</body>
</html>
"##,
        title = escape(title),
        count = entries.len(),
        refresh = refresh
            .map(|s| format!("<meta http-equiv=\"refresh\" content=\"{s}\">"))
            .unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(image: &str, name: &str) -> Entry {
        Entry {
            image: PathBuf::from(image),
            thumbnail: None,
            caption: Caption {
                name: name.to_string(),
                taken: Some("2026-11-14 18:02".to_string()),
                model: Some("ILCE-7M4".to_string()),
                exposure: None,
                rating: Some(3),
            },
        }
    }

    #[test]
    fn render_links_relative_images_and_escapes_text() {
        let page = render(
            "Shoot <1>",
            Path::new("/out"),
            &[entry("/out/2026-11-14/a b&c.jpg", "a b&c.ARW")],
            None,
        );
        assert!(page.contains("<title>Shoot &lt;1&gt;</title>"));
        assert!(page.contains(r#"src="2026-11-14/a%20b%26c.jpg""#), "{page}");
        assert!(page.contains("<b>a b&amp;c.ARW</b>"));
        assert!(page.contains(r#"<input type="checkbox" value="a b&amp;c.ARW">"#), "{page}");
        assert!(page.contains("ILCE-7M4 · ★★★"));
    }

    #[test]
    fn render_adds_a_refresh_only_when_asked() {
        let page = render("x", Path::new("/out"), &[], Some(10));
        assert!(page.contains(r#"<meta http-equiv="refresh" content="10">"#));
        assert!(!render("x", Path::new("/out"), &[], None).contains("refresh"));
    }

    #[test]
    fn render_shows_the_thumbnail_and_links_the_image() {
        let mut e = entry("/out/a.jpg", "a.ARW");
        e.thumbnail = Some(PathBuf::from("/out/gallery-thumbs/a.jpg"));
        let page = render("x", Path::new("/out"), &[e], None);
        assert!(page.contains(r#"<a href="a.jpg"><img src="gallery-thumbs/a.jpg""#), "{page}");
    }

    #[test]
    fn thumbnail_only_for_large_images_and_refreshed_when_older() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        let small = dir.join("small.jpg");
        image::RgbImage::new(1200, 800).save(&small).unwrap();
        assert_eq!(thumbnail(dir, &small), None);

        std::fs::create_dir_all(dir.join("day")).unwrap();
        let large = dir.join("day/large.png");
        image::RgbImage::from_pixel(2000, 1000, image::Rgb([200, 10, 10])).save(&large).unwrap();
        let thumb = thumbnail(dir, &large).unwrap();
        assert_eq!(thumb, dir.join("gallery-thumbs/day/large.png.jpg"));
        assert_eq!(image::image_dimensions(&thumb).unwrap(), (800, 400));

        // A thumbnail older than its image is made again.
        std::fs::write(&thumb, b"stale").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::File::options().write(true).open(&thumb).unwrap().set_modified(old).unwrap();
        thumbnail(dir, &large).unwrap();
        assert_eq!(image::image_dimensions(&thumb).unwrap(), (800, 400));
    }

    #[test]
    fn render_leaves_out_images_outside_the_page_directory() {
        let page = render("x", Path::new("/out"), &[entry("/elsewhere/a.jpg", "a")], None);
        assert!(!page.contains("<img"));
    }
}
