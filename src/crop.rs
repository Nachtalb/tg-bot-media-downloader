//! Crop downloaded screenshots down to their content with the `autocrop` detector.
//! Files that don't look like screenshots are left untouched.

use autocrop::{Encoding, Params, Rect, RgbImage, find_crop};
use std::path::{Path, PathBuf};
use tokio::fs;

type Error = Box<dyn std::error::Error + Send + Sync>;

/// Mean luminance at or below which a probe frame counts as black.
const BLACK_LUMA: f32 = 20.0;

/// Crop `path` in place. Returns whether the file was changed.
pub async fn autocrop_file(path: &Path) -> Result<bool, Error> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let encoding = match ext.as_str() {
        "jpg" | "jpeg" | "jpe" => Encoding::Jpeg { quality: 95 },
        "png" => Encoding::Png,
        "webp" => Encoding::WebPLossless,
        "mp4" | "webm" => return crop_clip(path, &ext).await,
        _ => return Ok(false),
    };
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let img = RgbImage::load(&path)?;
        let Some(rect) = find_crop(&img, &Params::default()).rect else {
            return Ok(false);
        };
        let tmp = tmp_path(&path, &ext);
        std::fs::write(&tmp, img.crop(&rect).encode(encoding)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(true)
    })
    .await?
}

async fn crop_clip(path: &Path, ext: &str) -> Result<bool, Error> {
    let frame = path.with_extension("probe.jpg");
    let rect = probe_clip(path, &frame).await;
    let _ = fs::remove_file(&frame).await;
    let Some(rect) = rect? else {
        return Ok(false);
    };
    let tmp = tmp_path(path, ext);
    let (w, h) = (rect.width() & !1, rect.height() & !1); // yuv420p needs even dims
    let mut args = vec![
        "-i".to_string(),
        path.to_string_lossy().into_owned(),
        "-vf".into(),
        format!("crop={w}:{h}:{}:{}", rect.x0, rect.y0),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-c:a".into(),
        "copy".into(),
    ];
    if ext == "mp4" {
        args.extend(["-movflags".into(), "+faststart".into()]);
    }
    args.push(tmp.to_string_lossy().into_owned());
    if !ffmpeg(&args).await? {
        let _ = fs::remove_file(&tmp).await;
        return Err("ffmpeg crop failed".into());
    }
    fs::rename(&tmp, path).await?;
    Ok(true)
}

/// Detect on the first frame; when that is black or yields nothing, retry 3 s
/// in, then on the last frame for clips shorter than that.
async fn probe_clip(path: &Path, frame: &Path) -> Result<Option<Rect>, Error> {
    let input = path.to_string_lossy().into_owned();
    let frame_s = frame.to_string_lossy().into_owned();
    let seeks: [&[&str]; 3] = [&[], &["-ss", "3"], &["-sseof", "-0.5"]];
    for seek in seeks {
        let _ = fs::remove_file(frame).await;
        let mut args: Vec<String> = seek.iter().map(|s| s.to_string()).collect();
        args.extend(["-i", &input, "-frames:v", "1", "-q:v", "2", &frame_s].map(String::from));
        if !ffmpeg(&args).await? || fs::metadata(frame).await.is_err() {
            continue; // seek past the end: no frame written
        }
        let frame = frame.to_owned();
        let rect = tokio::task::spawn_blocking(move || {
            let img = RgbImage::load(&frame)?;
            Ok::<_, Error>(if is_black(&img) {
                None
            } else {
                find_crop(&img, &Params::default()).rect
            })
        })
        .await??;
        if rect.is_some() {
            return Ok(rect);
        }
    }
    Ok(None)
}

fn is_black(img: &RgbImage) -> bool {
    let sum: f32 = img.pixels.iter().map(|&p| autocrop::image::luminance(p)).sum();
    sum / img.pixels.len().max(1) as f32 <= BLACK_LUMA
}

/// `x.jpg` -> `x.crop.tmp.jpg`: same directory, extension kept for encoders.
fn tmp_path(path: &Path, ext: &str) -> PathBuf {
    path.with_extension(format!("crop.tmp.{ext}"))
}

async fn ffmpeg(args: &[String]) -> Result<bool, Error> {
    let out = tokio::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-loglevel", "error", "-y"])
        .args(args)
        .output()
        .await?;
    if !out.status.success() {
        log::warn!("ffmpeg {}: {}", out.status, String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A screenshot is cropped in place; a flat image is left alone.
    #[tokio::test]
    async fn crops_screenshot_in_place() {
        let dir = std::env::temp_dir().join(format!("downloader-crop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shot = dir.join("shot.jpg");
        std::fs::copy("tests/screenshot.jpg", &shot).unwrap();
        assert!(autocrop_file(&shot).await.unwrap());
        let img = RgbImage::load(&shot).unwrap();
        assert!(img.height < 1286, "{}x{}", img.width, img.height);

        let flat = dir.join("flat.png");
        RgbImage::solid(64, 64, [200, 200, 200]).save(&flat).unwrap();
        assert!(!autocrop_file(&flat).await.unwrap());

        let clip = dir.join("clip.mp4");
        let args = ["-loop", "1", "-t", "1", "-i", "tests/screenshot.jpg", "-r", "5", "-pix_fmt", "yuv420p"]
            .map(String::from)
            .into_iter()
            .chain([clip.to_string_lossy().into_owned()])
            .collect::<Vec<_>>();
        assert!(ffmpeg(&args).await.unwrap());
        assert!(autocrop_file(&clip).await.unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
