fn main() {
    println!("cargo:rerun-if-changed=api-credentials.env");
    for key in ["TG_API_ID", "TG_API_HASH"] {
        println!("cargo:rerun-if-env-changed={key}");
        let value = std::env::var(key).ok().or_else(|| {
            let content = std::fs::read_to_string("api-credentials.env").ok()?;
            content.lines().find_map(|line| {
                let (name, value) = line.split_once('=')?;
                (name.trim() == key).then(|| value.trim().to_owned())
            })
        });
        if let Some(value) = value.filter(|value| !value.contains(['\r', '\n'])) {
            println!("cargo:rustc-env=TELEGA_BUILD_{key}={value}");
        }
    }
}
