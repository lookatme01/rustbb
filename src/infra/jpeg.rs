//! Lossless removal of privacy-sensitive metadata from JPEG files.
//!
//! A photo straight from a phone carries its GPS position, the device, a timestamp and often an
//! embedded thumbnail in an EXIF block. Attachments are stored exactly as uploaded and served
//! to everyone who can see the post, so that block is a leak. This drops it (and XMP, IPTC,
//! comments and other application segments) without touching the image data: nothing is decoded
//! or re-encoded. Two things are kept because the picture needs them: the ICC colour profile
//! and the Adobe colour-transform marker. The EXIF orientation is kept too, as a minimal EXIF
//! block holding only that tag, or a rotated photo would show sideways.
//!
//! Anything unexpected (truncated segments, no SOI) makes `strip_metadata` return `None` and the
//! caller keeps the original bytes: a stripper must never corrupt an upload.

const SOI: u8 = 0xD8;
const EOI: u8 = 0xD9;
const SOS: u8 = 0xDA;

/// The file without its metadata segments, or `None` when it is not a JPEG we can walk safely
/// or there was nothing to remove.
pub fn strip_metadata(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != SOI {
        return None;
    }
    let mut out = Vec::with_capacity(data.len());
    out.extend_from_slice(&data[..2]);
    let mut i = 2;
    loop {
        // Every segment starts with 0xFF, optionally preceded by fill 0xFFs.
        if *data.get(i)? != 0xFF {
            return None;
        }
        while *data.get(i + 1)? == 0xFF {
            i += 1;
        }
        let marker = *data.get(i + 1)?;
        i += 2;
        match marker {
            // The scan: entropy-coded data follows to the end of the file; copy it as is.
            SOS => {
                let len = u16::from_be_bytes([*data.get(i)?, *data.get(i + 1)?]) as usize;
                if len < 2 || i + len > data.len() {
                    return None;
                }
                out.extend_from_slice(&[0xFF, SOS]);
                out.extend_from_slice(&data[i..]);
                return (out.len() < data.len()).then_some(out);
            }
            EOI => {
                out.extend_from_slice(&[0xFF, EOI]);
                return (out.len() < data.len()).then_some(out);
            }
            // Markers without a length field.
            0x01 | 0xD0..=0xD7 => out.extend_from_slice(&[0xFF, marker]),
            _ => {
                let len = u16::from_be_bytes([*data.get(i)?, *data.get(i + 1)?]) as usize;
                if len < 2 || i + len > data.len() {
                    return None;
                }
                let body = &data[i + 2..i + len];
                match marker {
                    0xE1 if body.starts_with(b"Exif\0\0") => {
                        if let Some(o) = orientation(&body[6..]).filter(|o| *o > 1) {
                            out.extend_from_slice(&orientation_segment(o));
                        }
                    }
                    // JFIF, the ICC profile, Adobe's colour transform: the picture needs these.
                    0xE0 | 0xEE => push_segment(&mut out, marker, &data[i..i + len]),
                    0xE2 if body.starts_with(b"ICC_PROFILE\0") => {
                        push_segment(&mut out, marker, &data[i..i + len])
                    }
                    // Every other application segment (XMP, IPTC, MPF, vendor notes) and
                    // comments go.
                    0xE1..=0xEF | 0xFE => {}
                    // Tables, frame header, restart interval: untouched.
                    _ => push_segment(&mut out, marker, &data[i..i + len]),
                }
                i += len;
            }
        }
    }
}

fn push_segment(out: &mut Vec<u8>, marker: u8, with_length: &[u8]) {
    out.extend_from_slice(&[0xFF, marker]);
    out.extend_from_slice(with_length);
}

/// The IFD0 Orientation tag (1-8) from a TIFF structure, if it has a well-formed one.
fn orientation(tiff: &[u8]) -> Option<u16> {
    let big = match tiff.get(..4)? {
        b"MM\0*" => true,
        b"II*\0" => false,
        _ => return None,
    };
    let u16_at = |o: usize| -> Option<u16> {
        let b = [*tiff.get(o)?, *tiff.get(o + 1)?];
        Some(if big {
            u16::from_be_bytes(b)
        } else {
            u16::from_le_bytes(b)
        })
    };
    let u32_at = |o: usize| -> Option<u32> {
        let b = [
            *tiff.get(o)?,
            *tiff.get(o + 1)?,
            *tiff.get(o + 2)?,
            *tiff.get(o + 3)?,
        ];
        Some(if big {
            u32::from_be_bytes(b)
        } else {
            u32::from_le_bytes(b)
        })
    };
    let ifd = u32_at(4)? as usize;
    let count = u16_at(ifd)? as usize;
    for n in 0..count.min(256) {
        let e = ifd.checked_add(2)?.checked_add(n.checked_mul(12)?)?;
        if u16_at(e)? == 0x0112 && u16_at(e + 2)? == 3 && u32_at(e + 4)? == 1 {
            return u16_at(e + 8).filter(|o| (1..=8).contains(o));
        }
    }
    None
}

/// An APP1 EXIF segment holding nothing but the orientation tag.
fn orientation_segment(o: u16) -> Vec<u8> {
    let mut s = vec![0xFF, 0xE1, 0x00, 0x22];
    s.extend_from_slice(b"Exif\0\0");
    s.extend_from_slice(b"MM\0*\0\0\0\x08"); // big-endian TIFF, IFD0 at offset 8
    s.extend_from_slice(&[0x00, 0x01]); // one entry
    s.extend_from_slice(&[0x01, 0x12, 0x00, 0x03, 0x00, 0x00, 0x00, 0x01]); // Orientation, SHORT, 1
    s.extend_from_slice(&o.to_be_bytes());
    s.extend_from_slice(&[0x00, 0x00]); // value padding
    s.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // no next IFD
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A little-endian EXIF block with orientation, a GPS pointer and an ASCII "location".
    fn exif_with_gps(orientation: u16) -> Vec<u8> {
        let mut t = b"II*\0".to_vec();
        t.extend_from_slice(&8u32.to_le_bytes());
        t.extend_from_slice(&3u16.to_le_bytes());
        // Make (ASCII, 10 bytes at offset 50)
        t.extend_from_slice(&[0x0F, 0x01, 0x02, 0x00, 0x0A, 0x00, 0x00, 0x00, 50, 0, 0, 0]);
        // Orientation
        t.extend_from_slice(&[0x12, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00]);
        t.extend_from_slice(&orientation.to_le_bytes());
        t.extend_from_slice(&[0, 0]);
        // GPS IFD pointer
        t.extend_from_slice(&[0x25, 0x88, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 60, 0, 0, 0]);
        t.extend_from_slice(&0u32.to_le_bytes());
        t.resize(50, 0);
        t.extend_from_slice(b"SecretCam\0");
        t.extend_from_slice(b"GPSLATITUDE-51.5N");
        let mut seg = vec![0xFF, 0xE1];
        let len = (2 + 6 + t.len()) as u16;
        seg.extend_from_slice(&len.to_be_bytes());
        seg.extend_from_slice(b"Exif\0\0");
        seg.extend_from_slice(&t);
        seg
    }

    fn encoded_jpeg() -> Vec<u8> {
        let img = image::RgbImage::from_fn(40, 24, |x, y| {
            image::Rgb([(x * 6) as u8, (y * 10) as u8, 90])
        });
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Jpeg)
            .unwrap();
        out.into_inner()
    }

    fn with_segments(jpeg: &[u8], segments: &[Vec<u8>]) -> Vec<u8> {
        let mut v = jpeg[..2].to_vec();
        for s in segments {
            v.extend_from_slice(s);
        }
        v.extend_from_slice(&jpeg[2..]);
        v
    }

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn gps_camera_xmp_and_comments_are_removed_and_the_picture_is_unchanged() {
        let clean = encoded_jpeg();
        let xmp = {
            let body =
                b"http://ns.adobe.com/xap/1.0/\0<x:xmpmeta>photographer@example.test</x:xmpmeta>";
            let mut s = vec![0xFF, 0xE1];
            s.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
            s.extend_from_slice(body);
            s
        };
        let comment = {
            let body = b"taken at my house";
            let mut s = vec![0xFF, 0xFE];
            s.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
            s.extend_from_slice(body);
            s
        };
        let dirty = with_segments(&clean, &[exif_with_gps(1), xmp, comment]);
        assert!(contains(&dirty, b"GPSLATITUDE") && contains(&dirty, b"SecretCam"));
        let stripped = strip_metadata(&dirty).expect("there was metadata to remove");
        for leak in [
            &b"GPSLATITUDE"[..],
            b"SecretCam",
            b"photographer@example.test",
            b"my house",
            b"Exif",
        ] {
            assert!(
                !contains(&stripped, leak),
                "{:?} survived",
                String::from_utf8_lossy(leak)
            );
        }
        assert_eq!(&stripped[..2], &[0xFF, 0xD8]);
        assert_eq!(&stripped[stripped.len() - 2..], &[0xFF, 0xD9]);
        // Decodes to exactly the pixels of the original.
        let a = image::load_from_memory(&clean).unwrap().to_rgb8();
        let b = image::load_from_memory(&stripped).unwrap().to_rgb8();
        assert_eq!(a.dimensions(), b.dimensions());
        assert_eq!(a.as_raw(), b.as_raw());
    }

    #[test]
    fn a_rotated_photo_keeps_only_its_orientation() {
        let dirty = with_segments(&encoded_jpeg(), &[exif_with_gps(6)]);
        let stripped = strip_metadata(&dirty).unwrap();
        assert!(!contains(&stripped, b"GPSLATITUDE") && !contains(&stripped, b"SecretCam"));
        // The remaining EXIF block is the minimal one, and it still says "rotated 90 degrees".
        let at = stripped
            .windows(6)
            .position(|w| w == b"Exif\0\0")
            .expect("orientation kept");
        assert_eq!(orientation(&stripped[at + 6..]), Some(6));
        assert!(image::load_from_memory(&stripped).is_ok());
    }

    #[test]
    fn files_that_are_not_jpegs_or_are_cut_short_are_left_alone() {
        assert!(strip_metadata(b"GIF89a....").is_none());
        assert!(strip_metadata(b"").is_none());
        let dirty = with_segments(&encoded_jpeg(), &[exif_with_gps(1)]);
        // Cut in the middle of the EXIF segment: do not guess, keep the original.
        assert!(strip_metadata(&dirty[..40]).is_none());
        // A JPEG with nothing to remove is not rewritten.
        assert!(strip_metadata(&encoded_jpeg()).is_none());
    }
}
