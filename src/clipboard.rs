//! Read images off the system clipboard so they can be attached to a prompt.
//!
//! Terminals only forward *text* pastes to an application: pressing the
//! terminal's paste shortcut with an image on the clipboard does nothing.
//! Attaching an image therefore requires reading the OS clipboard directly,
//! which is what this module does — first through `arboard` (native paths for
//! Wayland via the data-control protocol, X11, macOS and Windows), then by
//! shelling out to `wl-paste`/`xclip` for environments arboard cannot reach.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow};

/// A PNG pulled off the clipboard, with its pixel size for display.
pub struct ClipboardImage {
    pub png: Vec<u8>,
    pub width: usize,
    pub height: usize,
}

/// Read an image from the system clipboard, as PNG bytes. `Ok(None)` means
/// the clipboard is reachable but holds no image; `Err` means no clipboard
/// backend worked at all (headless session, missing tooling).
pub fn read_image() -> Result<Option<ClipboardImage>> {
    match arboard_image() {
        Ok(Some(found)) => Ok(Some(found)),
        // arboard can come back empty-handed while the image is there: on a
        // Wayland compositor without the data-control protocol it reads the
        // X11 clipboard, which holds no image. wl-paste asks Wayland directly.
        Ok(None) => Ok(command_image()),
        // arboard failing outright (no Wayland data-control, no X11) is not
        // the end: a clipboard utility may still be installed.
        Err(arboard_error) => match command_image() {
            Some(image) => Ok(Some(image)),
            None => Err(anyhow!("clipboard unavailable ({arboard_error}); install wl-clipboard or xclip")),
        },
    }
}

/// Put text on the system clipboard. Tries the native backend first, then the
/// platform utilities, so copying works on setups arboard cannot reach.
pub fn write_text(text: &str) -> Result<()> {
    let native = arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(text));
    if native.is_ok() {
        return Ok(());
    }
    let candidates: [(&str, &[&str]); 4] = [
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
        ("pbcopy", &[]),
    ];
    for (program, args) in candidates {
        let Ok(mut child) = Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(stdin) = child.stdin.as_mut() {
            use std::io::Write as _;
            let _ = stdin.write_all(text.as_bytes());
        }
        if child.wait().map(|status| status.success()).unwrap_or(false) {
            return Ok(());
        }
    }
    match native {
        Err(error) => Err(error).context("no clipboard backend accepted the text"),
        Ok(()) => Ok(()),
    }
}

fn arboard_image() -> Result<Option<ClipboardImage>> {
    let mut clipboard = arboard::Clipboard::new().context("open clipboard")?;
    let image = match clipboard.get_image() {
        Ok(image) => image,
        // ContentNotAvailable is the "clipboard holds text/nothing" case.
        Err(arboard::Error::ContentNotAvailable) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let (width, height) = (image.width, image.height);
    let png = encode_png(&image.bytes, width, height)?;
    Ok(Some(ClipboardImage { png, width, height }))
}

/// Fallback: ask the platform's clipboard utility for PNG data directly.
fn command_image() -> Option<ClipboardImage> {
    let candidates: [(&str, &[&str]); 2] =
        [("wl-paste", &["-t", "image/png"]), ("xclip", &["-selection", "clipboard", "-t", "image/png", "-o"])];
    for (program, args) in candidates {
        let Ok(output) = Command::new(program).args(args).output() else {
            continue;
        };
        if !output.status.success() || output.stdout.is_empty() {
            continue;
        }
        let (width, height) = png_size(&output.stdout).unwrap_or((0, 0));
        return Some(ClipboardImage { png: output.stdout, width, height });
    }
    None
}

fn encode_png(rgba: &[u8], width: usize, height: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(
        Cursor::new(&mut out),
        u32::try_from(width).context("image width")?,
        u32::try_from(height).context("image height")?,
    );
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().context("encode png header")?;
    writer.write_image_data(rgba).context("encode png data")?;
    writer.finish().context("finish png")?;
    Ok(out)
}

fn png_size(png: &[u8]) -> Option<(usize, usize)> {
    let decoder = png::Decoder::new(Cursor::new(png));
    let reader = decoder.read_info().ok()?;
    let info = reader.info();
    Some((info.width as usize, info.height as usize))
}

/// Save a pasted image under the attachments directory and hand back the
/// short token the composer inserts. The token embeds only the file name;
/// the directory is fixed, so the reference survives session resume without
/// any in-memory state.
pub fn save_attachment(directory: &Path, image: &ClipboardImage) -> Result<(String, PathBuf)> {
    std::fs::create_dir_all(directory).context("create attachments directory")?;
    let name = format!("img-{}.png", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let path = directory.join(&name);
    std::fs::write(&path, &image.png).context("write attachment")?;
    Ok((format!("[image:{name}]"), path))
}

/// Copy an image file the person pasted or dropped into the attachments
/// directory, and hand back the `[image:…]` token that references it. A copy,
/// so the session still has the image after the original moves or is deleted.
pub fn attach_image_file(directory: &Path, path: &Path) -> Result<String> {
    // Validates the type and size before anything is copied.
    crate::context::image_data_url(path)?;
    std::fs::create_dir_all(directory).context("create attachments directory")?;
    let extension = path.extension().and_then(|value| value.to_str()).unwrap_or("png").to_ascii_lowercase();
    let name = format!("img-{}.{extension}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    std::fs::copy(path, directory.join(&name)).context("copy image into attachments")?;
    Ok(format!("[image:{name}]"))
}

/// The image files a paste names, when that is all it names: a path dropped
/// on the terminal, a file copied in a file manager (a `file://` URI), or
/// several of either. The quoting and backslash escapes terminals add around
/// dropped paths are undone. Only absolute, `~/` and `file://` paths count, so
/// pasting the word `logo.png` stays text.
pub fn pasted_image_paths(text: &str) -> Option<Vec<PathBuf>> {
    let words = split_words(text.trim())?;
    if words.is_empty() {
        return None;
    }
    words
        .iter()
        .map(|word| {
            let path = if let Some(uri) = word.strip_prefix("file://") {
                file_uri_path(uri)?
            } else if let Some(rest) = word.strip_prefix("~/") {
                home_dir()?.join(rest)
            } else {
                PathBuf::from(word)
            };
            (path.is_absolute() && path.is_file() && crate::context::is_image_path(&path)).then_some(path)
        })
        .collect()
}

/// Split on whitespace, honouring single and double quotes and, outside
/// Windows, backslash escapes. None for an unbalanced quote: that is text,
/// not a list of paths.
fn split_words(text: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        match quote {
            Some(open) if ch == open => quote = None,
            Some(_) => word.push(ch),
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                in_word = true;
            }
            None if ch == '\\' && !cfg!(windows) => {
                word.push(chars.next()?);
                in_word = true;
            }
            None if ch.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            None => {
                word.push(ch);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if in_word {
        words.push(word);
    }
    Some(words)
}

/// The local path a `file://` URI names (the part after the scheme),
/// percent-decoded.
fn file_uri_path(uri: &str) -> Option<PathBuf> {
    // `file:///home/me/a.png`, or with an explicit `localhost` host.
    let path = uri.strip_prefix("localhost").unwrap_or(uri);
    if !path.starts_with('/') {
        return None;
    }
    let decoded = percent_decode(path)?;
    // `file:///C:/Users/…` on Windows: the drive follows the slash.
    if cfg!(windows) && decoded.as_bytes().get(2) == Some(&b':') {
        return Some(PathBuf::from(&decoded[1..]));
    }
    Some(PathBuf::from(decoded))
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_paths_to_images_are_recognised_in_every_terminal_spelling() {
        let dir = tempfile::tempdir().unwrap();
        let spaced = dir.path().join("Screenshot 1.png");
        let plain = dir.path().join("plain.jpg");
        let notes = dir.path().join("notes.txt");
        for path in [&spaced, &plain, &notes] {
            std::fs::write(path, b"x").unwrap();
        }
        let spaced_text = spaced.display().to_string();
        let plain_text = plain.display().to_string();

        // Quoted, as Konsole and GNOME Terminal drop a path with a space.
        assert_eq!(pasted_image_paths(&format!("'{spaced_text}'")), Some(vec![spaced.clone()]));
        // Two files dropped at once.
        assert_eq!(
            pasted_image_paths(&format!("'{spaced_text}' {plain_text}\n")),
            Some(vec![spaced.clone(), plain.clone()])
        );
        if !cfg!(windows) {
            // A file copied in a file manager arrives as a URI.
            let uri = format!("file://{}", spaced_text.replace(' ', "%20"));
            assert_eq!(pasted_image_paths(&uri), Some(vec![spaced.clone()]));
            // Some terminals escape the space instead of quoting.
            assert_eq!(pasted_image_paths(&spaced_text.replace(' ', "\\ ")), Some(vec![spaced.clone()]));
        }

        // Anything else stays text.
        assert_eq!(pasted_image_paths(&notes.display().to_string()), None);
        assert_eq!(pasted_image_paths(&format!("{plain_text} and more words")), None);
        assert_eq!(pasted_image_paths("plain.jpg"), None);
        assert_eq!(pasted_image_paths("it's broken"), None);
        assert_eq!(pasted_image_paths("   "), None);
    }

    #[test]
    fn an_attached_file_is_copied_under_a_fresh_token() {
        let source = tempfile::tempdir().unwrap();
        let attachments = tempfile::tempdir().unwrap();
        let photo = source.path().join("photo.JPG");
        std::fs::write(&photo, b"jpeg bytes").unwrap();
        let token = attach_image_file(attachments.path(), &photo).unwrap();
        let name = token.strip_prefix("[image:").and_then(|t| t.strip_suffix(']')).unwrap();
        assert!(name.starts_with("img-") && name.ends_with(".jpg"), "{name}");
        assert_eq!(std::fs::read(attachments.path().join(name)).unwrap(), b"jpeg bytes");
        assert!(attach_image_file(attachments.path(), &source.path().join("missing.png")).is_err());
    }

    #[test]
    fn png_round_trip_preserves_dimensions() {
        let rgba = vec![255_u8; 4 * 3 * 2];
        let png = encode_png(&rgba, 3, 2).unwrap();
        assert_eq!(png_size(&png), Some((3, 2)));
    }

    #[test]
    fn save_attachment_writes_the_token_named_file() {
        let dir = tempfile::tempdir().unwrap();
        let image = ClipboardImage { png: encode_png(&[0_u8; 4], 1, 1).unwrap(), width: 1, height: 1 };
        let (token, path) = save_attachment(dir.path(), &image).unwrap();
        assert!(token.starts_with("[image:img-") && token.ends_with(".png]"));
        assert!(path.exists());
        let name = token.strip_prefix("[image:").and_then(|t| t.strip_suffix(']')).unwrap();
        assert_eq!(path.file_name().unwrap().to_str().unwrap(), name);
    }

    /// Talks to the real OS clipboard: run explicitly with `--ignored` on a
    /// desktop session. Sets an image, reads it back through the public path.
    #[test]
    #[ignore]
    fn clipboard_round_trip_on_a_desktop_session() {
        let rgba = vec![128_u8; 4 * 2 * 2];
        let mut clipboard = arboard::Clipboard::new().unwrap();
        clipboard.set_image(arboard::ImageData { width: 2, height: 2, bytes: rgba.into() }).unwrap();
        let image = read_image().unwrap().expect("an image was just set");
        assert_eq!((image.width, image.height), (2, 2));
    }
}
