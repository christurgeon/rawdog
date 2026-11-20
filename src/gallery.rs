//! The `--gallery` page: a static HTML proof sheet of the output images.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// One image on the page.
pub struct Entry {
    /// The output file.
    pub image: PathBuf,
    pub caption: Caption,
}

pub struct Caption {
    pub name: String,
    pub taken: Option<String>,
    pub model: Option<String>,
    pub rating: Option<i8>,
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
pub fn render(title: &str, page_dir: &Path, entries: &[Entry]) -> String {
    let mut cards = String::new();
    for entry in entries {
        let Some(url) = relative_url(page_dir, &entry.image) else {
            continue;
        };
        let c = &entry.caption;
        let mut details = Vec::new();
        if let Some(taken) = &c.taken {
            details.push(escape(taken));
        }
        if let Some(model) = &c.model {
            details.push(escape(model));
        }
        if let Some(r) = c.rating.filter(|r| *r > 0) {
            details.push("★".repeat(r as usize));
        }
        let _ = writeln!(
            cards,
            "<figure><a href=\"{url}\"><img src=\"{url}\" loading=\"lazy\" alt=\"{name}\"></a>\
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
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(image: &str, name: &str) -> Entry {
        Entry {
            image: PathBuf::from(image),
            caption: Caption {
                name: name.to_string(),
                taken: Some("2026-11-14 18:02".to_string()),
                model: Some("ILCE-7M4".to_string()),
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
        );
        assert!(page.contains("<title>Shoot &lt;1&gt;</title>"));
        assert!(page.contains(r#"src="2026-11-14/a%20b%26c.jpg""#), "{page}");
        assert!(page.contains("<b>a b&amp;c.ARW</b>"));
        assert!(page.contains(r#"<input type="checkbox" value="a b&amp;c.ARW">"#), "{page}");
        assert!(page.contains("ILCE-7M4 · ★★★"));
    }

    #[test]
    fn render_leaves_out_images_outside_the_page_directory() {
        let page = render("x", Path::new("/out"), &[entry("/elsewhere/a.jpg", "a")]);
        assert!(!page.contains("<img"));
    }
}
