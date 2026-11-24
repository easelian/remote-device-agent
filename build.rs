//! The page programs are sent to the browser whole with every call, and the
//! browser reads them anew each time. A fifth of each is comment and
//! indentation, which a call a millisecond long can feel: they are written
//! out without it, and that is what the crate embeds.
//!
//! Only whole-line `//` comments, blank lines and leading white space are
//! removed, so every statement stays on its own line. The programs hold no
//! string that runs over several lines; the build fails if one appears.

use std::{env, fs, path::Path};

fn main() {
    let out = env::var("OUT_DIR").expect("cargo sets OUT_DIR");
    for name in ["program.js", "evidence.js"] {
        let path = Path::new("src/browser/semantic").join(name);
        println!("cargo:rerun-if-changed={}", path.display());
        let source = fs::read_to_string(&path).expect("page program is readable");
        let mut lean = String::with_capacity(source.len());
        for line in source.lines() {
            assert!(
                line.matches('`').count() % 2 == 0,
                "{name}: a template string may not run over several lines"
            );
            let line = line.trim_start();
            if line.is_empty() || line.starts_with("//") {
                continue;
            }
            lean.push_str(line);
            lean.push('\n');
        }
        fs::write(Path::new(&out).join(name), lean).expect("OUT_DIR is writable");
    }
}
