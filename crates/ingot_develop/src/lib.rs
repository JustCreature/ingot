//! The RAW develop engine: turns sensor data into a display image, following
//! darktable's scene-referred pipeline.
//!
//! Order of operations, as in darktable: raw prepare, white balance, highlight
//! clipping, demosaic, exposure, input color matrix, sigmoid tone mapping, output.

mod color;
mod demosaic;
mod pipeline;
mod sensor;
mod sigmoid;

pub use demosaic::{BayerPattern, demosaic_rcd};
pub use pipeline::{DevelopParams, DevelopedImage, WhiteBalance, develop, develop_file};
pub use sensor::{SensorImage, load_sensor};
pub use sigmoid::Sigmoid;

pub type DevelopError = Box<dyn std::error::Error + Send + Sync>;
