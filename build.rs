fn main() {
    println!("cargo:rerun-if-changed=api-credentials.env");
    let local = std::fs::read_to_string("api-credentials.env").unwrap_or_default();
    let bundled = std::env::var_os("CARGO_FEATURE_BUNDLED_APP").is_some();
    for key in ["TG_API_ID", "TG_API_HASH"] {
        println!("cargo:rerun-if-env-changed={key}");
        let environment = std::env::var(key).ok();
        let value = environment
            .as_deref()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                local.lines().find_map(|line| {
                    let (name, value) = line.split_once('=')?;
                    (name.trim() == key).then(|| value.trim())
                })
            })
            .map(str::trim);
        if bundled {
            let valid = value.is_some_and(|v| match key {
                "TG_API_ID" => v.parse::<i32>().is_ok_and(|id| id > 0),
                _ => v.len() == 32 && v.bytes().all(|b| b.is_ascii_hexdigit()),
            });
            assert!(valid, "bundled-app requires a valid {key} at build time");
        }
        if let Some(value) = value.filter(|v| !v.is_empty() && !v.contains(['\r', '\n'])) {
            println!("cargo:rustc-env=TELEGA_BUILD_{key}={value}");
        }
    }
}
