//! Stitch rendered PNG frames into an animated PNG.
//!
//! An APNG is an ordinary PNG whose frames are wrapped in `acTL`, `fcTL` and
//! `fdAT` chunks, so a frame's compressed image data is re-wrapped rather than
//! re-encoded and stays pixel-identical to what the rasteriser produced. Only
//! frames whose pixel format has to be widened, or that are cropped here, are
//! decoded and recompressed.
//!
//! The frames directory holds one `NNN.png` per frame plus a `frames.txt`
//! manifest, in one of two forms. Frames rendered from the terminal UI know
//! which cells changed, so they arrive pre-cropped and say where they belong:
//!
//! ```text
//! NNN x y delay_ms
//! ```
//!
//! The offset is in CSS pixels and is multiplied by the scale factor to match
//! the rasterised device pixels. Frames rendered from an HTML mock-up have no
//! cells to diff, so they arrive as whole canvases and the changed region is
//! found here by comparing pixels:
//!
//! ```text
//! NNN delay_ms
//! ```
//!
//! Either way frame 000 covers the whole canvas and later frames patch only
//! what moved, which is what keeps a multi-second animation to a few tens of
//! kilobytes. A manifest may not mix the two forms.
//!
//! Compiled out unless the `screenshots` feature is enabled.

use std::fs;
use std::io::{Read as _, Write as _};
use std::path::Path;

use anyhow::{Context as _, Result, bail, ensure};
use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;

const MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
const RGB: u8 = 2;
const RGBA: u8 = 6;

/// One frame's image, with its `IDAT` payload left zlib-compressed so it can be
/// handed straight to an `fdAT` chunk when nothing needs changing.
struct Png {
    width: u32,
    height: u32,
    depth: u8,
    color: u8,
    interlace: u8,
    idat: Vec<u8>,
}

impl Png {
    fn ihdr(&self) -> [u8; 13] {
        let mut out = [0u8; 13];
        out[0..4].copy_from_slice(&self.width.to_be_bytes());
        out[4..8].copy_from_slice(&self.height.to_be_bytes());
        out[8] = self.depth;
        out[9] = self.color;
        out[12] = self.interlace;
        out
    }

    /// Bytes per pixel in the encoded rows, which is what row filters step by.
    fn step(&self) -> usize {
        if self.color == RGBA { 4 } else { 3 }
    }
}

fn be32(bytes: &[u8]) -> Result<u32> {
    Ok(u32::from_be_bytes(bytes.try_into()?))
}

fn read_png(path: &Path) -> Result<Png> {
    let data = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    ensure!(
        data.get(..8) == Some(&MAGIC[..]),
        "{}: not a PNG",
        path.display()
    );

    let mut header: Option<Vec<u8>> = None;
    let mut idat = Vec::new();
    let mut at = 8;
    while at + 12 <= data.len() {
        let len = usize::try_from(be32(&data[at..at + 4])?)?;
        let end = at + 8 + len;
        ensure!(end + 4 <= data.len(), "{}: truncated chunk", path.display());
        match &data[at + 4..at + 8] {
            b"IHDR" => header = Some(data[at + 8..end].to_vec()),
            b"IDAT" => idat.extend_from_slice(&data[at + 8..end]),
            _ => {}
        }
        at = end + 4;
    }

    let header = header.with_context(|| format!("{}: no IHDR", path.display()))?;
    ensure!(header.len() == 13, "{}: malformed IHDR", path.display());
    ensure!(!idat.is_empty(), "{}: no image data", path.display());
    let png = Png {
        width: be32(&header[0..4])?,
        height: be32(&header[4..8])?,
        depth: header[8],
        color: header[9],
        interlace: header[12],
        idat,
    };
    ensure!(
        png.depth == 8 && (png.color == RGB || png.color == RGBA) && png.interlace == 0,
        "{}: expected non-interlaced 8-bit truecolour, got depth {} colour type {}",
        path.display(),
        png.depth,
        png.color,
    );
    Ok(png)
}

fn inflate(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    ZlibDecoder::new(data).read_to_end(&mut out)?;
    Ok(out)
}

fn deflate(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = i32::from(a) + i32::from(b) - i32::from(c);
    let (pa, pb, pc) = (
        (p - i32::from(a)).abs(),
        (p - i32::from(b)).abs(),
        (p - i32::from(c)).abs(),
    );
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Undo one row's PNG filter in place, given the already-unfiltered row above.
fn unfilter(kind: u8, row: &mut [u8], prior: &[u8], step: usize) -> Result<()> {
    let left = |row: &[u8], i: usize| if i >= step { row[i - step] } else { 0 };
    match kind {
        0 => {}
        1 => {
            for i in step..row.len() {
                row[i] = row[i].wrapping_add(row[i - step]);
            }
        }
        2 => {
            for i in 0..row.len() {
                row[i] = row[i].wrapping_add(prior[i]);
            }
        }
        3 => {
            for i in 0..row.len() {
                // Mean of two bytes without widening: the carry of the sum is
                // the shared bit, and half the difference is the rest.
                let (a, b) = (left(row, i), prior[i]);
                row[i] = row[i].wrapping_add((a & b) + ((a ^ b) >> 1));
            }
        }
        4 => {
            for i in 0..row.len() {
                let corner = left(prior, i);
                row[i] = row[i].wrapping_add(paeth(left(row, i), prior[i], corner));
            }
        }
        _ => bail!("unknown row filter {kind}"),
    }
    Ok(())
}

/// Expand a frame to unfiltered RGBA.
fn decode(png: &Png) -> Result<Vec<u8>> {
    let width = usize::try_from(png.width)?;
    let height = usize::try_from(png.height)?;
    let step = png.step();
    let stride = width * step;
    let row_bytes = width * 4;

    let raw = inflate(&png.idat)?;
    ensure!(
        raw.len() >= height * (stride + 1),
        "frame is shorter than its {width}x{height} header claims"
    );

    let mut flat = vec![0u8; height * row_bytes];
    let mut prior = vec![0u8; stride];
    let mut row = vec![0u8; stride];
    let mut at = 0;
    for y in 0..height {
        let kind = raw[at];
        row.copy_from_slice(&raw[at + 1..at + 1 + stride]);
        at += 1 + stride;
        unfilter(kind, &mut row, &prior, step)?;

        let out = &mut flat[y * row_bytes..(y + 1) * row_bytes];
        if step == 4 {
            out.copy_from_slice(&row);
        } else {
            for (pixel, source) in out.chunks_exact_mut(4).zip(row.chunks_exact(3)) {
                pixel[0..3].copy_from_slice(source);
                pixel[3] = 0xff;
            }
        }
        prior.copy_from_slice(&row);
    }
    Ok(flat)
}

/// Compress unfiltered RGBA into a frame, filtering each row with Up, which
/// flattens the long runs of identical rows a terminal image is mostly made of.
fn encode(width: u32, height: u32, flat: &[u8]) -> Result<Png> {
    let row_bytes = usize::try_from(width)? * 4;
    let mut out = Vec::with_capacity(flat.len() + flat.len() / row_bytes.max(1));
    let mut prior = vec![0u8; row_bytes];
    for row in flat.chunks_exact(row_bytes) {
        out.push(2);
        out.extend(row.iter().zip(&prior).map(|(a, b)| a.wrapping_sub(*b)));
        prior.copy_from_slice(row);
    }
    Ok(Png {
        width,
        height,
        depth: 8,
        color: RGBA,
        interlace: 0,
        idat: deflate(&out)?,
    })
}

/// Widen a truecolour frame to RGBA.
///
/// Chrome drops the alpha channel from frames that happen to be fully opaque,
/// which is most of them: only the first frame has the window's rounded corners
/// showing through. APNG requires every frame to share one pixel format, so the
/// opaque ones are widened here rather than relying on the browser's encoder to
/// make a consistent choice.
fn to_rgba(png: Png) -> Result<Png> {
    if png.color == RGBA {
        return Ok(png);
    }
    let flat = decode(&png)?;
    encode(png.width, png.height, &flat)
}

/// First and last differing pixel in a row, or `None` if it is unchanged.
fn row_span(before: &[u8], after: &[u8]) -> Option<(usize, usize)> {
    let differs = |(a, b): (&u8, &u8)| a != b;
    let first = before.iter().zip(after).position(differs)?;
    let last = before.iter().zip(after).rposition(differs)?;
    Some((first / 4, last / 4))
}

/// Smallest rectangle covering every pixel that changed, as (x, y, w, h).
fn dirty_box(
    before: &[u8],
    after: &[u8],
    width: usize,
    height: usize,
) -> Option<(u32, u32, u32, u32)> {
    let stride = width * 4;
    let (mut left, mut right) = (usize::MAX, 0);
    let (mut top, mut bottom) = (usize::MAX, 0);
    for y in 0..height {
        let rows = (
            &before[y * stride..(y + 1) * stride],
            &after[y * stride..(y + 1) * stride],
        );
        if let Some((first, last)) = row_span(rows.0, rows.1) {
            top = top.min(y);
            bottom = y;
            left = left.min(first);
            right = right.max(last);
        }
    }
    if top == usize::MAX {
        return None;
    }
    Some((
        u32::try_from(left).ok()?,
        u32::try_from(top).ok()?,
        u32::try_from(right - left + 1).ok()?,
        u32::try_from(bottom - top + 1).ok()?,
    ))
}

/// Copy a rectangle out of an unfiltered RGBA buffer.
fn crop(flat: &[u8], width: usize, rect: (u32, u32, u32, u32)) -> Result<Vec<u8>> {
    let (x, y, w, h) = (
        usize::try_from(rect.0)?,
        usize::try_from(rect.1)?,
        usize::try_from(rect.2)?,
        usize::try_from(rect.3)?,
    );
    let stride = width * 4;
    let mut out = Vec::with_capacity(w * h * 4);
    for row in y..y + h {
        let start = row * stride + x * 4;
        out.extend_from_slice(&flat[start..start + w * 4]);
    }
    Ok(out)
}

/// A manifest line: which frame, where it goes if it is already cropped, and
/// how long it is held.
struct Entry {
    name: String,
    offset: Option<(u32, u32)>,
    delay_ms: u16,
}

fn parse_manifest(path: &Path) -> Result<Vec<Entry>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut entries = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let entry = match fields[..] {
            [name, x, y, delay] => Entry {
                name: name.to_owned(),
                offset: Some((x.parse()?, y.parse()?)),
                delay_ms: delay.parse()?,
            },
            [name, delay] => Entry {
                name: name.to_owned(),
                offset: None,
                delay_ms: delay.parse()?,
            },
            _ => bail!(
                "{}:{}: expected 'name x y delay' or 'name delay'",
                path.display(),
                number + 1
            ),
        };
        entries.push(entry);
    }
    let first = entries
        .first()
        .with_context(|| format!("{}: no frames", path.display()))?;
    ensure!(
        entries
            .iter()
            .all(|entry| entry.offset.is_some() == first.offset.is_some()),
        "{}: mixes pre-cropped and whole-canvas frames",
        path.display()
    );
    Ok(entries)
}

/// A frame ready to be written: its image and where it lands on the canvas.
struct Placed {
    png: Png,
    x: u32,
    y: u32,
    delay_ms: u16,
}

/// Frames that already carry only their changed region, from the terminal
/// renderer, which diffs cells before rasterising.
fn precropped(dir: &Path, entries: &[Entry], scale: u32) -> Result<Vec<Placed>> {
    let mut placed = Vec::with_capacity(entries.len());
    for entry in entries {
        let png = to_rgba(read_png(&dir.join(format!("{}.png", entry.name)))?)?;
        let (x, y) = entry.offset.unwrap_or((0, 0));
        placed.push(Placed {
            png,
            x: x * scale,
            y: y * scale,
            delay_ms: entry.delay_ms,
        });
    }
    Ok(placed)
}

/// Whole-canvas frames, as a browser renders an HTML mock-up: the changed
/// region is found by comparing pixels against the frame already on screen.
fn autocrop(dir: &Path, entries: &[Entry]) -> Result<Vec<Placed>> {
    let mut placed: Vec<Placed> = Vec::with_capacity(entries.len());
    let mut canvas = Vec::new();
    let mut size = (0, 0);

    for (index, entry) in entries.iter().enumerate() {
        let png = read_png(&dir.join(format!("{}.png", entry.name)))?;
        let flat = decode(&png)?;
        if index == 0 {
            size = (png.width, png.height);
            placed.push(Placed {
                png: encode(png.width, png.height, &flat)?,
                x: 0,
                y: 0,
                delay_ms: entry.delay_ms,
            });
            canvas = flat;
            continue;
        }
        ensure!(
            (png.width, png.height) == size,
            "{}.png: {}x{} does not match the canvas",
            entry.name,
            png.width,
            png.height
        );

        let width = usize::try_from(png.width)?;
        let height = usize::try_from(png.height)?;
        let Some(rect) = dirty_box(&canvas, &flat, width, height) else {
            // Nothing moved; give the time to the frame already on screen.
            if let Some(last) = placed.last_mut() {
                last.delay_ms = last.delay_ms.saturating_add(entry.delay_ms);
            }
            continue;
        };
        let patch = crop(&flat, width, rect)?;
        placed.push(Placed {
            png: encode(rect.2, rect.3, &patch)?,
            x: rect.0,
            y: rect.1,
            delay_ms: entry.delay_ms,
        });
        canvas = flat;
    }
    Ok(placed)
}

fn chunk(out: &mut Vec<u8>, tag: [u8; 4], payload: &[u8]) -> Result<()> {
    out.extend_from_slice(&u32::try_from(payload.len())?.to_be_bytes());
    out.extend_from_slice(&tag);
    out.extend_from_slice(payload);
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&tag);
    hasher.update(payload);
    out.extend_from_slice(&hasher.finalize().to_be_bytes());
    Ok(())
}

/// `fcTL` describes one frame's placement and how long it is shown.
///
/// `dispose_op` NONE keeps the frame on screen for the next one to patch, and
/// `blend_op` SOURCE replaces the region outright rather than compositing.
fn frame_control(sequence: u32, frame: &Placed) -> Vec<u8> {
    let mut payload = Vec::with_capacity(26);
    payload.extend_from_slice(&sequence.to_be_bytes());
    payload.extend_from_slice(&frame.png.width.to_be_bytes());
    payload.extend_from_slice(&frame.png.height.to_be_bytes());
    payload.extend_from_slice(&frame.x.to_be_bytes());
    payload.extend_from_slice(&frame.y.to_be_bytes());
    payload.extend_from_slice(&frame.delay_ms.to_be_bytes());
    payload.extend_from_slice(&1000u16.to_be_bytes());
    payload.extend_from_slice(&[0, 0]);
    payload
}

fn assemble(placed: &[Placed], plays: u32) -> Result<Vec<u8>> {
    let canvas = placed.first().context("no frames")?;
    let (width, height) = (canvas.png.width, canvas.png.height);
    ensure!(
        (canvas.x, canvas.y) == (0, 0),
        "the first frame must cover the whole canvas"
    );

    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    chunk(&mut out, *b"IHDR", &canvas.png.ihdr())?;

    let mut actl = Vec::with_capacity(8);
    actl.extend_from_slice(&u32::try_from(placed.len())?.to_be_bytes());
    actl.extend_from_slice(&plays.to_be_bytes());
    chunk(&mut out, *b"acTL", &actl)?;

    let mut sequence = 0;
    for (index, frame) in placed.iter().enumerate() {
        ensure!(
            frame.x + frame.png.width <= width && frame.y + frame.png.height <= height,
            "frame {index}: {}x{} at ({},{}) overflows the {width}x{height} canvas",
            frame.png.width,
            frame.png.height,
            frame.x,
            frame.y,
        );
        chunk(&mut out, *b"fcTL", &frame_control(sequence, frame))?;
        sequence += 1;

        if index == 0 {
            chunk(&mut out, *b"IDAT", &frame.png.idat)?;
        } else {
            let mut fdat = Vec::with_capacity(4 + frame.png.idat.len());
            fdat.extend_from_slice(&sequence.to_be_bytes());
            fdat.extend_from_slice(&frame.png.idat);
            chunk(&mut out, *b"fdAT", &fdat)?;
            sequence += 1;
        }
    }
    chunk(&mut out, *b"IEND", b"")?;
    Ok(out)
}

fn build(out: &Path, dir: &Path, scale: u32, plays: u32) -> Result<()> {
    let entries = parse_manifest(&dir.join("frames.txt"))?;
    let placed = if entries[0].offset.is_some() {
        precropped(dir, &entries, scale)?
    } else {
        autocrop(dir, &entries)?
    };

    let data = assemble(&placed, plays)?;
    fs::write(out, &data).with_context(|| format!("writing {}", out.display()))?;

    let seconds = f64::from(u32::from(
        placed.iter().map(|frame| frame.delay_ms).sum::<u16>(),
    )) / 1000.0;
    println!(
        "wrote {} ({} frames, {}x{}, {seconds:.1}s, {} KB)",
        out.display(),
        placed.len(),
        placed[0].png.width,
        placed[0].png.height,
        data.len() / 1024,
    );
    Ok(())
}

pub fn main(args: &[String]) -> Result<()> {
    let ([out, dir, scale], plays) = match args {
        [out, dir, scale] => ([out, dir, scale], 1),
        [out, dir, scale, plays] => ([out, dir, scale], plays.parse()?),
        _ => bail!("usage: __apng <out.png> <frames-dir> <scale> [plays]"),
    };
    build(Path::new(out), Path::new(dir), scale.parse()?, plays)
}

#[cfg(test)]
mod tests {
    use super::{Png, RGB, assemble, decode, dirty_box, encode, row_span, to_rgba};

    fn gradient(width: u32, height: u32) -> Vec<u8> {
        (0..width * height)
            .flat_map(|i| {
                let v = u8::try_from(i % 251).unwrap_or(0);
                [v, v.wrapping_mul(3), v.wrapping_add(17), 0xff]
            })
            .collect()
    }

    #[test]
    fn encode_decode_round_trips() {
        let flat = gradient(9, 7);
        let png = encode(9, 7, &flat).expect("encode");
        assert_eq!(decode(&png).expect("decode"), flat);
    }

    #[test]
    fn widening_rgb_preserves_pixels() {
        let flat = gradient(6, 4);
        // Re-pack as RGB rows with the None filter, which is what a browser
        // emits for a frame that happens to be fully opaque.
        let mut raw = Vec::new();
        for row in flat.chunks_exact(6 * 4) {
            raw.push(0);
            for pixel in row.chunks_exact(4) {
                raw.extend_from_slice(&pixel[0..3]);
            }
        }
        let rgb = Png {
            width: 6,
            height: 4,
            depth: 8,
            color: RGB,
            interlace: 0,
            idat: super::deflate(&raw).expect("deflate"),
        };
        let widened = to_rgba(rgb).expect("widen");
        assert_eq!(decode(&widened).expect("decode"), flat);
    }

    #[test]
    fn every_row_filter_round_trips() {
        // Up is what encode() writes; the rest arrive from the rasteriser, so
        // exercise them through the same unfilter path.
        let flat = gradient(5, 4);
        for kind in 0..=4u8 {
            let mut raw = Vec::new();
            let mut prior = vec![0u8; 5 * 4];
            for row in flat.chunks_exact(5 * 4) {
                raw.push(kind);
                raw.extend(filter(kind, row, &prior));
                prior.copy_from_slice(row);
            }
            let png = Png {
                width: 5,
                height: 4,
                depth: 8,
                color: super::RGBA,
                interlace: 0,
                idat: super::deflate(&raw).expect("deflate"),
            };
            assert_eq!(decode(&png).expect("decode"), flat, "filter {kind}");
        }
    }

    fn filter(kind: u8, row: &[u8], prior: &[u8]) -> Vec<u8> {
        let left = |i: usize| if i >= 4 { row[i - 4] } else { 0 };
        let corner = |i: usize| if i >= 4 { prior[i - 4] } else { 0 };
        (0..row.len())
            .map(|i| match kind {
                1 => row[i].wrapping_sub(left(i)),
                2 => row[i].wrapping_sub(prior[i]),
                3 => {
                    let (a, b) = (left(i), prior[i]);
                    row[i].wrapping_sub((a & b) + ((a ^ b) >> 1))
                }
                4 => row[i].wrapping_sub(super::paeth(left(i), prior[i], corner(i))),
                _ => row[i],
            })
            .collect()
    }

    #[test]
    fn row_span_finds_the_changed_pixels() {
        let before = vec![0u8; 5 * 4];
        let mut after = before.clone();
        let px = |i: usize| i * 4;
        after[px(1) + 2] = 9;
        after[px(3)] = 9;
        assert_eq!(row_span(&before, &after), Some((1, 3)));
        assert_eq!(row_span(&before, &before), None);
    }

    #[test]
    fn dirty_box_bounds_the_change() {
        let before = vec![0u8; 4 * 3 * 4];
        let mut after = before.clone();
        let px = |x: usize, y: usize| (y * 4 + x) * 4;
        after[px(1, 1)] = 7;
        after[px(2, 2)] = 7;
        assert_eq!(dirty_box(&before, &after, 4, 3), Some((1, 1, 2, 2)));
        assert_eq!(dirty_box(&before, &before, 4, 3), None);
    }

    #[test]
    fn assemble_rejects_a_frame_off_the_canvas() {
        let canvas = super::Placed {
            png: encode(4, 4, &gradient(4, 4)).expect("encode"),
            x: 0,
            y: 0,
            delay_ms: 10,
        };
        let overflowing = super::Placed {
            png: encode(3, 3, &gradient(3, 3)).expect("encode"),
            x: 2,
            y: 2,
            delay_ms: 10,
        };
        assert!(assemble(&[canvas, overflowing], 1).is_err());
    }
}
