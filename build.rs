//! Embed the git commit into the binary so the running build is identifiable
//! from `serverInfo` and the startup log — the antidote to deploy-skew
//! debugging ("which agentmail is the app actually running?"). Works from
//! whichever checkout compiles the crate (the app's `agentmail-mcp` clone or
//! the standalone repo); no git → "unknown".

fn main() {
    // AGENTMAIL_SHA short-circuits every git call AND every file watch below.
    //
    // The watches are correct locally and pathological in CI. `rerun-if-changed`
    // is compared by MTIME - `[build] fingerprint = "content"` explicitly does
    // not cover build scripts - and a CI checkout rewrites `HEAD`'s mtime on
    // every run. So this build script re-ran every time, and because `agentmail`
    // is a workspace dependency of both `app-api` and `src-tauri` it dragged 33
    // dependent units with it (consuming repo, CI run 33230223593).
    //
    // An env var is compared by VALUE, so the fingerprint is stable across
    // checkouts and still changes the moment the submodule pointer moves. The
    // caller passes this crate's OWN commit, not the superproject's:
    //
    //     AGENTMAIL_SHA=$(git -C agentmail-mcp rev-parse --short=9 HEAD)
    //
    // Unset - a normal local build - and everything below behaves as before.
    if let Ok(sha) = std::env::var("AGENTMAIL_SHA") {
        let sha = sha.trim();
        if !sha.is_empty() {
            println!("cargo:rustc-env=AGENTMAIL_BUILD_SHA={sha}");
            println!("cargo:rerun-if-env-changed=AGENTMAIL_SHA");
            return;
        }
    }
    println!("cargo:rerun-if-env-changed=AGENTMAIL_SHA");

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
    //
    // RESOLVE THE GITDIR FIRST. `.git` is a DIRECTORY only in a standalone
    // clone. When this crate is consumed as a SUBMODULE — which is how the app
    // consumes it — `.git` is a FILE holding `gitdir: ../.git/modules/<name>`,
    // so `.git/HEAD` names a path that does not exist.
    //
    // Cargo treats a `rerun-if-changed` target that is missing as PERMANENTLY
    // DIRTY, so this build script re-ran on every single cargo invocation, and
    // `agentmail` is a workspace dependency of both `app-api` and `src-tauri`.
    // Every `cargo test`/`build`/`run` in the app therefore recompiled
    // agentmail -> app-api -> agent before doing anything else. Measured on CI
    // run 33214508616: a smoke step that should have been a no-op spent 1m42s
    // recompiling, and `cargo report rebuilds` named the cause outright —
    // "agentmail@0.5.0 build-script (run): file missing: agentmail-mcp\.git/HEAD,
    // impact: 4 dependent units rebuilt". It also capped the value of caching
    // workspace crates, since the two most expensive crates were invalidated
    // every run no matter what the cache held.
    //
    // The SHA above was never affected: it shells out to `git`, which resolves
    // the indirection itself. Only these watches assumed a directory.
    let git_dir = match std::fs::read_to_string(".git") {
        // Submodule/worktree: `.git` is a file pointing elsewhere. The path is
        // relative to THIS directory, and cargo resolves rerun-if-changed
        // relative to the manifest dir, so it can be emitted as-is.
        Ok(pointer) => pointer
            .strip_prefix("gitdir:")
            .map(|p| p.trim().to_string())
            .unwrap_or_else(|| ".git".to_string()),
        // Standalone clone: `.git` is a directory, so reading it as a file
        // fails and the original behaviour is correct.
        Err(_) => ".git".to_string(),
    };
    println!("cargo:rerun-if-changed={git_dir}/HEAD");
    println!("cargo:rerun-if-changed={git_dir}/packed-refs");
    // A single flat `if let` over a combinator chain: portable to any toolchain
    // (no `let`-chain, so edition/MSRV-independent) AND not a nested `if`, so it
    // does not trip clippy's `collapsible_if`.
    if let Some(reference) = std::fs::read_to_string(format!("{git_dir}/HEAD"))
        .ok()
        .and_then(|head| head.strip_prefix("ref: ").map(|r| r.trim().to_string()))
    {
        println!("cargo:rerun-if-changed={git_dir}/{reference}");
    }
}
