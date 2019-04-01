use std::{fs, io};
use std::io::prelude::*;
use std::borrow::Cow;
use std::collections::HashMap;
use structopt::StructOpt;
#[macro_use]
extern crate clap;
use directories;
#[macro_use]
extern crate failure;
use failure::Fallible;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate serde_derive;

lazy_static! {
    static ref APPDIRS : directories::ProjectDirs = directories::ProjectDirs::from("org", "openshift", "xokdinst").expect("creating appdirs");
}

/// Holds extra keys from a map we didn't explicitly parse
type SerdeYamlMap = HashMap<String, serde_yaml::Value>;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum InstallConfigPlatform {
    Libvirt(SerdeYamlMap),
    AWS(SerdeYamlMap),
}

#[derive(Serialize, Deserialize)]
#[allow(dead_code)]
struct InstallConfigMachines {
    name: String,
    replicas: u32,
}

#[derive(Serialize, Deserialize)]
#[allow(dead_code)]
struct InstallConfigMetadata {
    name: String,

    #[serde(flatten)]
    extra: Option<SerdeYamlMap>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct InstallConfig {
    api_version: String,
    base_domain: String,
    compute: Vec<InstallConfigMachines>,
    control_plane: InstallConfigMachines,
    metadata: Option<InstallConfigMetadata>,
    platform: InstallConfigPlatform,
    pull_secret: String,
    ssh_key: Option<String>,

    #[serde(flatten)]
    extra: SerdeYamlMap
}

arg_enum! {
    #[derive(Debug, Clone, PartialEq)]
    enum Platform {
        Libvirt,
        AWS,
    }
}

impl InstallConfigPlatform {
    fn to_platform(&self) -> Platform {
        match self {
            InstallConfigPlatform::Libvirt(_) => Platform::Libvirt,
            InstallConfigPlatform::AWS(_) => Platform::AWS,
        }
    }
}

#[derive(Debug, StructOpt)]
struct LaunchOpts {
    /// Name of the cluster to launch
    name: String,
    #[structopt(short = "p", raw(possible_values = "&Platform::variants()", case_insensitive = "true"))]
    platform: Option<Platform>,
    #[structopt(short = "c", long = "config")]
    /// The name of the base configuration (overrides platform)
    config: Option<String>,
    /// Override the release image (for development/testing)
    release_image: Option<String>,
}

#[derive(Debug, StructOpt)]
struct GenConfigOpts {
    /// Name for this configuration; if not specified, will be named config-<platform>.yaml
    name: Option<String>,
    /// Overwrite an existing default configuration
    overwrite: bool,
}

#[derive(Debug, StructOpt)]
#[structopt(name = "xokdinst", about = "Extended OpenShift installer wrapper")]
#[structopt(rename_all = "kebab-case")]
/// Main options struct
enum Opt {
    /// Generate the default configuration for a given platform
    GenConfig(GenConfigOpts),
    /// List all configuration sources
    ListConfigs,
    /// List all clusters
    List,
    /// Launch a new cluster
    Launch(LaunchOpts),
    Kubeconfig {
        /// Print the kubeconfig path for this cluster
        name: String,
    },
    /// Destroy a running cluster
    Destroy {
        /// Name of cluster to destroy
        name: String,
        /// Ignore failure to delete cluster, remove directory anyways
        #[structopt(short = "f", long = "force")]
        force: bool
    },
}

fn cmd_installer() -> std::process::Command {
    let localbin : &std::path::Path = "bin/openshift-install".as_ref();
    let cmd = if localbin.exists() {
        localbin
    } else {
        "openshift-install".as_ref()
    };
    let mut cmd = std::process::Command::new(cmd);
    // Override the libvirt defaults
    // TODO(walters) compute these from what's available on the host
    // https://github.com/openshift/installer/pull/785
    cmd.env("TF_VAR_libvirt_master_memory", "8192")
        .env("TF_VAR_libvirt_master_vcpu", "4");
    cmd
}

fn run_installer(cmd: &mut std::process::Command) -> Fallible<()> {
    let status = cmd.status().map_err(|e| format_err!("Executing openshift-install").context(e))?;
    if !status.success() {
        bail!("openshift-install failed")
    }
    Ok(())
}

/// Return the filename for a given config, using platform name if necessary
fn get_config_name(name: &Option<String>, platform: &Platform) -> String {
    if let Some(name) = name {
        format!("config-{}.yaml", name)
    } else {
        format!("config-{}.yaml", platform.to_string().to_lowercase())
    }
}

/// Create a config-X.yaml
fn generate_config(o: GenConfigOpts) -> Fallible<String> {
    if let Some(name) = o.name.as_ref() {
        let path = APPDIRS.config_dir().join(&name);
        if !o.overwrite && path.exists() {
            bail!("Configuration '{}' already exists and overwrite not specified", name);
        }
    }

    let tmpd = tempfile::Builder::new().prefix("xokdinst").tempdir()?;
    println!("Executing `openshift-install create install-config`");
    let mut cmd = cmd_installer();
    cmd.args(&["create", "install-config", "--dir"]);
    cmd.arg(tmpd.path());
    run_installer(&mut cmd)?;

    let tmp_config_path = tmpd.path().join("install-config.yaml");
    let mut parsed_config : InstallConfig = serde_yaml::from_reader(io::BufReader::new(fs::File::open(tmp_config_path)?))?;
    let platform = parsed_config.platform.to_platform();
    let name = get_config_name(&o.name, &platform);
    let path = APPDIRS.config_dir().join(&name);
    if !o.overwrite && path.exists() {
        bail!("Configuration '{}' already exists and overwrite not specified", name);
    }

    // Remove the metadata/name value, we want this one to be a template
    match parsed_config.metadata.take() {
        None => bail!("Didn't find expected metadata key"),
        Some(_) => {}
    };

    fs::create_dir_all(APPDIRS.config_dir())?;
    let mut w = io::BufWriter::new(fs::File::create(path)?);
    serde_yaml::to_writer(&mut w, &parsed_config)?;
    w.flush()?;

    Ok(name)
}

/// Get all configurations
fn get_configs() -> Fallible<Vec<String>> {
    let mut r = Vec::new();
    for entry in fs::read_dir(APPDIRS.config_dir())? {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str() {
            if name.starts_with("config-") && name.ends_with(".yaml") {
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

/// 🚀
fn launch(o: LaunchOpts) -> Fallible<()> {
    fs::create_dir_all(APPDIRS.config_dir())?;
    let clusterdir = APPDIRS.config_dir().join(&o.name);
    if clusterdir.exists() {
        bail!("Cluster {} already exists, use destroy to remove it", o.name.as_str());
    }
    let configs = get_configs()?;
    let config_name = if o.config.is_none() && configs.len() == 0 {
        println!("No configurations found; generating one now");
        Cow::Owned(generate_config(GenConfigOpts {
            name: None,
            overwrite: false,
        })?)
    } else if let Some(platform) = o.platform.as_ref() {
        Cow::Owned(get_config_name(&None, &platform))
    } else {
        if let Some(name) = o.config.as_ref() {
            Cow::Borrowed(name)
        } else if configs.len() == 1 {
            Cow::Borrowed(&configs[0])
        } else {
            bail!("Have multiple configs, and no config specified")
        }
    };
    let config_path = APPDIRS.config_dir().join(config_name.as_str());
    if !config_path.exists() {
        bail!("No such configuration: {}", config_name);
    }

    let mut config : InstallConfig = serde_yaml::from_reader(io::BufReader::new(fs::File::open(config_path)?))?;
    config.metadata = Some(InstallConfigMetadata {
        name: o.name.to_string(),
        extra: None,
    });

    fs::create_dir(&clusterdir)?;
    let mut w = io::BufWriter::new(fs::File::create(clusterdir.join("install-config.yaml"))?);
    serde_yaml::to_writer(&mut w, &config)?;
    w.flush()?;

    let mut cmd = cmd_installer();
    if let Some(image) = o.release_image {
        cmd.env("OPENSHIFT_INSTALL_RELEASE_IMAGE_OVERRIDE", image);
    }
    cmd.args(&["create", "cluster", "--dir"]);
    cmd.arg(&clusterdir);
    println!("Executing `openshift-install create cluster`");
    run_installer(&mut cmd)?;

    Ok(())
}

/// Ensure base configdir, and get the path to a cluster
fn get_clusterdir(name: &str) -> Fallible<Box<std::path::Path>> {
    fs::create_dir_all(APPDIRS.config_dir())?;
    let clusterdir = APPDIRS.config_dir().join(name);
    if !clusterdir.exists() {
        bail!("No such cluster: {}", name);
    }
    Ok(clusterdir.into_boxed_path())
}

/// Destroy a cluster
fn destroy(name: &str, force: bool) -> Fallible<()> {
    let clusterdir = get_clusterdir(name)?;

    let mut cmd = cmd_installer();
    cmd.args(&["destroy", "cluster", "--dir"]);
    cmd.arg(&*clusterdir);
    println!("Executing `openshift-install destroy cluster`");
    let status = cmd.status().map_err(|e| format_err!("Executing openshift-install").context(e))?;
    if !status.success() {
        if !force {
            bail!("openshift-install failed")
        } else {
            eprintln!("Warning: destroy cluster failed, continuing anyways")
        }
    }

    fs::remove_dir_all(clusterdir)?;
    Ok(())
}

/// Primary entrypoint
fn main() -> Fallible<()> {
    match Opt::from_args() {
        Opt::GenConfig(o) => {
            generate_config(o)?;
        },
        Opt::ListConfigs => {
            let configs = get_configs()?;
            print_list("configs", &configs.as_slice());
        },
        Opt::List => {
            let clusters = get_clusters()?;
            print_list("clusters", &clusters.as_slice());
        },
        Opt::Launch(o) => {
            launch(o)?;
        },
        Opt::Destroy { name, force } => {
            destroy(&name, force)?;
        },
        Opt::Kubeconfig { name } => {
            let clusterdir = get_clusterdir(&name)?;
            println!("{}", clusterdir.join("auth/kubeconfig").to_str().unwrap());
        },
    }

    Ok(())
}
