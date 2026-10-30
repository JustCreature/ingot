//! Develops one RAW file to a binary PPM and prints how long each stage took.
//!
//!   cargo run --release -p ingot_develop --example develop -- <raw> <out.ppm>

use std::{fs::File, io::Write, path::Path, time::Instant};

use ingot_develop::{DevelopParams, develop, load_sensor};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [raw, out] = args.as_slice() else {
        eprintln!("usage: develop <raw> <out.ppm>");
        std::process::exit(2);
    };

    let t = Instant::now();
    let sensor = load_sensor(Path::new(raw)).expect("decode failed");
    let decoded = t.elapsed();
    println!("{}x{} {:?}", sensor.width, sensor.height, sensor.pattern);
    println!(
        "wb {:?}, orientation {:?}",
        sensor.camera_wb, sensor.orientation
    );

    let t = Instant::now();
    let image = develop(sensor, &DevelopParams::default()).expect("develop failed");
    println!("decode {decoded:.2?}, develop {:.2?}", t.elapsed());

    let mut file = File::create_new(out).expect("output exists or can't be created");
    write!(file, "P6\n{} {}\n255\n", image.width, image.height).unwrap();
    file.write_all(&image.rgb).unwrap();
}
