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
        Ok(found) => Ok(found),
        // arboard failing outright (no Wayland data-control, no X11) is not
        // the end: a clipboard utility may still be installed.
        Err(arboard_error) => match command_image() {
            Some(image) => Ok(Some(image)),
            None => Err(anyhow!(
                "clipboard unavailable ({arboard_error}); install wl-clipboard or xclip"
            )),
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
    let candidates: [(&str, &[&str]); 2] = [
        ("wl-paste", &["-t", "image/png"]),
        (
            "xclip",
            &["-selection", "clipboard", "-t", "image/png", "-o"],
        ),
    ];
    for (program, args) in candidates {
        let Ok(output) = Command::new(program).args(args).output() else {
            continue;
        };
        if !output.status.success() || output.stdout.is_empty() {
            continue;
        }
        let (width, height) = png_size(&output.stdout).unwrap_or((0, 0));
        return Some(ClipboardImage {
            png: output.stdout,
            width,
            height,
        });
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
    let name = format!(
        "img-{}.png",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let path = directory.join(&name);
    std::fs::write(&path, &image.png).context("write attachment")?;
    Ok((format!("[image:{name}]"), path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_round_trip_preserves_dimensions() {
        let rgba = vec![255_u8; 4 * 3 * 2];
        let png = encode_png(&rgba, 3, 2).unwrap();
        assert_eq!(png_size(&png), Some((3, 2)));
    }

    #[test]
    fn save_attachment_writes_the_token_named_file() {
        let dir = tempfile::tempdir().unwrap();
        let image = ClipboardImage {
            png: encode_png(&[0_u8; 4], 1, 1).unwrap(),
            width: 1,
            height: 1,
        };
        let (token, path) = save_attachment(dir.path(), &image).unwrap();
        assert!(token.starts_with("[image:img-") && token.ends_with(".png]"));
        assert!(path.exists());
        let name = token
            .strip_prefix("[image:")
            .and_then(|t| t.strip_suffix(']'))
            .unwrap();
        assert_eq!(path.file_name().unwrap().to_str().unwrap(), name);
    }

    /// Talks to the real OS clipboard: run explicitly with `--ignored` on a
    /// desktop session. Sets an image, reads it back through the public path.
    #[test]
    #[ignore]
    fn clipboard_round_trip_on_a_desktop_session() {
        let rgba = vec![128_u8; 4 * 2 * 2];
        let mut clipboard = arboard::Clipboard::new().unwrap();
        clipboard
            .set_image(arboard::ImageData {
                width: 2,
                height: 2,
                bytes: rgba.into(),
            })
            .unwrap();
        let image = read_image().unwrap().expect("an image was just set");
        assert_eq!((image.width, image.height), (2, 2));
    }
}
