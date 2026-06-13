use ingot_core::{Engine, EngineConfig};

// The UI app. For now this only proves the engine wiring: construct a
// configured engine instance the way the real app will. The UI and the
// streaming `ingest` consumer land in later phases.
fn main() {
    let engine = Engine::new(EngineConfig::default());
    println!("ingot engine ready with config: {:?}", engine.config());
}
