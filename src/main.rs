#[macro_use]
extern crate anyhow;

use std::collections::HashMap;
use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek};
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::sync::Mutex;

use anyhow::{Context, Result};

use rayon::iter;
use rayon::prelude::{
    IndexedParallelIterator, IntoParallelRefIterator, ParallelExtend, ParallelIterator,
};

use rix::parsers::derivations;

use tempfile::{tempdir, tempfile};

fn run(cmd: &str, args: &[&str], path: &[&Path]) -> Result<File> {
    let mut command = Command::new(cmd);

    command.env_clear();
    if !path.is_empty() {
        command.current_dir(path[0]);
    }
    command.env("HOME", "/homeless-shelter");
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

fn get_output_hash(drv_path: &Path) -> Result<Option<String>> {
    let drv = fs::read_to_string(drv_path)
        .context(format!("Reading derivation {}", drv_path.display()))?;

    let parsed =
        derivations::parse_derivation(&drv).map_err(|_| anyhow!("Could not parse derivation"))?;

    Ok(parsed
        .1
        .outputs
        .get("out")
        .ok_or(anyhow!("No output named 'out'"))?
        .hash
        .clone())
}

fn is_fod(drv_path: &Path) -> Result<bool> {
    Ok(get_output_hash(drv_path)?.is_some())
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
            roots_path.join(attr).to_str().expect("Path to string"),
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
        ],
        &[],
    )
    .is_ok()
}

fn delete(drv_path: &Path, roots_path: &Path) -> Result<()> {
    let root_path = roots_path.join(drv_path.file_name().expect("Derivation name"));

    run(
        "nix-store",
        &["--delete", root_path.to_str().expect("Path to string")],
        &[],
    )
    .context(format!("Deleting {}", root_path.display()))?;

    Ok(())
}

fn check_all_fods(nixpkgs: &Path) -> Result<HashMap<(String, PathBuf), bool>> {
    let drvs = Mutex::new(HashMap::<PathBuf, String>::new());

    let fods = Mutex::new(HashMap::<(String, PathBuf), bool>::new());

    let roots = tempdir().expect("Roots directory");

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

        drvs.lock().expect("Acquiring derivation mutex").par_extend(
            reqs.par_iter()
                .cloned()
                .zip(iter::repeatn(attr.clone(), reqs.len())),
        );
    });

    drvs.lock()
        .expect("Acquiring derivation mutex")
        .par_iter()
        .for_each(|(drv, attr)| {
            if is_fod(drv).expect("Checking whether derivation is a FOD") {
                println!("Realising {}", drv.display());

                if let Ok(path) = realise(drv, roots.path()) {
                    fods.lock()
                        .expect("Acquiring FOD result mutex")
                        .insert((attr.clone(), drv.to_owned()), check(drv));

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
