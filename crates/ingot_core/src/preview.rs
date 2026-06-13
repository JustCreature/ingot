use fast_image_resize as fir;

pub fn make_preview_from_jpeg_bytes(src: &[u8]) -> Option<Vec<u8>> {
    let target_long_dim = 1920usize;
    let new_image = get_resized_from_jpeg_bytes(src, target_long_dim)?;

    Some(new_image)
}

pub fn make_thumbnail_from_jpeg_bytes(src: &[u8]) -> Option<Vec<u8>> {
    let target_long_dim = 512usize;
    let new_image = get_resized_from_jpeg_bytes(src, target_long_dim)?;

    Some(new_image)
}

pub fn get_resized_from_jpeg_bytes(src: &[u8], target_long_dim: usize) -> Option<Vec<u8>> {
    let decompressed_image = decompress(src, target_long_dim)?;
    let resized_image = resize(decompressed_image, target_long_dim)?;
    let new_image = compress(resized_image)?;

    Some(new_image)
}

fn pick_scaling_factor(src_dim: usize, target_dim: usize) -> turbojpeg::ScalingFactor {
    turbojpeg::Decompressor::supported_scaling_factors()
        .iter()
        .copied()
        .filter(|f| f.scale(src_dim) >= target_dim)
        .min_by_key(|f| f.scale(src_dim))
        .unwrap_or(turbojpeg::ScalingFactor::ONE)
}

pub fn decompress(src: &[u8], target_long_dim: usize) -> Option<turbojpeg::Image<Vec<u8>>> {
    let mut decompressor = turbojpeg::Decompressor::new().ok()?;

    let header = decompressor.read_header(src).ok()?;
    let scaling: turbojpeg::ScalingFactor =
        pick_scaling_factor(header.width.max(header.height), target_long_dim);
    decompressor.set_scaling_factor(scaling).ok()?;

    let scaled_header = header.scaled(scaling);

    // initialize the image (Image<Vec<u8>>)
    let mut image = turbojpeg::Image {
        pixels: vec![0; 3 * scaled_header.width * scaled_header.height],
        width: scaled_header.width,
        pitch: 3 * scaled_header.width, // size of one image row in memory
        height: scaled_header.height,
        format: turbojpeg::PixelFormat::RGB,
    };

    // decompress the JPEG into the image
    // (we use as_deref_mut() to convert from &mut Image<Vec<u8>> into Image<&mut [u8]>)
    decompressor.decompress(src, image.as_deref_mut()).ok()?;

    Some(image)
}

pub fn compress(src: turbojpeg::Image<Vec<u8>>) -> Option<Vec<u8>> {
    let mut compressor = turbojpeg::Compressor::new().ok()?;
    compressor.set_quality(85).ok()?;

    // compressor.compress(image, output)

    // initialize the output buffer
    let mut output_buf = turbojpeg::OutputBuf::new_owned();

    // compress the image into JPEG
    // (we use as_deref() to convert from &Image<Vec<u8>> to Image<&[u8]>)
    compressor.compress(src.as_deref(), &mut output_buf).ok()?;

    Some(output_buf.to_owned())
}

pub fn resize(
    src: turbojpeg::Image<Vec<u8>>,
    target_long_dim: usize,
) -> Option<turbojpeg::Image<Vec<u8>>> {
    // resize(src)
    // turbojpeg::Transform::op(turbojpeg::TransformOp::Transpose)
    let mut resizer = fir::Resizer::new();

    let long = src.width.max(src.height);
    let short = src.width.min(src.height);
    let scaled_short = (target_long_dim as f32 * short as f32 / long as f32).round() as usize;
    let (dst_w, dst_h) = if src.width >= src.height {
        (target_long_dim as u32, scaled_short as u32) // landscape
    } else {
        (scaled_short as u32, target_long_dim as u32) // portrait
    };

    let mut dst = fir::images::Image::new(dst_w, dst_h, fir::PixelType::U8x3);

    let source = fir::images::Image::from_vec_u8(
        src.width as u32,
        src.height as u32,
        src.pixels,
        fir::PixelType::U8x3,
    )
    .ok()?;

    let resize_alg = fir::ResizeAlg::Convolution(fir::FilterType::Lanczos3);
    let resize_opts = fir::ResizeOptions::new().resize_alg(resize_alg);

    resizer.resize(&source, &mut dst, &resize_opts).ok()?;

    let dst_width = dst.width() as usize;
    let dst_height = dst.height() as usize;

    Some(turbojpeg::Image {
        pixels: dst.into_vec(),
        width: dst_width,
        pitch: 3 * dst_width,
        height: dst_height,
        format: turbojpeg::PixelFormat::RGB,
    })
}
