use anyhow::Result;
use vergen::{vergen, Config};

fn main() -> Result<()> {
    // Generate build/cargo environment variables for the version banner.
    // The `git` feature is intentionally disabled: it pulls native libgit2,
    // which breaks MSVC build-script linking and complicates cross builds.
    vergen(Config::default())
}
