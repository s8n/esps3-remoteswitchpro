use std::env;
use std::path::PathBuf;

fn main() {
    linker_be_nice();
    println!("cargo:rustc-link-arg=-Tlinkall.x");

    // Bake the config into the firmware: your personal config.json if present,
    // the template otherwise.
    println!("cargo:rerun-if-changed=config.json");
    println!("cargo:rerun-if-changed=config.json.template");
    let src = if std::path::Path::new("config.json").exists() {
        "config.json"
    } else {
        println!("cargo:warning=no config.json, building with config.json.template (wifi won't connect)");
        "config.json.template"
    };
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("config.json");
    std::fs::copy(src, out).unwrap();
}

fn linker_be_nice() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        let kind = &args[1];
        let what = &args[2];
        match kind.as_str() {
            "undefined-symbol" => match what.as_str() {
                "_stack_start" => {
                    eprintln!();
                    eprintln!("💡 Is the linker script `linkall.x` missing?");
                    eprintln!();
                }
                what if what.starts_with("esp_rtos_") => {
                    eprintln!();
                    eprintln!("💡 `esp-radio` has no scheduler enabled. Make sure you have initialized `esp-rtos` or provided an external scheduler.");
                    eprintln!();
                }
                _ => (),
            },
            _ => std::process::exit(1),
        }
        std::process::exit(0);
    }
    let script = std::env::current_exe().unwrap();
    println!("cargo:rustc-link-arg=-Wl,--error-handling-script={}", script.display());
}
