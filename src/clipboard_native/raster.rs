//! Bounded bitmap conversion shared by Windows and platform-independent tests.
use lan_mouse_data::clipboard_image::{MAX_IMAGE, MAX_PIXELS, bounded_dimensions};
use std::io::{self, Cursor};

fn invalid() -> io::Error {
    io::ErrorKind::InvalidData.into()
}
pub(super) fn thumbnail(png: &[u8]) -> io::Result<Vec<u8>> {
    let (w, h, rgba) = decode(png)?;
    let scale = (w as f64 / 160.0).max(h as f64 / 100.0).max(1.0);
    let (tw, th) = (
        (w as f64 / scale).max(1.0) as u32,
        (h as f64 / scale).max(1.0) as u32,
    );
    let mut pixels = Vec::with_capacity(tw as usize * th as usize * 4);
    for y in 0..th {
        for x in 0..tw {
            let source = ((y as u64 * h as u64 / th as u64) * w as u64
                + x as u64 * w as u64 / tw as u64) as usize
                * 4;
            pixels.extend_from_slice(&rgba[source..source + 4]);
        }
    }
    encode(tw, th, &pixels)
}
pub(super) fn encode(width: u32, height: u32, rgba: &[u8]) -> io::Result<Vec<u8>> {
    bounded_dimensions(width, height)?;
    if rgba.len() != width as usize * height as usize * 4 {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header()?.write_image_data(rgba)?;
    }
    if bytes.len() > MAX_IMAGE {
        return Err(invalid());
    }
    Ok(bytes)
}
pub(super) fn decode(bytes: &[u8]) -> io::Result<(u32, u32, Vec<u8>)> {
    if bytes.len() > MAX_IMAGE {
        return Err(invalid());
    }
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_limits(png::Limits {
        bytes: MAX_PIXELS as usize * 8,
    });
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info()?;
    let (w, h) = bounded_dimensions(reader.info().width, reader.info().height)?;
    let size = reader.output_buffer_size().ok_or_else(invalid)?;
    if size > MAX_PIXELS as usize * 4 {
        return Err(invalid());
    }
    let mut buffer = vec![0; size];
    let info = reader.next_frame(&mut buffer)?;
    let channels = info.color_type.samples();
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    for p in buffer[..info.buffer_size()].chunks_exact(channels) {
        match info.color_type {
            png::ColorType::Rgba => rgba.extend_from_slice(p),
            png::ColorType::Rgb => rgba.extend_from_slice(&[p[0], p[1], p[2], 255]),
            png::ColorType::Grayscale => rgba.extend_from_slice(&[p[0], p[0], p[0], 255]),
            png::ColorType::GrayscaleAlpha => rgba.extend_from_slice(&[p[0], p[0], p[0], p[1]]),
            _ => return Err(invalid()),
        }
    }
    if rgba.len() != w as usize * h as usize * 4 {
        return Err(invalid());
    }
    Ok((w, h, rgba))
}
#[cfg(any(windows, test))]
fn u32_at(bytes: &[u8], offset: usize) -> io::Result<u32> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or_else(invalid)?
            .try_into()
            .unwrap(),
    ))
}
#[cfg(any(windows, test))]
fn set(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[cfg(any(windows, test))]
pub(super) fn to_dib(png: &[u8]) -> io::Result<Vec<u8>> {
    let (w, h, rgba) = decode(png)?;
    let mut dib = vec![0; 124 + rgba.len()];
    set(&mut dib, 0, 124);
    set(&mut dib, 4, w);
    set(&mut dib, 8, (-(h as i32)) as u32);
    dib[12..14].copy_from_slice(&1_u16.to_le_bytes());
    dib[14..16].copy_from_slice(&32_u16.to_le_bytes());
    set(&mut dib, 16, 3);
    set(&mut dib, 20, rgba.len() as u32);
    for (offset, mask) in [
        (40, 0xff0000),
        (44, 0xff00),
        (48, 0xff),
        (52, 0xff000000),
        (56, 0x73524742),
    ] {
        set(&mut dib, offset, mask);
    }
    for (source, target) in rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(dib[124..].as_chunks_mut::<4>().0.iter_mut())
    {
        target.copy_from_slice(&[source[2], source[1], source[0], source[3]]);
    }
    Ok(dib)
}
#[cfg(any(windows, test))]
pub(super) fn from_dib(dib: &[u8]) -> io::Result<Vec<u8>> {
    let header = u32_at(dib, 0)? as usize;
    if ![40, 52, 56, 108, 124].contains(&header) || dib.len() < header {
        return Err(invalid());
    }
    let w = u32_at(dib, 4)? as i32;
    let signed_h = u32_at(dib, 8)? as i32;
    let (w, h) = bounded_dimensions(
        u32::try_from(w).map_err(|_| invalid())?,
        signed_h.unsigned_abs(),
    )?;
    let planes = u16::from_le_bytes(dib[12..14].try_into().unwrap());
    let bits = u16::from_le_bytes(dib[14..16].try_into().unwrap()) as usize;
    let compression = u32_at(dib, 16)?;
    if planes != 1
        || ![24, 32].contains(&bits)
        || !(compression == 0 || compression == 3 && bits == 32)
    {
        return Err(invalid());
    }
    // Accept the common byte-aligned BGRA masks. Other layouts require a
    // decoder, rather than silently guessing and losing colour or alpha.
    let mut offset = header;
    let alpha = if compression == 3 {
        if header == 40 {
            offset += 12;
        }
        if [u32_at(dib, 40)?, u32_at(dib, 44)?, u32_at(dib, 48)?] != [0xff0000, 0xff00, 0xff] {
            return Err(invalid());
        }
        if header >= 56 {
            match u32_at(dib, 52)? {
                0 => false,
                0xff000000 => true,
                _ => return Err(invalid()),
            }
        } else {
            false
        }
    } else {
        false
    };
    if u32_at(dib, 32)? != 0 {
        return Err(invalid());
    } // no colour table
    // Packed DIBs with an embedded profile before the pixels are ambiguous.
    if header == 124 && (u32_at(dib, 112)? != 0 || u32_at(dib, 116)? != 0) {
        return Err(invalid());
    }
    let stride = (w as usize * bits).div_ceil(32) * 4;
    let pixels = dib
        .get(offset..offset + stride * h as usize)
        .ok_or_else(invalid)?;
    let rgb_alpha =
        compression == 0 && bits == 32 && pixels.as_chunks::<4>().0.iter().any(|p| p[3] != 0);
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h as usize {
        let row = if signed_h < 0 { y } else { h as usize - 1 - y };
        for p in pixels[row * stride..row * stride + w as usize * (bits / 8)].chunks_exact(bits / 8)
        {
            rgba.extend_from_slice(&[
                p[2],
                p[1],
                p[0],
                if alpha || rgb_alpha { p[3] } else { 255 },
            ]);
        }
    }
    encode(w, h, &rgba)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn png_dib_preserves_zero_partial_and_full_alpha() {
        let rgba = [0, 0, 0, 0, 20, 100, 200, 127, 200, 30, 50, 255];
        let png = encode(3, 1, &rgba).unwrap();
        let dib = to_dib(&png).unwrap();
        assert_eq!(
            decode(&from_dib(&dib).unwrap()).unwrap(),
            (3, 1, rgba.to_vec())
        );
        // Explicit alpha masks preserve an entirely transparent image.
        let transparent = encode(1, 1, &[0, 0, 0, 0]).unwrap();
        assert_eq!(
            decode(&from_dib(&to_dib(&transparent).unwrap()).unwrap())
                .unwrap()
                .2,
            [0, 0, 0, 0]
        );
    }
    #[test]
    fn bottom_up_24_bit_padding_and_malformed_bounds() {
        let mut dib = vec![0; 48];
        set(&mut dib, 0, 40);
        set(&mut dib, 4, 1);
        set(&mut dib, 8, 2);
        dib[12] = 1;
        dib[14] = 24;
        dib[40..48].copy_from_slice(&[255, 0, 0, 0, 0, 0, 255, 0]);
        assert_eq!(
            decode(&from_dib(&dib).unwrap()).unwrap().2,
            [255, 0, 0, 255, 0, 0, 255, 255]
        );
        assert!(from_dib(&dib[..47]).is_err());
        set(&mut dib, 8, i32::MIN as u32);
        assert!(from_dib(&dib).is_err());
        assert!(decode(b"not a png").is_err());
    }
}
