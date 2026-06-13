// use std::time::Instant;

use ingot::preview::make_preview_from_jpeg_bytes;
// use rayon::prelude::*;

fn main() {
    let Some(src) = std::fs::read("testdata/test_exif_read/IMG_1800.JPG").ok() else {
        panic!("lol")
    };
    let Some(new_image) = make_preview_from_jpeg_bytes(&src) else {
        panic!("lol3")
    };
    let Some(_) = std::fs::write("testdata/test_exif_read/example.JPG", new_image).ok() else {
        panic!("lol4")
    };

    // let t = Instant::now();
    // (1..1000).into_par_iter().for_each(|_| {
    //     let Some(decompressed_image) = preview_from_jpeg_bytes(&src) else {
    //         panic!("lol2")
    //     };
    //     let Some(resized_image) = resize(decompressed_image) else {
    //         panic!("lol2")
    //     };
    //     let Some(new_image) = compress(resized_image) else {
    //         panic!("lol3")
    //     };
    // });
    // let elapsed = t.elapsed();
    // println!("with resize elapsed: {elapsed:?}");

    // let t = Instant::now();
    // (1..1000).into_par_iter().for_each(|_| {
    //     let Some(decompressed_image) = preview_from_jpeg_bytes(&src) else {
    //         panic!("lol2")
    //     };
    //     let Some(new_image) = compress(decompressed_image) else {
    //         panic!("lol3")
    //     };
    // });
    // let elapsed = t.elapsed();
    // println!("withou resize elapsed: {elapsed:?}");
}
