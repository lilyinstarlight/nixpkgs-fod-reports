#[macro_use]
extern crate anyhow;

use std::collections::HashMap;
use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::sync::Mutex;

use anyhow::{Context, Result};

use rayon::iter;
use rayon::prelude::{
    IndexedParallelIterator, IntoParallelRefIterator, ParallelExtend, ParallelIterator,
};

use regex::bytes::Regex;

use tempfile::{tempdir, tempfile};

fn run(cmd: &str, args: &[&str], path: &[&Path]) -> Result<File> {
    let nixpkgs_config_dir =
        tempdir().context("Creating temporary directory for Nixpkgs config")?;
    let nixpkgs_config = nixpkgs_config_dir.path().join("nixpkgs-config.nix");

    writeln!(
        File::create(nixpkgs_config.clone()).context("Creating Nixpkgs config file")?,
        "{{ allowAliases = false; }}"
    )
    .context("Writing Nixpkgs config file")?;

    let mut command = Command::new(cmd);

    command.env_clear();
    if !path.is_empty() {
        command.current_dir(path[0]);
    }
    command.env("HOME", "/homeless-shelter");
    command.env("NIXPKGS_CONFIG", nixpkgs_config);
    command.env(
        "NIX_PATH",
        path.iter()
            .map(|p| p.to_str().expect("Path to string"))
            .collect::<Vec<&str>>()
            .join(":"),
    );

    command.args(["--option", "restrict-eval", "true"]);

    command.args(args);

    let stdout = tempfile().context("Creating temporary file for Nix command")?;
    let mut reader = stdout
        .try_clone()
        .context("Creating reader for temporary file")?;

    let status = command
        .stdout(Stdio::from(stdout))
        .status()
        .context("Running Nix command")?;

    reader
        .rewind()
        .context("Rewinding temporary file for reading the Nix output")?;

    if status.success() {
        Ok(reader)
    } else {
        Err(anyhow!("Nix process failed, see above output"))
    }
}

fn is_fod(drv_path: &Path) -> Result<bool> {
    let drv = fs::read(drv_path).context(format!("Reading derivation {}", drv_path.display()))?;

    Ok(Regex::new(r#"(?-u)^Derive\(\s*\[\s*\(\s*"(?:[^"]+)"\s*,\s*"(?:[^"]+)"\s*,\s*"(?:[^"]+)"\s*,\s*"(?:[^"]+)"\s*\)"#).unwrap().is_match(&drv))
}

fn attrs(nixpkgs: &Path) -> Result<Vec<String>> {
    let output = run(
        "nix-env",
        &[
            "--query",
            "--available",
            "--no-name",
            "--attr-path",
            "-f",
            ".",
        ],
        &[nixpkgs],
    )?;

    Ok(BufReader::new(output)
        .lines()
        .map(|line| line.expect("Read output lines"))
        .collect())
}

fn instantiate(nixpkgs: &Path, attr: &str, roots_path: &Path) -> Result<PathBuf> {
    let output = run(
        "nix-instantiate",
        &[
            ".",
            "-A",
            attr,
            "--add-root",
            roots_path
                .join("attrs")
                .join(attr)
                .to_str()
                .expect("Path to string"),
        ],
        &[nixpkgs],
    )?;

    PathBuf::from(
        BufReader::new(output)
            .lines()
            .next()
            .ok_or(anyhow!("No derivation in Nix output"))?
            .context("Reading Nix output")?,
    )
    .read_link()
    .context("Finding GC root target")
}

fn release(attr: &str, roots_path: &Path) -> Result<()> {
    let root_path = roots_path.join("attrs").join(attr);

    fs::remove_file(root_path).context("Deleting attribute GC root")
}

fn requisites(drv_path: &Path) -> Result<Vec<PathBuf>> {
    let output = run(
        "nix-store",
        &[
            "--query",
            "--requisites",
            drv_path.to_str().expect("Path to string"),
        ],
        &[],
    )?;

    Ok(BufReader::new(output)
        .lines()
        .map(|line| line.expect("Read output lines").into())
        .collect())
}

fn realise(drv_path: &Path, roots_path: &Path) -> Result<PathBuf> {
    let output = run(
        "nix-store",
        &[
            "--realise",
            drv_path.to_str().expect("Path to string"),
            "--add-root",
            roots_path
                .join("drvs")
                .join(drv_path.file_name().expect("Derivation name"))
                .to_str()
                .expect("Path to string"),
        ],
        &[],
    )?;

    PathBuf::from(
        BufReader::new(output)
            .lines()
            .next()
            .ok_or(anyhow!("No derivation in Nix output"))?
            .context("Reading Nix output")?,
    )
    .read_link()
    .context("Finding GC root target")
}

fn check(drv_path: &Path) -> bool {
    run(
        "nix-store",
        &[
            "--realise",
            "--check",
            drv_path.to_str().expect("Path to string"),
            "--no-gc-warning",
        ],
        &[],
    )
    .is_ok()
}

fn delete(drv_path: &Path, roots_path: &Path) -> Result<()> {
    let root_path = roots_path
        .join("drvs")
        .join(drv_path.file_name().expect("Derivation name"));

    run(
        "nix-store",
        &["--delete", root_path.to_str().expect("Path to string")],
        &[],
    )
    .context(format!("Deleting {}", root_path.display()))?;

    Ok(())
}

fn check_all_fods(nixpkgs: &Path) -> Result<HashMap<(String, PathBuf), bool>> {
    let cache = env::var("NIXPKGS_FOD_REPORTS_DRV_CACHE").unwrap_or_default();

    let drvs = Mutex::new(HashMap::<PathBuf, String>::new());
    let fods = Mutex::new(HashMap::<(String, PathBuf), bool>::new());

    let roots = tempdir().expect("Roots directory");

    if !cache.is_empty() && Path::new(&cache).try_exists().unwrap_or(false) {
        drvs.lock().expect("Acquiring derivation mutex").extend(
            serde_json::from_str::<HashMap<PathBuf, String>>(
                &fs::read_to_string(&cache).context("Reading derivation cache file")?,
            )
            .context("Deserializing derivation cache")?,
        );
    }

    println!("Generating attrs to check in {}", nixpkgs.display());

    attrs(nixpkgs)?.par_iter().for_each(|attr| {
        println!("Instantiating {}", attr);

        let reqs = if let Ok(drv) = instantiate(nixpkgs, attr, roots.path()) {
            if !drvs
                .lock()
                .expect("Acquiring derivation mutex")
                .contains_key(&drv)
            {
                println!("Getting requisites for {}", drv.display());

                requisites(&drv).expect("Getting requisite derivations")
            } else {
                println!("Ignoring duplicate derivation {}", drv.display());
                vec![]
            }
        } else {
            eprintln!("Evaluation for {} failed", attr);

            vec![]
        };

        if let Err(_err) = release(attr, roots.path()) {
            eprintln!("Failed to release derivation root for {}, ignoring", attr);
        }

        drvs.lock().expect("Acquiring derivation mutex").par_extend(
            reqs.par_iter()
                .cloned()
                .zip(iter::repeatn(attr.clone(), reqs.len())),
        );
    });

    if !cache.is_empty() {
        fs::write(
            &cache,
            serde_json::to_string(&*drvs.lock().expect("Acquiring derivation mutex"))
                .context("Serializing derivation cache")?,
        )
        .context("Writing derivation cache file")?;
    }

    drvs.lock()
        .expect("Acquiring derivation mutex")
        .par_iter()
        .for_each(|(drv, attr)| {
            if !drv.exists() {
                if let Err(_err) = instantiate(nixpkgs, attr, roots.path()) {
                    eprintln!(
                        "Error re-instantiating derivation from {} at {}",
                        attr,
                        drv.display()
                    );
                }
            }

            match is_fod(drv) {
                Ok(fod) => {
                    if !fod {
                        return;
                    }
                }
                Err(_err) => {
                    eprintln!(
                        "Error checking whether derivation at {} is a FOD, assuming not",
                        drv.display()
                    );
                    return;
                }
            }

            println!("Realising {}", drv.display());

            if let Ok(path) = realise(drv, roots.path()) {
                fods.lock()
                    .expect("Acquiring FOD result mutex")
                    .insert((attr.clone(), drv.to_owned()), check(drv));

                if let Err(_err) = release(attr, roots.path()) {
                    eprintln!("Failed to release derivation root for {}, ignoring", attr);
                }

                if let Err(_err) = delete(drv, roots.path()) {
                    eprintln!(
                        "Error removing root and output path from {} at {}",
                        drv.display(),
                        path.display(),
                    );
                }
            } else {
                eprintln!(
                    "Error realising derivation from {} at {}",
                    attr,
                    drv.display(),
                );
            }
        });

    Ok(fods.into_inner().expect("Consuming FOD result mutex"))
}

fn main() {
    let args: Vec<String> = env::args().collect();

    match check_all_fods(Path::new(&args[1])) {
        Ok(fods) => {
            for ((attr, drv), reproduced) in fods {
                if !reproduced {
                    println!("FOD from {} at {} is not reproducible", attr, drv.display());
                }
            }
        }
        Err(err) => {
            eprintln!("Erroring reproducing all FODs: {}", err);
            process::exit(1);
        }
    }
}
