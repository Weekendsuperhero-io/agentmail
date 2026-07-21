//! Embed the git commit into the binary so the running build is identifiable
//! from `serverInfo` and the startup log — the antidote to deploy-skew
//! debugging ("which agentmail is the app actually running?"). Works from
//! whichever checkout compiles the crate (the app's `agentmail-mcp` clone or
//! the standalone repo); no git → "unknown".

fn main() {
    let sha = std::process::Command::new("git")
        .args(["rev-parse", "--short=9", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|sha| !sha.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let dirty = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .is_some_and(|output| output.status.success() && !output.stdout.is_empty());
    println!(
        "cargo:rustc-env=AGENTMAIL_BUILD_SHA={sha}{}",
        if dirty { "-dirty" } else { "" }
    );
    // Recompile when the checkout moves so a rebuilt app can't silently keep
    // an old SHA baked in.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");
}
