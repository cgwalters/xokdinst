use std::{io, fs};
use std::str::FromStr;
extern crate structopt;
use structopt::StructOpt;
use directories;
use failure::Fallible;
#[macro_use]
extern crate lazy_static;

lazy_static! {
    static ref APPDIRS : directories::ProjectDirs = directories::ProjectDirs::from("org", "openshift", "xokdinst").expect("creating appdirs");
}

/// The name of the config used
static DEFAULT_CONFIG : &str = "config.yaml";

#[derive(Debug, StructOpt, PartialEq)]
enum Platform {
    Libvirt,
    AWS,
}

impl FromStr for Platform {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Platform, &'static str> {
        match s {
            "libvirt" => Ok(Platform::Libvirt),
            "aws" => Ok(Platform::AWS),
            _ => Err("invalid platform"),
        }
    }
}

#[derive(Debug, StructOpt)]
#[structopt(name = "xokdinst", about = "Extended OpenShift installer wrapper")]
#[structopt(rename_all = "kebab-case")]
/// Main options struct
enum Opt {
    /// List all configuration sources
    GetConfigs,
    /// List all clusters
    List,
    /// Launch a new cluster
    Launch {
        name: String,
        #[structopt(short = "p", long = "platform")]
        platform: Option<Platform>,
        #[structopt(short = "c", long = "config")]
        /// The name of the base configuration (overrides platform)
        config: Option<String>,
        release_image: Option<String>,
    },
    /// Destroy a running cluster
    Destroy {
        name: String,
    },
}

/// Get all configurations
fn get_configs() -> Fallible<Vec<String>> {
    let mut r = Vec::new();
    for entry in fs::read_dir(APPDIRS.config_dir())? {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str() {
            if name == DEFAULT_CONFIG || (name.starts_with("config-") && name.ends_with(".yaml")) {
                r.push(String::from(name));
            }
        }
    }
    Ok(r)
}

/// Get all cluster names
fn get_clusters() -> Fallible<Vec<String>> {
    let mut r = Vec::new();
    for entry in fs::read_dir(APPDIRS.config_dir())? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if !meta.is_dir() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            r.push(String::from(name));
        }
    }
    Ok(r)
}

fn print_list(header: &str, l: &[String]) {
    if l.len() == 0 {
        println!("No {}.", header);
    } else {
        for v in l.iter() {
            println!("  {}", v);
        }
    }
}

/// Primary entrypoint
fn main() -> Fallible<()> {
    match Opt::from_args() {
        Opt::GetConfigs => {
            let configs = get_configs()?;
            print_list("configs", &configs.as_slice());
        },
        Opt::List => {
            let clusters = get_clusters()?;
            print_list("clusters", &clusters.as_slice());
        },
        _ => unimplemented!(),
    }

    Ok(())
}
