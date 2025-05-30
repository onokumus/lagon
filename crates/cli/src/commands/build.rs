use std::{fs, path::PathBuf};

use anyhow::{anyhow, Result};

use crate::utils::{
    bundle_function, debug, print_progress, success, validate_code_file, validate_public_dir,
};

pub fn build(file: PathBuf, client: Option<PathBuf>, public_dir: Option<PathBuf>) -> Result<()> {
    validate_code_file(&file)?;

    let client = match client {
        Some(client) => {
            validate_code_file(&client)?;
            Some(client)
        }
        None => None,
    };

    let public_dir = validate_public_dir(public_dir)?;
    let (index, assets) = bundle_function(&file, &client, &public_dir)?;

    let end_progress = print_progress("Writting index.js...");

    fs::create_dir_all(".lagon")?;
    fs::write(".lagon/index.js", index)?;

    end_progress();

    for (path, content) in assets {
        let message = format!("Writting {}...", path);
        let end_progress = print_progress(&message);

        let dir = PathBuf::from(".lagon").join("public").join(
            PathBuf::from(&path)
                .parent()
                .ok_or_else(|| anyhow!("Could not find parent of {}", path))?,
        );
        fs::create_dir_all(dir)?;
        fs::write(format!(".lagon/public/{}", path), content)?;

        end_progress();
    }

    println!();
    println!(
        "{} {}",
        success("Build successful!"),
        debug("You can find it in .lagon folder.")
    );

    Ok(())
}
