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
    // Recompile when the checkout moves so a rebuilt app can't silently keep an
    // old SHA baked in. `.git/HEAD` catches branch switches, but a NEW COMMIT on
    // the current branch only rewrites that branch's ref FILE — and a watch on
    // the `.git/refs` DIRECTORY does not fire on a file-content change — so
    // resolve HEAD to its ref file and watch that directly. `packed-refs` covers
    // a packed (loose-file-absent) ref; a detached HEAD has no `ref:` line and
    // is covered by the HEAD watch itself.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/packed-refs");
    // A single flat `if let` over a combinator chain: portable to any toolchain
    // (no `let`-chain, so edition/MSRV-independent) AND not a nested `if`, so it
    // does not trip clippy's `collapsible_if`.
    if let Some(reference) = std::fs::read_to_string(".git/HEAD")
        .ok()
        .and_then(|head| head.strip_prefix("ref: ").map(|r| r.trim().to_string()))
    {
        println!("cargo:rerun-if-changed=.git/{reference}");
    }
}
