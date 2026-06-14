use std::path::Path;

use ingot_core::{Engine, EngineConfig, IngestEvent};

// The UI app. For now this only proves the engine wiring: construct a
// configured engine instance the way the real app will. The UI and the
// streaming `ingest` consumer land in later phases.
fn main() {
    let mut engine = Engine::new(EngineConfig::default());
    println!("ingot engine ready with config: {:?}", engine.config());
    let source = Path::new("crates/ingot_core/testdata/test_exif_read");
    engine.scan(source);
    let rx = engine.ingest();

    // println!("{:?}", engine.scan_response);

    for IngestEvent::Preview(item) in rx.into_iter() {
        println!("{:?}", item);
        println!("----");
        let out = Path::new("ingot_bench_preview.JPG");
        let readable_scan_response = engine.scan_response.read().unwrap();
        let img = readable_scan_response
            .assets
            .get(&item.key)
            .expect("msg")
            .image_data
            .thumb
            .clone()
            .expect("msg");
        let Some(_) = std::fs::write(out, img).ok() else {
            panic!("lol4")
        };

        std::thread::sleep(std::time::Duration::from_millis(300));

        let img = readable_scan_response
            .assets
            .get(&item.key)
            .expect("msg")
            .image_data
            .preview
            .clone()
            .expect("msg");
        let Some(_) = std::fs::write(out, img).ok() else {
            panic!("lol4")
        };

        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}
